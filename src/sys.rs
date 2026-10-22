//! The libc calls std does not expose: a signal handler, a signal sent to a
//! process group, and the default SIGPIPE action.
//!
//! Linking libc is not a dependency: the musl target is linked statically and
//! every symbol here is in it. The numbers are the ones both Linux and macOS
//! use for these signals.

extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
    fn kill(pid: i32, sig: i32) -> i32;
    fn write(fd: i32, buf: *const std::ffi::c_void, count: usize) -> isize;
}

pub const SIGHUP: i32 = 1;
pub const SIGINT: i32 = 2;
pub const SIGPIPE: i32 = 13;
pub const SIGTERM: i32 = 15;

const SIG_DFL: usize = 0;

/// Rust ignores SIGPIPE; the shell loop this program replaces died on it, and
/// a loop whose reader is gone has nobody left to answer. Restoring the
/// default action keeps that: writing to a closed stdout ends the process.
pub fn default_sigpipe() {
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

pub fn handle(sig: i32, handler: extern "C" fn(i32)) {
    unsafe {
        signal(sig, handler as usize);
    }
}

/// One TERM to a whole process group: the command, the children it forked and
/// anything they started, as long as none of them left the group.
pub fn kill_group(pid: i32) {
    unsafe {
        kill(-pid, SIGTERM);
    }
}

/// One byte, async-signal-safe: all the handler does.
pub fn write_byte(fd: i32, byte: u8) {
    unsafe {
        write(fd, (&byte as *const u8).cast(), 1);
    }
}
