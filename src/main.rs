//! sbx-lane-loop — the sandbox-side half of the lane stream.
//!
//! One long-lived process per sandbox. Its stdin is a line-framed request
//! channel; its stdout is the frames the client reads back over the Docker
//! process-API stream. The wire is the one worktrunk-sbx's bash loop defined;
//! `docs/lane-loop.md` records what is kept and what is improved.
//!
//! Requests run in their own process groups, so ending one — by `K`, by a
//! successor request taking its key, or by the loop ending at all — reaches every
//! process the command started.

mod base64;
mod env;
mod frames;
mod lane;
mod protocol;
mod sha256;
mod signals;
mod sys;

use std::sync::Arc;
use std::time::Duration;

const USAGE: &str = "\
usage: sbx-lane-loop [seconds]
       sbx-lane-loop --protocol

  seconds     tick interval in seconds (default 5)
  --protocol  print the protocol id this loop speaks, then exit

The loop reads requests on stdin and writes frames on stdout; end of stdin
ends the loop and every request's process group. Exit: 0 after end of stdin
or TERM/INT/HUP, 1 when stdin could not be read or the reader of the frames
went away, 2 for a usage error.";

fn parse_tick(value: &str) -> Option<Duration> {
    let seconds: f64 = value.parse().ok()?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(seconds))
}

fn main() {
    let mut tick = Duration::from_secs(5);
    let mut intervals = 0;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--protocol" => {
                println!("{}", protocol::protocol_id());
                return;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            value => {
                intervals += 1;
                match parse_tick(value) {
                    Some(parsed) if intervals == 1 => tick = parsed,
                    _ => {
                        eprintln!("sbx-lane-loop: not a tick interval: {value:?}\n{USAGE}");
                        std::process::exit(2);
                    }
                }
            }
        }
    }

    sys::ignore_sigpipe();
    let lane = Arc::new(lane::Lane::new(tick, env::login_env()));
    {
        let stop = Arc::clone(&lane);
        signals::install(move || stop.shutdown());
    }
    {
        let stop = Arc::clone(&lane);
        frames::on_client_gone(move || stop.shutdown());
    }
    if let Err(error) = lane.serve() {
        eprintln!("sbx-lane-loop: reading stdin: {error}");
        lane.shutdown();
        std::process::exit(1);
    }
    lane.shutdown();
}
