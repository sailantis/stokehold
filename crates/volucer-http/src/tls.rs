//! Certificates: loading them, and choosing one for a name the client claimed.
//!
//! This is the only place in the project that reads key material, and it exists because SNI
//! happens *before* any HTTP request: the client names the host in the handshake, so the
//! certificate has to be selected without a `Host` header to read. Site selection cannot help
//! â€” it runs on a request that does not exist yet.
//!
//! What it shares with site selection is the *rule*, not the table:
//! [`volucer_core::HostMap`] does exact-name-then-longest-wildcard there and here, so a name
//! that selects a site selects the matching certificate.
//!
//! # Why the resolver is cheap
//!
//! rustls calls [`rustls::server::ResolvesServerCert::resolve`] once per handshake, on whatever
//! thread the handshake is on, and it must be `Send + Sync`. The map is behind one pointer that
//! a reload replaces whole, so a resolve is one pointer load, one lookup and an `Arc` clone â€”
//! no allocation, no lock, and it is on the handshake path rather than the request path.
//!
//! # Why the map is replaceable at all
//!
//! A certificate issued while the process is running â€” by ACME, or by an operator who renewed
//! one by hand â€” has to appear in the map that is consulted *now*, not on the next restart.
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

use volucer_core::config::{Config, MinTlsVersion};
use volucer_core::hostmap::HostMap;

use volucer_acme::ChallengeSource;
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
/// per-host entries and the fallback are two answers to one question â€” *which certificate is
/// this name served under* â€” and a request that took one from each generation could present a
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
    /// claims must still complete â€” that is what makes an unknown SNI name reach the `Host`
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

    /// Every named entry, as host-and-certificate pairs.
    ///
    /// For a caller building the *next* table from this one â€” an ACME installation, which adds a
    /// name and must not drop the rest. Sorted by host, and exact names before wildcards, because
    /// [`HostMap::iter`] guarantees it.
    pub fn entries(&self) -> Vec<(String, LoadedCert)> {
        self.hosts
            .iter()
            .map(|(host, cert)| (host.clone(), Arc::clone(cert)))
            .collect()
    }
}

/// Resolve a certificate for a name the client claimed in the handshake.
///
/// Falls back to the default entry for a name nothing claims, which is what makes an unknown
/// SNI name a handshake that succeeds and then meets whatever site the `Host` selects â€” rather
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
/// as long as the map is only read â€” which is why the map is replaced rather than mutated.
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
    /// The ACME challenge source, when this deployment manages certificates automatically.
    ///
    /// Held as its own resolver rather than merged into the table above, because the two answer
    /// **different questions** and must not be confused: this one produces a certificate for
    /// *proving control of a name* â€” a one-off, valid for a single validation â€” while the table
    /// is what the deployment serves to real clients. Merging them would put a validation
    /// certificate in the serving table.
    ///
    /// A trait object rather than [`volucer_acme::ChallengeResolver`], so the dispatch below is
    /// testable with a stub. The safety does not come from the type being concrete â€” it comes from
    /// the *order* in [`SniResolver::resolve`], which asks [`ChallengeSource::is_challenge`] before
    /// consulting a source at all, and from `volucer_acme::ChallengeResolver`'s own implementation
    /// of that method being the exact ALPN match.
    acme: Option<Arc<dyn ChallengeSource>>,
    /// Per-site challenge sources, tried after the deployment-wide one.
    ///
    /// A `Vec` rather than a map because the order is the rule and the count is the number of sites
    /// with their own ACME table â€” small, and read once per *handshake*, never per request.
    site_acme: Vec<(String, Arc<dyn ChallengeSource>)>,
}

/// Which certificates a deployment serves, as the config states them.
///
/// Two sources, deliberately merged into one value: a [`Config`] describes a *whole*
/// deployment, while a *reload* often changes only certificates â€” an ACME renewal, or an
/// operator replacing a renewed PEM. Taking `Config` as the only constructor would force every
/// certificate update to round-trip through a config file, which is the slower loop with more
/// ways to fail. Both are accepted; both produce the same table.
#[derive(Debug, Clone)]
pub struct CertificateSet {
    /// The certificate `[server.tls]` names, if it is configured.
    pub server: Option<ServerCertificate>,
    /// Per-site certificates, by site name, each with the hosts it covers.
    pub sites: Vec<SiteCertificate>,
    /// Sites whose placeholder is **generated** until their own ACME order provides one.
    ///
    /// Separate from `sites` because a generated pair has no path: the site states
    /// `[sites.x.acme]` and leaves the certificate out, and the placeholder covers that site's own
    /// hosts so the issued certificate replaces it for exactly those.
    pub generated_sites: Vec<GeneratedSite>,
}

/// A certificate pair for the whole listener, named as `[server.tls]` names it.
///
/// A name and a key rather than a whole [`TlsConfig`], because everything else in that struct
/// is a property of the [`rustls::ServerConfig`] rather than of the certificate â€” and those are
/// **restart-only** (Â§ `tls.rs`'s module doc). Storing them here would invite a reload that
/// appeared to apply them.
#[derive(Debug, Clone)]
pub struct ServerCertificate {
    /// The `[server]` names this certificate stands for, for the SANs of a generated placeholder.
    ///
    /// Only read when the pair is absent. The configured pair states its own names; the host
    /// patterns are read only because a generated certificate has nothing to copy from, and a
    /// browser checks the SAN list whatever the SNI asked for.
    pub names: Vec<String>,
    /// PEM certificate chain, leaf first. Relative paths resolve against the config file.
    ///
    /// `None` means the configuration left the pair out, and a self-signed placeholder is
    /// generated in memory â€” see [`load_certified`]. It is only allowed when an ACME table will
    /// replace it, which `Config::validate` enforces.
    pub certificate: Option<PathBuf>,
    /// PEM private key. `None` for the same reason as `certificate`, and always together with it.
    pub key: Option<PathBuf>,
}

/// One site's certificate and the host patterns it is served for.
#[derive(Debug, Clone)]
pub struct SiteCertificate {
    /// The site's name, for the error message when the pair cannot be loaded.
    pub name: String,
    /// The host patterns from `[sites.<name>].hosts`, unnormalised â€” `HostMap` normalises.
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

/// A site whose certificate is **generated** until its own ACME order provides one.
///
/// Separate from [`SiteCertificate`] because there is no pair to name: the site has
/// `[sites.x.acme]` and either no `[sites.x.tls]` or one with both keys left out. Carrying the hosts
/// is the point — the placeholder covers *this* site's names, so the certificate its order produces
/// replaces it for exactly those hosts and no others, without touching another site's.
#[derive(Debug, Clone)]
pub struct GeneratedSite {
    /// The site's name, which is also how its challenge source is scoped.
    pub name: String,
    /// The host patterns the placeholder covers.
    pub hosts: Vec<String>,
}

impl CertificateSet {
    /// The certificates a configuration states.
    ///
    /// Reads `[server.tls]` and every `[sites.*.tls]`, and nothing else â€” so a reload of the
    /// table alone does not depend on the rest of the file being unchanged. A site naming no
    /// certificate contributes nothing rather than an error: it is served under the server's
    /// certificate, which is the same rule `SniResolver::from_config` has always applied.
    pub fn from_config(config: &Config) -> Self {
        CertificateSet {
            server: config.server.tls.as_ref().map(|tls| ServerCertificate {
                names: default_names(config),
                certificate: tls.certificate.clone(),
                key: tls.key.clone(),
            }),
            sites: config
                .sites
                .iter()
                .filter_map(|(name, site)| {
                    // Only a site that states a pair contributes one here. A site with its **own**
                    // ACME table gets a placeholder instead, which is built per site below — so that
                    // its placeholder covers its own names and is replaced by the certificate its own
                    // order produced, without touching another site's.
                    let tls = site.tls.as_ref()?;
                    Some(SiteCertificate {
                        name: name.clone(),
                        hosts: site.hosts.clone(),
                        tls: SiteCertificatePair {
                            certificate: tls.certificate.clone()?,
                            key: tls.key.clone()?,
                        },
                    })
                })
                .collect(),
            generated_sites: config
                .sites
                .iter()
                .filter(|(_, site)| {
                    // A site with its own ACME table and no *named* pair gets a placeholder. A site
                    // that names one has decided against issuing, and `Config::validate` refuses
                    // both at once.
                    site.acme.is_some()
                        && !site
                            .tls
                            .as_ref()
                            .is_some_and(|tls| tls.certificate.is_some() && tls.key.is_some())
                })
                .map(|(name, site)| GeneratedSite {
                    name: name.clone(),
                    hosts: site.hosts.clone(),
                })
                .collect(),
        }
    }
}

impl SniResolver {
    /// Build a resolver from a config and the directory the config file lives in.
    ///
    /// `config_dir` is where relative `certificate`/`key` paths resolve, which is a deliberate
    /// exception to `document_root`'s rule â€” see [`volucer_core::config::TlsConfig`].
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
            acme: None,
            site_acme: Vec::new(),
        })
    }

    /// Build a resolver around a certificate table the caller has already loaded.
    ///
    /// For a caller that obtains certificates itself rather than from a config file â€” ACME â€”
    /// and for tests that want a resolver with a known table.
    pub fn from_certs(state: Certs, config_dir: &Path) -> Self {
        SniResolver {
            config_dir: config_dir.to_path_buf(),
            state: Arc::new(Swappable::new(state)),
            acme: None,
            site_acme: Vec::new(),
        }
    }

    /// Build a resolver around a table another resolver already holds.
    ///
    /// The ACME case, and the reason it exists rather than a `clone`: the challenge source has to
    /// be composed in at construction, while the certificate table is what a reload and an issuance
    /// replace. Rebuilding the resolver around the *same* table means there is one table and two
    /// names for it â€” the composed resolver answers handshakes and the original handle installs
    /// certificates â€” rather than two tables that would drift apart on the next issuance.
    ///
    /// So: one `Swappable` behind two `SniResolver`s, which is safe because the field is an `Arc`
    /// and everything about it is a pointer swap.
    pub fn from_shared(table: Arc<Swappable<Certs>>, config_dir: &Path) -> Self {
        SniResolver {
            config_dir: config_dir.to_path_buf(),
            state: table,
            acme: None,
            site_acme: Vec::new(),
        }
    }

    /// The table this resolver reads and replaces, for a caller building another resolver over it.
    pub fn table(&self) -> Arc<Swappable<Certs>> {
        Arc::clone(&self.state)
    }

    /// Where relative `certificate`/`key` paths resolve for this resolver.
    ///
    /// Exposed for the same reason as [`SniResolver::table`]: a caller composing a second resolver
    /// over the same table has to give it the same directory, and reading this one's answer is what
    /// keeps the two from being told different things.
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Compose an ACME challenge source into this resolver, keeping the site table's rule.
    ///
    /// **This is the whole reason the composition is ours.** `rustls-acme`'s
    /// `ResolvesServerCertAcme` holds exactly one `cert`, and on any handshake that is not a
    /// challenge it returns that certificate for *every* SNI name â€” including names this
    /// deployment deliberately falls back on, and including a name belonging to a different site.
    /// Installing it with `with_cert_resolver(...)` would therefore silently replace the
    /// hostâ†’certificate rule the whole TLS layer is built around: the symptom would be one site
    /// presenting another site's certificate, which is a security failure rather than a
    /// misconfiguration.
    ///
    /// So the ACME resolver is consulted **only** for a handshake that offers the `acme-tls/1`
    /// ALPN protocol, and everything else is answered by [`SniResolver::resolve_site`] exactly as
    /// it was before. Taking it through a constructor rather than a public field keeps that
    /// property in one place: there is no way to install the ACME resolver so that it answers a
    /// normal handshake.
    #[must_use]
    pub fn with_acme(mut self, acme: Arc<dyn ChallengeSource>) -> Self {
        self.acme = Some(acme);
        self
    }

    /// Add a challenge source for **one site**, leaving the deployment-wide one in place.
    ///
    /// Several tables can exist at once â€” the deployment's and one per site â€” and a validation
    /// handshake carries only an SNI name, so *something* has to say which table answers it. That
    /// something is the name: [`ChallengeSource::site_for`] is asked, and a source that names a
    /// different site, or the deployment-wide one, is passed over.
    ///
    /// The order is therefore a rule rather than an accident: the deployment-wide source stays first
    /// because it is the fallback for a name no site claims, and a site's source is consulted after
    /// it. `rustls-acme`'s resolver returns `None` for a name it is not validating, so a wrong
    /// source declines rather than answering with somebody else's challenge certificate â€” which is
    /// what makes the search safe rather than merely usually right.
    ///
    /// Bounded on purpose. With one table per site this is a walk over a handful of entries, and
    /// `HashMap` order would be fine; it is **ordered** because the deployment-wide source has to be
    /// tried before, not after, the site sources.
    #[must_use]
    pub fn with_site_acme(
        mut self,
        site: impl Into<String>,
        acme: Arc<dyn ChallengeSource>,
    ) -> Self {
        self.site_acme.push((site.into(), acme));
        self
    }

    /// Which challenge source should answer this handshake, if any.
    ///
    /// The decision is separated from answering because [`ClientHello`] is not `Clone`: a caller has
    /// to know *which* source before it hands the hello over, and asking each source in turn while
    /// consuming would not compile.
    ///
    /// Several tables can be in play at once — the deployment's and one per site — and a validation
    /// handshake carries only an SNI name, so something has to say which table owns it. That
    /// something is [`ChallengeSource::site_for`], and the two rules are:
    ///
    /// - A source that names a site answers only for that site. This is the per-site case, and it is
    ///   why the site name is carried on the source at all.
    /// - A source with no opinion (`site_for` returns `None`) answers for the whole deployment, so it
    ///   goes first. It is the fallback for a name no site claims, and putting a site's source ahead
    ///   of it would let a site's order answer a challenge for another site's name.
    ///
    /// A source that is not validating the name in the hello returns `None` from
    /// `challenge_cert`, so "which source" is a hint rather than a claim — the certificate
    /// `rustls-acme` holds is returned only for the exact name it is validating.
    fn challenge_source_for(&self, hello: &ClientHello<'_>) -> Option<&dyn ChallengeSource> {
        if let Some(acme) = &self.acme {
            // Inlined rather than through a helper closure: the closure's argument lifetime cannot
            // be named, so the borrow `site_for` returns would not be tied to the source it came
            // from and the compiler refuses it — correctly, since that borrow is what proves the
            // comparison is against *this* source's answer.
            if acme.is_challenge(hello) && acme.site_for(hello).is_none() {
                return Some(acme.as_ref());
            }
        }
        for (name, source) in &self.site_acme {
            if !source.is_challenge(hello) {
                continue;
            }
            if source.site_for(hello) == Some(name.as_str()) {
                return Some(source.as_ref());
            }
        }
        None
    }

    /// Whether this resolver will consult an ACME challenge source.
    ///
    /// Reported rather than inferred, so `serve` can say whether issuance is wired up without a
    /// caller having to know how the composition is built.
    pub fn has_acme(&self) -> bool {
        self.acme.is_some() || !self.site_acme.is_empty()
    }

    /// Replace the certificate table, loading the certificates named by `certificates`.
    ///
    /// Whole-or-nothing: every certificate in the set is loaded and paired **before** anything
    /// is installed, so a set with one unreadable pair is refused entirely and the deployment
    /// keeps serving the table it had. That is the only safe direction â€” a table that dropped
    /// one entry would fail every handshake for the names it covered, which is a worse failure
    /// than the one that was being reported.
    ///
    /// Returns the table that was replaced, so a caller can say *which* certificate changed
    /// rather than only that the file was read.
    pub fn refresh(&self, certificates: &CertificateSet) -> Result<Arc<Certs>, TlsError> {
        // The table being served is passed in so an **issued** certificate survives: it is not in
        // `certificates`, which is only what the configuration states. Without this a reload would
        // replace every ACME certificate with a placeholder, which is hazard 71's failure arriving by
        // a different route. See [`load_keeping`] for why the merge is the right answer rather than
        // skipping the rebuild, which would strand a renewed `[sites.x.tls]` PEM.
        let previous = self.state.load();
        let next = load_keeping(certificates, &self.config_dir, Some(&previous))?;
        Ok(self.state.replace(next))
    }

    /// Install a certificate table that is already in memory.
    ///
    /// For a certificate whose bytes never went through a file this process wrote â€” an ACME
    /// issuance. The PEM is still written for durability and for an operator to inspect, but
    /// correctness does not depend on re-reading it, and that is what keeps a renewal off the
    /// filesystem at the moment it matters.
    pub fn replace(&self, state: Certs) -> Arc<Certs> {
        self.state.replace(state)
    }

    /// Install one issued certificate, so the **next handshake** presents it.
    ///
    /// The ACME path, and the whole reason this resolver is replaceable at all. What it does is
    /// narrow on purpose: the issued pair becomes the certificate for the names it covers *and*
    /// the fallback, while every other entry in the table is left exactly as it was.
    ///
    /// That last part is the correctness rather than an optimisation. A deployment can serve
    /// several sites from several certificates â€” a per-site table, or a mix of issued and named
    /// pairs â€” and a renewal of one name must not take away the others. So this is a *partial*
    /// update where [`refresh`](SniResolver::refresh) is whole-or-nothing: refresh rebuilds from a
    /// configuration, and this adds what a CA just issued to what is already serving.
    ///
    /// Returns the table that was replaced, so a caller can report what changed.
    pub fn install_issued(
        &self,
        cert_pem: &[u8],
        key_pem: &[u8],
        domains: &[String],
        takes_default: bool,
    ) -> Result<Arc<Certs>, TlsError> {
        let certified = load_from_pem(cert_pem, key_pem, "acme")?;
        let previous = self.state.load();

        // Rebuilt rather than mutated: rustls may be resolving against the current table on
        // another thread, so an edit in place would be visible mid-lookup. Same rule as `Certs`
        // itself follows.
        //
        // `takes_default` is what keeps a **site's** certificate off other sites' names. The default
        // entry answers every name that has no entry of its own, so an engine installing its result as
        // the default would have one site's order answer for the whole deployment — which is a wrong
        // certificate on a name nobody asked about, and the failure that would appear is another
        // site's browser warning. A site's engine therefore contributes **named** entries only, and
        // the deployment's placeholder stays the default.
        let default = match takes_default {
            true => Arc::clone(&certified),
            false => Arc::clone(previous.default_cert()),
        };
        let mut next = Certs::new(default, std::iter::empty());
        for (host, cert) in previous.entries() {
            next = next.with_host(&host, Arc::clone(&cert));
        }
        for domain in domains {
            next = next.with_host(domain, Arc::clone(&certified));
        }
        Ok(self.state.replace(next))
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
        // The ACME challenge branch comes **first**, and it is the only place the two resolvers
        // may be combined. See [`SniResolver::with_acme`] for why the composition is ours rather
        // than `rustls-acme`'s.
        //
        // Which source answers is decided before `hello` is consumed, because `ClientHello` is not
        // `Clone` — see [`SniResolver::challenge_source_for`]. A challenge no source claims falls
        // through to the site table, which fails that handshake rather than answering it with a
        // certificate for a name nobody is validating.
        if let Some(source) = self.challenge_source_for(&hello) {
            return source.challenge_cert(hello);
        }
        self.resolve_site(hello)
    }
}

impl SniResolver {
    /// Resolve against the site table alone, ignoring any ACME challenge.
    ///
    /// Split out from [`ResolvesServerCert::resolve`] so the challenge branch is one `if` and the
    /// behaviour for everything else is one call â€” a caller in a test can therefore assert the
    /// site table's answer without a challenge in the way.
    pub fn resolve_site(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
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
    load_keeping(certificates, config_dir, None)
}

/// Rebuild a certificate table from a configuration, optionally **keeping** what a previous table had.
///
/// `keep` is what makes the reload correct once one deployment can have several ACME scopes. Those
/// certificates are not in the configuration at all — they are in a cache and in the running table —
/// so a rebuild from the file alone would replace every one of them with a placeholder. The skip that
/// used to guard against that is too blunt here: a site's own `[sites.x.tls]` *is* in the file and must
/// still be re-read when its PEM is renewed, so the two populations need separating rather than
/// choosing between.
///
/// So: **named** entries are rebuilt from the configuration, and every **issued** entry in `keep` is
/// carried over. What keeps a stale certificate from surviving a genuine configuration change is that
/// the carried entry is registered first and the rebuilt one overwrites it — the configuration wins
/// wherever it says anything, and the issued certificate survives only for the names nothing else
/// claims.
///
/// `None` is the startup path, where there is nothing to keep.
fn load_keeping(
    certificates: &CertificateSet,
    config_dir: &Path,
    keep: Option<&Certs>,
) -> Result<Certs, TlsError> {
    let server = certificates
        .server
        .as_ref()
        .ok_or(TlsError::NotConfigured)?;
    // A pair that is named is loaded; a pair left out is generated, because the whole point of the
    // optional pair is that the deployment has something to present before an order completes.
    let named_server = matches!((&server.certificate, &server.key), (Some(_), Some(_)));
    let default = match (&server.certificate, &server.key) {
        (Some(certificate), Some(key)) => {
            load_certified(certificate, key, config_dir, "server.tls")?
        }
        // The configuration names nothing here, so an **issued** certificate is what should be served,
        // if there is one — regenerating a placeholder would take a working certificate out of service
        // on every reload. The placeholder is the last resort, which is what it is at startup too.
        _ => match keep {
            Some(previous) => Arc::clone(previous.default_cert()),
            None => generate(&server.names, "server.tls")?,
        },
    };

    let mut named: Vec<(String, LoadedCert)> = Vec::new();
    // Carried first, so a certificate the configuration *does* name overwrites it below.
    if let Some(previous) = keep {
        if !named_server {
            for host in &server.names {
                named.push((host.clone(), Arc::clone(previous.for_name(host))));
            }
        }
        for (host, cert) in previous.entries() {
            named.push((host, cert));
        }
    }
    // A site that names its own pair overrides the server's certificate for its hosts, which is
    // what `[sites.x.tls]` has always meant.
    for site in &certificates.sites {
        let loaded = load_certified(
            &site.tls.certificate,
            &site.tls.key,
            config_dir,
            &format!("sites.{}.tls", site.name),
        )?;
        for host in &site.hosts {
            named.push((host.clone(), Arc::clone(&loaded)));
        }
    }
    // A site with its own ACME table but no named pair gets a placeholder for **its own** names, so
    // the certificate its order produces replaces it without disturbing any other site's. Registered
    // after the named pairs so two sites cannot both claim a host with the generated one winning.
    //
    // On a reload the carried entries above have already put the issued certificate here, so this
    // overwrites it with a fresh placeholder for a moment — which is why it is skipped when the name
    // is already covered. Regenerating would take a working certificate out of service on every
    // reload and put it back only when the next `install_cached` ran.
    for site in &certificates.generated_sites {
        for host in &site.hosts {
            if named.iter().any(|(claimed, _)| claimed == host) {
                continue;
            }
            let loaded = generate(&site.hosts, &format!("sites.{}.acme", site.name))?;
            for host in &site.hosts {
                named.push((host.clone(), Arc::clone(&loaded)));
            }
        }
    }

    Ok(Certs::new(default, named))
}

/// Generate a self-signed placeholder for a set of names, in memory.
///
/// **Not written anywhere.** The only claim this certificate makes is "this deployment can complete
/// a handshake", and it stops being served the moment an authority issues a real one â€” the install
/// path replaces it without a restart. Persisting it would leave a private key on disk that nothing
/// ever needed and that a backup would carry for years.
///
/// The names are the host patterns the deployment answers to, wildcards included, because a browser
/// checks the SAN list regardless of what SNI asked for â€” a generated certificate with no names
/// fails every handshake with a name mismatch, which reads as a broken server rather than as "no
/// certificate yet". A wildcard pattern goes on as-is: `rcgen` accepts `*.example.com` and rustls
/// matches it, which is exactly what a placeholder is for.
fn generate(names: &[String], where_: &str) -> Result<LoadedCert, TlsError> {
    install_crypto_provider();

    // A name is required, because a certificate with no SAN is one no client accepts. An empty list
    // here means the configuration had no names either â€” a server with no sites â€” and an empty
    // certificate would fail every handshake while looking like a TLS bug.
    let names: Vec<String> = if names.is_empty() {
        vec!["localhost".to_string()]
    } else {
        names.to_vec()
    };
    tracing::info!(
        where_,
        names = ?names,
        "no certificate is named here, so a placeholder was generated in memory for the first \
         handshakes; it is replaced when the ACME authority issues"
    );

    let certified =
        rcgen::generate_simple_self_signed(names).map_err(|source| TlsError::Generate {
            where_: where_.to_string(),
            source,
        })?;
    load_from_pem(
        certified.cert.pem().as_bytes(),
        certified.key_pair.serialize_pem().as_bytes(),
        where_,
    )
}

/// The names a **generated** server certificate should carry.
///
/// `[server.acme].domains` when there is one, because that is literally "what this certificate must
/// cover" — the same list the order will ask for, so the placeholder and the issued certificate
/// answer to the same names and the swap is invisible to a client.
///
/// Without ACME, every host a site answers to: the server certificate *is* the certificate for sites
/// that name none, so it has to cover their names or their handshakes fail with a mismatch. A site
/// with its own ACME table is skipped, because its **own** generated certificate covers its hosts and
/// putting them here would have the deployment placeholder answer for a name whose order belongs to a
/// site.
///
/// And `localhost` when there are no sites at all, because a certificate with no SAN is one no client
/// accepts. Only read when `[server.tls]` names no pair — a stated certificate lists its own names.
fn default_names(config: &Config) -> Vec<String> {
    // `[server.acme].domains` first, when there is one: that is literally "what this certificate must
    // cover", so the placeholder answers to the same names the order will ask for and the swap is
    // invisible to a client.
    if let Some(acme) = &config.server.acme {
        if !acme.domains.is_empty() {
            return acme.domains.clone();
        }
    }
    let mut names: Vec<String> = Vec::new();
    for site in config.sites.values() {
        // A site with its own ACME table is covered by its **own** generated certificate, so its hosts
        // do not belong here — and putting them here would have the deployment placeholder answering
        // for a name whose order belongs to a site.
        if site.acme.is_some() {
            continue;
        }
        for host in &site.hosts {
            if !names.contains(host) {
                names.push(host.clone());
            }
        }
    }
    if names.is_empty() {
        names.push("localhost".to_string());
    }
    names
}
/// Install the process-wide crypto provider, once.
///
/// rustls 0.23 leaves the provider to the crate features but only *installs* it lazily, and
/// only on the paths that ask for it â€” `ServerConfig::builder` does, `CertifiedKey::from_der`
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

/// Load a certificate chain and private key **from named files**, and pair them.
///
/// The named-file entry point: load a certificate chain and key from two paths.
///
/// A caller that lets the pair be left out â€” because an ACME authority will issue one â€” goes
/// through [`load_or_generate`] instead, which is what an *absent* pair means in a configuration.
/// This function is for a pair that is definitely there.
pub fn load_certified(
    certificate: &Path,
    key: &Path,
    config_dir: &Path,
    where_: &str,
) -> Result<LoadedCert, TlsError> {
    install_crypto_provider();
    let cert_path = resolve(config_dir, certificate);
    let key_path = resolve(config_dir, key);

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

/// Load a certificate chain and private key from PEM text already in memory.
///
/// The path ACME takes, and the reason it is a separate function rather than a variant of
/// [`load_certified`]: an issued certificate's bytes never went through a file this process wrote,
/// so requiring a path would mean writing a PEM and reading it back â€” a second, slower loop that
/// can fail *after* the first already succeeded. `rustls-acme` hands over PEM, so PEM is what this
/// takes.
///
/// Paired through exactly the same `CertifiedKey::from_der` call as the file path, deliberately:
/// a certificate loaded from memory and one loaded from disk must produce the same value, and two
/// construction sites is how they would come to differ.
pub fn load_from_pem(
    cert_pem: &[u8],
    key_pem: &[u8],
    where_: &str,
) -> Result<LoadedCert, TlsError> {
    install_crypto_provider();

    let chain: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut std::io::BufReader::new(std::io::Cursor::new(cert_pem)))
            .collect::<Result<_, _>>()
            .map_err(|source| TlsError::Certificate {
                where_: where_.to_string(),
                path: "(in memory)".to_string(),
                source: ChainError::Pem(source),
            })?;
    if chain.is_empty() {
        return Err(TlsError::Certificate {
            where_: where_.to_string(),
            path: "(in memory)".to_string(),
            source: ChainError::Empty,
        });
    }

    let key =
        rustls_pemfile::private_key(&mut std::io::BufReader::new(std::io::Cursor::new(key_pem)))
            .map_err(|source| TlsError::Key {
                where_: where_.to_string(),
                path: "(in memory)".to_string(),
                source: KeyError::Pem(source),
            })?
            .ok_or(TlsError::Key {
                where_: where_.to_string(),
                path: "(in memory)".to_string(),
                source: KeyError::Empty,
            })?;

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

    // ALPN decided the protocol, so it must be exactly what the config asks for â€” `h2` first,
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

    #[error(
        "{where_}: no certificate is named and a placeholder could not be generated: {source}"
    )]
    Generate {
        where_: String,
        #[source]
        source: rcgen::Error,
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
