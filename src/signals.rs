//! TERM, INT and HUP end the loop the way end of stdin does: nothing more is
//! answered and every request's process group gets a TERM.
//!
//! The handler only writes one byte to a pipe. A watcher thread does the rest,
//! because killing process groups takes locks a signal handler must not. The
//! bash loop's trap is what this replaces: without it, a client signalling a
//! foreign loop (the protocol handshake's "end a loop you cannot talk to")
//! would leave the loop's commands running in the sandbox.

use std::io::{ErrorKind, Read};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;

use crate::sys;

static WAKE: AtomicI32 = AtomicI32::new(-1);

pub fn install(stop: impl FnOnce() + Send + 'static) {
    let Ok((wake, mut wait)) = UnixStream::pair() else {
        return;
    };
    WAKE.store(wake.as_raw_fd(), Ordering::SeqCst);
    sys::handle(sys::SIGHUP, handler);
    sys::handle(sys::SIGINT, handler);
    sys::handle(sys::SIGTERM, handler);
    thread::spawn(move || {
        // The write end lives with the reader: it is the fd the handler writes.
        let _wake = wake;
        let mut byte = [0u8; 1];
        loop {
            match wait.read(&mut byte) {
                Ok(_) => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        stop();
        std::process::exit(0);
    });
}

extern "C" fn handler(_signal: i32) {
    let fd = WAKE.load(Ordering::SeqCst);
    if fd >= 0 {
        sys::write_byte(fd, b'!');
    }
}
