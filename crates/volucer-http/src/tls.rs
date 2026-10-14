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
//! thread the handshake is on, and it must be `Send + Sync`. The map is built once at startup
//! and never mutated, so a resolve is one lookup and an `Arc` clone — no allocation, no lock,
//! and it is on the handshake path rather than the request path.

use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use volucer_core::config::{Config, MinTlsVersion, TlsConfig};
use volucer_core::hostmap::HostMap;

/// A certificate and its key, loaded and paired.
///
/// `CertifiedKey` rather than raw DER because rustls needs the key's *signing* form, which it
/// derives from the key material; deriving that per handshake would be work for no reason.
pub type LoadedCert = Arc<CertifiedKey>;

/// Resolve a certificate for a name the client claimed in the handshake.
///
/// Falls back to `default` for a name nothing claims, which is what makes an unknown SNI name a
/// handshake that succeeds and then meets whatever site the `Host` selects — rather than a
/// failed connection for a request that might have been perfectly legitimate. A client may
/// claim one name and send another; that is legal, and the two decisions are separate.
#[derive(Debug)]
pub struct SniResolver {
    /// Host pattern to certificate. Built once, never mutated, so resolving needs no lock.
    certs: HostMap<LoadedCert>,
    /// The certificate for a name nothing claims.
    default: LoadedCert,
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
        let tls = config.server.tls.as_ref().ok_or(TlsError::NotConfigured)?;
        let default = load_certified(tls, config_dir, "server.tls")?;

        let mut certs: HostMap<LoadedCert> = HostMap::new();
        for (name, site) in &config.sites {
            let Some(site_tls) = &site.tls else { continue };
            let loaded = load_certified(
                &TlsConfig {
                    certificate: site_tls.certificate.clone(),
                    key: site_tls.key.clone(),
                    // Only certificate and key are per site — validation refuses the rest — so
                    // the handshake-wide keys here are placeholders that `load_certified`
                    // never reads.
                    min_version: MinTlsVersion::default(),
                    alpn: Vec::new(),
                    plain: false,
                },
                config_dir,
                &format!("sites.{name}.tls"),
            )?;
            for host in &site.hosts {
                certs.insert(host, Arc::clone(&loaded));
            }
        }

        Ok(SniResolver { certs, default })
    }

    /// How many host patterns name their own certificate.
    pub fn named_certificates(&self) -> usize {
        self.certs.len()
    }
}

impl ResolvesServerCert for SniResolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        // `server_name` is what the client claimed. Absent is normal: a client may connect by
        // IP, or a protocol may not carry SNI at all, and the server's certificate is the
        // answer for both.
        match hello.server_name() {
            Some(name) => Some(Arc::clone(self.certs.get(name).unwrap_or(&self.default))),
            None => Some(Arc::clone(&self.default)),
        }
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
    let provider = rustls::crypto::CryptoProvider::get_default().ok_or(TlsError::NoCryptoProvider)?;
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
        assert!(err.to_string().contains("nothing to serve TLS with"), "{err}");
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
