//! Base64 for the wire: the standard alphabet, padding required, and pieces
//! cut on four-character boundaries so every piece decodes on its own.
//!
//! The decoder is as strict about padding as the `base64 -d` the bash loop
//! pipes through in the image, and — like it — ignores the unused trailing
//! bits of a final group. It also rejects data after a `=`, which is what
//! makes `AA=A` a refusal rather than a silent answer.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        out.push(ALPHABET[(a >> 2) as usize] as char);
        out.push(ALPHABET[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(c & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

pub fn decode(input: &[u8]) -> Option<Vec<u8>> {
    if input.is_empty() {
        return Some(Vec::new());
    }
    if !input.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let groups = input.len() / 4;
    for (index, group) in input.chunks(4).enumerate() {
        let last = index + 1 == groups;
        let mut values = [0u8; 4];
        let mut padding = 0;
        for (position, &byte) in group.iter().enumerate() {
            if byte == b'=' {
                // Padding closes the input: only the last group, only its last
                // one or two characters.
                if !last || position < 2 {
                    return None;
                }
                padding += 1;
            } else {
                if padding > 0 {
                    return None;
                }
                values[position] = value(byte)?;
            }
        }
        out.push((values[0] << 2) | (values[1] >> 4));
        if padding < 2 {
            out.push((values[1] << 4) | (values[2] >> 2));
        }
        if padding < 1 {
            out.push((values[2] << 6) | values[3]);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};

    #[test]
    fn encode_pads_every_tail() {
        assert_eq!(encode(b""), "");
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
    }

    #[test]
    fn decode_round_trips_every_tail() {
        for input in [&b""[..], b"f", b"fo", b"foo", b"lane loop"] {
            assert_eq!(decode(encode(input).as_bytes()).as_deref(), Some(input));
        }
    }

    #[test]
    fn decode_rejects_what_the_shell_rejects() {
        // GNU coreutils in the image: strict padding, no data past a '='.
        for bad in ["s1", "AAA", "A", "AA=A", "====", "A===", "AA==AA=="] {
            assert_eq!(decode(bad.as_bytes()), None, "{bad:?} should be refused");
        }
        // Unused trailing bits are not checked, as coreutils does not.
        assert_eq!(decode(b"AR=="), Some(vec![1]));
        assert_eq!(decode(b"AA=="), Some(vec![0]));
    }
}
