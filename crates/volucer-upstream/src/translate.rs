//! Turning an inbound request into an origin's request, and an origin's answer back.
//!
//! Two functions, and they are the whole difference between this path and the CGI one. Both are
//! deliberately free of I/O so they can be tested without a socket: the assertions that matter here
//! are about *which headers survive and in what order*, and a test that needs a network to make
//! them is a test that will be skipped.
//!
//! # What is lost and what is not
//!
//! | | CGI path | this path |
//! | --- | --- | --- |
//! | header case | lowercased (a CGI variable cannot carry case) | preserved on the response leg; the request leg is normalised by `hyper`'s reader before any code sees it |
//! | duplicate headers | joined with `", "` into one variable | preserved — `HeaderMap` is a multimap |
//! | the request line | three separate variables | a method and a URI |
//! | the body | collected before dispatch | streamed |
//!
//! The request-leg case limitation is `hyper`'s, not a choice made here, and it is harmless: header
//! names are case-insensitive (RFC 9110 §5.1), so an origin reading `content-type` reads the same
//! header. A test asserting the original case on the request leg would be a test that cannot pass,
//! which is worse than not having one.

use hyper::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_LENGTH, HOST};
use hyper::{Method, Uri, Version};

use crate::client::{connection_named, UpstreamRequest, UpstreamResponse};

/// Headers that never cross a hop, by name.
///
/// `Content-Length` and `Transfer-Encoding` are here because `hyper` re-derives framing from the
/// body's size hint: forwarding a stale length is how a proxy desynchronises a connection, and a
/// chunked upload only works end to end if the incoming framing is dropped.
///
/// `Expect` is here for a reason of its own — see [`prepare_request`].
const NEVER_FORWARDED: [&str; 10] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "expect",
];

fn never_forwarded(name: &HeaderName) -> bool {
    NEVER_FORWARDED
        .iter()
        .any(|candidate| name.as_str().eq_ignore_ascii_case(candidate))
}

/// Build the request an origin should receive.
///
/// `peer` is the address the request came from, and `cleartext` whether it arrived without TLS —
/// which is not the same question as "is this deployment TLS", because `[server.tls].plain` makes a
/// cleartext request on a TLS port a real shape here. Both go into the `X-Forwarded-*` set.
///
/// # `X-Forwarded-*` is appended, never replaced
///
/// This is the one security-relevant rule in the function. An origin that trusts this server for
/// `X-Forwarded-For` must not be reachable by a client that simply sent its own: if the client's
/// value were replaced, the origin would read the attacker's claim as fact; if the client's value
/// were kept and ours dropped, the origin would read nothing true. Appending puts the real peer
/// *last*, which is the position a correct origin reads, and leaves the client's own claim visible
/// for the origin to discount. `X-Forwarded-Proto` and `-Host` follow the same rule for the same
/// reason.
///
/// # `Expect: 100-continue` is stripped
///
/// We forward on the same connection the client is waiting on and never generate an interim `100`,
/// so an `Expect` that reached the origin would make a client wait for a reply that is coming from
/// a place it is not listening. Stripping it makes the client stop waiting, which is correct.
pub fn prepare_request<B>(
    headers: HeaderMap,
    method: Method,
    uri: Uri,
    version: Version,
    body: B,
    peer: Option<std::net::IpAddr>,
    cleartext: bool,
) -> UpstreamRequest<B> {
    // Read before filtering: `Connection: X-Private` says `X-Private` is hop-local *for this
    // message*, so it has to be dropped along with the names above. Skipping this rule is how a
    // proxy leaks hop-local headers into the next hop.
    let named: Vec<HeaderName> = connection_named(&headers)
        .into_iter()
        .filter(|name| !never_forwarded(name))
        .collect();

    // An address, not a `SocketAddr`: the port of an *incoming* connection is not part of what
    // `X-Forwarded-For` carries, and a caller that has only the peer's address (which is what a
    // listener reports) must not have to invent one to satisfy this signature.
    let forwarded_for = peer.map(|peer| peer.to_string());
    let forwarded_host = headers
        .get(HOST)
        .and_then(|host| host.to_str().ok())
        .map(str::to_string);

    // Copied rather than filtered in place, because `HeaderMap` has no retain-while-preserving-all-
    // values operation and `remove` by name would need the full list of names up front — including
    // the ones only this message's own `Connection` header knows about.
    let mut outgoing = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers.iter() {
        if never_forwarded(name) || named.iter().any(|named| named == name) {
            continue;
        }
        if name == CONTENT_LENGTH {
            continue;
        }
        outgoing.append(name.clone(), value.clone());
    }
    drop(headers);

    if let Some(address) = forwarded_for {
        // Appended, so a client-supplied value survives *before* ours rather than instead of it.
        if let Ok(value) = HeaderValue::from_str(&address) {
            outgoing.append("x-forwarded-for", value);
        }
    }
    if let Some(host) = forwarded_host {
        if let Ok(value) = HeaderValue::from_str(&host) {
            outgoing.append("x-forwarded-host", value);
        }
    }
    // From the socket, not from a header: `X-Forwarded-Proto` describes how *this* hop was
    // received, and a client cannot be allowed to claim it was TLS when it was not.
    outgoing.append(
        "x-forwarded-proto",
        HeaderValue::from_static(if cleartext { "http" } else { "https" }),
    );

    // `TE` and `TRAILER` are referenced so the constants above stay the single source of names;
    // `CONNECTION`, `EXPECT`, `TRANSFER_ENCODING`, `UPGRADE` likewise.

    UpstreamRequest {
        method,
        uri,
        version,
        headers: outgoing,
        body,
    }
}

/// Make an origin's answer safe to pass to the client.
///
/// `Content-Length` is **kept**, and this is the one place the two paths deliberately differ.
/// `from_cgi` drops it and recomputes, because a CGI response's length is application-supplied and
/// can be wrong. An HTTP origin's is protocol-level framing: `hyper` re-validates it against the
/// frames it reads, so a mismatch is a clean protocol error rather than a desynchronised
/// connection — which is the failure mode to prefer.
pub fn translate_response(response: UpstreamResponse) -> UpstreamResponse {
    let names = connection_named(&response.headers);
    let UpstreamResponse {
        status,
        mut headers,
        body,
        version,
    } = response;

    let mut outgoing = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers.iter() {
        if never_forwarded(name) || names.iter().any(|named| named == name) {
            continue;
        }
        // `Expect` is an inbound request directive; an origin echoing one back is not a reason to
        // drop anything else, but it must not be forwarded either.
        outgoing.append(name.clone(), value.clone());
    }
    headers = outgoing;

    UpstreamResponse {
        status,
        headers,
        body,
        version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Empty};
    use hyper::header::{CONNECTION, EXPECT, TRANSFER_ENCODING, UPGRADE};
    use std::net::IpAddr;

    fn peer() -> Option<IpAddr> {
        // An address with no port, which is what a listener reports and all `X-Forwarded-For`
        // carries.
        Some("203.0.113.7".parse().expect("an address"))
    }

    fn request(headers: HeaderMap) -> UpstreamRequest<Empty<bytes::Bytes>> {
        prepare_request(
            headers,
            Method::GET,
            Uri::from_static("/api?x=1"),
            Version::HTTP_11,
            Empty::new(),
            peer(),
            true,
        )
    }

    fn values(headers: &HeaderMap, name: &str) -> Vec<String> {
        headers
            .get_all(name)
            .iter()
            .map(|value| value.to_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn hop_by_hop_headers_do_not_cross() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        headers.insert("keep-alive", HeaderValue::from_static("timeout=5"));
        headers.insert(TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        headers.insert(UPGRADE, HeaderValue::from_static("websocket"));
        headers.insert("proxy-authorization", HeaderValue::from_static("Basic x"));
        let outgoing = request(headers).headers;

        for absent in [
            "connection",
            "keep-alive",
            "transfer-encoding",
            "upgrade",
            "proxy-authorization",
        ] {
            assert!(
                !outgoing.contains_key(absent),
                "{absent} must not cross a hop"
            );
        }
    }

    #[test]
    fn a_header_named_by_connection_is_also_hop_local() {
        // The RFC 9110 §7.6.1 rule that is easy to miss, and the one a proxy test should pin:
        // `Connection: X-Private` makes `X-Private` hop-by-hop for *this* message.
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("X-Private"));
        headers.insert("X-Private", HeaderValue::from_static("secret"));
        headers.insert("x-public", HeaderValue::from_static("fine"));
        let outgoing = request(headers).headers;

        assert!(!outgoing.contains_key("x-private"), "named by Connection");
        assert!(outgoing.contains_key("x-public"), "not named");
    }

    #[test]
    fn content_length_and_expect_are_dropped_from_the_request() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("12"));
        headers.insert(EXPECT, HeaderValue::from_static("100-continue"));
        let outgoing = request(headers).headers;

        // hyper re-derives framing from the body, and a stale length desynchronises a connection.
        assert!(!outgoing.contains_key("content-length"));
        // We never emit an interim 100, so a client waiting for one would wait for nothing.
        assert!(!outgoing.contains_key("expect"));
    }

    #[test]
    fn duplicate_headers_survive_as_duplicates() {
        // The loss the CGI path makes on purpose (`build_params` joins with ", ") and this path
        // must not: two `Cookie` headers are two cookies, and a join changes their meaning.
        let mut headers = HeaderMap::new();
        headers.append("x-tag", HeaderValue::from_static("one"));
        headers.append("x-tag", HeaderValue::from_static("two"));
        let outgoing = request(headers).headers;

        assert_eq!(values(&outgoing, "x-tag"), vec!["one", "two"]);
    }

    #[test]
    fn the_clients_host_is_kept_verbatim() {
        // It is the vhost identity the origin routes on, so it must be what the client asked for
        // and not what this server dialled.
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_static("shop.example.com"));
        let outgoing = request(headers).headers;

        assert_eq!(values(&outgoing, "host"), vec!["shop.example.com"]);
        assert_eq!(
            values(&outgoing, "x-forwarded-host"),
            vec!["shop.example.com"]
        );
    }

    #[test]
    fn forwarded_for_is_appended_to_the_clients_own_claim() {
        let mut headers = HeaderMap::new();
        headers.append("x-forwarded-for", HeaderValue::from_static("198.51.100.9"));
        let outgoing = request(headers).headers;

        // The client's claim stays visible, and the peer we actually saw is last — which is the
        // position a correct origin reads. Replacing instead would let a client forge its own
        // address to an origin that trusts this server for the header.
        assert_eq!(
            values(&outgoing, "x-forwarded-for"),
            vec!["198.51.100.9", "203.0.113.7"]
        );
    }

    #[test]
    fn forwarded_proto_comes_from_the_socket_not_from_a_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        // A cleartext request that *claims* to be https: the claim must not become the only value,
        // and ours must say what actually happened.
        let outgoing = prepare_request(
            headers,
            Method::GET,
            Uri::from_static("/"),
            Version::HTTP_11,
            Empty::<bytes::Bytes>::new(),
            peer(),
            true,
        )
        .headers;

        assert_eq!(
            values(&outgoing, "x-forwarded-proto"),
            vec!["https", "http"]
        );
    }

    #[test]
    fn a_request_arriving_without_a_peer_still_gets_a_proto() {
        let outgoing = prepare_request(
            HeaderMap::new(),
            Method::GET,
            Uri::from_static("/"),
            Version::HTTP_11,
            Empty::<bytes::Bytes>::new(),
            None,
            false,
        )
        .headers;

        assert!(!outgoing.contains_key("x-forwarded-for"));
        assert_eq!(values(&outgoing, "x-forwarded-proto"), vec!["https"]);
    }

    fn response(headers: HeaderMap) -> UpstreamResponse {
        translate_response(UpstreamResponse {
            status: hyper::StatusCode::OK,
            headers,
            body: http_body_util::Full::new(bytes::Bytes::new())
                .map_err(|never: std::convert::Infallible| match never {})
                .boxed_unsync(),
            version: Version::HTTP_11,
        })
    }

    #[test]
    fn two_set_cookie_headers_stay_two() {
        let mut headers = HeaderMap::new();
        headers.append("set-cookie", HeaderValue::from_static("a=1; Path=/"));
        headers.append("set-cookie", HeaderValue::from_static("b=2; Path=/"));
        let outgoing = response(headers).headers;

        assert_eq!(
            values(&outgoing, "set-cookie"),
            vec!["a=1; Path=/", "b=2; Path=/"]
        );
    }

    #[test]
    fn the_origins_content_length_is_kept() {
        // The deliberate difference from `from_cgi`: this one is protocol framing, and hyper
        // re-validates it, so a mismatch is a protocol error rather than a desync.
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("5"));
        let outgoing = response(headers).headers;

        assert_eq!(values(&outgoing, "content-length"), vec!["5"]);
    }

    #[test]
    fn the_response_side_honours_connection_naming_too() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, HeaderValue::from_static("X-Origin-Internal"));
        headers.insert("X-Origin-Internal", HeaderValue::from_static("hop"));
        headers.insert("content-type", HeaderValue::from_static("text/plain"));
        let outgoing = response(headers).headers;

        assert!(!outgoing.contains_key("x-origin-internal"));
        assert!(!outgoing.contains_key("connection"));
        assert_eq!(values(&outgoing, "content-type"), vec!["text/plain"]);
    }

    #[test]
    fn header_names_are_compared_case_insensitively() {
        // A client sending `CONNECTION` in any case still gets the header dropped; `HeaderName`
        // normalises, so this pins that the *rule* does not depend on it.
        let mut headers = HeaderMap::new();
        headers.insert("Connection", HeaderValue::from_static("X-Y"));
        headers.insert("x-y", HeaderValue::from_static("z"));
        let outgoing = request(headers).headers;

        assert!(!outgoing.contains_key("x-y"));
    }
}
