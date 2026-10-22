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

const SIG_IGN: usize = 1;

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

/// Ignores SIGPIPE, for the whole process.
///
/// The thread that feeds a request's script writes to a pipe the command owns,
/// and a command that exits without reading it — `exit 0`, `head` — closes the
/// read end first. Under the default action that write ends the loop, and on
/// Darwin the signal that kills it is delivered process-wide, so a mask on the
/// writer would not hold it either. Ignored, the write is an `EPIPE` the feeder
/// drops; the loop's own frames are what keep the shell's rule, in `Frames`.
pub fn ignore_sigpipe() {
    unsafe {
        signal(SIGPIPE, SIG_IGN);
    }
}

/// One byte, async-signal-safe: all the handler does.
pub fn write_byte(fd: i32, byte: u8) {
    unsafe {
        write(fd, (&byte as *const u8).cast(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::ignore_sigpipe;
    use std::io::Write;

    #[test]
    fn an_ignored_sigpipe_turns_a_dead_pipe_write_into_an_error() {
        ignore_sigpipe();
        let (mut writer, reader) = std::os::unix::net::UnixStream::pair().expect("a socket pair");
        drop(reader);
        assert!(
            writer.write_all(b"x").is_err(),
            "the write should report EPIPE"
        );
    }
}
