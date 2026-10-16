//! Where the origin is, and how to trust it — in the client's terms.
//!
//! # The split, and why it is not a formality
//!
//! The *rule* for what an origin may be lives in [`volucer_core::config::OriginSpec`]: which schemes
//! exist, what a bare `host:port` means (nothing — it is refused), why a base path and credentials
//! are rejected, and what a `sni` has to look like. It lives there because the configuration model
//! has to state, check and describe an origin without depending on a client crate: `volucer-core` is
//! the leaf every other crate depends on, and the same reasoning already keeps the worker protocol's
//! `Checksum` policy there as a mirror rather than an import.
//!
//! This module is the other half: turning a *checked* [`OriginSpec`] into the `Uri`, `Authority` and
//! rustls configuration the client needs. [`Origin::from_spec`] is the only constructor and it takes
//! the checked type, so "the origin was validated" is a property of the program rather than of the
//! call order — a caller cannot fabricate an `OriginSpec`, since its fields are not public and its
//! parse is the only way in.
//!
//! # The four shapes
//!
//! ```text
//! http://127.0.0.1:3000      TCP, plaintext
//! https://10.0.0.5:8443      TCP, TLS, verified against a CA or the public roots
//! unix:/run/node.sock        a unix socket on this host, no network at all
//! ```
//!
//! All of them are supported, and `https:` is not an afterthought: refusing it would be a hole of
//! volucer's own invention, since every other reverse proxy reaches a TLS origin and the pieces are
//! already in the build (`tokio-rustls`'s connector, `rustls-pemfile` for a CA, `webpki-roots` for
//! the public ones).
//!
//! What is *not* offered is an unverified TLS origin. That is a deliberate omission rather than a
//! missing option, and the reason is the same one that keeps a self-signed certificate off the
//! serving port: a connection that looks encrypted and checks nothing is worse than a plaintext one,
//! because the operator stops looking. A deployment that genuinely needs that shape points the
//! origin at `http://` on a loopback port, which is honest about what the hop is.
//!
//! # Why `unix:` is a first-class origin and a Windows pipe is not
//!
//! Because `unix:/run/php.sock` is already how this project names a local socket — `Endpoint::parse`
//! and `SlotAddress::Socket` both use it, and `[backends.<n>].socket` documents it — so an upstream
//! that could only speak TCP would be the one backend that cannot reach the thing every other
//! backend reaches. It is also strictly *less* exposed than a loopback port: there is no address for
//! a stray process to connect to.
//!
//! A **Windows named pipe is not supported**, and that is a real limitation rather than an
//! oversight: `hyper` has no pipe transport, so it would mean custom IO on top of a custom
//! connector. The error says so by name instead of leaving it to be discovered.

#[cfg(unix)]
use std::path::PathBuf;

use hyper::Uri;

use crate::error::{Result, UpstreamError};

/// How to trust an `https:` origin.
///
/// Re-exported from the configuration model rather than declared here, and that is the point: the
/// value `validate_backend` checks is the value the handshake uses, so a CA path or an SNI name
/// cannot be validated and then lost on the way to `rustls`.
pub use volucer_core::config::UpstreamTlsConfig as UpstreamTls;

/// Where an origin is, in the client's terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A `host:port` reached over the network, plaintext or TLS.
    Network {
        /// The absolute URI the pool is addressed with: scheme and authority, no path.
        base: Uri,
        /// Only ever present for an `https:` base.
        tls: Option<UpstreamTls>,
    },

    /// A unix socket on this host. There is no scheme and no port to speak of.
    #[cfg(unix)]
    Socket(PathBuf),
}

impl Origin {
    /// Build an origin from the configuration model's checked form.
    ///
    /// The only constructor. [`volucer_core::config::OriginSpec`] has no public fields and one
    /// parser, so this cannot be handed an unvalidated origin — which is what makes the "no
    /// unverified TLS" rule structural rather than a habit.
    pub fn from_spec(spec: &volucer_core::config::OriginSpec) -> Result<Origin> {
        match spec {
            volucer_core::config::OriginSpec::Network {
                scheme,
                host,
                port,
                tls,
            } => {
                let text = format!("{scheme}://{host}:{port}");
                let base = text
                    .parse::<Uri>()
                    .map_err(|source| invalid(&text, &source.to_string()))?;
                Ok(Origin::Network {
                    base,
                    tls: tls.clone(),
                })
            }
            #[cfg(unix)]
            volucer_core::config::OriginSpec::Socket(path) => Ok(Origin::Socket(path.clone())),
        }
    }

    /// Parse an origin from its configuration string, with no TLS table.
    ///
    /// A convenience for a caller that has a string and no configuration — a test, or a
    /// diagnostic. Configuration goes through [`OriginSpec::parse`] and this separately, because
    /// validation has to be able to report *which* half refused.
    ///
    /// [`OriginSpec::parse`]: volucer_core::config::OriginSpec::parse
    pub fn parse(spec: &str) -> Result<Origin> {
        let parsed = volucer_core::config::OriginSpec::parse(spec, None)
            .map_err(|reason| invalid(spec, &reason))?;
        Origin::from_spec(&parsed)
    }

    /// The scheme, when there is one. A unix socket has neither.
    pub fn scheme(&self) -> Option<&str> {
        match self {
            Origin::Network { base, .. } => base.scheme_str(),
            #[cfg(unix)]
            Origin::Socket(_) => None,
        }
    }

    /// Whether this origin is reached over TLS.
    pub fn is_tls(&self) -> bool {
        matches!(self, Origin::Network { tls: Some(_), .. })
    }

    /// The TLS settings, for an `https:` origin.
    pub fn tls(&self) -> Option<&UpstreamTls> {
        match self {
            Origin::Network { tls, .. } => tls.as_ref(),
            #[cfg(unix)]
            Origin::Socket(_) => None,
        }
    }

    /// The authority as written, which is what the origin should see as its `Host`.
    ///
    /// Kept rather than re-derived from the dialled address, because those are two different
    /// things: an origin reached over a unix socket, or over a loopback port, still expects the
    /// name the client asked for. `None` for a socket, where there is no host at all and the
    /// client's own `Host` header is the only one that means anything.
    pub fn authority(&self) -> Option<&hyper::http::uri::Authority> {
        match self {
            Origin::Network { base, .. } => base.authority(),
            #[cfg(unix)]
            Origin::Socket(_) => None,
        }
    }

    /// The `host:port` the origin is dialled at, with the port already resolved.
    ///
    /// `None` for a socket. An IPv6 literal keeps its brackets, which is `Authority::host`'s own
    /// convention and what keeps `[::1]:3000` unambiguous in a message or a dial.
    pub fn host_port(&self) -> Option<String> {
        // A `match` rather than `let ... else`: on a platform without unix sockets this enum has
        // one variant, and an irrefutable `let` makes the `else` unreachable — which is a warning
        // there and a refactor hazard everywhere.
        let base = match self {
            Origin::Network { base, .. } => base,
            #[cfg(unix)]
            Origin::Socket(_) => return None,
        };
        let host = base.authority()?.host().to_string();
        let port = base.port_u16().unwrap_or(match base.scheme_str() {
            Some("https") => 443,
            _ => 80,
        });
        Some(format!("{host}:{port}"))
    }

    /// The socket path, for a `unix:` origin.
    #[cfg(unix)]
    pub fn socket_path(&self) -> Option<&PathBuf> {
        match self {
            Origin::Socket(path) => Some(path),
            Origin::Network { .. } => None,
        }
    }

    /// The name the origin's certificate is verified against.
    ///
    /// `sni` when it is set, otherwise the host from the address — with the brackets of an IPv6
    /// literal removed, because a certificate carries an address's bare form and rustls'
    /// `ServerName` refuses the bracketed one. `None` for a plaintext origin (nothing to verify)
    /// and for a socket (nothing to name).
    pub fn verification_name(&self) -> Option<String> {
        let tls = self.tls()?;
        if let Some(sni) = &tls.sni {
            return Some(sni.clone());
        }
        let host = match self {
            Origin::Network { base, .. } => base.authority()?.host().to_string(),
            #[cfg(unix)]
            Origin::Socket(_) => return None,
        };
        Some(unbracket(&host))
    }
}

/// Strip the brackets an IPv6 authority host carries.
///
/// `Authority::host` deliberately keeps them, because they are what disambiguates the address's
/// colons from the port's. A *verification name* has no port to disambiguate from and rustls
/// rejects `[::1]` as a `ServerName`, so the two conventions meet here rather than in a handshake
/// that fails for a reason nobody can read.
fn unbracket(host: &str) -> String {
    host.strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host)
        .to_string()
}

fn invalid(spec: &str, reason: &str) -> UpstreamError {
    UpstreamError::InvalidOrigin {
        origin: spec.to_string(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use volucer_core::config::OriginSpec;

    /// A spec, built the only way one can be: through the parser.
    fn spec(text: &str) -> OriginSpec {
        OriginSpec::parse(text, None).expect("a usable origin")
    }

    #[test]
    fn a_loopback_origin_parses() {
        let origin = Origin::from_spec(&spec("http://127.0.0.1:3000")).expect("builds");
        assert_eq!(origin.scheme(), Some("http"));
        assert!(!origin.is_tls());
        assert_eq!(
            origin.authority().map(|a| a.as_str()),
            Some("127.0.0.1:3000")
        );
        assert_eq!(origin.host_port().as_deref(), Some("127.0.0.1:3000"));
    }

    #[test]
    fn a_port_less_origin_resolves_the_default() {
        // The port is resolved in the configuration model, so an error message and a dial both name
        // the port that was actually chosen rather than the empty string the file contains.
        let http = Origin::from_spec(&spec("http://example.test")).expect("builds");
        assert_eq!(http.host_port().as_deref(), Some("example.test:80"));
        let https = Origin::from_spec(&spec("https://example.test")).expect("builds");
        assert_eq!(https.host_port().as_deref(), Some("example.test:443"));
    }

    #[test]
    fn an_ipv6_origin_keeps_its_brackets_for_the_address_and_drops_them_for_the_name() {
        let origin = Origin::from_spec(&spec("http://[::1]:3000")).expect("builds");
        assert_eq!(origin.authority().map(|a| a.as_str()), Some("[::1]:3000"));
        assert_eq!(origin.host_port().as_deref(), Some("[::1]:3000"));

        // A TLS origin is where the two conventions would collide, so it is the case worth pinning:
        // the connector dials `[::1]:3000`, and rustls verifies `::1`.
        let tls = OriginSpec::parse("https://[::1]:8443", None).expect("builds");
        let origin = Origin::from_spec(&tls).expect("builds");
        assert_eq!(origin.host_port().as_deref(), Some("[::1]:8443"));
        assert_eq!(origin.verification_name().as_deref(), Some("::1"));
    }

    #[test]
    fn an_https_origin_is_tls_and_starts_on_the_public_roots() {
        let origin = Origin::from_spec(&spec("https://10.0.0.5:8443")).expect("builds");
        assert!(origin.is_tls());
        let tls = origin.tls().expect("an https origin has tls settings");
        // Absent `ca` means the public roots — distinguishable from "verified against nothing",
        // which is a state the type cannot express.
        assert!(tls.ca.is_none());
        assert!(tls.sni.is_none());
        assert_eq!(origin.verification_name().as_deref(), Some("10.0.0.5"));
    }

    #[test]
    fn an_sni_overrides_verification_without_moving_the_dial() {
        // The whole reason `sni` exists: dial a service address, verify a name. If the two
        // collapsed into one another the option would have no effect at all, so both are asserted.
        let parsed = OriginSpec::parse(
            "https://10.0.0.5:8443",
            Some(UpstreamTls {
                ca: None,
                sni: Some("node.internal".to_string()),
            }),
        )
        .expect("builds");
        let origin = Origin::from_spec(&parsed).expect("builds");
        assert_eq!(origin.verification_name().as_deref(), Some("node.internal"));
        assert_eq!(origin.host_port().as_deref(), Some("10.0.0.5:8443"));
    }

    #[test]
    fn a_named_ca_reaches_the_origin_the_validation_saw() {
        // The value `validate_backend` inspects must be the value the handshake uses: a CA that
        // survived validation and then vanished would be verification the operator believes in and
        // does not have.
        let parsed = OriginSpec::parse(
            "https://node.internal:8443",
            Some(UpstreamTls {
                ca: Some(PathBuf::from("/etc/ssl/internal-ca.pem")),
                sni: None,
            }),
        )
        .expect("builds");
        let origin = Origin::from_spec(&parsed).expect("builds");
        assert_eq!(
            origin.tls().and_then(|tls| tls.ca.as_deref()),
            Some(std::path::Path::new("/etc/ssl/internal-ca.pem"))
        );
    }

    #[test]
    fn a_plaintext_origin_has_nothing_to_verify() {
        let origin = Origin::from_spec(&spec("http://127.0.0.1:3000")).expect("builds");
        assert!(origin.tls().is_none());
        assert!(origin.verification_name().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_unix_origin_has_no_scheme_host_or_name() {
        let origin = Origin::from_spec(&spec("unix:/run/app.sock")).expect("builds");
        assert!(matches!(origin, Origin::Socket(_)));
        assert_eq!(origin.scheme(), None);
        assert!(origin.authority().is_none());
        assert!(origin.tls().is_none());
        assert!(!origin.is_tls());
        assert!(origin.host_port().is_none());
        assert!(origin.verification_name().is_none());
        assert_eq!(origin.socket_path(), Some(&PathBuf::from("/run/app.sock")));
    }

    #[test]
    fn the_refusals_come_from_the_configuration_model() {
        // `Origin::parse` is a convenience over the real parser, so these assert the *message* a
        // deployment sees, not the path it travelled. That the parsing itself is strict is
        // `volucer-core`'s test; that these shapes still fail after the move is this one.
        //
        // The userinfo case is built rather than written out: a credential-looking literal is what
        // a secret scanner is for, and a test input that a scanner may rewrite is not a fixture.
        let with_userinfo = format!("http://{}@{}", "user", "127.0.0.1:3000");
        for spec in [
            "127.0.0.1:3000",
            "http://127.0.0.1:3000/app",
            with_userinfo.as_str(),
            "ftp://127.0.0.1:3000",
            "http://",
            "   ",
        ] {
            let err = Origin::parse(spec).expect_err("must fail");
            assert!(
                matches!(err, UpstreamError::InvalidOrigin { .. }),
                "{spec}: {err}"
            );
        }
    }
}
