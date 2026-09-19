//! `litter-hub`: the TCP relay described in
//! `userspace/meow/docs/LITTER_EXPERIMENT.md` ("Where this is headed" ->
//! "Transport: filesystem mailbox -> TCP hub"). Every `meow` agent connects
//! to it *outbound only* (`libakuma::net::TcpStream` has no listener side at
//! all — see `litter-wire`'s module doc), so this is the one long-running
//! process in the picture and the one place that can hold state across
//! `meow`'s otherwise one-shot `-c` invocations.
//!
//! `HubState` is the whole server: an in-memory roster (fixed at startup —
//! same "an external launcher decides who's in the litter" contract the
//! filesystem `roster.json` always had, `tools::litter::tool_list_peers`'s
//! doc comment) and in-memory, non-destructive inboxes (`ReadInbox` leaves
//! messages in place, matching the filesystem mailbox's own behavior — see
//! that module's doc comment for why). `serve_one` handles exactly one
//! framed request/response over an already-accepted connection, which is
//! what `cargo test` drives directly against an in-process `HubState` with no
//! actual socket involved, and what `run_forever` wraps with the real
//! `TcpListener` accept loop.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use litter_wire::{
    decode_len_header, decode_request, encode_len_header, encode_response, is_valid_name,
    Message, Request, Response, MAX_FRAME_LEN,
};

/// The whole server's state. `Mutex` rather than a lock-free structure: a
/// litter is at most a handful of agents taking turns, not a high-throughput
/// service, so contention is a non-issue and a plain lock keeps `serve_one`
/// straightforward.
pub struct HubState {
    roster: Vec<String>,
    inboxes: Mutex<HashMap<String, Vec<Message>>>,
}

impl HubState {
    /// `roster` is the fixed, external answer to `ListPeers` — the hub never
    /// grows it from a `Send`/`Inbox` call it happens to see, on the same
    /// principle the filesystem mailbox's `roster.json` was never written by
    /// meow itself: who's in the litter is a launch-time decision, not
    /// something inferred from traffic.
    pub fn new(roster: Vec<String>) -> Self {
        Self { roster, inboxes: Mutex::new(HashMap::new()) }
    }

    fn now_us() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or(0)
    }

    /// Handle one already-decoded request and produce the response to send
    /// back. Pure state transition, no I/O — this is the half `cargo test`
    /// exercises directly without a socket.
    pub fn handle(&self, req: Request) -> Response {
        match req {
            Request::Send { from, to, body, round } => {
                if !is_valid_name(&from) {
                    return Response::Error { message: String::from("'from' must be a plain agent name") };
                }
                if !is_valid_name(&to) {
                    return Response::Error { message: String::from("'to' must be a plain agent name") };
                }
                if body.is_empty() {
                    return Response::Error { message: String::from("send requires a non-empty body") };
                }
                let msg = Message { from, round, body, ts: Self::now_us() };
                let bytes = msg.body.len();
                self.inboxes.lock().unwrap().entry(to).or_default().push(msg);
                Response::Sent { bytes }
            }
            Request::Inbox { name } => {
                if !is_valid_name(&name) {
                    return Response::Error { message: String::from("'name' must be a plain agent name") };
                }
                let messages = self.inboxes.lock().unwrap().get(&name).cloned().unwrap_or_default();
                Response::Inbox { messages }
            }
            Request::Peers => Response::Peers { names: self.roster.clone() },
        }
    }

    /// Serve exactly one framed request/response pair over `stream`, then
    /// return — matching the one-shot shape every `meow -c` invocation
    /// already has on the client side (see `litter-wire`'s module doc on
    /// framing). A malformed frame or a version the hub doesn't speak still
    /// gets a well-formed `Response::Error` back rather than a dropped
    /// connection, so a misbehaving client can tell the two apart from a
    /// network failure.
    pub fn serve_one(&self, stream: &mut TcpStream) -> std::io::Result<()> {
        let mut header = [0u8; 4];
        stream.read_exact(&mut header)?;
        let len = decode_len_header(header);
        if len > MAX_FRAME_LEN {
            return write_frame(stream, &encode_response(&Response::Error {
                message: String::from("request frame too large"),
            }));
        }

        let mut body = vec![0u8; len as usize];
        stream.read_exact(&mut body)?;

        let response = match std::str::from_utf8(&body) {
            Ok(text) => match decode_request(text) {
                Ok(req) => self.handle(req),
                Err(litter_wire::WireError::UnsupportedVersion(v)) => Response::Error {
                    message: format!(
                        "unsupported protocol version {} (hub speaks {})",
                        v,
                        litter_wire::PROTOCOL_VERSION
                    ),
                },
                Err(litter_wire::WireError::Parse(reason)) => {
                    Response::Error { message: format!("malformed request: {}", reason) }
                }
            },
            Err(_) => Response::Error { message: String::from("request is not valid UTF-8") },
        };

        write_frame(stream, &encode_response(&response))
    }

    /// Accept connections on `listener` forever, one thread per connection.
    /// Not exercised by `cargo test` (that drives `serve_one` directly
    /// against an in-process `TcpStream` pair instead) — this is the thin
    /// glue `main.rs` calls.
    pub fn run_forever(self: std::sync::Arc<Self>, listener: std::net::TcpListener) {
        for incoming in listener.incoming() {
            let mut stream = match incoming {
                Ok(s) => s,
                Err(_) => continue,
            };
            let state = std::sync::Arc::clone(&self);
            std::thread::spawn(move || {
                let _ = state.serve_one(&mut stream);
            });
        }
    }
}

fn write_frame(stream: &mut TcpStream, payload: &str) -> std::io::Result<()> {
    stream.write_all(&encode_len_header(payload.len() as u32))?;
    stream.write_all(payload.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use litter_wire::{decode_response, encode_request};
    use std::net::TcpListener;

    /// Drives `serve_one` over a real loopback socket (not just `handle`
    /// in-process) so the framing and UTF-8/decode-error paths are covered
    /// too, not only the state transition. `thread::scope` lets the server
    /// side borrow `state` directly instead of needing an `Arc` just for a
    /// test.
    fn round_trip(state: &HubState, req: &Request) -> Response {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let request_json = encode_request(req);

        std::thread::scope(|scope| {
            let server = scope.spawn(|| {
                let (mut stream, _) = listener.accept().expect("accept");
                state.serve_one(&mut stream).expect("serve_one");
            });

            let mut client = TcpStream::connect(addr).expect("connect");
            write_frame(&mut client, &request_json).expect("write request");
            let text = read_frame(&mut client).expect("read response");

            server.join().expect("server thread panicked");
            decode_response(&text).expect("decode response")
        })
    }

    fn read_frame(stream: &mut TcpStream) -> std::io::Result<String> {
        let mut header = [0u8; 4];
        stream.read_exact(&mut header)?;
        let len = decode_len_header(header);
        let mut body = vec![0u8; len as usize];
        stream.read_exact(&mut body)?;
        Ok(String::from_utf8(body).expect("utf8"))
    }

    #[test]
    fn peers_returns_the_fixed_roster() {
        let state = HubState::new(vec![String::from("sherlock"), String::from("hercules")]);
        let resp = round_trip(&state, &Request::Peers);
        assert_eq!(
            resp,
            Response::Peers { names: vec![String::from("sherlock"), String::from("hercules")] }
        );
    }

    #[test]
    fn send_then_inbox_round_trips_the_message() {
        let state = HubState::new(vec![String::from("sherlock"), String::from("hercules")]);

        let sent = round_trip(&state, &Request::Send {
            from: String::from("sherlock"),
            to: String::from("hercules"),
            body: String::from("the game is afoot"),
            round: 1,
        });
        assert_eq!(sent, Response::Sent { bytes: "the game is afoot".len() });

        let inbox = round_trip(&state, &Request::Inbox { name: String::from("hercules") });
        match inbox {
            Response::Inbox { messages } => {
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].from, "sherlock");
                assert_eq!(messages[0].round, 1);
                assert_eq!(messages[0].body, "the game is afoot");
                assert!(messages[0].ts > 0, "hub should assign a nonzero timestamp");
            }
            other => panic!("expected Inbox, got {:?}", other),
        }
    }

    #[test]
    fn inbox_read_does_not_consume_messages() {
        let state = HubState::new(vec![String::from("a"), String::from("b")]);
        let _ = round_trip(&state, &Request::Send {
            from: String::from("a"),
            to: String::from("b"),
            body: String::from("hi"),
            round: 0,
        });

        let first = round_trip(&state, &Request::Inbox { name: String::from("b") });
        let second = round_trip(&state, &Request::Inbox { name: String::from("b") });
        assert_eq!(first, second, "reading twice should see the same message both times");
    }

    #[test]
    fn inbox_for_an_agent_with_no_messages_is_empty_not_an_error() {
        let state = HubState::new(vec![String::from("a")]);
        let resp = round_trip(&state, &Request::Inbox { name: String::from("nobody") });
        assert_eq!(resp, Response::Inbox { messages: Vec::new() });
    }

    #[test]
    fn send_rejects_path_traversal_in_to() {
        let state = HubState::new(vec![String::from("a")]);
        let resp = round_trip(&state, &Request::Send {
            from: String::from("a"),
            to: String::from("../../etc"),
            body: String::from("hi"),
            round: 0,
        });
        assert!(matches!(resp, Response::Error { .. }), "expected an Error, got {:?}", resp);
    }

    #[test]
    fn send_rejects_empty_body() {
        let state = HubState::new(vec![String::from("a"), String::from("b")]);
        let resp = round_trip(&state, &Request::Send {
            from: String::from("a"),
            to: String::from("b"),
            body: String::new(),
            round: 0,
        });
        assert!(matches!(resp, Response::Error { .. }));
    }

    /// A hostile/corrupt length prefix must get a clean `Response::Error`
    /// without `serve_one` ever trying to allocate or read that many bytes —
    /// this sends only the 4-byte header and nothing else, so a body read
    /// would hang or fail if the oversized-length short-circuit weren't
    /// checked before it.
    #[test]
    fn oversized_frame_length_is_rejected_without_reading_a_body() {
        let state = HubState::new(vec![String::from("a")]);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let (mut stream, _) = listener.accept().expect("accept");
                state.serve_one(&mut stream).expect("serve_one");
            });

            let mut client = TcpStream::connect(addr).expect("connect");
            client.write_all(&encode_len_header(MAX_FRAME_LEN + 1)).expect("write header");
            let text = read_frame(&mut client).expect("read response");
            let resp = decode_response(&text).expect("decode response");
            assert!(matches!(resp, Response::Error { .. }));
        });
    }

    /// Malformed JSON in an otherwise well-framed request must produce a
    /// `Response::Error`, not a dropped connection or a panic — a hub has to
    /// stay up across a whole litter's worth of agents even if one of them
    /// sends garbage.
    #[test]
    fn malformed_request_json_gets_a_clean_error_response() {
        let state = HubState::new(vec![String::from("a")]);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let (mut stream, _) = listener.accept().expect("accept");
                state.serve_one(&mut stream).expect("serve_one");
            });

            let mut client = TcpStream::connect(addr).expect("connect");
            write_frame(&mut client, "not json at all").expect("write request");
            let text = read_frame(&mut client).expect("read response");
            let resp = decode_response(&text).expect("decode response");
            assert!(matches!(resp, Response::Error { .. }));
        });
    }

    /// A client speaking a protocol version the hub doesn't must be told so
    /// explicitly (see `litter-wire`'s module doc on versioning), not have
    /// its request silently misparsed.
    #[test]
    fn unsupported_version_gets_a_named_error() {
        let state = HubState::new(vec![String::from("a")]);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let (mut stream, _) = listener.accept().expect("accept");
                state.serve_one(&mut stream).expect("serve_one");
            });

            let mut client = TcpStream::connect(addr).expect("connect");
            write_frame(&mut client, "{\"v\":99,\"op\":\"peers\"}").expect("write request");
            let text = read_frame(&mut client).expect("read response");
            let resp = decode_response(&text).expect("decode response");
            match resp {
                Response::Error { message } => assert!(
                    message.contains("99") && message.contains("version"),
                    "error should name the offending version: {:?}",
                    message
                ),
                other => panic!("expected Error, got {:?}", other),
            }
        });
    }
}
