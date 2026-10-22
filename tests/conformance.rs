//! The contract, as a suite: every scenario worktrunk-sbx's bash loop passes,
//! run against this crate's binary and against the bash loop itself.
//!
//! Both implementations are started the way the sandbox starts them — no
//! login environment, a temporary `HOME` — and both are read the way the
//! client reads them: whole lines, base64 pieces, `D` last. A scenario that
//! holds for one must hold for the other; where the two are allowed to differ
//! the test says so.
//!
//! `LANE_LOOP_BIN` points at a binary (a release build, or the image's when
//! this suite runs inside it); otherwise the test binary's own build is used.
//! The bash half is not vendored: `SBX_LANE_LOOP_BASH` names worktrunk-sbx's
//! `src/sandbox/lane-loop.ts`, whose `LANE_LOOP` is extracted and run. With
//! that variable unset the suite runs the binary half only — which is what CI
//! does, because that repository is private.

use std::env;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// The largest frame POSIX guarantees a non-interleaved pipe write for.
const PIPE_BUF: usize = 512;

// --- base64, for building requests and reading answers ---------------------

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        out.push(ALPHABET[(a >> 2) as usize] as char);
        out.push(ALPHABET[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(c & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn unb64(text: &str) -> Vec<u8> {
    let digit = |byte: u8| match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' => 62,
        b'/' => 63,
        _ => 0,
    };
    let mut out = Vec::new();
    for group in text.as_bytes().chunks(4) {
        let mut values = [0u8; 4];
        let mut padding = 0;
        for (position, &byte) in group.iter().enumerate() {
            if byte == b'=' {
                padding += 1;
            } else {
                values[position] = digit(byte);
            }
        }
        out.push((values[0] << 2) | (values[1] >> 4));
        if padding < 2 && group.len() > 2 {
            out.push((values[1] << 4) | (values[2] >> 2));
        }
        if padding < 1 && group.len() > 3 {
            out.push((values[2] << 6) | values[3]);
        }
    }
    out
}

// --- the two implementations ----------------------------------------------

struct Implementation {
    name: &'static str,
    program: String,
    args: Vec<String>,
}

fn implementations() -> Vec<Implementation> {
    let program = env::var("LANE_LOOP_BIN")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_sbx-lane-loop").to_string());
    let mut all = vec![Implementation {
        name: "sbx-lane-loop",
        program,
        args: vec!["0.05".to_string()],
    }];
    match bash_loop() {
        Some(script) => all.push(Implementation {
            name: "bash loop",
            program: "bash".to_string(),
            args: vec![
                "-c".to_string(),
                script,
                "lane-loop".to_string(),
                "0.05".to_string(),
            ],
        }),
        None => println!(
            "SBX_LANE_LOOP_BASH is not set: running the binary half only, without the bash loop"
        ),
    }
    all
}

fn bash_loop() -> Option<String> {
    let path = env::var("SBX_LANE_LOOP_BASH").ok()?;
    extract(&fs::read_to_string(path).ok()?)
}

/// `LANE_LOOP`'s template literal, unescaped: the script the bash loop runs.
fn extract(source: &str) -> Option<String> {
    const MARKER: &str = "export const LANE_LOOP = `";
    let start = source.find(MARKER)? + MARKER.len();
    let bytes = source.as_bytes();
    let mut script = String::new();
    let mut at = start;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => {
                let escaped = *bytes.get(at + 1)?;
                script.push(match escaped {
                    b'n' => '\n',
                    b't' => '\t',
                    other => other as char,
                });
                at += 2;
            }
            b'`' => return Some(script),
            byte => {
                script.push(byte as char);
                at += 1;
            }
        }
    }
    None
}

// --- one loop process ------------------------------------------------------

static TEMP: AtomicUsize = AtomicUsize::new(0);

fn temp_home() -> PathBuf {
    let home = env::temp_dir().join(format!(
        "sbx-lane-loop-{}-{}",
        std::process::id(),
        TEMP.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&home).expect("a temp HOME");
    home
}

struct Loop {
    implementation: &'static str,
    child: Child,
    stdin: Option<ChildStdin>,
    frames: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<String>>,
    home: PathBuf,
    exited: bool,
}

impl Loop {
    fn start(implementation: &Implementation) -> Self {
        let home = temp_home();
        let mut child = Command::new(&implementation.program)
            .args(&implementation.args)
            .env_clear()
            .env("PATH", env::var("PATH").unwrap_or_default())
            .env("HOME", &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("starting {}: {error}", implementation.name));

        let stdin = child.stdin.take();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(String::new()));

        let reading = Arc::clone(&frames);
        let mut stdout = child.stdout.take().expect("stdout is piped");
        thread::spawn(move || {
            let mut partial = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = match stdout.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => read,
                };
                partial.extend_from_slice(&buffer[..read]);
                while let Some(end) = partial.iter().position(|byte| *byte == b'\n') {
                    let line: Vec<u8> = partial.drain(..=end).collect();
                    let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
                    reading.lock().unwrap().push(text);
                }
            }
        });

        let complaints = Arc::clone(&stderr);
        let mut errors = child.stderr.take().expect("stderr is piped");
        thread::spawn(move || {
            let mut text = String::new();
            let _ = errors.read_to_string(&mut text);
            complaints.lock().unwrap().push_str(&text);
        });

        Self {
            implementation: implementation.name,
            child,
            stdin,
            frames,
            stderr,
            home,
            exited: false,
        }
    }

    fn frames(&self) -> Vec<String> {
        self.frames.lock().unwrap().clone()
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin is open");
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .expect("writing a frame");
        stdin.flush().expect("flushing a frame");
    }

    fn request(&mut self, id: &str, key: Option<&str>, command: &str) {
        let payload = b64(command.as_bytes());
        match key {
            Some(key) => self.send(&format!("S {id} {key} {payload}")),
            None => self.send(&format!("R {id} {payload}")),
        }
    }

    fn cancel(&mut self, id: &str) {
        self.send(&format!("K {id}"));
    }

    fn home(&self) -> &Path {
        &self.home
    }

    fn until(&self, what: &str, predicate: impl Fn(&[String]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if predicate(&self.frames()) {
                return;
            }
            if Instant::now() > deadline {
                panic!(
                    "{}: timed out waiting for {what}; frames: {:?}; stderr: {:?}",
                    self.implementation,
                    self.frames(),
                    self.stderr()
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn answered(&self, id: &str) -> Answer {
        let frames = self.frames();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        for frame in &frames {
            let mut parts = frame.splitn(4, ' ');
            if parts.next() != Some("O") || parts.next() != Some(id) {
                continue;
            }
            let stream = parts.next().unwrap_or("");
            let bytes = unb64(parts.next().unwrap_or(""));
            match stream {
                "1" => stdout.extend(bytes),
                "2" => stderr.extend(bytes),
                _ => {}
            }
        }
        let prefix = format!("D {id} ");
        let done = frames
            .iter()
            .find(|frame| frame.starts_with(&prefix))
            .unwrap_or_else(|| panic!("{}: no answer for {id} in {frames:?}", self.implementation));
        let rest = &done[prefix.len()..];
        let (code, flags) = rest.split_once(' ').unwrap_or((rest, ""));
        Answer {
            stdout,
            stderr,
            exit: code.parse().unwrap_or_else(|_| {
                panic!("{}: bad exit {code:?} in {done:?}", self.implementation)
            }),
            truncated: flags.contains('t'),
        }
    }

    fn wait(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("waiting on the loop") {
                self.exited = true;
                return Some(status);
            }
            if Instant::now() > deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// End of stdin, then the loop's exit status.
    fn end(&mut self) -> i32 {
        self.stdin.take();
        match self.wait(Duration::from_secs(10)) {
            Some(status) => status.code().unwrap_or(-1),
            None => {
                let _ = self.child.kill();
                panic!("{}: did not exit at end of stdin", self.implementation);
            }
        }
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        if !self.exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let _ = fs::remove_dir_all(&self.home);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Answer {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: i32,
    truncated: bool,
}

fn refusal_reason(frame: &str) -> String {
    let mut parts = frame.splitn(3, ' ');
    let _tag = parts.next();
    let _id = parts.next();
    String::from_utf8_lossy(&unb64(parts.next().unwrap_or(""))).into_owned()
}

fn oversized(frames: &[String]) -> Vec<&String> {
    frames
        .iter()
        .filter(|frame| frame.len() + 1 > PIPE_BUF)
        .collect()
}

/// Run one scenario against both implementations, naming the one that failed.
fn for_each(body: impl Fn(&Implementation)) {
    for implementation in implementations() {
        println!("--- {} ---", implementation.name);
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(&implementation)));
        if let Err(payload) = result {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|text| text.to_string()))
                .unwrap_or_else(|| "panicked".to_string());
            panic!("{}: {message}", implementation.name);
        }
    }
}

// --- the scenarios ---------------------------------------------------------

#[test]
fn a_request_is_acked_answered_and_ticks_keep_coming() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        lane.request("r1", None, "printf 'hi\\n'");
        lane.until("r1's answer", |frames| {
            frames.iter().any(|frame| frame.starts_with("D r1 "))
        });
        let frames = lane.frames();
        let order: Vec<&String> = frames
            .iter()
            .filter(|frame| *frame == "A r1" || frame.starts_with("D r1 "))
            .collect();
        assert_eq!(order.len(), 2, "{order:?}");
        assert_eq!(order[0], "A r1");
        assert!(order[1].starts_with("D r1 "));
        assert_eq!(
            lane.answered("r1"),
            Answer {
                stdout: b"hi\n".to_vec(),
                stderr: Vec::new(),
                exit: 0,
                truncated: false,
            }
        );
        lane.until("a tick", |frames| frames.iter().any(|frame| frame == "T"));
        assert_eq!(lane.end(), 0);
    });
}

#[test]
fn an_unreadable_frame_is_answered_and_the_loop_carries_on() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        lane.send("this is not a frame");
        lane.until("the refusal", |frames| {
            frames.iter().any(|frame| frame.starts_with("E - "))
        });
        let refusals: Vec<String> = lane
            .frames()
            .into_iter()
            .filter(|frame| frame.starts_with("E - "))
            .collect();
        assert_eq!(refusals.len(), 1, "{refusals:?}");
        assert_eq!(refusal_reason(&refusals[0]), "unreadable frame");

        lane.request("r2", None, "printf 'still here\\n'");
        lane.until("r2's answer", |frames| {
            frames.iter().any(|frame| frame.starts_with("D r2 "))
        });
        assert_eq!(lane.answered("r2").stdout, b"still here\n");
        assert_eq!(lane.end(), 0);
    });
}

#[test]
fn a_command_that_is_not_base64_is_refused() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        lane.send("R r3 not base64!!");
        lane.until("the refusal", |frames| {
            frames.iter().any(|frame| frame.starts_with("E r3 "))
        });
        assert_eq!(
            refusal_reason(
                &lane
                    .frames()
                    .into_iter()
                    .find(|f| f.starts_with("E r3 "))
                    .unwrap()
            ),
            "the request is not base64"
        );
        assert!(!lane.frames().iter().any(|frame| frame == "A r3"));
        assert_eq!(lane.end(), 0);
    });
}

#[test]
fn stderr_and_the_exit_status_come_back_as_their_own_pieces() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        lane.request("r4", None, "printf 'boom\\n' >&2; exit 3");
        lane.until("r4's answer", |frames| {
            frames.iter().any(|frame| frame.starts_with("D r4 "))
        });
        assert_eq!(
            lane.answered("r4"),
            Answer {
                stdout: Vec::new(),
                stderr: b"boom\n".to_vec(),
                exit: 3,
                truncated: false,
            }
        );
        assert_eq!(lane.end(), 0);
    });
}

#[test]
fn a_streamed_command_forwards_each_line_then_its_status() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        lane.request("s1", Some("k1"), "printf 'one\\ntwo\\n'");
        lane.until("s1's answer", |frames| {
            frames.iter().any(|frame| frame.starts_with("D s1 "))
        });
        assert_eq!(
            lane.answered("s1"),
            Answer {
                stdout: b"one\ntwo\n".to_vec(),
                stderr: Vec::new(),
                exit: 0,
                truncated: false,
            }
        );
        assert_eq!(lane.end(), 0);
    });
}

#[test]
fn a_streamed_commands_last_line_without_its_newline_is_dropped() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        // The bash loop reads stdout a line at a time, and a `read` without a
        // newline fails: the partial tail never reaches the wire.
        lane.request("s2", Some("k2"), "printf 'one\\ntwo'");
        lane.until("s2's answer", |frames| {
            frames.iter().any(|frame| frame.starts_with("D s2 "))
        });
        assert_eq!(lane.answered("s2").stdout, b"one\n");
        assert_eq!(lane.end(), 0);
    });
}

#[test]
fn a_final_frame_without_its_newline_is_dropped_never_run() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        // A writer that died mid-frame: the bytes decode to a command prefix,
        // so running it would act on something nobody sent whole.
        let marker = lane.home().join("marker");
        let stdin = lane.stdin.as_mut().expect("stdin is open");
        let command = format!("printf ran > {:?}", marker);
        stdin
            .write_all(format!("R z1 {}", b64(command.as_bytes())).as_bytes())
            .expect("writing a partial frame");
        stdin.flush().expect("flushing a partial frame");
        assert_eq!(lane.end(), 0);
        assert!(!lane.frames().iter().any(|frame| frame == "A z1"));
        assert!(!marker.exists(), "the dropped frame ran");
    });
}

#[test]
fn every_frame_fits_one_pipe_buffer_whatever_its_kind() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        lane.request("a1", None, "printf 'out\\n'; printf 'err\\n' >&2");
        lane.request("s1", Some("k1"), "printf 'one\\ntwo\\n'");
        lane.send("nonsense");
        lane.send(&format!("R {} {}", "x".repeat(40), b64(b"printf ran")));
        lane.request("big1", None, "head -c 8192 /dev/zero | tr '\\0' y");
        lane.until("big1's answer", |frames| {
            frames.iter().any(|frame| frame.starts_with("D big1 "))
        });
        lane.until("a tick", |frames| frames.iter().any(|frame| frame == "T"));

        let frames = lane.frames();
        assert_eq!(oversized(&frames), Vec::<&String>::new());
        // Each piece decodes on its own: base64 cut at four-character
        // boundaries.
        let payloads: Vec<String> = frames
            .iter()
            .filter_map(|frame| {
                let parts: Vec<&str> = frame.split(' ').collect();
                (parts.first() == Some(&"O")).then(|| parts.get(3).unwrap_or(&"").to_string())
            })
            .collect();
        assert!(payloads.len() > 1);
        assert!(payloads.iter().all(|payload| payload.len() % 4 == 0));

        // An id past the cap is refused, so the bound holds for every id sent.
        let refusals: Vec<String> = frames
            .iter()
            .filter(|frame| frame.starts_with("E - "))
            .cloned()
            .collect();
        assert_eq!(refusals.len(), 2, "{refusals:?}");
        assert!(refusals
            .iter()
            .any(|frame| refusal_reason(frame).contains("longer than 32")));
        assert!(!frames.iter().any(|frame| frame.starts_with("A x")));
        assert_eq!(lane.end(), 0);
    });
}

#[test]
fn a_cut_answer_says_so() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        lane.request("r5", None, "head -c 300000 /dev/zero | tr '\\0' x");
        lane.until("r5's answer", |frames| {
            frames.iter().any(|frame| frame.starts_with("D r5 "))
        });
        let answer = lane.answered("r5");
        assert!(answer.truncated);
        assert_eq!(answer.stdout.len(), 256 * 1024);
        assert_eq!(oversized(&lane.frames()), Vec::<&String>::new());
        assert_eq!(lane.end(), 0);
    });
}

#[test]
fn a_request_holding_a_key_is_replaced_whole_children_included() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        // The child outlives its parent's command unless the whole group is
        // signalled: it leaves a marker two seconds after the replacement.
        lane.request(
            "k1",
            Some("slot"),
            "sleep 300 & echo $! > \"$HOME/child\"; ( sleep 2; printf late > \"$HOME/late\" ) & echo $! > \"$HOME/late-child\"; sleep 300",
        );
        lane.until("k1's ack", |frames| {
            frames.iter().any(|frame| frame == "A k1")
        });
        let (child, late_child) = children(&lane);

        lane.request("k2", Some("slot"), "printf 'second\\n'");
        lane.until("k2's answer", |frames| {
            frames.iter().any(|frame| frame.starts_with("D k2 "))
        });
        let endings = lane
            .frames()
            .into_iter()
            .filter(|frame| frame == "D k1 143 ")
            .count();
        assert_eq!(endings, 1);
        assert_eq!(lane.answered("k2").stdout, b"second\n");

        gone(child, "a child of the replaced request survived");
        gone(
            late_child,
            "a forked subshell of the replaced request survived",
        );
        thread::sleep(Duration::from_millis(2500));
        assert!(
            !lane.home().join("late").exists(),
            "a child of the replaced request wrote after it ended"
        );
        assert_eq!(lane.end(), 0);
    });
}

#[test]
fn a_cancel_ends_a_request_and_its_processes() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        lane.request(
            "c1",
            Some("slot"),
            "sleep 300 & echo $! > \"$HOME/child\"; ( sleep 2; printf late > \"$HOME/late\" ) & echo $! > \"$HOME/late-child\"; sleep 300",
        );
        lane.until("c1's ack", |frames| {
            frames.iter().any(|frame| frame == "A c1")
        });
        let (child, late_child) = children(&lane);

        lane.cancel("c1");
        lane.until("c1's ending", |frames| {
            frames.iter().any(|frame| frame == "D c1 143 ")
        });
        gone(child, "a child of the cancelled request survived");
        gone(
            late_child,
            "a forked subshell of the cancelled request survived",
        );

        // An id the loop never held is not a request to answer for.
        lane.cancel("nope");
        lane.request("c2", None, "printf 'alive\\n'");
        lane.until("c2's answer", |frames| {
            frames.iter().any(|frame| frame.starts_with("D c2 "))
        });
        assert!(!lane
            .frames()
            .iter()
            .any(|frame| frame.starts_with("D nope")));

        thread::sleep(Duration::from_millis(2500));
        assert!(!lane.home().join("late").exists());
        assert_eq!(lane.end(), 0);
    });
}

#[test]
fn a_maximum_id_and_key_keep_every_frame_inside_one_pipe_buffer() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        let id = "a".repeat(32);
        let key = "b".repeat(32);
        lane.request(
            &id,
            Some(&key),
            "head -c 8192 /dev/zero | tr '\\0' y; printf '\\n'",
        );
        lane.until("the first answer", |frames| {
            frames
                .iter()
                .any(|frame| frame.starts_with(&format!("D {id} ")))
        });

        lane.request(&id, Some(&key), "sleep 300");
        lane.until("two acks", |frames| {
            frames
                .iter()
                .filter(|frame| **frame == format!("A {id}"))
                .count()
                == 2
        });
        lane.cancel(&id);
        lane.until("the cancel's ending", |frames| {
            frames.iter().any(|frame| frame == &format!("D {id} 143 "))
        });

        // The loop's own frames carry the id, so its output bound is the one a
        // maximum id could break.
        assert_eq!(oversized(&lane.frames()), Vec::<&String>::new());
        assert_eq!(lane.end(), 0);
    });
}

#[test]
fn concurrent_requests_run_side_by_side() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        let started = Instant::now();
        lane.request("p1", None, "sleep 1; printf 'one\\n'");
        lane.request("p2", None, "sleep 1; printf 'two\\n'");
        lane.until("both answers", |frames| {
            frames.iter().any(|frame| frame.starts_with("D p1 "))
                && frames.iter().any(|frame| frame.starts_with("D p2 "))
        });
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(1900),
            "two one-second requests took {elapsed:?}: the loop blocked one behind the other"
        );
        assert_eq!(lane.answered("p1").stdout, b"one\n");
        assert_eq!(lane.answered("p2").stdout, b"two\n");
        assert_eq!(lane.end(), 0);
    });
}

#[test]
fn a_signal_ends_the_loop_and_every_group() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        lane.request(
            "t1",
            None,
            "sleep 300 & echo $! > \"$HOME/child\"; sleep 300",
        );
        lane.until("t1's ack", |frames| {
            frames.iter().any(|frame| frame == "A t1")
        });
        lane.until("the child pid", |_| lane.home().join("child").exists());
        let child: i32 = fs::read_to_string(lane.home().join("child"))
            .expect("the child pid")
            .trim()
            .parse()
            .expect("a pid");

        // What a client does to a loop of a foreign wire: `term`, so the
        // loop's own handler ends its requests instead of orphaning them.
        unsafe {
            kill(lane.child.id() as i32, 15);
        }
        let status = lane
            .wait(Duration::from_secs(10))
            .expect("the loop ended on TERM");
        assert_eq!(status.code(), Some(0));
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(child) {
            assert!(
                Instant::now() < deadline,
                "a request's child survived the loop's TERM"
            );
            thread::sleep(Duration::from_millis(5));
        }
    });
}

#[test]
fn a_request_answers_once_even_against_an_immediate_cancel() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        for round in 0..20 {
            let id = format!("q{round}");
            lane.request(&id, None, "exit 0");
            lane.cancel(&id);
            lane.until("one answer", |frames| {
                frames
                    .iter()
                    .any(|frame| frame.starts_with(&format!("D {id} ")))
            });
        }
        thread::sleep(Duration::from_millis(200));
        let frames = lane.frames();
        // The bash loop can answer twice when a `K` lands between its pid
        // file's removal and the request's own `D`; this program claims the
        // one answer before writing it. A client drops the second `D` as an
        // unknown id, so no client can see the difference — the suite holds
        // this program to the stricter rule and records the bash loop's.
        let most = if implementation.name == "sbx-lane-loop" {
            1
        } else {
            2
        };
        for round in 0..20 {
            let id = format!("q{round}");
            let endings = frames
                .iter()
                .filter(|frame| frame.starts_with(&format!("D {id} ")))
                .count();
            assert!(
                (1..=most).contains(&endings),
                "{id} answered {endings} times ({} allows {most})",
                implementation.name
            );
        }
        assert_eq!(lane.end(), 0);
    });
}

#[test]
fn a_second_request_on_its_own_key_is_refused() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        lane.request("a1", Some("k1"), "sleep 300");
        lane.until("a1's ack", |frames| {
            frames.iter().any(|frame| frame == "A a1")
        });
        lane.request("a1", Some("k1"), "printf 'again\\n'");
        lane.until("the refusal", |frames| {
            frames.iter().any(|frame| frame.starts_with("E a1 "))
        });
        let refusal = lane
            .frames()
            .into_iter()
            .find(|frame| frame.starts_with("E a1 "))
            .expect("the refusal");
        assert_eq!(refusal_reason(&refusal), "the request is already running");
        lane.cancel("a1");
        lane.until("a1's ending", |frames| {
            frames.iter().any(|frame| frame == "D a1 143 ")
        });
        assert_eq!(lane.end(), 0);
    });
}

#[test]
fn a_request_that_forks_children_answers_with_all_of_them() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        lane.request(
            "f1",
            None,
            "( printf 'forked\\n'; sleep 0.2; printf 'late\\n' ) & wait",
        );
        lane.until("f1's answer", |frames| {
            frames.iter().any(|frame| frame.starts_with("D f1 "))
        });
        the_same(&lane.answered("f1").stdout, b"forked\nlate\n");
        assert_eq!(lane.end(), 0);
    });
}

#[test]
fn end_of_stdin_ends_the_loop_and_every_group() {
    for_each(|implementation| {
        let mut lane = Loop::start(implementation);
        lane.request(
            "e1",
            None,
            "sleep 300 & echo $! > \"$HOME/child\"; sleep 300",
        );
        lane.until("e1's ack", |frames| {
            frames.iter().any(|frame| frame == "A e1")
        });
        lane.until("the child pid", |_| lane.home().join("child").exists());
        let child: i32 = fs::read_to_string(lane.home().join("child"))
            .expect("the child pid")
            .trim()
            .parse()
            .expect("a pid");

        assert_eq!(lane.end(), 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(child) {
            assert!(
                Instant::now() < deadline,
                "a request's child survived end of stdin"
            );
            thread::sleep(Duration::from_millis(5));
        }
    });
}

/// The two pids a group-testing command writes once it has forked both.
/// Waiting for them keeps the test from racing the second fork: a member
/// forked after a group's TERM is in the group but never got the signal.
fn children(lane: &Loop) -> (i32, i32) {
    lane.until("both child pids", |_| {
        lane.home().join("child").exists() && lane.home().join("late-child").exists()
    });
    let read = |name: &str| -> i32 {
        fs::read_to_string(lane.home().join(name))
            .unwrap_or_else(|_| panic!("{name} pid"))
            .trim()
            .parse()
            .expect("a pid")
    };
    (read("child"), read("late-child"))
}

/// Buffer equality with a readable failure message.
fn the_same(actual: &[u8], expected: &[u8]) {
    assert_eq!(
        String::from_utf8_lossy(actual),
        String::from_utf8_lossy(expected)
    );
}

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

fn alive(pid: i32) -> bool {
    unsafe { kill(pid, 0) == 0 }
}

/// A signal is delivered synchronously but the process exits when it gets to
/// it — and the bash loop reaches a group through a middle shell's trap — so
/// death is polled rather than sampled once.
fn gone(pid: i32, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) {
        assert!(Instant::now() < deadline, "{what}");
        thread::sleep(Duration::from_millis(5));
    }
}
