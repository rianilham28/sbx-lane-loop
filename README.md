# sbx-lane-loop

The sandbox-side half of the lane stream: **one long-lived process per
sandbox**. Its stdin is a line-framed request channel and its stdout is the
frames the client reads back over the Docker process-API stream. A request
names a shell command; the loop acks it, answers with its output and exit
status, and can end it — whole process group and all — before it finishes.

It replaces the bash loop that shipped inside the client
(`worktrunk-sbx/src/sandbox/lane-loop.ts`) with the same wire, so a client
built for either adopts both. The bash loop stays the fallback for images
that do not have this binary.

The design — language, process handling, the protocol id, what is kept
byte for byte and what is improved — is in [`docs/design.md`](docs/design.md).

## Install

The binary is a static `x86_64`/`aarch64-unknown-linux-musl` build with no
runtime dependencies, released as a tarball. [mise](https://mise.jdx.dev)'s
`github` backend installs it, verified against a pinned digest:

```toml
[tools."github:rianilham28/sbx-lane-loop"]
version = "0.1.0"

[tools."github:rianilham28/sbx-lane-loop".platforms]
linux-x64 = { asset_pattern = "sbx-lane-loop-{{ version }}-x86_64-unknown-linux-musl.tar.gz", checksum = "sha256:<from SHA256SUMS>" }
linux-arm64 = { asset_pattern = "sbx-lane-loop-{{ version }}-aarch64-unknown-linux-musl.tar.gz", checksum = "sha256:<from SHA256SUMS>" }
```

`mise install` puts the executable under `MISE_DATA_DIR` and a shim on
`PATH`, so `sbx-lane-loop` resolves from any shell that sourced the image
environment.

## Run

```
sbx-lane-loop [seconds]    # tick interval, default 5
sbx-lane-loop --protocol   # the wire this build speaks, then exit
sbx-lane-loop --help
```

The process reads until **end of stdin**, which ends the loop and every
request's process group. `TERM`, `INT` and `HUP` do the same and exit 0.
Exit codes: `0` after end of input or a signal, `1` when stdin could not be
read, `2` for a usage error.

Nothing but frames is written to stdout. Diagnostics go to stderr, which is
not part of the wire.

## The wire

One ASCII line per frame in each direction. A request's payload is base64, so
a newline or a space in a command cannot break framing. Every frame the loop
writes is at most 512 bytes — POSIX's `_POSIX_PIPE_BUF` — and one writer
writes each frame whole.

| frame | direction | meaning |
|---|---|---|
| `R <id> <b64>` | client → loop | run a command; its output is sent when it finishes |
| `S <id> <key> <b64>` | client → loop | run a command under `key`; its stdout is forwarded line by line |
| `K <id>` | client → loop | end the request `id`, its whole process group |
| `A <id>` | loop → client | the request arrived and was accepted |
| `O <id> <1\|2> <b64>` | loop → client | an output piece: at most 345 bytes of stdout (`1`) or stderr (`2`) |
| `D <id> <exit> <flags>` | loop → client | finished; `flags` is empty or `t` (the answer was cut at 256 KiB) |
| `E <id\|-> <b64>` | loop → client | refused; `-` when not even an id was readable |
| `T` | loop → client | tick, every interval (liveness, not part of any request) |

An **id** is an opaque token of `[A-Za-z0-9]`, at most 32 characters, echoed
back as read. A **key** is the same, at most 32 characters.

- **Acks.** `A` is written before the command starts, so "unacked" means the
  loop never saw the frame — the one outcome a reconnect cannot mend.
- **Buffered answers.** `R` runs to completion; stdout and stderr are
  captured, and at most 256 KiB of each is sent. The cut is on the wire, not
  on the command.
- **Streamed answers.** `S` forwards stdout as each line completes; stderr is
  buffered and sent after the command ends. A last line without its newline
  is dropped, as the bash loop drops it. `S` output is unbounded on the wire.
- **Keys.** At most one request holds a key. An `S` naming a held key ends
  the holder first, with `D <holder> 143`, so a round that was lost cannot
  leave a request running beside its successor.
- **Ending.** `K` ends the request by id; a request that never existed
  answers nothing. Every request runs in its own process group, so ending one
  reaches every process it started. The killed request's answer is
  `D <id> 143`.
- **Refusals.** `E` carries one of: `unreadable frame`, `unreadable request
  id`, `request id is longer than 32 characters`, `unreadable request key`,
  `request key is longer than 32 characters`, `the request is already
  running`, `the request is not base64`. A refusal never ends the loop.

```
# run `printf 'hi\n'` and read its answer
R 7a0f cHJpbnRmICdoaVxuJw==
A 7a0f
O 7a0f 1 aGkK
D 7a0f 0
```

## `--protocol`

The protocol id is eight lowercase hex characters derived from the wire
definition (`src/protocol.rs`, `WIRE`): the first four bytes of its SHA-256.
It exists so a client can tell a loop of its own wire from one an earlier
build left behind: the id goes into the process label such as
`sbx-lane-stream-dea4911a-1f2e3d4c`, and a client adopts only a label naming
an id it speaks.

This binary and the bash loop are distinct ids even though they speak the
same frames: a label that cannot name which implementation answers cannot be
used to decide whether to start this one, and the bash id is a hash of a
script this program does not contain. A client that knows both ids can adopt
either and start this one when the image has it. An older client sees a
foreign id, ends that loop once, and starts its own — which is why the image
keeps the bash loop available as the fallback.

## Verify a release

```bash
version=0.1.0
target=x86_64-unknown-linux-musl
curl -fsSLO "https://github.com/rianilham28/sbx-lane-loop/releases/download/v$version/sbx-lane-loop-$version-$target.tar.gz"
curl -fsSLO "https://github.com/rianilham28/sbx-lane-loop/releases/download/v$version/SHA256SUMS"
sha256sum --ignore-missing -c SHA256SUMS
gh attestation verify "sbx-lane-loop-$version-$target.tar.gz" --repo rianilham28/sbx-lane-loop
tar -xzf "sbx-lane-loop-$version-$target.tar.gz"
./sbx-lane-loop --protocol
```

Every release builds in GitHub Actions and carries a build-provenance
attestation; mise verifies the pinned checksum before installing.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`tests/conformance.rs` drives the real binary through the whole wire: frames,
refusals, the 256 KiB cut, keys, `K`, ticks, concurrent requests, forked
children, end of input and signals.

The same suite runs against the bash loop when it is pointed at
worktrunk-sbx's source, which is not vendored here:

```bash
SBX_LANE_LOOP_BASH=~/workspace/projects/worktrunk-sbx/src/sandbox/lane-loop.ts \
  cargo test --test conformance
```

`LANE_LOOP_BIN=/path/to/sbx-lane-loop` runs the suite against a built binary —
a release artifact, or the binary inside an image.
