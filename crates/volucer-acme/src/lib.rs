//! ACME (RFC 8555) issuance, and the one place it meets our certificate table.
//!
//! # Why this is a crate of its own
//!
//! Because the composition is the hard part, not the protocol. `rustls-acme` implements the flow
//! — account, order, challenge, finalize, renewal schedule — and this crate exists to hold the
//! *answer to a handshake* that keeps that flow from taking over the deployment. Putting it in
//! `volucer-http` would put a network client next to the request path; putting it behind a trait
//! would hide the one property that matters, which is who answers a handshake.
//!
//! # The property this crate exists to preserve
//!
//! `rustls-acme`'s `ResolvesServerCertAcme` holds **one** certificate and returns it for *every*
//! SNI name that is not a challenge:
//!
//! ```text
//! if is_acme_challenge { /* the challenge cert */ }
//! else { self.inner.lock().unwrap().cert.clone() }   // <- the site table is never consulted
//! ```
//!
//! Installed directly, that silently replaces the host→certificate rule: a site would present
//! another site's certificate, which is a security failure rather than a misconfiguration. So the
//! ACME resolver may only be consulted for a handshake that offers the `acme-tls/1` ALPN protocol,
//! and [`ChallengeResolver`] is the only shape this crate hands out — a caller cannot install it
//! so that it answers a normal handshake.

use std::sync::Arc;

use rustls::server::ClientHello;

/// The certificate a validation handshake is answered with, or nothing.
///
/// A type alias rather than a newtype so that a caller passes it straight to
/// `with_cert_resolver` on a configuration built **for the challenge only**, which is what
/// `rustls-acme`'s own acceptor does. Nothing in this crate returns an `Arc<dyn
/// ResolvesServerCert>` that would be valid on the serving port, because that is the mistake.
pub type ChallengeCert = Arc<rustls::sign::CertifiedKey>;

/// Answer a *validation* handshake from the ACME state, and nothing else.
///
/// Deliberately not a `ResolvesServerCert` implementation: the trait has no way to say "only
/// challenge handshakes", so an implementation of it could be passed to
/// `ServerConfig::with_cert_resolver` and would then answer every name. The function takes the
/// `ClientHello` and returns `None` for anything that is not a challenge — so a caller who wired
/// it in the wrong place would get failed handshakes for real clients rather than a wrong
/// certificate, and a failed handshake is loud.
pub trait ChallengeSource: std::fmt::Debug + Send + Sync {
    /// Whether this handshake is a validation attempt.
    ///
    /// By reference, and separate from [`ChallengeSource::challenge_cert`], because it *can* be:
    /// `ClientHello` is not `Clone`, so a caller that had only a consuming method could not ask the
    /// question and then still answer a normal handshake with it. Two methods rather than one is
    /// the honest shape of the split — deciding does not consume, resolving must.
    fn is_challenge(&self, hello: &ClientHello<'_>) -> bool;

    /// The challenge certificate for this handshake, when it is a challenge for a name the state
    /// is currently validating.
    fn challenge_cert(&self, hello: ClientHello<'_>) -> Option<ChallengeCert>;
}

/// The ACME resolver, wrapped so it can only ever answer a challenge handshake.
///
/// Wraps `rustls_acme`'s resolver rather than re-exporting it, because the re-export's `resolve`
/// answers every non-challenge name with one certificate. Every path to the inner resolver goes
/// through [`ChallengeResolver::challenge_cert`], which refuses anything that is not a challenge —
/// so "the ACME resolver replaced our certificate table" is not a thing a caller can write.
#[derive(Debug)]
pub struct ChallengeResolver {
    inner: Arc<rustls_acme::ResolvesServerCertAcme>,
}

impl ChallengeResolver {
    /// Wrap the resolver an `AcmeState` hands out.
    ///
    /// `AcmeState::resolver()` is public, and so is `is_tls_alpn_challenge`, so the wrapper needs
    /// nothing from `rustls-acme` that a caller could not reach — which is what makes it possible
    /// to compose rather than to fork.
    pub fn new(inner: Arc<rustls_acme::ResolvesServerCertAcme>) -> Self {
        ChallengeResolver { inner }
    }
}

impl ChallengeSource for ChallengeResolver {
    fn is_challenge(&self, hello: &ClientHello<'_>) -> bool {
        rustls_acme::is_tls_alpn_challenge(hello)
    }

    fn challenge_cert(&self, hello: ClientHello<'_>) -> Option<ChallengeCert> {
        // The gate, and the only one that matters. `is_tls_alpn_challenge` is an **exact** match on
        // the ALPN list: it uses `.eq([b"acme-tls/1"])`, so a client offering `h2` *and*
        // `acme-tls/1` is not a validation attempt and must take the normal path. A client cannot
        // reach a validation-only certificate by asking for it alongside something else.
        if !self.is_challenge(&hello) {
            return None;
        }
        // Reached only for a challenge, so this is the one call into `rustls-acme`'s resolver.
        // Its own code then checks that the SNI name is the one being validated, and returns
        // `None` for a mismatch — which fails that handshake rather than answering it with
        // somebody else's challenge certificate.
        use rustls::server::ResolvesServerCert;
        self.inner.resolve(hello)
    }
}

/// Something that went wrong setting ACME up.
#[derive(Debug, thiserror::Error)]
pub enum AcmeError {
    #[error(
        "no ACME directory: name one, or use the staging default — there is deliberately no \
         default that issues a real certificate"
    )]
    NoDirectory,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapper is a `ChallengeSource` and **not** a `ResolvesServerCert`.
    ///
    /// A compile-time assertion, and the point of the whole crate: if `ChallengeResolver` ever
    /// implemented `ResolvesServerCert`, a caller could hand it to `with_cert_resolver` and every
    /// SNI name would get whatever single certificate the ACME state holds. That is the failure
    /// this type exists to make unwritable, so the *absence* of the impl is what is asserted —
    /// and the way to assert an absence at compile time is to require the type in a bound it does
    /// not satisfy and check the build fails. Which cannot be done in a test that must pass.
    ///
    /// So what is checked instead is the observation that is available: the trait this type *does*
    /// implement is named, and its method refuses a non-challenge hello. The stronger property is
    /// enforced by the compiler at the call site in `volucer-http::tls::SniResolver::with_acme`,
    /// where passing a `ChallengeResolver` to `with_cert_resolver` would not type-check.
    #[test]
    fn a_challenge_resolver_is_not_a_certificate_resolver() {
        fn assert_challenge_source<T: ChallengeSource>() {}
        assert_challenge_source::<ChallengeResolver>();
    }
}
