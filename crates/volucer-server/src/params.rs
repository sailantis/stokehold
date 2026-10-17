//! The `Call` payload format: parameters read out of a JSON object, and bytes that survive it.
//!
//! # Why JSON, and why this shape
//!
//! `docs/plugins.md` §4.4 says params and results are JSON "and the codec stays behind its boundary;
//! msgpack remains available for the same reason it does in the worker protocol". So this is the codec
//! for one half of one message pair, and it is deliberately the smallest thing that works:
//!
//! ```json
//! // params
//! {"key": "c2Vzc2lvbjox", "ttl": 60}
//! // a result
//! {"value": "…", "hits": 3}
//! ```
//!
//! A flat JSON object, because that is what a PHP caller produces with `json_encode` and what a Rust
//! handler reads with `serde_json`. The alternative considered and rejected was a
//! `#[derive(Deserialize)]` struct per method: a dozen structs for KV alone, each a place for the wire
//! format and the code to drift apart, and each needing its own test to pin it.
//!
//! It lives in `volucer-server` rather than beside any one service because it is about the *envelope* —
//! how a call's arguments are read — and not about what any service does with them. A KV storage, a lock
//! and a job queue all read their arguments this way, and a copy of this per service is a copy per
//! place a rule about wrong types can be got wrong.
//!
//! # The rules this enforces, and why each is not left to the caller
//!
//! - **An absent `params` is `{}`, not an error.** A call with no arguments is the common case, and a
//!   handler that had to write `call.params.as_object().unwrap_or_default()` at every entry point is a
//!   handler with a panic waiting for the first client that omits it.
//! - **A param that is present with the wrong type is an error, not a default.** This is the difference
//!   between `ttl = "60"` being refused and being silently zero, and the second one is a cache that grows
//!   for ever because the caller's mistake looked like "no expiry".
//! - **Unknown params are ignored**, deliberately. The registry answers by name, so a client sending an
//!   extra field is a client built against a newer server, and refusing it would make forward
//!   compatibility cost something when it costs nothing.

use serde_json::{Map, Value};
use std::time::Duration;

/// The params of a call, with typed accessors that refuse the wrong type rather than defaulting it.
#[derive(Debug, Clone, Default)]
pub struct Params {
    object: Map<String, Value>,
}

/// A parameter was present but not what the method asked for.
#[derive(Debug, thiserror::Error)]
#[error("parameter {name:?} {reason}")]
pub struct ParamError {
    name: String,
    reason: String,
}

/// The result of reading one parameter.
pub type ParamResult<T> = std::result::Result<T, ParamError>;

impl Params {
    /// The `params` of a call, which may be absent, `null`, an object, or **an empty list**.
    ///
    /// # Why an empty list is accepted
    ///
    /// Because PHP cannot produce an empty object. `json_encode([])` is `[]`, not `{}`, so a client
    /// written in PHP that calls a no-argument method — the natural spelling of
    /// `Worker::call('jobs.pipelines')` — sends `"params": []` and has no way to send anything else
    /// without being awkward about it. Refusing that would make every no-argument method uncallable from
    /// the language this client exists for, and the failure is a *refusal of the call itself*, which reads
    /// as a server bug.
    ///
    /// This was found by the first end-to-end test of a no-argument method through `Params`, and it is
    /// worth recording because the shape of the mistake is subtle: the rule below was written for a client
    /// that confused two container types, and an empty array is not that client.
    ///
    /// # And why a non-empty list is still refused
    ///
    /// `"params": [1,2,3]` is the case the rule was written for: a client that confused the two JSON
    /// container types, whose arguments would otherwise read as "no arguments" and let the method run with
    /// defaults. That is how a `delete` becomes a `clear`, and the distinction that keeps it safe is
    /// *empty* rather than "is a list".
    pub fn from_value(value: &Value, method: &str) -> ParamResult<Self> {
        match value {
            Value::Null => Ok(Params::default()),
            Value::Object(object) => Ok(Params {
                object: object.clone(),
            }),
            // The one list that is not a mistake — see above.
            Value::Array(items) if items.is_empty() => Ok(Params::default()),
            other => Err(ParamError {
                name: "params".to_string(),
                reason: format!(
                    "is a {} and {} takes an object; see the method's documentation for its arguments",
                    json_type_of(other),
                    method
                ),
            }),
        }
    }

    /// Whether the call carried no parameters at all.
    pub fn is_empty(&self) -> bool {
        self.object.is_empty()
    }

    /// The raw value of a parameter, if it is present.
    ///
    /// Present-and-`null` is a value, not an absence: `ttl = null` is how a client says "no expiry"
    /// where `ttl` absent might mean "the method's default". Methods that do not care about the
    /// difference should use the typed accessors, which treat `null` as absent.
    pub fn raw(&self, name: &str) -> Option<&Value> {
        self.object.get(name)
    }

    /// A string parameter.
    pub fn string(&self, name: &str) -> ParamResult<Option<String>> {
        match self.raw(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(other) => Err(ParamError {
                name: name.to_string(),
                reason: format!(
                    "is a {}, and this method needs a string",
                    json_type_of(other)
                ),
            }),
        }
    }

    /// A string parameter that must be present and non-empty.
    ///
    /// `Some("")` is refused rather than accepted, because the two things a required string parameter
    /// names — a key, a storage — are both meaningless when empty, and a handler that received one would
    /// have to refuse it anyway with the same message.
    pub fn required_string(&self, name: &str) -> ParamResult<String> {
        match self.string(name)? {
            Some(value) if !value.is_empty() => Ok(value),
            Some(_) => Err(ParamError {
                name: name.to_string(),
                reason: "is empty, and this method needs a non-empty one".to_string(),
            }),
            None => Err(ParamError {
                name: name.to_string(),
                reason: "is required and was not given".to_string(),
            }),
        }
    }

    /// A number parameter that must be a non-negative integer within `u64`.
    pub fn unsigned(&self, name: &str) -> ParamResult<Option<u64>> {
        match self.raw(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(number)) => {
                match number.as_u64() {
                    Some(value) => Ok(Some(value)),
                    None => Err(ParamError {
                        name: name.to_string(),
                        // The two ways this fails are worth telling apart in the message: a float, and a
                        // negative integer. `-1` reaching a TTL is the one that really matters, because a
                        // TTL of `-1` seconds would otherwise become a duration computed by wrapping.
                        reason: if number.as_f64().is_some_and(|f| f < 0.0) {
                            "is negative, and this method needs a count of seconds or items"
                                .to_string()
                        } else if number.is_f64() {
                            "is fractional, and this method needs a whole number".to_string()
                        } else {
                            "is too large to be a count".to_string()
                        },
                    }),
                }
            }
            Some(other) => Err(ParamError {
                name: name.to_string(),
                reason: format!(
                    "is a {}, and this method needs a number",
                    json_type_of(other)
                ),
            }),
        }
    }

    /// A duration parameter, in microseconds.
    ///
    /// **Microseconds, not seconds**, and that is the one place this module's units are not the obvious
    /// ones. The reason is RoadRunner's own wire format: its lock API takes `Request.ttl` and
    /// `Request.wait` in microseconds and its PHP SDK converts from seconds, precisely so that a
    /// sub-second wait is expressible. Taking seconds here would make `wait` coarser than the thing the
    /// lock is for — a contended cache fill wants tens of milliseconds — and the conversion belongs on
    /// the client, where the application's own units already live.
    ///
    /// A float is refused rather than rounded, and a negative value is refused with a message that says
    /// so: `wait = -1` becoming a duration by wrapping is the kind of quiet arithmetic mistake that turns
    /// "do not wait" into "wait for ever".
    pub fn microseconds(&self, name: &str) -> ParamResult<Option<Duration>> {
        match self.unsigned(name)? {
            None => Ok(None),
            Some(micros) => Ok(Some(Duration::from_micros(micros))),
        }
    }

    /// A boolean parameter.
    pub fn boolean(&self, name: &str) -> ParamResult<Option<bool>> {
        match self.raw(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(other) => Err(ParamError {
                name: name.to_string(),
                reason: format!(
                    "is a {}, and this method needs a boolean",
                    json_type_of(other)
                ),
            }),
        }
    }

    /// A list of strings, each of which must be a string.
    ///
    /// One bad element refuses the whole list rather than skipping it, for the same reason a wrong-typed
    /// single parameter does: a `deleteMany` that quietly dropped the key it could not parse is a method
    /// that reports deleting more than it did.
    pub fn string_list(&self, name: &str) -> ParamResult<Option<Vec<String>>> {
        match self.raw(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for (index, item) in items.iter().enumerate() {
                    match item.as_str() {
                        Some(text) => out.push(text.to_string()),
                        None => {
                            return Err(ParamError {
                                name: format!("{name}[{index}]"),
                                reason: format!(
                                    "is a {}, and this method needs strings",
                                    json_type_of(item)
                                ),
                            })
                        }
                    }
                }
                Ok(Some(out))
            }
            Some(other) => Err(ParamError {
                name: name.to_string(),
                reason: format!("is a {}, and this method needs a list", json_type_of(other)),
            }),
        }
    }

    /// A `base64` parameter, decoded.
    ///
    /// The encoding is how an opaque byte string crosses a JSON wire, and it is **not** negotiable per
    /// call: a method that accepted raw text would corrupt an application's compressed or encrypted value
    /// on any key containing a byte above `0x7f`. See `volucer-kv`'s module documentation for why the
    /// value is opaque in the first place.
    pub fn base64(&self, name: &str) -> ParamResult<Option<Vec<u8>>> {
        match self.string(name)? {
            None => Ok(None),
            Some(encoded) => decode_base64(&encoded)
                .map(Some)
                .map_err(|reason| ParamError {
                    name: name.to_string(),
                    reason,
                }),
        }
    }

    /// A `base64` parameter that must be present.
    pub fn required_base64(&self, name: &str) -> ParamResult<Vec<u8>> {
        self.base64(name)?.ok_or_else(|| ParamError {
            name: name.to_string(),
            reason: "is required and was not given".to_string(),
        })
    }
}

/// The JSON type of a value, for an error message.
fn json_type_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "list",
        Value::Object(_) => "object",
    }
}

/// The standard alphabet with padding, which is what `base64_encode` in PHP produces by default.
///
/// Not the URL-safe alphabet, and that is worth stating: a client writing `strtr(base64_encode(...))`
/// would produce something this refuses, and the failure mode of accepting both would be a key whose
/// bytes differ depending on which client wrote it.
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode bytes the way PHP's `base64_encode` does.
///
/// Hand-written, and the same argument `volucer-metrics` makes for its exposition encoder: it is a page
/// of specification, the dependency is in the tree only as a transitive of the TLS stack (so naming it
/// could not add a crate, but pinning it here would make a *protocol* format depend on a version of a
/// crate that `rustls` chose), and the round trip is pinned by a test against vectors from PHP's own
/// implementation.
pub fn encode_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(BASE64_ALPHABET[(n >> 18) as usize & 0x3f] as char);
        out.push(BASE64_ALPHABET[(n >> 12) as usize & 0x3f] as char);
        // Padding, so the length of the encoded form says the length of the decoded one: a client
        // that has to guess whether it got 2 or 3 bytes is a client that will guess wrong.
        out.push(if chunk.len() > 1 {
            BASE64_ALPHABET[(n >> 6) as usize & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            BASE64_ALPHABET[n as usize & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

/// Decode standard base64, refusing anything that is not exactly that.
pub fn decode_base64(encoded: &str) -> std::result::Result<Vec<u8>, String> {
    let bytes = encoded.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(format!(
            "is not valid base64: its length {} is not a multiple of 4, so a character was lost in \
             transit",
            bytes.len()
        ));
    }

    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (index, chunk) in bytes.chunks(4).enumerate() {
        let mut n = 0u32;
        let mut padding = 0usize;
        for (at, byte) in chunk.iter().enumerate() {
            let value = match byte {
                b'=' => {
                    // Padding is only legal in the last two columns of the last chunk.
                    if at < 2 || index != bytes.len() / 4 - 1 {
                        return Err(format!(
                            "is not valid base64: padding at position {}",
                            index * 4 + at
                        ));
                    }
                    padding += 1;
                    0
                }
                _ if padding > 0 => {
                    return Err(format!(
                        "is not valid base64: a character after padding at position {}",
                        index * 4 + at
                    ))
                }
                b => BASE64_ALPHABET
                    .iter()
                    .position(|candidate| candidate == b)
                    .ok_or_else(|| {
                        format!(
                            "is not valid base64: {b:?} at position {} is not in the standard alphabet",
                            index * 4 + at
                        )
                    })? as u32,
            };
            n = n << 6 | value;
        }
        out.push((n >> 16) as u8);
        if padding < 2 {
            out.push((n >> 8) as u8);
        }
        if padding < 1 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params_of(json: &str) -> Params {
        let value: Value = serde_json::from_str(json).expect("the fixture is JSON");
        Params::from_value(&value, "test.method").expect("the fixture is an object")
    }

    #[test]
    fn an_absent_or_null_params_is_an_empty_set_and_anything_else_is_refused() {
        for json in ["null", "{}"] {
            let value: Value = serde_json::from_str(json).unwrap();
            let params = Params::from_value(&value, "kv.get").expect("accepted");
            assert!(params.is_empty(), "{json} should carry no parameters");
        }

        // **An empty list is accepted**, and it is the one list that is not a mistake: PHP cannot produce
        // an empty object, so `json_encode([])` is `[]` and a no-argument call from the language this
        // client exists for arrives that way. Found by the first end-to-end test of a no-argument method.
        let value: Value = serde_json::from_str("[]").unwrap();
        let params =
            Params::from_value(&value, "jobs.pipelines").expect("an empty list is no arguments");
        assert!(params.is_empty());

        // The one that matters: a client that confused the two JSON container types would otherwise get
        // the method's defaults, which is how a `delete` becomes a `clear`. A *non-empty* list is still
        // refused, and the boundary between the two is exactly "empty".
        let value: Value = serde_json::from_str("[1,2,3]").unwrap();
        let err = Params::from_value(&value, "kv.set").expect_err("must be refused");
        assert!(err.to_string().contains("list"), "{err}");
        assert!(
            err.to_string().contains("kv.set"),
            "the message must name the method that was called: {err}"
        );
    }

    #[test]
    fn a_present_parameter_of_the_wrong_type_is_refused_rather_than_defaulted() {
        let p = params_of(r#"{"ttl": "60"}"#);
        let err = p.unsigned("ttl").expect_err("must be refused");
        assert!(
            err.to_string().contains("string"),
            "the message must say what was sent: {err}"
        );
        assert!(
            !err.to_string().contains("default"),
            "and must not offer a default, because a TTL of zero from a string is how a cache grows \
             for ever: {err}"
        );
    }

    #[test]
    fn a_negative_or_fractional_number_is_told_apart_in_the_message() {
        let negative = params_of(r#"{"ttl": -1}"#)
            .unsigned("ttl")
            .expect_err("refused");
        assert!(negative.to_string().contains("negative"), "{negative}");

        let fractional = params_of(r#"{"ttl": 1.5}"#)
            .unsigned("ttl")
            .expect_err("refused");
        assert!(
            fractional.to_string().contains("fractional"),
            "{fractional}"
        );

        // And the three that are fine, including the boundary the conversion turns on.
        assert_eq!(params_of(r#"{"ttl": 0}"#).unsigned("ttl").unwrap(), Some(0));
        assert_eq!(
            params_of(r#"{"ttl": 18446744073709551615}"#)
                .unsigned("ttl")
                .unwrap(),
            Some(u64::MAX)
        );
        assert_eq!(params_of("{}").unsigned("ttl").unwrap(), None);
        assert_eq!(
            params_of(r#"{"ttl": null}"#).unsigned("ttl").unwrap(),
            None,
            "an explicit null means the same as absent, because a client that clears the field is \
             expressing the same thing"
        );
    }

    #[test]
    fn a_required_string_is_refused_when_absent_or_empty() {
        let p = params_of("{}");
        assert!(p
            .required_string("key")
            .expect_err("absent is refused")
            .to_string()
            .contains("required"));

        let p = params_of(r#"{"key": ""}"#);
        assert!(
            p.required_string("key")
                .expect_err("empty is refused")
                .to_string()
                .contains("empty"),
            "an empty key is refused as empty rather than as absent, because the two are different \
             mistakes and the message should say which"
        );

        let p = params_of(r#"{"key": "session:1"}"#);
        assert_eq!(p.required_string("key").unwrap(), "session:1");
    }

    #[test]
    fn a_list_with_one_bad_element_is_refused_whole() {
        let p = params_of(r#"{"keys": ["a", "b"]}"#);
        assert_eq!(
            p.string_list("keys").unwrap(),
            Some(vec!["a".to_string(), "b".to_string()])
        );

        let p = params_of(r#"{"keys": ["a", 2]}"#);
        let err = p.string_list("keys").expect_err("refused");
        assert!(
            err.to_string().contains("keys[1]"),
            "the message must name the element, or a caller with a hundred keys has to guess: {err}"
        );
    }

    #[test]
    fn base64_round_trips_and_matches_phps_own_encoder() {
        // The vectors are from PHP's `base64_encode`, which is what a client will use. Pinned rather
        // than computed, because the point is that the two implementations agree — a test that
        // round-tripped through this crate's own encoder would pass with any self-consistent alphabet,
        // including one PHP cannot read.
        let vectors: &[(&[u8], &str)] = &[
            (b"", ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
            // The three cases the padding exists for: one byte left over, two, and none.
            (&[0xff], "/w=="),
            (&[0xff, 0xfe], "//4="),
            (&[0x00, 0x00, 0x00], "AAAA"),
        ];
        for (bytes, encoded) in vectors {
            assert_eq!(
                &encode_base64(bytes),
                encoded,
                "encoding {bytes:?} must match PHP's base64_encode"
            );
            assert_eq!(
                &decode_base64(encoded).expect("decodes"),
                bytes,
                "and {encoded} must decode back to {bytes:?}"
            );
        }

        // Anything an application might store, including the bytes that make a text wire format fail.
        let opaque: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode_base64(&encode_base64(&opaque)).unwrap(), opaque);
    }

    #[test]
    fn malformed_base64_is_refused_with_the_reason() {
        for (bad, expected) in [
            ("Zg=", "multiple of 4"),
            ("Zg===", "multiple of 4"),
            ("Z===", "padding at position 1"),
            ("Zg==Zg==", "padding at position 2"),
            ("Zg=Z", "after padding"),
            ("Zm9!", "not in the standard alphabet"),
            ("Zm-v", "not in the standard alphabet"),
        ] {
            let err = decode_base64(bad).expect_err("must be refused");
            assert!(
                err.contains(expected),
                "{bad:?} should be refused with {expected:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn a_rounded_trip_through_the_wire_keeps_every_byte() {
        // The property the whole opaque-bytes design rests on, as one test: whatever an application
        // stores comes back byte for byte, through the base64 a JSON wire requires.
        for len in 0..64usize {
            let value: Vec<u8> = (0..len).map(|i| (i * 37 % 256) as u8).collect();
            let json = serde_json::json!({ "value": encode_base64(&value) });
            let params = Params::from_value(&json, "kv.set").unwrap();
            assert_eq!(params.required_base64("value").unwrap(), value);
        }
    }
}
