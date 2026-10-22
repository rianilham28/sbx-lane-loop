//! Writing frames: one line, one write, one writer at a time.
//!
//! The bash loop's 512-byte bound existed because its ticker and its requests
//! shared one pipe and a larger write could be split by another writer. One
//! mutex and one `write_all` per frame make that impossible here, but the
//! frames stay inside the same bound: the client's buffers and its tests were
//! built to it, and two implementations that are indistinguishable on the
//! wire are what the conformance suite proves.

use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};

use crate::base64;

pub struct Frames {
    out: Mutex<io::Stdout>,
}

/// What to do when a frame cannot be written: nobody is reading, and a loop
/// with no reader has nobody left to answer. Set once, at start.
static CLIENT_GONE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

pub fn on_client_gone(action: impl Fn() + Send + Sync + 'static) {
    let _ = CLIENT_GONE.set(Box::new(action));
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
        if out.write_all(frame.as_bytes()).is_err() {
            // The reader is gone. An ignored SIGPIPE means the loop would
            // otherwise run forever with nobody to answer; end it the way the
            // bash loop ends under SIGPIPE — after killing its requests'
            // groups, which the signal would have skipped.
            if let Some(action) = CLIENT_GONE.get() {
                action();
            }
            std::process::exit(1);
        }
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
