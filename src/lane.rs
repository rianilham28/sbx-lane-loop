//! The loop: one process per sandbox, its stdin the request channel and its
//! stdout the frames.
//!
//! Each request is a `sh` in a process group of its own, so ending one —
//! `K`, a successor stream taking its key, or the loop ending at all — reaches
//! every process the command started. The state that decides is two maps: an
//! id to its request (the newest of that id, as the bash loop's pid file) and
//! a key to the request holding it.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use crate::frames::Frames;
use crate::protocol::{self, Incoming, Kind, MAX_OUTPUT};
use crate::sys;

pub struct Lane {
    tick: Duration,
    env: Option<Vec<(OsString, OsString)>>,
    frames: Frames,
    state: Mutex<State>,
    stopping: AtomicBool,
}

#[derive(Default)]
struct State {
    /// The request an id names, newest first: the bash loop keeps one pid file
    /// per id, so a second request of the same id is the one `K` ends.
    by_id: HashMap<String, Arc<Request>>,
    /// The request holding a key. At most one, whatever came before it.
    by_key: HashMap<String, Arc<Request>>,
}

struct Request {
    id: String,
    key: Option<String>,
    streamed: bool,
    command: Vec<u8>,
    /// `Pending` until the shell exists, then its pid — the process group's.
    /// Under the same lock a killer looks, so a request cannot start after its
    /// end has been decided, nor be missed between fork and registration.
    phase: Mutex<Phase>,
    ended: AtomicBool,
}

#[derive(Clone, Copy)]
enum Phase {
    Pending,
    Running(i32),
    Finished,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn remove_if_same(map: &mut HashMap<String, Arc<Request>>, name: &str, request: &Arc<Request>) {
    if map.get(name).is_some_and(|held| Arc::ptr_eq(held, request)) {
        map.remove(name);
    }
}

fn shell() -> &'static str {
    // `/bin/sh` is what `sh` resolves to in the image and on macOS; the
    // absolute path keeps a repo's PATH shim from replacing the script shell.
    if std::path::Path::new("/bin/sh").exists() {
        "/bin/sh"
    } else {
        "sh"
    }
}

fn exit_code(status: io::Result<ExitStatus>) -> i32 {
    match status {
        Ok(status) => status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
        Err(_) => 127,
    }
}

/// Everything a stream wrote, bounded: the head up to the wire's cut is kept,
/// the rest is only counted. The bash loop wrote the whole output to a temp
/// file and cut it on the way out; the answer is the same either way, and a
/// runaway command no longer fills the sandbox's disk.
fn capture(mut reader: impl Read) -> (Vec<u8>, u64) {
    let mut kept = Vec::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                total += read as u64;
                if kept.len() < MAX_OUTPUT {
                    let room = MAX_OUTPUT - kept.len();
                    kept.extend_from_slice(&buffer[..read.min(room)]);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    (kept, total)
}

impl Lane {
    pub fn new(tick: Duration, env: Option<Vec<(OsString, OsString)>>) -> Self {
        Self {
            tick,
            env,
            frames: Frames::new(),
            state: Mutex::new(State::default()),
            stopping: AtomicBool::new(false),
        }
    }

    /// Read until end of stdin. A final line without its newline is dropped:
    /// a writer that died mid-frame left a command that is only a prefix.
    pub fn serve(self: &Arc<Self>) -> io::Result<()> {
        self.start_ticker();
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if line.last() != Some(&b'\n') {
                break;
            }
            line.pop();
            self.handle(protocol::parse(&line));
        }
        Ok(())
    }

    fn handle(self: &Arc<Self>, incoming: Incoming<'_>) {
        match incoming {
            Incoming::Refused(id, reason) => self.frames.refused(id, reason),
            Incoming::Cancel(id) => {
                let held = lock(&self.state).by_id.get(id).cloned();
                if let Some(request) = held {
                    self.end(&request);
                }
            }
            Incoming::Request(frame) => self.start(frame),
        }
    }

    fn start(self: &Arc<Self>, frame: protocol::Request<'_>) {
        if let Some(key) = frame.key {
            let held = lock(&self.state).by_key.get(key).cloned();
            if let Some(held) = held {
                if held.id == frame.id {
                    self.frames
                        .refused(Some(frame.id), "the request is already running");
                    return;
                }
            }
        }
        let Some(command) = protocol::decode_payload(frame.payload) else {
            self.frames
                .refused(Some(frame.id), "the request is not base64");
            return;
        };
        let request = Arc::new(Request {
            id: frame.id.to_owned(),
            key: frame.key.map(str::to_owned),
            streamed: frame.kind == Kind::Streamed,
            command,
            phase: Mutex::new(Phase::Pending),
            ended: AtomicBool::new(false),
        });
        let replaced = {
            let mut state = lock(&self.state);
            state.by_id.insert(request.id.clone(), Arc::clone(&request));
            match &request.key {
                Some(key) => state.by_key.insert(key.clone(), Arc::clone(&request)),
                None => None,
            }
        };
        // An S naming a held key ends the holder before the successor is
        // acked, so its answer cannot arrive after the successor's ack.
        if let Some(replaced) = replaced {
            self.end(&replaced);
        }
        self.frames.ack(&request.id);
        let lane = Arc::clone(self);
        thread::spawn(move || lane.run(request));
    }

    /// End a request: its whole group gets TERM and it answers `D 143`, the
    /// status a killed request cannot report itself.
    fn end(&self, request: &Arc<Request>) {
        {
            let phase = lock(&request.phase);
            if request.ended.swap(true, Ordering::SeqCst) {
                return;
            }
            if let Phase::Running(pid) = *phase {
                sys::kill_group(pid);
            }
        }
        {
            let mut state = lock(&self.state);
            remove_if_same(&mut state.by_id, &request.id, request);
        }
        self.frames.done(&request.id, 143, "");
    }

    /// End of stdin or a signal: kill every group, answer nothing — the bash
    /// loop's trap does the same, because nobody is left to read an answer.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        let requests = {
            let state = lock(&self.state);
            let mut all: Vec<Arc<Request>> = Vec::new();
            for request in state.by_id.values().chain(state.by_key.values()) {
                if !all.iter().any(|seen| Arc::ptr_eq(seen, request)) {
                    all.push(Arc::clone(request));
                }
            }
            all
        };
        for request in requests {
            // A request that was already ended had its group signalled then.
            if request.ended.swap(true, Ordering::SeqCst) {
                continue;
            }
            if let Phase::Running(pid) = *lock(&request.phase) {
                sys::kill_group(pid);
            }
        }
    }

    fn start_ticker(self: &Arc<Self>) {
        let tick = self.tick;
        let lane = Arc::clone(self);
        thread::spawn(move || loop {
            thread::sleep(tick);
            if lane.stopping.load(Ordering::SeqCst) {
                return;
            }
            lane.frames.tick();
        });
    }

    fn release(&self, request: &Arc<Request>) {
        let mut state = lock(&self.state);
        remove_if_same(&mut state.by_id, &request.id, request);
        if let Some(key) = &request.key {
            remove_if_same(&mut state.by_key, key, request);
        }
    }

    fn run(self: Arc<Self>, request: Arc<Request>) {
        let mut command = Command::new(shell());
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(environment) = &self.env {
            command.env_clear().envs(environment.iter().cloned());
        }

        // The spawn happens under the phase lock: `end` takes the same lock
        // before deciding, so a request either never starts or is killed.
        let spawned = {
            let mut phase = lock(&request.phase);
            if request.ended.load(Ordering::SeqCst) {
                *phase = Phase::Finished;
                drop(phase);
                self.release(&request);
                return;
            }
            let spawned = command.spawn();
            match &spawned {
                Ok(child) => *phase = Phase::Running(child.id() as i32),
                Err(_) => *phase = Phase::Finished,
            }
            // Only a signal-driven `shutdown` swaps `ended` outside the lock;
            // this covers the group it would have missed.
            if let (Ok(child), true) = (&spawned, request.ended.load(Ordering::SeqCst)) {
                sys::kill_group(child.id() as i32);
            }
            spawned
        };

        let mut child = match spawned {
            Ok(child) => child,
            Err(_) => {
                // No shell to run it. The bash loop's `sh` would have exited
                // 127 with its message on a stderr nobody reads.
                let answered = !request.ended.swap(true, Ordering::SeqCst);
                self.release(&request);
                if answered {
                    self.frames.done(&request.id, 127, "");
                }
                return;
            }
        };

        let mut stdin = child.stdin.take().expect("stdin is piped");
        let script = request.command.clone();
        let feeder = thread::spawn(move || {
            // `sh` reads the script from stdin; writing on its own thread
            // keeps a long command from meeting a full stdout pipe. A command
            // that exits without reading its script closes the pipe first:
            // SIGPIPE is ignored, so that write is a dropped `EPIPE`.
            let _ = stdin.write_all(&script);
        });

        if request.streamed {
            let stderr = child.stderr.take().expect("stderr is piped");
            let stderr_task = thread::spawn(move || capture(stderr));
            let stdout = child.stdout.take().expect("stdout is piped");
            let mut lines = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                let read = match lines.read_until(b'\n', &mut line) {
                    Ok(read) => read,
                    Err(_) => break,
                };
                if read == 0 {
                    break;
                }
                if line.last() != Some(&b'\n') {
                    // The bash loop's `read` fails on a last line without its
                    // newline, and the line is dropped; same answer here.
                    break;
                }
                // NULs do not survive a shell variable, and the line passes
                // through one in the bash loop.
                let line: Vec<u8> = line.iter().copied().filter(|byte| *byte != 0).collect();
                for piece in protocol::pieces(&line) {
                    self.frames.output(&request.id, false, &piece);
                }
            }
            let status = child.wait();
            *lock(&request.phase) = Phase::Finished;
            let _ = feeder.join();
            let (stderr, total) = stderr_task.join().unwrap_or((Vec::new(), 0));
            // One claim on the single answer this request will ever get: an
            // end that got there first wrote `D 143`, and this one stays
            // quiet — the same as the bash loop, whose pid file is gone
            // before it answers.
            let answered = !request.ended.swap(true, Ordering::SeqCst);
            self.release(&request);
            if answered {
                for piece in protocol::pieces(&stderr) {
                    self.frames.output(&request.id, true, &piece);
                }
                let flags = if total > MAX_OUTPUT as u64 { "t" } else { "" };
                self.frames.done(&request.id, exit_code(status), flags);
            }
        } else {
            let stdout = child.stdout.take().expect("stdout is piped");
            let stderr = child.stderr.take().expect("stderr is piped");
            let stdout_task = thread::spawn(move || capture(stdout));
            let stderr_task = thread::spawn(move || capture(stderr));
            let status = child.wait();
            *lock(&request.phase) = Phase::Finished;
            let _ = feeder.join();
            let (stdout, stdout_total) = stdout_task.join().unwrap_or((Vec::new(), 0));
            let (stderr, stderr_total) = stderr_task.join().unwrap_or((Vec::new(), 0));
            let answered = !request.ended.swap(true, Ordering::SeqCst);
            self.release(&request);
            if answered {
                for piece in protocol::pieces(&stdout) {
                    self.frames.output(&request.id, false, &piece);
                }
                for piece in protocol::pieces(&stderr) {
                    self.frames.output(&request.id, true, &piece);
                }
                let cut = stdout_total > MAX_OUTPUT as u64 || stderr_total > MAX_OUTPUT as u64;
                self.frames
                    .done(&request.id, exit_code(status), if cut { "t" } else { "" });
            }
        }
    }
}
