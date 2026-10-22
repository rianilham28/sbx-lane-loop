//! Writing frames: one line, one write, one writer at a time.
//!
//! The bash loop's 512-byte bound existed because its ticker and its requests
//! shared one pipe and a larger write could be split by another writer. One
//! mutex and one `write_all` per frame make that impossible here, but the
//! frames stay inside the same bound: the client's buffers and its tests were
//! built to it, and two implementations that are indistinguishable on the
//! wire are what the conformance suite proves.

use std::io::{self, Write};
use std::sync::Mutex;

use crate::base64;

pub struct Frames {
    out: Mutex<io::Stdout>,
}

/// `E <id|-> <b64 reason>`: the one frame built outside `Frames`, so the
/// bound tests can measure it too.
pub fn refusal(id: Option<&str>, reason: &str) -> String {
    format!(
        "E {} {}",
        id.unwrap_or("-"),
        base64::encode(reason.as_bytes())
    )
}

impl Frames {
    pub fn new() -> Self {
        Self {
            out: Mutex::new(io::stdout()),
        }
    }

    fn send(&self, line: &str) {
        let mut out = self
            .out
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut frame = String::with_capacity(line.len() + 1);
        frame.push_str(line);
        frame.push('\n');
        // stdout is line-buffered: this reaches the pipe as one write.
        let _ = out.write_all(frame.as_bytes());
    }

    pub fn ack(&self, id: &str) {
        self.send(&format!("A {id}"));
    }

    pub fn output(&self, id: &str, stderr: bool, piece: &str) {
        self.send(&format!("O {id} {} {piece}", u8::from(stderr) + 1));
    }

    /// The flags field is always there, empty or `t`, so the frame keeps the
    /// trailing space the bash loop writes.
    pub fn done(&self, id: &str, code: i32, flags: &str) {
        self.send(&format!("D {id} {code} {flags}"));
    }

    pub fn refused(&self, id: Option<&str>, reason: &str) {
        self.send(&refusal(id, reason));
    }

    pub fn tick(&self) {
        self.send("T");
    }
}
