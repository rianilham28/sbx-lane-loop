//! The login environment a request runs with.
//!
//! A process started through the API gets no login environment: `herdr` and
//! the rest of the mise shims are on `PATH` only for a shell that sourced
//! what an ssh session sources. The bash loop sources the two files in
//! itself, once, and its requests inherit the result; this program does the
//! same — one `bash -c` at start prints the environment after sourcing, and
//! every request is started with exactly that. Sourcing per request would
//! read the files again for no gain: no request could change the loop's
//! environment in the bash loop either, and a request sees only its own copy.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::process::{Command, Stdio};

/// What the bash loop does in its own body, then a NUL-separated dump so a
/// value with a newline or a space survives the trip.
const SOURCE: &str = r#"for f in /etc/stack-base/env.sh "$HOME/.sbx-env"; do
  [ -r "$f" ] && . "$f"
done
for n in $(compgen -e); do printf '%s=%s\0' "$n" "${!n}"; done
"#;

/// `None` when no bash could be run: requests then inherit the loop's own
/// environment, which is what the loop itself got from the platform.
pub fn login_env() -> Option<Vec<(OsString, OsString)>> {
    for bash in ["bash", "/bin/bash", "/usr/bin/bash"] {
        let child = Command::new(bash)
            .arg("-c")
            .arg(SOURCE)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        let Ok(child) = child else { continue };
        let Ok(output) = child.wait_with_output() else {
            continue;
        };
        if output.status.success() {
            return Some(parse(&output.stdout));
        }
    }
    eprintln!(
        "sbx-lane-loop: no bash to source the login environment; requests inherit the loop's own"
    );
    None
}

fn parse(bytes: &[u8]) -> Vec<(OsString, OsString)> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let at = entry.iter().position(|byte| *byte == b'=')?;
            Some((
                OsString::from_vec(entry[..at].to_vec()),
                OsString::from_vec(entry[at + 1..].to_vec()),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse;
    use std::ffi::OsString;

    #[test]
    fn a_dump_splits_on_nuls_and_the_first_equals() {
        let environment = parse(b"PATH=/opt/mise/shims:/usr/bin\0EMPTY=\0LATE=a=b\0\0");
        assert_eq!(
            environment,
            vec![
                (
                    OsString::from("PATH"),
                    OsString::from("/opt/mise/shims:/usr/bin")
                ),
                (OsString::from("EMPTY"), OsString::from("")),
                (OsString::from("LATE"), OsString::from("a=b")),
            ]
        );
        assert!(parse(b"").is_empty());
    }

    #[test]
    fn a_value_keeps_its_newlines() {
        let environment = parse(b"KEY=a\nb\0");
        assert_eq!(
            environment,
            vec![(OsString::from("KEY"), OsString::from("a\nb"))]
        );
    }
}
