# Design: the lane loop as a small static program

The lane stream is how a client talks to a lane's sandbox: one long-lived
process per sandbox, its stdin a line-framed request channel, its stdout the
frames the client reads back over Docker's process-API stream. That process
was a bash script shipped inside the client (`LANE_LOOP`), started with
`bash -c`. Monitor mode, pid files and 512-byte pipe writes made it the most
fragile part of the system — three failures in a week: a stdin lifeline that
ate the script, label reuse, handshake races. This is that loop as a program.

The wire is unchanged. A client built for the bash loop reads this loop's
frames and vice versa, and the conformance suite runs the same scenarios
against both. What changes is the machinery: real process groups, one writer,
no shell parsing of the protocol.

## Language and shape

Rust, built for `x86_64-unknown-linux-musl` (and `aarch64-unknown-linux-musl`)
with no runtime dependencies: one static file, a few hundred KB, nothing to
install beside it. `std::os::unix::process::CommandExt::process_group` gives
each request a process group at `exec` time — the thing bash needed monitor
mode, a middle subshell and pid files to get. The whole program is
`std::thread`: one thread per request, one ticker, one reader. There is no
async runtime because there is no concurrency to schedule: a request is a
fork, two pipes and a wait, and the loop's other work is appending lines.

The crate has no dependencies. Base64, SHA-256 and the libc calls std does not
expose (`signal`, `kill`, `write`) are small, tested pieces in `src/`. A
supply chain of zero is worth more here than a familiar crate.

## Where it lives, and how it is installed

Releases are GitHub tarballs (`sbx-lane-loop-<version>-<target>.tar.gz`) with
a `SHA256SUMS` and a build-provenance attestation. The image installs it with
mise's `github:` backend, pinned to a version and a per-platform
`sha256:` checksum, so the executable lands under `MISE_DATA_DIR` and the
shim `/opt/mise/shims/sbx-lane-loop` is on `PATH` for every shell the image
prepares. A client starts it as `sbx-lane-loop` — not an absolute path — so
the hot mise manifest can update it without an image rebuild, and an image
that predates it simply has no shim.

`--protocol` prints the wire's id without starting the loop, so a client can
ask a binary what it speaks before starting it, and an operator can check an
install.

## The login environment

A process started through the platform's API gets no login environment:
`herdr` and the rest of the mise shims are on `PATH` only for a shell that
sourced what an ssh session sources. The loop therefore sources
`/etc/stack-base/env.sh` and `~/.sbx-env` — once, at start, through one
`bash -c` that prints the resulting environment NUL-separated — and starts
every request with exactly that environment.

Once rather than per request, because that is what the bash loop does: it
sources the two files in its own process and its requests inherit. Reading
them again per request would change nothing (no request can change the loop's
environment) and would run bash-shaped files on every request. The files are
read by bash, not reimplemented, because they are bash-shaped. When no bash
can be run, requests inherit the loop's own environment and one line goes to
stderr; stderr is not the frame channel.

Requests themselves run as `/bin/sh` with the command on stdin — exactly
`printf '%s' "$cmd" | sh` in the bash loop.

## The protocol id

The program answers `--protocol` with eight hex characters: the first four
bytes of the SHA-256 of the canonical wire string in `src/protocol.rs`
(`WIRE`), which spells every frame and every bound. Editing a frame or a
bound means editing that string, so the id moves with the wire. This build
answers `dea4911a`.

The bash loop's `LANE_LOOP_PROTOCOL` is the same construction over that
script's text. The two ids are deliberately **distinct** although the frames
are the same:

- A label is how a client adopts a loop. A label that cannot name which
  implementation answers cannot tell a client whether it should start this
  binary or fall back to bash; an id shared by both would make every old
  client silently adopt the new implementation, including any behaviour it
  was not built for.
- The bash id hashes a script this program does not contain. Two different
  programs answering the same "I am this script's hash" is a false identity,
  and it would let a future divergence ship under an unchanged id.

Coexistence is what the two ids make explicit. A sandbox can hold a loop of
either wire while images and clients roll forward:

| on the sandbox | client speaks this id | client speaks the bash id |
|---|---|---|
| this loop | adopt | foreign class: end it once, start its own bash loop |
| bash loop | adopt (same frames) | adopt |
| no loop | start this one when the image has it | start bash |

A client that knows both ids adopts whichever is running and starts this
binary when `command -v sbx-lane-loop` succeeds. Ending a foreign loop uses
`term`, so the loop's own handler kills its requests instead of orphaning
them — the same rule the bash loop's trap keeps.

## Frame sizes

The bash loop's 460-character pieces exist because its ticker and its
requests shared one pipe and a write larger than `PIPE_BUF` (512 bytes) could
be split by another writer. A single writer with a mutex per frame could lift
the bound. It does not: the same 460-character pieces and the same 32-character
id bound are kept, so the largest frame is still 498 bytes and the two
implementations are indistinguishable on the wire. Nothing needs the larger
frames, and the client's line buffer, its tests and the monitor's chunk
handling were all reasoned about at 512.

## What is kept byte for byte

- Every refusal text, and `E -` for a line that is not a frame, including a
  blank line.
- `D <id> <exit> <flags>` with an empty flags field still followed by a space;
  `D <id> 143 ` for a request ended by `K` or by a key's successor.
- The 256 KiB cut per stream and the `t` flag when either stream was longer.
- Streamed stdout as whole lines only: a last line without its newline is
  dropped, and stderr arrives after the command ends.
- Commands run through `sh` on stdin, with NULs stripped and trailing
  newlines trimmed as the shell's command substitution did.
- A final request line without its newline is dropped, never run.
- An `S` naming a key the same id already holds is refused; a `K` for an id
  the loop does not hold answers nothing; a key of `-` holds no slot.
- End of stdin ends the loop and every request's group; `TERM`/`INT`/`HUP` do
  the same and exit 0.

## What is improved, invisibly

- **No temp files.** A buffered request's output is captured in memory up to
  the 256 KiB cut and the rest is counted, where bash wrote the whole output
  to disk and cut it on the way out. The frames are identical; a runaway
  command no longer fills the sandbox's disk.
- **No registration race.** A request is registered before its `A` is
  written, so a `K` that arrives immediately is honoured; bash wrote its pid
  file after forking and could miss one.
- **Ending is one signal to the group.** The loop signals the process group
  it created. The bash loop signalled a middle subshell, whose trap signalled
  the group; a stop that landed between the fork and the pid write needed a
  second kill to cover the gap.
- **The ticker is a timer.** `sleep`-in-a-loop drifts; ticks are only
  liveness, so no client can tell.
- **SIGPIPE is ignored, and a dead reader ends the loop explicitly.** The
  thread that feeds a request's script writes to a pipe the command owns, and
  a command that exits without reading it (`exit 0`, `head`) closes the read
  end first: with the default action that write kills the whole loop, and on
  Darwin the signal is delivered process-wide, so blocking it only for the
  writer does not help. Ignored, it is a dropped `EPIPE`, as it is inside the
  bash loop's pipeline. The frames keep the rule the shell's death stood for:
  a frame that cannot be written is a reader that is gone, so the loop kills
  its request groups and exits 1. The bash loop dies on SIGPIPE without its
  trap and leaves them; no client is there to see either, and the suite
  records both.
- **One answer.** A completion claims the request's single answer before
  writing it, so a `K` racing a finishing command cannot produce a second `D`.
  The bash loop can: its cancel reads the pid file just before the request
  removes it, and both then answer. A client drops a second `D` for an id it
  has already finished as an unknown id, so the rule is invisible — the
  conformance suite holds this program to one and records the bash loop's
  two.

## Testing

Unit tests pin the codec, the frame bounds, the parser's refusals, the
environment dump and the protocol id. `tests/conformance.rs` drives the real
binary through the wire: acks, refusals, stderr and exit status, streamed
lines, the cut, key replacement, `K`, maximum ids, two requests in parallel,
forked children, end of stdin and `TERM`. With `SBX_LANE_LOOP_BASH` pointing
at worktrunk-sbx's `lane-loop.ts`, the same suite extracts `LANE_LOOP` and
runs every scenario against the bash loop too — the parity claim, executed
rather than asserted. The variable is unset in CI, which runs the binary
half against the test build and again against the release build; that
repository is private and its script is not vendored here.
