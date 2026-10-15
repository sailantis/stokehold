//! A table of host patterns, resolved in one place.
//!
//! Three things in this project answer the question "which of these configured names is this
//! Host header?": the **site** table (which document root serves it), the **certificate** table
//! (which certificate SNI presents for it), and the **route** table (which backend handles it).
//! They must agree — a name a route matched but a site did not would be a bug nobody could
//! reproduce from reading either table alone — so the rule lives here once.
//!
//! The rule, whole: an exact name wins, then the longest matching `*.` wildcard, then whatever
//! the caller supplies as the fallback. A wildcard names a *subdomain*, so `*.example.com` does
//! not match `example.com`.
//!
//! # Why the wildcards are keyed rather than scanned
//!
//! A wildcard matches a host exactly when the host ends with it, so the host's own suffixes can
//! be probed instead of every pattern. Stripping leading labels one at a time yields those
//! suffixes longest-first, which is the rule for free: the first hit is the longest match. That
//! is `O(number of labels in the host)` — two to four for any real host name — rather than
//! `O(wildcard patterns)`, and it allocates nothing.
//!
//! Two distinct suffixes cannot both match one host at the same length, so there is no tie to
//! break and declaration order cannot matter. `Config::validate` refuses a pattern that is not a
//! bare name and refuses two sites claiming one host, so the keys cannot collide.

use std::collections::BTreeMap;

/// Host pattern to value, with exact names and `*.` wildcards.
///
/// `V` is a site name in [`crate::sites::Sites`], and will be a certificate in the TLS
/// resolver. The table holds no default: what an unmatched host means differs between callers
/// — the default site, or the server's own certificate — so the caller supplies it. That is
/// deliberate, because a table that silently invented a fallback would be the thing that made
/// SNI and site selection disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMap<V> {
    exact: BTreeMap<String, V>,
    wildcard: BTreeMap<String, V>,
}

impl<V> Default for HostMap<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V> HostMap<V> {
    /// An empty table.
    pub fn new() -> Self {
        HostMap {
            exact: BTreeMap::new(),
            wildcard: BTreeMap::new(),
        }
    }

    /// Add a pattern, replacing any value already stored under it.
    ///
    /// The pattern is an exact name or a leading `*.` wildcard, lowercased and trimmed. The
    /// `*.` is stripped before storage, because what a wildcard is compared against is the
    /// host's *suffix* — keeping the marker would mean re-parsing it on every request.
    ///
    /// A malformed pattern is stored as written rather than rejected: validation owns that
    /// judgement and has already refused what it will not accept, so a second opinion here
    /// would be a second answer that could differ.
    pub fn insert(&mut self, pattern: &str, value: V) {
        let pattern = pattern.trim().to_ascii_lowercase();
        match pattern.strip_prefix("*.") {
            None => {
                self.exact.insert(pattern, value);
            }
            Some(suffix) => {
                self.wildcard.insert(suffix.to_string(), value);
            }
        }
    }

    /// The value for a `Host` header, or `None` when nothing claims it.
    ///
    /// The header is normalised here — trimmed, lowercased, and any `:port` removed — because
    /// a `Host` is a client's word and neither a port nor a case is part of a name anyone
    /// configured. An empty host, or an absent one, is `None`: there is nothing to match, and
    /// answering with a fallback is the caller's decision to make.
    pub fn get(&self, host: &str) -> Option<&V> {
        let host = strip_port(host);
        if host.is_empty() {
            return None;
        }
        let host = host.to_ascii_lowercase();

        if let Some(value) = self.exact.get(&host) {
            return Some(value);
        }

        // Walk the host's labels left to right, probing the suffix table at each step. A step
        // strips one leading label *before* probing, so `a.b.example.com` tries `b.example.com`
        // and then `example.com` — strictly longest first, which is the rule. It also never
        // tries the host itself, which is what keeps a bare `example.com` from being claimed by
        // a `*.example.com` rule: that is the wildcard's subdomain-only meaning, enforced by the
        // shape of the loop rather than by an extra comparison.
        //
        // The label being stripped must be non-empty. `.example.com` strips to the empty label
        // and the suffix `example.com`, which *is* in the table — so without this the wildcard
        // would be claimed by an empty label, and `host_matches` refuses exactly the same shape.
        // Two answers to "does this host match" is the drift this module exists to remove.
        let mut rest = host.as_str();
        while let Some((label, tail)) = rest.split_once('.') {
            if !label.is_empty() {
                if let Some(value) = self.wildcard.get(tail) {
                    return Some(value);
                }
            }
            rest = tail;
        }

        None
    }

    /// The value for an optional `Host` header.
    pub fn get_opt(&self, host: Option<&str>) -> Option<&V> {
        host.and_then(|host| self.get(host))
    }

    /// Whether any pattern is stored.
    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.wildcard.is_empty()
    }

    /// How many patterns are stored, exact and wildcard together.
    pub fn len(&self) -> usize {
        self.exact.len() + self.wildcard.len()
    }

    /// Every entry, exact names first then wildcards, each group in key order.
    ///
    /// For a caller that has to rebuild this table rather than resolve against it — the TLS
    /// resolver installing an ACME-issued certificate must keep every *other* name, and a reload
    /// of one site's certificate must not drop another's. Iteration order is therefore part of the
    /// contract: `BTreeMap` order, so a table rebuilt from this one and a table rebuilt identically
    /// agree, and a difference between two of them is attributable to the value rather than to
    /// where the iteration happened to start.
    ///
    /// Exact names before wildcards, matching the rule in the module doc: a reader of a rebuilt
    /// table sees the precedence in the order it is listed.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.exact.iter().chain(self.wildcard.iter())
    }
}

/// A `Host` header without its `:port`, handling a bracketed IPv6 literal.
///
/// The one normalisation in the project, so the site table, the certificate table and the route
/// table cannot strip a port differently. `host_matches` in [`crate::config`] calls this too,
/// which is what keeps the route table's comparison identical to this lookup's.
///
/// A bare `::1` has several colons and no port, so only a *single* colon followed by digits is
/// read as a port.
pub(crate) fn strip_port(host: &str) -> &str {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        // `[::1]:8080` and `[::1]`.
        return rest.split(']').next().unwrap_or(rest);
    }
    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => {
            name
        }
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HostMap<String> {
        let mut map = HostMap::new();
        for (pattern, value) in pairs {
            map.insert(pattern, value.to_string());
        }
        map
    }

    #[test]
    fn an_exact_name_wins_then_the_longest_wildcard() {
        let map = map(&[
            ("example.com", "exact"),
            ("*.example.com", "wide"),
            ("*.deep.example.com", "narrow"),
        ]);

        assert_eq!(map.get("example.com").map(String::as_str), Some("exact"));
        assert_eq!(map.get("a.example.com").map(String::as_str), Some("wide"));
        // The longer suffix wins, and it must: both match, so the answer is a real decision.
        assert_eq!(
            map.get("x.deep.example.com").map(String::as_str),
            Some("narrow")
        );
        assert_eq!(map.get("nothing.test"), None);
    }

    #[test]
    fn a_wildcard_does_not_claim_the_bare_domain() {
        // The subdomain-only meaning. Matching the bare name too would silently widen every
        // rule that used a wildcard.
        let map = map(&[("*.example.com", "wide")]);
        assert_eq!(map.get("a.example.com").map(String::as_str), Some("wide"));
        assert_eq!(map.get("example.com"), None, "a wildcard names a subdomain");
        assert_eq!(map.get("notexample.com"), None, "the dot must be there");
        assert_eq!(
            map.get(".example.com"),
            None,
            "an empty label is not a label"
        );
    }

    #[test]
    fn a_host_matches_without_its_port_and_without_regard_to_case() {
        let map = map(&[("example.com", "exact"), ("*.corp.test", "wild")]);

        assert_eq!(
            map.get("EXAMPLE.com:8080").map(String::as_str),
            Some("exact")
        );
        assert_eq!(
            map.get("corp.test:443"),
            None,
            "the bare suffix is not the subdomain"
        );
        assert_eq!(map.get("a.CORP.test:443").map(String::as_str), Some("wild"));
        assert_eq!(map.get("  spaced.test  "), None);
    }

    #[test]
    fn an_ipv6_literal_is_not_mistaken_for_a_port() {
        let mut map: HostMap<String> = HostMap::new();
        map.insert("::1", "loopback".to_string());

        assert_eq!(map.get("::1").map(String::as_str), Some("loopback"));
        assert_eq!(map.get("[::1]:8080").map(String::as_str), Some("loopback"));
    }

    #[test]
    fn an_empty_or_absent_host_matches_nothing() {
        // Not a fallback: what an unmatched host means is the caller's decision, and inventing
        // one here is exactly how SNI and site selection would come to disagree.
        let map = map(&[("example.com", "exact")]);
        assert_eq!(map.get(""), None);
        assert_eq!(map.get("   "), None);
        assert_eq!(map.get(":8080"), None);
        assert_eq!(map.get(":80").map(String::as_str), None);
        assert_eq!(map.get_opt(None), None);
        assert_eq!(map.get_opt(Some("")), None);
    }

    #[test]
    fn a_later_pattern_replaces_an_earlier_one() {
        // `validate` refuses a duplicate host across sites, so this only matters for a caller
        // building a table from something else — but "last write wins" must be defined rather
        // than accidental.
        let map = map(&[("a.test", "first"), ("a.test", "second")]);
        assert_eq!(map.get("a.test").map(String::as_str), Some("second"));
    }

    #[test]
    fn the_counts_cover_both_kinds() {
        let empty: HostMap<String> = HostMap::new();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);

        let map = map(&[("a.test", "1"), ("*.b.test", "2"), ("c.test", "3")]);
        assert!(!map.is_empty());
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn this_table_and_host_matches_agree_on_every_shape() {
        // The two implementations of "does this host match" must not drift, so they are
        // compared rather than trusted. `host_matches` is what the route table uses;
        // `HostMap::get` is what sites and certificates use. A disagreement is the bug this
        // module exists to prevent, and it would be invisible from either side alone.
        let patterns = [
            "example.com",
            "*.example.com",
            "*.deep.example.com",
            "corp.internal",
        ];
        let hosts = [
            "example.com",
            "www.example.com",
            "a.b.example.com",
            "x.deep.example.com",
            "deep.example.com",
            ".example.com",
            "a..example.com",
            "example.com.",
            "notexample.com",
            "EXAMPLE.COM",
            "example.com:8080",
            "corp.internal",
            "a.corp.internal",
            "elsewhere.test",
            "localhost",
            "",
        ];

        for pattern in patterns {
            let table = map(&[(pattern, "hit")]);
            for host in hosts {
                let from_map = table.get(host).is_some();
                let from_matches = crate::config::host_matches(pattern, Some(host));
                assert_eq!(
                    from_map, from_matches,
                    "pattern {pattern:?} against host {host:?}: HostMap said {from_map}, \
                     host_matches said {from_matches}"
                );
            }
        }
    }

    #[test]
    fn labels_are_walked_to_the_end_without_matching() {
        // A host with more labels than any suffix: the loop must terminate rather than spin,
        // and must not match a suffix it never reaches. `*.wide.example.com` is the wildcard
        // form, so a deeper host is a subdomain of it and does match.
        let table = map(&[("*.wide.example.com", "inner")]);
        assert_eq!(
            table
                .get("a.b.c.d.e.f.wide.example.com")
                .map(String::as_str),
            Some("inner")
        );
        // A single label has nothing to strip, so nothing is probed.
        assert_eq!(table.get("localhost"), None);

        // An exact pattern is not a suffix rule: it matches only itself.
        let exact = map(&[("wide.example.com", "inner")]);
        assert_eq!(
            exact.get("wide.example.com").map(String::as_str),
            Some("inner")
        );
        assert_eq!(exact.get("a.wide.example.com"), None);

        // A trailing dot leaves an empty label, which matches nothing rather than matching
        // everything.
        let wild = map(&[("*.example.com", "wide")]);
        assert_eq!(wild.get("example.com."), None);
    }
}
