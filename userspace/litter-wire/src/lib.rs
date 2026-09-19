//! Wire format for the `litter-hub` TCP relay described in
//! `userspace/meow/docs/LITTER_EXPERIMENT.md` ("Where this is headed" ->
//! "Transport: filesystem mailbox -> TCP hub"). This crate is pure
//! encode/decode + framing logic and does no I/O of its own — same shape as
//! `litter-raft`: a caller on each end owns the actual socket (`meow` reads
//! and writes through `libakuma::net::TcpStream`, no_std; `litter-hub` reads
//! and writes through `std::net::TcpStream`) and this crate is what stops the
//! two ends from independently drifting on what the bytes between them mean.
//!
//! JSON reading and writing both go through `nojson`
//! (`DisplayJson` to write, `TryFrom<RawJsonValue>` to read) rather than a
//! hand-rolled scanner/writer pair: it has zero dependencies of its own, no
//! unsafe, no macros, and — unlike a flat-object-only hand-rolled reader —
//! handles genuinely nested JSON, so `Response::Inbox`'s message list is an
//! honest JSON array of objects rather than an array of pre-escaped strings
//! each holding one object's text.
//!
//! ## Framing
//!
//! Every message, request or response, is a 4-byte big-endian length prefix
//! followed by that many bytes of JSON (`encode_len_header`/`decode_len_header`,
//! `MAX_FRAME_LEN`). One request per connection, one response back, then the
//! connection closes — `meow`'s own `TcpStream` has no listener/accept side at
//! all (a deliberate absence, so no code in `meow` can ever bind a socket),
//! and every `meow` invocation that talks to the hub is itself one-shot (see
//! "Debate protocol" in the doc above), so there is no long-lived connection
//! to multiplex requests over in the first place.
//!
//! ## Versioning
//!
//! Every request and response carries a top-level `"v"` field
//! (`PROTOCOL_VERSION`). `decode_request`/`decode_response` check it *before*
//! looking at anything else and return `WireError::UnsupportedVersion` rather
//! than guessing at a shape a future version might have changed — a hub
//! speaking v1 must refuse a v2 client's request outright, not
//! partially-parse it and misroute a field that moved.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use nojson::{DisplayJson, JsonFormatter, RawJsonValue};
pub use nojson::JsonParseError;

/// Bumped whenever a request or response's JSON shape changes in a way an
/// older decoder can't safely ignore (a field removed, a meaning changed —
/// not a field added, since every decode here already ignores unknown keys).
pub const PROTOCOL_VERSION: i64 = 1;

/// Sanity cap on a single frame's declared length, checked against the 4-byte
/// header before a caller allocates a buffer for it — generous enough for a
/// whole debate round's worth of inbox messages, small enough that a
/// corrupt or hostile length prefix can't drive an unbounded allocation.
pub const MAX_FRAME_LEN: u32 = 1024 * 1024;

#[derive(Debug)]
pub enum WireError {
    /// The document isn't valid JSON, or isn't the shape this protocol
    /// expects for the `op` in question (missing/wrong-typed field, unknown
    /// `op`) — the latter raised through `RawJsonValue::invalid` so it still
    /// carries nojson's position information.
    Parse(JsonParseError),
    /// The document parsed, but its `"v"` doesn't match `PROTOCOL_VERSION`.
    /// Carries the version it actually claimed so the caller can log it.
    UnsupportedVersion(i64),
}

impl From<JsonParseError> for WireError {
    fn from(e: JsonParseError) -> Self {
        WireError::Parse(e)
    }
}

/// One mailbox entry as it travels over the wire. `ts` is hub-assigned
/// (microseconds since the Unix epoch, hub-local clock) rather than
/// client-supplied, precisely so that an inbox's ordering doesn't depend on
/// every agent's clock agreeing — the filesystem mailbox got this for free
/// from the write timestamp in each envelope's filename; the wire protocol
/// has no filename, so the field moves into the message itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub from: String,
    pub round: i64,
    pub body: String,
    pub ts: u64,
}

impl DisplayJson for Message {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> core::fmt::Result {
        f.object(|f| {
            f.member("from", &self.from)?;
            f.member("round", self.round)?;
            f.member("body", &self.body)?;
            f.member("ts", self.ts)
        })
    }
}

impl<'text, 'raw> TryFrom<RawJsonValue<'text, 'raw>> for Message {
    type Error = JsonParseError;

    fn try_from(value: RawJsonValue<'text, 'raw>) -> Result<Self, Self::Error> {
        let from: String = value.to_member("from")?.required()?.try_into()?;
        let body: String = value.to_member("body")?.required()?.try_into()?;
        let round: Option<i64> = value.to_member("round")?.try_into()?;
        let ts: Option<u64> = value.to_member("ts")?.try_into()?;
        Ok(Message { from, round: round.unwrap_or(0), body, ts: ts.unwrap_or(0) })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Send { from: String, to: String, body: String, round: i64 },
    Inbox { name: String },
    Peers,
}

impl DisplayJson for Request {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> core::fmt::Result {
        match self {
            Request::Send { from, to, body, round } => f.object(|f| {
                f.member("v", PROTOCOL_VERSION)?;
                f.member("op", "send")?;
                f.member("from", from)?;
                f.member("to", to)?;
                f.member("body", body)?;
                f.member("round", *round)
            }),
            Request::Inbox { name } => f.object(|f| {
                f.member("v", PROTOCOL_VERSION)?;
                f.member("op", "inbox")?;
                f.member("name", name)
            }),
            Request::Peers => f.object(|f| {
                f.member("v", PROTOCOL_VERSION)?;
                f.member("op", "peers")
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Sent { bytes: usize },
    Inbox { messages: Vec<Message> },
    Peers { names: Vec<String> },
    Error { message: String },
}

impl DisplayJson for Response {
    fn fmt(&self, f: &mut JsonFormatter<'_, '_>) -> core::fmt::Result {
        match self {
            Response::Sent { bytes } => f.object(|f| {
                f.member("v", PROTOCOL_VERSION)?;
                f.member("ok", true)?;
                f.member("op", "sent")?;
                f.member("bytes", *bytes)
            }),
            Response::Inbox { messages } => f.object(|f| {
                f.member("v", PROTOCOL_VERSION)?;
                f.member("ok", true)?;
                f.member("op", "inbox")?;
                f.member("messages", messages)
            }),
            Response::Peers { names } => f.object(|f| {
                f.member("v", PROTOCOL_VERSION)?;
                f.member("ok", true)?;
                f.member("op", "peers")?;
                f.member("names", names)
            }),
            Response::Error { message } => f.object(|f| {
                f.member("v", PROTOCOL_VERSION)?;
                f.member("ok", false)?;
                f.member("error", message)
            }),
        }
    }
}

/// `to`/`from`/`name` travel as plain text and land directly in a hub-side
/// lookup key (an in-memory map, and — for `litter-hub`'s on-disk mirror — a
/// file path), so both ends enforce the exact bare-token rule the filesystem
/// mailbox's own `tools::litter::mod::is_valid_name` always has: this is that
/// rule, moved here so a hub built independently of `meow` can't drift from
/// it and accept something like `"../../etc"`.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn check_version(value: RawJsonValue<'_, '_>) -> Result<(), WireError> {
    let v: i64 = value.to_member("v")?.required()?.try_into()?;
    if v != PROTOCOL_VERSION {
        return Err(WireError::UnsupportedVersion(v));
    }
    Ok(())
}

pub fn encode_message(m: &Message) -> String {
    nojson::Json(m).to_string()
}

pub fn decode_message(json: &str) -> Result<Message, WireError> {
    let raw = nojson::RawJson::parse(json)?;
    Ok(raw.value().try_into()?)
}

pub fn encode_request(r: &Request) -> String {
    nojson::Json(r).to_string()
}

pub fn decode_request(json: &str) -> Result<Request, WireError> {
    let raw = nojson::RawJson::parse(json)?;
    let value = raw.value();
    check_version(value)?;

    let op: String = value.to_member("op")?.required()?.try_into()?;
    match op.as_str() {
        "send" => {
            let from: String = value.to_member("from")?.required()?.try_into()?;
            let to: String = value.to_member("to")?.required()?.try_into()?;
            let body: String = value.to_member("body")?.required()?.try_into()?;
            let round: Option<i64> = value.to_member("round")?.try_into()?;
            Ok(Request::Send { from, to, body, round: round.unwrap_or(0) })
        }
        "inbox" => {
            let name: String = value.to_member("name")?.required()?.try_into()?;
            Ok(Request::Inbox { name })
        }
        "peers" => Ok(Request::Peers),
        _ => Err(value.invalid("unknown 'op'").into()),
    }
}

pub fn encode_response(r: &Response) -> String {
    nojson::Json(r).to_string()
}

pub fn decode_response(json: &str) -> Result<Response, WireError> {
    let raw = nojson::RawJson::parse(json)?;
    let value = raw.value();
    check_version(value)?;

    let ok: bool = value.to_member("ok")?.required()?.try_into()?;
    if !ok {
        let message: String = value.to_member("error")?.required()?.try_into()?;
        return Ok(Response::Error { message });
    }

    let op: String = value.to_member("op")?.required()?.try_into()?;
    match op.as_str() {
        "sent" => {
            let bytes: usize = value.to_member("bytes")?.required()?.try_into()?;
            Ok(Response::Sent { bytes })
        }
        "inbox" => {
            let messages: Vec<Message> = value.to_member("messages")?.required()?.try_into()?;
            Ok(Response::Inbox { messages })
        }
        "peers" => {
            let names: Vec<String> = value.to_member("names")?.required()?.try_into()?;
            Ok(Response::Peers { names })
        }
        _ => Err(value.invalid("unknown response 'op'").into()),
    }
}

// ============================================================================
// Framing
// ============================================================================

pub fn encode_len_header(len: u32) -> [u8; 4] {
    len.to_be_bytes()
}

pub fn decode_len_header(bytes: [u8; 4]) -> u32 {
    u32::from_be_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_valid_name_accepts_plain_tokens_and_rejects_traversal() {
        let cases: &[(&str, bool)] = &[
            ("sherlock", true),
            ("agent_2", true),
            ("", false),
            ("../etc", false),
            ("a/b", false),
            ("has space", false),
        ];
        for (name, want) in cases {
            assert_eq!(is_valid_name(name), *want, "is_valid_name({:?})", name);
        }
    }

    #[test]
    fn message_round_trips_including_quotes_and_newlines() {
        let m = Message {
            from: String::from("sherlock"),
            round: 3,
            body: String::from("line1\nline2 \"quoted\""),
            ts: 1234567890,
        };
        let json = encode_message(&m);
        let back = decode_message(&json).expect("decode");
        assert_eq!(back, m);
    }

    #[test]
    fn decode_message_rejects_missing_body() {
        assert!(decode_message("{\"from\":\"a\"}").is_err());
    }

    #[test]
    fn decode_message_defaults_missing_round_and_ts_to_zero() {
        let m = decode_message("{\"from\":\"a\",\"body\":\"hi\"}").expect("decode");
        assert_eq!(m.round, 0);
        assert_eq!(m.ts, 0);
    }

    #[test]
    fn send_request_round_trips() {
        let r = Request::Send {
            from: String::from("sherlock"),
            to: String::from("hercules"),
            body: String::from("what have you found?"),
            round: 2,
        };
        let json = encode_request(&r);
        assert_eq!(decode_request(&json).expect("decode"), r);
    }

    #[test]
    fn inbox_and_peers_requests_round_trip() {
        let inbox = Request::Inbox { name: String::from("hercules") };
        assert_eq!(decode_request(&encode_request(&inbox)).expect("decode"), inbox);

        let peers = Request::Peers;
        assert_eq!(decode_request(&encode_request(&peers)).expect("decode"), peers);
    }

    #[test]
    fn decode_request_rejects_unknown_op() {
        let json = "{\"v\":1,\"op\":\"launch_missiles\"}";
        assert!(matches!(decode_request(json), Err(WireError::Parse(_))));
    }

    #[test]
    fn decode_request_rejects_wrong_version() {
        let json = "{\"v\":99,\"op\":\"peers\"}";
        assert!(matches!(decode_request(json), Err(WireError::UnsupportedVersion(99))));
    }

    #[test]
    fn decode_request_rejects_missing_version() {
        let json = "{\"op\":\"peers\"}";
        assert!(matches!(decode_request(json), Err(WireError::Parse(_))));
    }

    #[test]
    fn sent_response_round_trips() {
        let r = Response::Sent { bytes: 42 };
        assert_eq!(decode_response(&encode_response(&r)).expect("decode"), r);
    }

    #[test]
    fn error_response_round_trips() {
        let r = Response::Error { message: String::from("no such agent \"ghost\"") };
        assert_eq!(decode_response(&encode_response(&r)).expect("decode"), r);
    }

    #[test]
    fn peers_response_round_trips_including_empty_list() {
        let r = Response::Peers { names: alloc::vec![String::from("sherlock"), String::from("hercules")] };
        assert_eq!(decode_response(&encode_response(&r)).expect("decode"), r);

        let empty = Response::Peers { names: Vec::new() };
        assert_eq!(decode_response(&encode_response(&empty)).expect("decode"), empty);
    }

    #[test]
    fn inbox_response_round_trips_several_messages_in_order() {
        let r = Response::Inbox {
            messages: alloc::vec![
                Message { from: String::from("a"), round: 0, body: String::from("first"), ts: 1 },
                Message { from: String::from("b"), round: 1, body: String::from("second, with a \"quote\""), ts: 2 },
            ],
        };
        let json = encode_response(&r);
        // Genuinely nested JSON now (an array of objects), not an array of
        // pre-escaped strings each holding one object's text.
        assert!(json.contains(r#""messages":[{"#));
        assert_eq!(decode_response(&json).expect("decode"), r);
    }

    #[test]
    fn decode_response_rejects_wrong_version() {
        let json = "{\"v\":2,\"ok\":true,\"op\":\"peers\",\"names\":[]}";
        assert!(matches!(decode_response(json), Err(WireError::UnsupportedVersion(2))));
    }

    #[test]
    fn decode_rejects_malformed_json() {
        assert!(decode_request("not json at all").is_err());
        assert!(decode_response("{\"v\":1,\"ok\":true").is_err());
    }

    #[test]
    fn len_header_round_trips() {
        for len in [0u32, 1, 4096, MAX_FRAME_LEN, u32::MAX] {
            assert_eq!(decode_len_header(encode_len_header(len)), len);
        }
    }
}
