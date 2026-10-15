//! Certificates: loading them, and choosing one for a name the client claimed.
//!
//! This is the only place in the project that reads key material, and it exists because SNI
//! happens *before* any HTTP request: the client names the host in the handshake, so the
//! certificate has to be selected without a `Host` header to read. Site selection cannot help
//! — it runs on a request that does not exist yet.
//!
//! What it shares with site selection is the *rule*, not the table:
//! [`volucer_core::HostMap`] does exact-name-then-longest-wildcard there and here, so a name
//! that selects a site selects the matching certificate.
//!
//! # Why the resolver is cheap
//!
//! rustls calls [`rustls::server::ResolvesServerCert::resolve`] once per handshake, on whatever
//! thread the handshake is on, and it must be `Send + Sync`. The map is behind one pointer that
//! a reload replaces whole, so a resolve is one pointer load, one lookup and an `Arc` clone —
//! no allocation, no lock, and it is on the handshake path rather than the request path.
//!
//! # Why the map is replaceable at all
//!
//! A certificate issued while the process is running — by ACME, or by an operator who renewed
//! one by hand — has to appear in the map that is consulted *now*, not on the next restart.
//! Everything else here follows from that: the resolver handed to rustls is stable and what is
//! behind it moves, and replacing it is a whole-or-nothing operation that fails without
//! touching what is already serving.
//!
//! # What is *not* replaceable here, and must therefore be reported
//!
//! [`server_config`] bakes `min_version` and `alpn` into the [`rustls::ServerConfig`] that
//! `bind_with_tls` wraps in the acceptor. Those are **restart-only**, and a reload that appeared
//! to apply them while the acceptor kept the old values would be exactly the half-reload this
//! project refuses to do. `volucer check` and the reload output name them for that reason.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use volucer_core::config::{Config, MinTlsVersion, TlsConfig};
use volucer_core::hostmap::HostMap;

use volucer_watch::Swappable;

/// A certificate and its key, loaded and paired.
///
/// `CertifiedKey` rather than raw DER because rustls needs the key's *signing* form, which it
/// derives from the key material; deriving that per handshake would be work for no reason.
pub type LoadedCert = Arc<CertifiedKey>;

/// The certificate table, as one replaceable value.
///
/// Separate from [`SniResolver`] so that the resolver rustls holds is stable while the table
/// behind it moves. Both halves are one value for the same reason [`crate::Reloaded`] is: the
/// per-host entries and the fallback are two answers to one question — *which certificate is
/// this name served under* — and a request that took one from each generation could present a
/// certificate the deployment had already replaced.
pub struct Certs {
    /// Host pattern to certificate. Rebuilt on a reload rather than mutated in place, so a
    /// resolve is a lookup with no lock to take.
    hosts: HostMap<LoadedCert>,
    /// The certificate for a name nothing claims.
    default: LoadedCert,
}

impl std::fmt::Debug for Certs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Certs")
            .field("named", &self.hosts.len())
            .finish_non_exhaustive()
    }
}

impl Certs {
    /// Build a table from a fallback certificate and the names that get their own.
    ///
    /// The fallback is required rather than optional because a handshake for a name nothing
    /// claims must still complete — that is what makes an unknown SNI name reach the `Host`
    /// decision instead of failing before it.
    pub fn new(default: LoadedCert, hosts: impl IntoIterator<Item = (String, LoadedCert)>) -> Self {
        let mut map: HostMap<LoadedCert> = HostMap::new();
        for (host, cert) in hosts {
            map.insert(&host, cert);
        }
        Certs {
            hosts: map,
            default,
        }
    }

    /// The table with one more name pointing at a certificate.
    ///
    /// Rebuilds rather than mutates, and that is the point: rustls may be resolving against the
    /// current table on another thread, so an edit in place would be visible mid-lookup. The
    /// caller installs the new value with [`SniResolver::replace`].
    #[must_use]
    pub fn with_host(&self, host: &str, cert: LoadedCert) -> Self {
        let mut map = self.hosts.clone();
        map.insert(host, cert);
        Certs {
            hosts: map,
            default: Arc::clone(&self.default),
        }
    }

    /// The certificate for a name, or the fallback when nothing claims it.
    pub fn for_name(&self, name: &str) -> &LoadedCert {
        self.hosts.get(name).unwrap_or(&self.default)
    }

    /// The fallback certificate, for a deployment with no config of its own.
    pub fn default_cert(&self) -> &LoadedCert {
        &self.default
    }

    /// How many host patterns name their own certificate.
    pub fn named(&self) -> usize {
        self.hosts.len()
    }
}

/// Resolve a certificate for a name the client claimed in the handshake.
///
/// Falls back to the default entry for a name nothing claims, which is what makes an unknown
/// SNI name a handshake that succeeds and then meets whatever site the `Host` selects — rather
/// than a failed connection for a request that might have been perfectly legitimate. A client
/// may claim one name and send another; that is legal, and the two decisions are separate.
///
/// # Why this is not rebuilt when the table is
///
/// rustls owns this resolver for the life of the [`rustls::ServerConfig`] it was installed
/// into, and that config is built once. So the resolver is the stable part and the table is
/// what moves: [`refresh`](SniResolver::refresh) loads the current table, and a handshake that
/// arrives after a reload reads the new one without anything being reconfigured.
///
/// `T: Send + Sync` is rustls' requirement on this trait, and `HostMap<LoadedCert>` provides it
/// as long as the map is only read — which is why the map is replaced rather than mutated.
#[derive(Debug)]
pub struct SniResolver {
    /// Where relative `certificate`/`key` paths resolve, kept so a reload can read the same
    /// files the startup read did.
    ///
    /// `PathBuf` rather than the borrowed `&Path` [`from_config`](SniResolver::from_config)
    /// takes, because the resolver outlives the call: the config it was built for is dropped
    /// when `serve` returns, while the listener keeps serving.
    config_dir: PathBuf,
    /// The table, behind one pointer.
    state: Arc<Swappable<Certs>>,
}

/// Which certificates a deployment serves, as the config states them.
///
/// Two sources, deliberately merged into one value: a [`Config`] describes a *whole*
/// deployment, while a *reload* often changes only certificates — an ACME renewal, or an
/// operator replacing a renewed PEM. Taking `Config` as the only constructor would force every
/// certificate update to round-trip through a config file, which is the slower loop with more
/// ways to fail. Both are accepted; both produce the same table.
#[derive(Debug, Clone)]
pub struct CertificateSet {
    /// The certificate `[server.tls]` names, if it is configured.
    pub server: Option<ServerCertificate>,
    /// Per-site certificates, by site name, each with the hosts it covers.
    pub sites: Vec<SiteCertificate>,
}

/// A certificate pair for the whole listener, named as `[server.tls]` names it.
///
/// A name and a key rather than a whole [`TlsConfig`], because everything else in that struct
/// is a property of the [`rustls::ServerConfig`] rather than of the certificate — and those are
/// **restart-only** (§ `tls.rs`'s module doc). Storing them here would invite a reload that
/// appeared to apply them.
#[derive(Debug, Clone)]
pub struct ServerCertificate {
    /// PEM certificate chain, leaf first. Relative paths resolve against the config file.
    pub certificate: PathBuf,
    /// PEM private key.
    pub key: PathBuf,
}

/// One site's certificate and the host patterns it is served for.
#[derive(Debug, Clone)]
pub struct SiteCertificate {
    /// The site's name, for the error message when the pair cannot be loaded.
    pub name: String,
    /// The host patterns from `[sites.<name>].hosts`, unnormalised — `HostMap` normalises.
    pub hosts: Vec<String>,
    /// The certificate and key.
    pub tls: SiteCertificatePair,
}

/// A per-site certificate pair, mirroring `[sites.<n>.tls]`.
///
/// Deliberately a separate type from [`ServerCertificate`] rather than a shared one: the two
/// are separate tables in the config, and merging them here would make a future key valid in
/// one of them silently valid in the other.
#[derive(Debug, Clone)]
pub struct SiteCertificatePair {
    /// PEM certificate chain for this site's hosts.
    pub certificate: PathBuf,
    /// PEM private key for this site's hosts.
    pub key: PathBuf,
}

impl CertificateSet {
    /// The certificates a configuration states.
    ///
    /// Reads `[server.tls]` and every `[sites.*.tls]`, and nothing else — so a reload of the
    /// table alone does not depend on the rest of the file being unchanged. A site naming no
    /// certificate contributes nothing rather than an error: it is served under the server's
    /// certificate, which is the same rule `SniResolver::from_config` has always applied.
    pub fn from_config(config: &Config) -> Self {
        CertificateSet {
            server: config.server.tls.as_ref().map(|tls| ServerCertificate {
                certificate: tls.certificate.clone(),
                key: tls.key.clone(),
            }),
            sites: config
                .sites
                .iter()
                .filter_map(|(name, site)| {
                    let tls = site.tls.as_ref()?;
                    Some(SiteCertificate {
                        name: name.clone(),
                        hosts: site.hosts.clone(),
                        tls: SiteCertificatePair {
                            certificate: tls.certificate.clone(),
                            key: tls.key.clone(),
                        },
                    })
                })
                .collect(),
        }
    }
}

impl SniResolver {
    /// Build a resolver from a config and the directory the config file lives in.
    ///
    /// `config_dir` is where relative `certificate`/`key` paths resolve, which is a deliberate
    /// exception to `document_root`'s rule — see [`volucer_core::config::TlsConfig`].
    ///
    /// Fails rather than warning, and that is the opposite of `preflight` on purpose: preflight
    /// is a diagnostic that must tolerate a certificate that is not there *yet*, while this
    /// runs at startup and a certificate that cannot load must stop the listener rather than
    /// produce one that accepts connections and fails every handshake.
    pub fn from_config(config: &Config, config_dir: &Path) -> Result<Self, TlsError> {
        let state = load(&CertificateSet::from_config(config), config_dir)?;
        Ok(SniResolver {
            config_dir: config_dir.to_path_buf(),
            state: Arc::new(Swappable::new(state)),
        })
    }

    /// Build a resolver around a certificate table the caller has already loaded.
    ///
    /// For a caller that obtains certificates itself rather than from a config file — ACME —
    /// and for tests that want a resolver with a known table.
    pub fn from_certs(state: Certs, config_dir: &Path) -> Self {
        SniResolver {
            config_dir: config_dir.to_path_buf(),
            state: Arc::new(Swappable::new(state)),
        }
    }

    /// Replace the certificate table, loading the certificates named by `certificates`.
    ///
    /// Whole-or-nothing: every certificate in the set is loaded and paired **before** anything
    /// is installed, so a set with one unreadable pair is refused entirely and the deployment
    /// keeps serving the table it had. That is the only safe direction — a table that dropped
    /// one entry would fail every handshake for the names it covered, which is a worse failure
    /// than the one that was being reported.
    ///
    /// Returns the table that was replaced, so a caller can say *which* certificate changed
    /// rather than only that the file was read.
    pub fn refresh(&self, certificates: &CertificateSet) -> Result<Arc<Certs>, TlsError> {
        let next = load(certificates, &self.config_dir)?;
        Ok(self.state.replace(next))
    }

    /// Install a certificate table that is already in memory.
    ///
    /// For a certificate whose bytes never went through a file this process wrote — an ACME
    /// issuance. The PEM is still written for durability and for an operator to inspect, but
    /// correctness does not depend on re-reading it, and that is what keeps a renewal off the
    /// filesystem at the moment it matters.
    pub fn replace(&self, state: Certs) -> Arc<Certs> {
        self.state.replace(state)
    }

    /// The certificate table being served right now.
    pub fn current(&self) -> Arc<Certs> {
        self.state.load()
    }

    /// How many host patterns name their own certificate.
    pub fn named_certificates(&self) -> usize {
        self.state.load().named()
    }
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        // One load, once per handshake: the table this handshake resolves against is one
        // generation, so a reload landing here cannot present a certificate from neither.
        let certs = self.state.load();
        // `server_name` is what the client claimed. Absent is normal: a client may connect by
        // IP, or a protocol may not carry SNI at all, and the server's certificate is the
        // answer for both.
        match hello.server_name() {
            Some(name) => Some(Arc::clone(certs.for_name(name))),
            None => Some(Arc::clone(certs.default_cert())),
        }
    }
}

/// Load every certificate a set names, or fail with the first that cannot be read.
///
/// Nothing is installed until all of them have loaded, which is what makes a refresh
/// whole-or-nothing. The `where_` strings match the ones startup has always produced, so a
/// certificate that fails on a reload and on startup is reported the same way.
fn load(certificates: &CertificateSet, config_dir: &Path) -> Result<Certs, TlsError> {
    let server = certificates
        .server
        .as_ref()
        .ok_or(TlsError::NotConfigured)?;
    let default = load_certified(
        &pair(&server.certificate, &server.key),
        config_dir,
        "server.tls",
    )?;

    let mut named: Vec<(String, LoadedCert)> = Vec::new();
    for site in &certificates.sites {
        let loaded = load_certified(
            &pair(&site.tls.certificate, &site.tls.key),
            config_dir,
            &format!("sites.{}.tls", site.name),
        )?;
        for host in &site.hosts {
            named.push((host.clone(), Arc::clone(&loaded)));
        }
    }

    Ok(Certs::new(default, named))
}

/// A pair as [`TlsConfig`], for the one function that reads certificate files.
///
/// The handshake-wide keys are placeholders `load_certified` never reads — it takes the chain
/// and the key and pairs them — and they are not settable here so that a caller cannot believe
/// it has set them.
fn pair(certificate: &Path, key: &Path) -> TlsConfig {
    TlsConfig {
        certificate: certificate.to_path_buf(),
        key: key.to_path_buf(),
        min_version: MinTlsVersion::default(),
        alpn: Vec::new(),
        plain: false,
    }
}

/// Install the process-wide crypto provider, once.
///
/// rustls 0.23 leaves the provider to the crate features but only *installs* it lazily, and
/// only on the paths that ask for it — `ServerConfig::builder` does, `CertifiedKey::from_der`
/// does not. So a build that loads a certificate before it builds a config finds no provider
/// and fails, which is what happened the first time this ran. Installing it here makes the
/// order irrelevant.
///
/// Idempotent, and idempotent in the direction that matters: a second call with a different
/// provider is an error rustls reports, and it is ignored rather than propagated because the
/// first install already won and re-reporting it would turn a working deployment into a
/// failing one.
pub fn install_crypto_provider() {
    use std::sync::OnceLock;
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        // Ignored: `install_default` only fails if something already installed one, which is
        // the state we want anyway.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Load a certificate chain and private key, and pair them.
pub fn load_certified(
    tls: &TlsConfig,
    config_dir: &Path,
    where_: &str,
) -> Result<LoadedCert, TlsError> {
    install_crypto_provider();
    let cert_path = resolve(config_dir, &tls.certificate);
    let key_path = resolve(config_dir, &tls.key);

    let chain = load_chain(&cert_path).map_err(|source| TlsError::Certificate {
        where_: where_.to_string(),
        path: cert_path.display().to_string(),
        source,
    })?;
    let key = load_key(&key_path).map_err(|source| TlsError::Key {
        where_: where_.to_string(),
        path: key_path.display().to_string(),
        source,
    })?;

    // The provider is installed by `install_crypto_provider` above, which is what makes the
    // order of loading and configuring irrelevant.
    let provider =
        rustls::crypto::CryptoProvider::get_default().ok_or(TlsError::NoCryptoProvider)?;
    CertifiedKey::from_der(chain, key, provider.as_ref())
        .map(Arc::new)
        .map_err(|source| TlsError::KeyDoesNotMatch {
            where_: where_.to_string(),
            source,
        })
}

/// Build rustls' server configuration for this deployment.
pub fn server_config(
    config: &Config,
    config_dir: &Path,
    resolver: Arc<SniResolver>,
) -> Result<Arc<rustls::ServerConfig>, TlsError> {
    let tls = config.server.tls.as_ref().ok_or(TlsError::NotConfigured)?;

    let versions: &[&'static rustls::SupportedProtocolVersion] = match tls.min_version {
        MinTlsVersion::V1_2 => &[&rustls::version::TLS13, &rustls::version::TLS12],
        MinTlsVersion::V1_3 => &[&rustls::version::TLS13],
    };

    let mut server = rustls::ServerConfig::builder_with_protocol_versions(versions)
        .with_no_client_auth()
        .with_cert_resolver(resolver);

    // ALPN decided the protocol, so it must be exactly what the config asks for — `h2` first,
    // and `http/1.1` present unless someone deliberately removed it. `validate` has already
    // refused an unknown name, so passing them through cannot advertise something unroutable.
    server.alpn_protocols = tls.alpn.iter().map(|p| p.as_bytes().to_vec()).collect();

    let _ = config_dir;
    Ok(Arc::new(server))
}

/// Resolve a certificate path against the config file's directory.
fn resolve(config_dir: &Path, path: &Path) -> std::path::PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    config_dir.join(path)
}

/// Read a PEM certificate chain.
fn load_chain(path: &Path) -> Result<Vec<CertificateDer<'static>>, ChainError> {
    let file = std::fs::File::open(path).map_err(ChainError::Io)?;
    let chain: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut BufReader::new(file))
        .collect::<Result<_, _>>()
        .map_err(ChainError::Pem)?;
    if chain.is_empty() {
        return Err(ChainError::Empty);
    }
    Ok(chain)
}

/// Read a PEM private key.
///
/// Accepts PKCS#8, PKCS#1 and SEC1, because which one a `openssl` invocation produced is not
/// something an operator should have to think about, and rustls can use all three.
fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>, KeyError> {
    let file = std::fs::File::open(path).map_err(KeyError::Io)?;
    rustls_pemfile::private_key(&mut BufReader::new(file))
        .map_err(KeyError::Pem)?
        .ok_or(KeyError::Empty)
}

/// Something wrong with the TLS configuration or its files.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("[server.tls] is not configured, so there is nothing to serve TLS with")]
    NotConfigured,

    #[error("no usable crypto provider is installed in this build")]
    NoCryptoProvider,

    #[error("{where_}: cannot read the certificate {path}: {source}")]
    Certificate {
        where_: String,
        path: String,
        #[source]
        source: ChainError,
    },

    #[error("{where_}: cannot read the private key {path}: {source}")]
    Key {
        where_: String,
        path: String,
        #[source]
        source: KeyError,
    },

    #[error("{where_}: the private key does not match the certificate: {source}")]
    KeyDoesNotMatch {
        where_: String,
        #[source]
        source: rustls::Error,
    },
}

/// Why a certificate chain could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    #[error("{0}")]
    Io(std::io::Error),
    #[error("the PEM is malformed: {0}")]
    Pem(std::io::Error),
    #[error("the file contains no certificate")]
    Empty,
}

/// Why a private key could not be read.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("{0}")]
    Io(std::io::Error),
    #[error("the PEM is malformed: {0}")]
    Pem(std::io::Error),
    #[error("the file contains no PKCS#8, PKCS#1 or SEC1 private key")]
    Empty,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The certificate pair the end-to-end fixture uses, so a test and a running server agree.
    fn fixture(name: &str) -> (std::path::PathBuf, Config) {
        let dir = std::env::temp_dir().join(format!("volucer-tls-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        (dir, Config::default())
    }

    #[test]
    fn a_config_without_tls_has_no_resolver() {
        let (dir, config) = fixture("none");
        let err = SniResolver::from_config(&config, &dir).expect_err("must fail");
        assert!(matches!(err, TlsError::NotConfigured), "{err}");
        assert!(
            err.to_string().contains("nothing to serve TLS with"),
            "{err}"
        );
    }

    #[test]
    fn a_missing_certificate_names_the_file_and_the_layer() {
        let (dir, _) = fixture("missing");
        let text = r#"
[server]
http = "127.0.0.1:443"

[server.tls]
certificate = "certs/default.crt"
key = "certs/default.key"
"#;
        let config = Config::from_toml(text).expect("parses");
        let err = SniResolver::from_config(&config, &dir).expect_err("must fail");

        // The message must name the *resolved* path, because `certs/default.crt` names two
        // different files depending on the directory it is resolved against.
        let message = err.to_string();
        assert!(message.contains("server.tls"), "{message}");
        assert!(message.contains("certs"), "{message}");
        assert!(message.contains("default.crt"), "{message}");
    }
}
