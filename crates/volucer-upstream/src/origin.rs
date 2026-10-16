//! Where the origin is, and how to trust it.
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
//! # Why `unix:` is expressed with a scheme and a path
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

use std::path::PathBuf;

use hyper::Uri;

use crate::error::{Result, UpstreamError};

/// Where an origin is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A `host:port` reached over the network, plaintext or TLS.
    Network {
        /// As written, including the scheme: what decides plaintext vs TLS, and what the
        /// `Host` header is built from.
        base: Uri,
        /// Only ever present for an `https:` base.
        tls: Option<UpstreamTls>,
    },

    /// A unix socket on this host. There is no scheme and no port to speak of.
    #[cfg(unix)]
    Socket(PathBuf),
}

/// How to trust an `https:` origin.
///
/// Two knobs, and no third one for verification: see the module docs for why there is no
/// "verify" flag. `min_version` and the ALPN preference list are not here either — the defaults
/// (TLS 1.2 as the floor, `h2` preferred with `http/1.1` as the fallback) are what every origin
/// this is built for expects, and a knob nobody asked for is a knob nobody has tested.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpstreamTls {
    /// PEM file holding the certificates to verify the origin against.
    ///
    /// Absent means the public roots — correct for an origin holding a publicly-trusted
    /// certificate, and wrong for an internal service, which is exactly why this is not guessed
    /// at.
    pub ca: Option<PathBuf>,

    /// The name to verify the origin against, when it is not the one in the origin's address.
    ///
    /// The common reason: dialling an IP or a service address while the certificate names a host.
    pub sni: Option<String>,
}

impl Default for Origin {
    fn default() -> Self {
        // Only used by tests that want a placeholder before overwriting it; `parse` is the only
        // constructor that is ever reached from a configuration.
        Origin::Network {
            base: Uri::from_static("http://127.0.0.1:80"),
            tls: None,
        }
    }
}

impl Origin {
    /// Parse an origin from its configuration string.
    ///
    /// Strict on purpose. Every rejected shape here is one that would otherwise be *silently*
    /// interpreted as something else: a bare `host:port` would be ambiguous with a path, a base
    /// path or credentials would look honoured when they are not, and an unknown scheme would be
    /// treated as plaintext.
    pub fn parse(spec: &str) -> Result<Origin> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(invalid(spec, "it is empty"));
        }

        if let Some(path) = spec.strip_prefix("unix:") {
            if path.is_empty() {
                return Err(invalid(spec, "the socket path is empty"));
            }
            #[cfg(unix)]
            {
                return Ok(Origin::Socket(PathBuf::from(path)));
            }
            #[cfg(not(unix))]
            {
                return Err(invalid(
                    spec,
                    "unix sockets are not available on this platform, and a Windows named pipe \
                     cannot be used here: hyper has no pipe transport, so an origin must be reached \
                     over TCP on Windows",
                ));
            }
        }

        let base: Uri = spec
            .parse()
            .map_err(|source| invalid(spec, &format!("it is not a URL: {source}")))?;

        let scheme = base
            .scheme_str()
            .ok_or_else(|| invalid(spec, "there is no scheme; write `http://host:port`"))?;
        let tls = match scheme {
            "http" => None,
            "https" => Some(UpstreamTls::default()),
            other => {
                return Err(invalid(
                    spec,
                    &format!(
                        "{other:?} is not a scheme an origin can be reached with; use `http`, \
                         `https` or `unix`"
                    ),
                ))
            }
        };
        if base.authority().is_none() {
            return Err(invalid(
                spec,
                "there is no host; write `http://127.0.0.1:3000`",
            ));
        }
        if base
            .authority()
            .is_some_and(|authority| authority.as_str().contains('@'))
        {
            return Err(invalid(
                spec,
                "it carries credentials; volucer forwards the client's credentials, not origin's \
                 of its own",
            ));
        }
        // A base path would have to be joined with the request path, and a prefix that is joined
        // without saying so is worse than one that is refused: the two rules differ on trailing
        // slashes and on percent-encoding, and neither is guessable. Route prefixes belong in the
        // route table, where they already are.
        match base.path_and_query().map(|path| path.as_str()) {
            None | Some("") | Some("/") => {}
            Some(path) => {
                return Err(invalid(
                    spec,
                    &format!(
                        "it carries the path {path:?}; an origin is only an address, and a route \
                         prefix belongs in the route table (`path = ...`)"
                    ),
                ))
            }
        }

        Ok(Origin::Network { base, tls })
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
    /// client's own `Host` is the only one that means anything.
    pub fn authority(&self) -> Option<&hyper::http::uri::Authority> {
        match self {
            Origin::Network { base, .. } => base.authority(),
            #[cfg(unix)]
            Origin::Socket(_) => None,
        }
    }

    /// What the connector is asked to connect to.
    ///
    /// For an `https:` origin this is the same address; the scheme is the only reason the
    /// connector knows to handshake, and it refuses to complete one without a verified chain.
    pub fn dial_uri(&self) -> Result<Uri> {
        match self {
            Origin::Network { base, .. } => Ok(base.clone()),
            #[cfg(unix)]
            Origin::Socket(path) => hyper::http::uri::Builder::new()
                .scheme("unix")
                .authority("localhost")
                .path_and_query("/")
                .build()
                .map_err(|source| UpstreamError::InvalidOrigin {
                    origin: path.display().to_string(),
                    reason: format!("a placeholder dial URI could not be built: {source}"),
                }),
        }
    }

    /// The default port for the scheme, used when the address names none.
    pub fn default_port(&self) -> Option<u16> {
        match self.scheme() {
            Some("http") => Some(80),
            Some("https") => Some(443),
            _ => None,
        }
    }
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

    #[test]
    fn a_loopback_origin_parses() {
        let origin = Origin::parse("http://127.0.0.1:3000").expect("parses");
        assert_eq!(origin.scheme(), Some("http"));
        assert!(!origin.is_tls());
        assert_eq!(
            origin.authority().map(|a| a.as_str()),
            Some("127.0.0.1:3000")
        );
        assert_eq!(origin.default_port(), Some(80));
    }

    #[test]
    fn an_ipv6_origin_keeps_its_brackets() {
        let origin = Origin::parse("http://[::1]:3000").expect("parses");
        assert_eq!(origin.authority().map(|a| a.as_str()), Some("[::1]:3000"));
    }

    #[test]
    fn an_https_origin_is_tls_and_starts_unverified_against_named_roots() {
        let origin = Origin::parse("https://10.0.0.5:8443").expect("parses");
        assert!(origin.is_tls());
        let tls = origin.tls().expect("an https origin has tls settings");
        // Absent `ca` means the public roots, so a test can tell "no CA named" from "verified
        // against nothing".
        assert!(tls.ca.is_none());
        assert!(tls.sni.is_none());
        assert_eq!(origin.default_port(), Some(443));
    }

    #[test]
    fn a_bare_host_and_port_is_refused_rather_than_guessed_at() {
        let err = Origin::parse("127.0.0.1:3000").expect_err("must fail");
        assert!(err.to_string().contains("no scheme"), "{err}");
        // It must not have been quietly read as plaintext.
        assert!(Origin::parse("127.0.0.1:3000").is_err());
    }

    #[test]
    fn a_base_path_is_refused_by_name() {
        // `/app` would have to be joined with the request path, and the join rule is not
        // guessable — so it is refused with the alternative rather than silently dropped.
        let err = Origin::parse("http://127.0.0.1:3000/app").expect_err("must fail");
        assert!(err.to_string().contains("/app"), "{err}");
        assert!(err.to_string().contains("route table"), "{err}");
    }

    #[test]
    fn credentials_in_the_origin_are_refused() {
        let err = Origin::parse("http://user:pw@127.0.0.1:3000").expect_err("must fail");
        assert!(err.to_string().contains("credentials"), "{err}");
    }

    #[test]
    fn an_unknown_scheme_names_the_ones_that_work() {
        let err = Origin::parse("ftp://127.0.0.1:3000").expect_err("must fail");
        assert!(err.to_string().contains("ftp"), "{err}");
        assert!(err.to_string().contains("unix"), "{err}");
    }

    #[test]
    fn a_scheme_without_a_host_is_refused() {
        // Which of these `Uri` accepts is not something to guess at, so both the shape its own
        // parser rejects and the one that reaches *our* authority check are asserted.
        for spec in ["http://", "http:/foo", "http:foo"] {
            let err = Origin::parse(spec).expect_err("must fail");
            let message = err.to_string();
            assert!(
                message.contains("no host")
                    || message.contains("not a URL")
                    || message.contains("no scheme"),
                "{spec}: {message}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_unix_origin_has_no_scheme_and_no_authority() {
        let origin = Origin::parse("unix:/run/app.sock").expect("parses");
        assert!(matches!(origin, Origin::Socket(_)));
        assert_eq!(origin.scheme(), None);
        assert!(origin.authority().is_none());
        assert!(origin.tls().is_none());
        assert!(!origin.is_tls());
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_unix_path_is_refused() {
        let err = Origin::parse("unix:").expect_err("must fail");
        assert!(err.to_string().contains("socket path"), "{err}");
    }

    #[test]
    fn an_empty_origin_is_refused() {
        let err = Origin::parse("   ").expect_err("must fail");
        assert!(err.to_string().contains("empty"), "{err}");
    }
}
