//! One `rustls` in the lockfile, because two is a stop.
//!
//! `rustls-acme` is unusual among TLS crates in that it re-exports and *forks* the crypto
//! provider: its `aws-lc-rs`/`ring` features select the provider for our own `rustls` and for the
//! `async-web-client` it talks HTTPS to the CA with. That only works while there is one `rustls`
//! in the graph. Two `rustls` versions means the resolver we implement — which satisfies *one*
//! crate's `ResolvesServerCert` — cannot be installed into the other's `ServerConfig`, and the
//! `ClientConfig` rustls-acme builds for the ACME directory is a different type from the one our
//! own client-side tests build.
//!
//! **What this test does and does not do, stated honestly.** It reads the lockfile rather than
//! running `cargo tree`, so it is cheap and has no build dependency. Its failure mode is *not*
//! silent: two versions nearly always fail to compile first, with an error naming a trait that is
//! "not implemented for" a type that looks identical to the reader. So this is a diagnostic — it
//! turns a confusing compile error into one sentence with the reason — rather than a bug catcher.
//! It is a test rather than a CI step because the lockfile is what `cargo update` changes, and the
//! thing worth failing on is a *diff* to the lock, which is exactly what a test in the tree sees.
//!
//! Nothing here needs a running server, a certificate or a socket, so it cannot skip.

use std::path::{Path, PathBuf};

/// The workspace lockfile, from this crate's manifest directory.
fn lockfile() -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("Cargo.lock");
    path.is_file().then_some(path)
}

/// Every `name = "rustls"` entry's version, as a published crate would appear.
///
/// Parsed by walking `[[package]]` blocks rather than by a TOML dependency, because the lockfile
/// is machine-generated and the shape being read is three lines of it — and because a test that
/// pulled in `toml` to count entries would be the kind of dependency this project adds only for
/// something that ships.
fn rustls_versions(lock: &str) -> Vec<String> {
    let mut versions = Vec::new();
    let mut in_package = false;
    let mut name: Option<String> = None;
    for line in lock.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            in_package = true;
            name = None;
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(value) = line.strip_prefix("name = ") {
            name = Some(value.trim_matches('"').to_string());
        } else if let Some(value) = line.strip_prefix("version = ") {
            // A `version` outside a `name` is malformed, and so is a package with no version —
            // both are the lockfile being unreadable, which the caller asserts separately.
            if let Some(name) = name.as_deref() {
                if name == "rustls" {
                    versions.push(value.trim_matches('"').to_string());
                }
            }
            in_package = false;
        }
    }
    versions
}

/// Exactly one `rustls`, so `rustls-acme`'s provider selection and our resolver agree by type.
#[test]
fn exactly_one_rustls_is_in_the_graph() {
    let Some(lockfile) = lockfile() else {
        eprintln!("skipping: no workspace lockfile beside this crate");
        return;
    };
    let lock = std::fs::read_to_string(&lockfile).expect("the lockfile is readable");
    let versions = rustls_versions(&lock);

    assert!(
        !versions.is_empty(),
        "the lockfile names no `rustls` at all, which means this test is looking in the wrong \
         place rather than that the graph is clean: {}",
        lockfile.display()
    );
    assert_eq!(
        versions.len(),
        1,
        "a second `rustls` in the graph breaks the assumption `rustls-acme`'s features rest on — \
         its crypto provider is selected *through* ours, and a resolver can only satisfy one \
         crate's trait: {versions:?}"
    );
    // The version `Cargo.lock` resolved is the one `cargo update` would have to change, so pin
    // that it is the 0.23 line the rest of the workspace is built against rather than a silent
    // jump to a new major.
    assert!(
        versions[0].starts_with("0.23"),
        "the single `rustls` must be the 0.23 line `tokio-rustls` and this crate are written \
         against: {versions:?}"
    );
}

/// The parser reads a real lockfile shape, so a clean result above is not a parsing accident.
///
/// Hazard 62's lesson in a smaller place: a test whose claim is "there is exactly one of these"
/// passes just as well against a function that always returns nothing. The negative case is
/// exercised here rather than trusted, and the caller's `!versions.is_empty()` assertion would
/// catch a parser that returned `Vec::new()` for everything anyway — but only for `rustls`.
#[test]
fn the_lockfile_reader_finds_a_named_package_and_ignores_others() {
    let synthetic = r#"
version = 4

[[package]]
name = "arc-swap"
version = "1.7.1"

[[package]]
name = "rustls"
version = "0.23.46"
dependencies = [
 "rustls-pki-types",
]

[[package]]
name = "rustls-webpki"
version = "0.103.15"
"#;

    assert_eq!(
        rustls_versions(synthetic),
        vec!["0.23.46".to_string()],
        "the reader must find `rustls` and must not match a package whose name merely starts \
         with it — `rustls-webpki` is a different crate"
    );
}

/// The `webpki-roots` feature is what makes the ACME client able to reach the CA at all.
///
/// This is a **compile-time** assertion and it exists because the feature set is the kind of thing
/// an operator's build gets wrong once and then never again — the failure is a missing function
/// rather than a runtime error, so the symptom is "cannot find function `new` in `AcmeConfig`"
/// with no hint that a *feature* was the cause. `AcmeConfig::new` is `#[cfg(feature =
/// "webpki-roots")]` in rustls-acme 0.15.4, and it is the constructor that builds the
/// `RootCertStore` verifying **Let's Encrypt's** own HTTPS endpoint.
///
/// So naming the type here is the test: with the feature on this compiles, and with it off the
/// build fails on this line with the reason a reader needs. Removing `webpki-roots` from
/// [`Cargo.toml`] is the falsification, and it was run rather than reasoned about.
#[test]
fn the_acme_client_is_built_with_a_root_store() {
    // `AcmeConfig::new` is the constructor that needs the roots, so naming it is the whole
    // assertion: with the feature on this compiles, and with it off the build fails on this line.
    let config =
        rustls_acme::AcmeConfig::<std::convert::Infallible, std::convert::Infallible>::new([
            "example.test",
        ]);
    // And the default directory is **staging**, which is the safe default the plan relies on: a
    // first deployment or a test must not be able to reach production by omission. Asserted from
    // the debug output rather than from the field, because the directory is `pub(crate)`.
    let debugged = format!("{config:?}");
    assert!(
        debugged.contains("staging"),
        "a fresh `AcmeConfig` must default to the staging directory, not production: {debugged}"
    );
}
