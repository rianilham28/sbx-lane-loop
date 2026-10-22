//! The wire: how a line from the client becomes a request, and the bounds
//! every frame keeps.
//!
//! Parsing follows the bash loop step for step, including what it does with a
//! line that is only almost a request: the refusals here are its refusals,
//! word for word, because a client already reads them.

use crate::base64;
use crate::sha256;

/// The most characters an id may have — the bound that keeps a frame inside
/// one pipe buffer: `O ` + id + ` ` + stream + ` ` + piece + newline is 498
/// bytes at 32 characters and a 460-character piece.
pub const MAX_ID: usize = 32;
pub const MAX_KEY: usize = 32;

/// The most of one stream a buffered request answers with; the rest is
/// captured but not sent, and the `D` frame says `t`.
pub const MAX_OUTPUT: usize = 256 * 1024;

/// Base64 characters per output piece: 345 bytes, a multiple of four so each
/// piece decodes alone.
pub const PIECE: usize = 460;

/// The wire, in one canonical string. Its SHA-256's first four bytes are the
/// protocol id a label carries — `LANE_LOOP_PROTOCOL`'s sibling on the bash
/// side, which hashes that script's text the same way. Editing any frame or
/// bound means editing this string: that is what lets a client tell a loop of
/// its own wire from one a different build left behind.
pub const WIRE: &str = "\
lane-loop/1
request: R <id> <b64> | S <id> <key> <b64> | K <id>
answer: A <id> | O <id> <1|2> <460-char b64 piece> | D <id> <exit> <flags> | E <id|-> <b64 reason> | T
bounds: id and key <= 32 of [A-Za-z0-9]; output <= 262144 bytes per stream; flags = t when cut; ended exit 143; tick 5s
";

pub fn protocol_id() -> String {
    let digest = sha256::digest(WIRE.as_bytes());
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `R`: capture both streams and answer when the command finishes.
    Buffered,
    /// `S`: forward stdout line by line as it is produced.
    Streamed,
}

#[derive(Debug)]
pub struct Request<'a> {
    pub kind: Kind,
    pub id: &'a str,
    /// The slot a streamed request holds. `None` for `R`, and for an `S`
    /// whose key is `-` — the loop then holds it under no slot at all.
    pub key: Option<&'a str>,
    pub payload: &'a [u8],
}

#[derive(Debug)]
pub enum Incoming<'a> {
    Request(Request<'a>),
    Cancel(&'a str),
    /// Nothing the loop can act on: answered with `E` and the loop carries on.
    Refused(Option<&'a str>, &'static str),
}

fn split_space(bytes: &[u8]) -> (&[u8], Option<&[u8]>) {
    match bytes.iter().position(|byte| *byte == b' ') {
        Some(at) => (&bytes[..at], Some(&bytes[at + 1..])),
        None => (bytes, None),
    }
}

fn is_token(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(|byte| byte.is_ascii_alphanumeric())
}

fn token(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("a token is alphanumeric ASCII")
}

pub fn parse(line: &[u8]) -> Incoming<'_> {
    let (tag, rest) = if let Some(rest) = line.strip_prefix(&b"R "[..]) {
        (b'R', rest)
    } else if let Some(rest) = line.strip_prefix(&b"S "[..]) {
        (b'S', rest)
    } else if let Some(rest) = line.strip_prefix(&b"K "[..]) {
        (b'K', rest)
    } else {
        return Incoming::Refused(None, "unreadable frame");
    };

    let (id, after_id) = split_space(rest);
    if !is_token(id) {
        return Incoming::Refused(None, "unreadable request id");
    }
    if id.len() > MAX_ID {
        return Incoming::Refused(None, "request id is longer than 32 characters");
    }
    let id = token(id);
    if tag == b'K' {
        return Incoming::Cancel(id);
    }

    // `S <id> <key> <payload>`; the bash loop reads the key and the payload
    // with `${rest#* }`, which leaves the string whole when there is no space
    // — so a line that stops early puts the rest in both places, refused as a
    // payload that is not base64.
    let (key, payload) = if tag == b'S' {
        match after_id {
            Some(rest) => {
                let (key, payload) = split_space(rest);
                (Some(key), payload.unwrap_or(rest))
            }
            None => (Some(rest), rest),
        }
    } else {
        (None, after_id.unwrap_or(rest))
    };

    let key = match key {
        None => None,
        Some(key) if key == b"-" => None,
        Some(key) => {
            if !is_token(key) {
                return Incoming::Refused(Some(id), "unreadable request key");
            }
            if key.len() > MAX_KEY {
                return Incoming::Refused(Some(id), "request key is longer than 32 characters");
            }
            Some(token(key))
        }
    };

    let kind = if tag == b'S' {
        Kind::Streamed
    } else {
        Kind::Buffered
    };
    Incoming::Request(Request {
        kind,
        id,
        key,
        payload,
    })
}

/// The command a payload carries, or `None` when it is not base64. The shell
/// strips NULs out of a command substitution and trims the trailing newlines;
/// both keep the command exactly as the bash loop ran it.
pub fn decode_payload(payload: &[u8]) -> Option<Vec<u8>> {
    let plausible = payload
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='));
    if !plausible {
        return None;
    }
    let mut command = base64::decode(payload)?;
    command.retain(|byte| *byte != 0);
    while command.last() == Some(&b'\n') {
        command.pop();
    }
    Some(command)
}

/// The pieces one stream of bytes becomes: base64, cut every `PIECE`
/// characters. Empty output is no frame at all, as in the bash loop.
pub fn pieces(bytes: &[u8]) -> Vec<String> {
    if bytes.is_empty() {
        return Vec::new();
    }
    base64::encode(bytes)
        .as_bytes()
        .chunks(PIECE)
        .map(|piece| String::from_utf8(piece.to_vec()).expect("base64 is ASCII"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        decode_payload, parse, pieces, protocol_id, Incoming, Kind, MAX_ID, MAX_KEY, PIECE,
    };

    const COMMAND: &str = "printf 'hi\\n'";

    fn encoded(command: &str) -> String {
        crate::base64::encode(command.as_bytes())
    }

    #[test]
    fn a_buffered_request_is_its_id_and_payload() {
        let line = format!("R r1 {}", encoded(COMMAND));
        let Incoming::Request(request) = parse(line.as_bytes()) else {
            panic!("not a request");
        };
        assert_eq!(request.kind, Kind::Buffered);
        assert_eq!(request.id, "r1");
        assert_eq!(request.key, None);
        assert_eq!(decode_payload(request.payload).unwrap(), COMMAND.as_bytes());
    }

    #[test]
    fn a_streamed_request_holds_its_key() {
        let line = format!("S s1 k1 {}", encoded(COMMAND));
        let Incoming::Request(request) = parse(line.as_bytes()) else {
            panic!("not a request");
        };
        assert_eq!(request.kind, Kind::Streamed);
        assert_eq!(request.key, Some("k1"));
        assert_eq!(decode_payload(request.payload).unwrap(), COMMAND.as_bytes());
    }

    #[test]
    fn a_dash_key_holds_no_slot() {
        let line = format!("S s1 - {}", encoded(COMMAND));
        let Incoming::Request(request) = parse(line.as_bytes()) else {
            panic!("not a request");
        };
        assert_eq!(request.key, None);
        assert_eq!(decode_payload(request.payload).unwrap(), COMMAND.as_bytes());
    }

    #[test]
    fn a_cancel_names_only_an_id() {
        let Incoming::Cancel(id) = parse(b"K c1") else {
            panic!("not a cancel");
        };
        assert_eq!(id, "c1");
    }

    #[test]
    fn anything_but_the_three_frames_is_refused() {
        for line in ["", "nonsense", "R", "S", "K", "r1", "X r1", " R r1"] {
            match parse(line.as_bytes()) {
                Incoming::Refused(None, reason) => assert_eq!(reason, "unreadable frame"),
                other => panic!("{line:?} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn an_id_must_be_alphanumeric_and_inside_the_bound() {
        let payload = encoded(COMMAND);
        for line in [
            format!("R - {payload}"),
            format!("R r-1 {payload}"),
            format!("R r/1 {payload}"),
            format!("R {} {payload}", "x".repeat(MAX_ID + 1)),
        ] {
            match parse(line.as_bytes()) {
                Incoming::Refused(None, reason) => {
                    assert!(
                        reason.starts_with("unreadable request id")
                            || reason.starts_with("request id is longer"),
                        "{reason}"
                    );
                }
                other => panic!("{line:?} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn a_key_is_checked_before_the_payload() {
        let payload = encoded(COMMAND);
        for (line, expected) in [
            (format!("S s1 k-1 {payload}"), "unreadable request key"),
            (
                format!("S s1 {} {payload}", "k".repeat(MAX_KEY + 1)),
                "request key is longer than 32 characters",
            ),
        ] {
            match parse(line.as_bytes()) {
                Incoming::Refused(Some(id), reason) => {
                    assert_eq!(id, "s1");
                    assert_eq!(reason, expected);
                }
                other => panic!("{line:?} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn a_payload_that_is_not_base64_is_refused() {
        for payload in ["not base64!!", "s1", "AAA", "AA=A", "A"] {
            let line = format!("R r3 {payload}");
            let Incoming::Request(request) = parse(line.as_bytes()) else {
                panic!("{line:?} was refused too early");
            };
            assert_eq!(decode_payload(request.payload), None, "{payload:?}");
        }
    }

    #[test]
    fn a_command_loses_its_nuls_and_trailing_newlines() {
        let mut command = b"echo hi\0".to_vec();
        command.extend_from_slice(b"\n\n");
        let line = format!("R r1 {}", crate::base64::encode(&command));
        let Incoming::Request(request) = parse(line.as_bytes()) else {
            panic!("not a request");
        };
        assert_eq!(decode_payload(request.payload).unwrap(), b"echo hi");
    }

    #[test]
    fn pieces_fold_on_four_character_boundaries() {
        let bytes = vec![b'y'; 8192];
        let folded = pieces(&bytes);
        assert!(folded.len() > 1);
        assert!(folded
            .iter()
            .all(|piece| piece.len() <= PIECE && piece.len() % 4 == 0));
        let joined: String = folded.concat();
        assert_eq!(crate::base64::decode(joined.as_bytes()).unwrap(), bytes);
        assert!(pieces(&[]).is_empty());
    }

    #[test]
    fn every_frame_fits_one_pipe_buffer() {
        // The largest frame any bound allows: a maximum id, output stream 1,
        // a full piece. 512 bytes is what the bash loop's frames keep to and
        // what the client's buffers and tests were built around.
        let piece = "A".repeat(PIECE);
        let frame = format!("O {} 1 {piece}", "i".repeat(MAX_ID));
        assert_eq!(frame.len() + 1, 498);
        assert!(frame.len() < 512);
        let done = format!("D {} 143 ", "i".repeat(MAX_ID));
        assert!(done.len() < 512);
        let refused = crate::frames::refusal(
            Some(&"i".repeat(MAX_ID)),
            "request key is longer than 32 characters",
        );
        assert!(refused.len() < 512);
    }

    #[test]
    fn the_protocol_id_is_eight_hex_characters() {
        let id = protocol_id();
        assert_eq!(id.len(), 8);
        assert!(id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        // Derived, not typed in: the same wiring always answers the same id.
        assert_eq!(id, protocol_id());
    }
}
