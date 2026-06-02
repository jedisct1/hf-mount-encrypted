//! Base91 binary-to-text encoding over a filesystem-safe alphabet.
//!
//! Ported from the `zig-base91` reference. The filesystem alphabet is the
//! standard basE91 alphabet with `/` swapped for `'`, so an encoded value is
//! always a single legal path component: it never contains `/` or NUL.
//!
//! Encrypted filenames are both produced and consumed by hf-mount, so the
//! property that matters is that [`encode`] and [`decode`] are exact inverses.

const ALPHABET: &[u8; 91] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!#$%&()*+,.':;<=>?@[]^_`{|}~\"";

const NONE: u8 = 0xff;

const INVERSE: [u8; 256] = {
    let mut inv = [NONE; 256];
    let mut i = 0;
    while i < ALPHABET.len() {
        inv[ALPHABET[i] as usize] = i as u8;
        i += 1;
    }
    inv
};

/// Failure modes when decoding malformed base91 input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A byte that is not part of the filesystem alphabet.
    InvalidCharacter,
    /// Trailing bits that cannot correspond to any encoded input.
    InvalidPadding,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::InvalidCharacter => f.write_str("invalid base91 character"),
            Error::InvalidPadding => f.write_str("invalid base91 padding"),
        }
    }
}

impl std::error::Error for Error {}

/// Upper bound on the encoded length of `src_len` input bytes. Exact for the
/// worst case, which is what callers use to enforce a name-length limit.
pub fn encoded_len_upper_bound(src_len: usize) -> usize {
    let input_bits = src_len * 8;
    let full_blocks = input_bits / 13;
    let remaining_bits = input_bits % 13;
    let mut out = full_blocks * 2;
    if remaining_bits > 0 {
        out += 2;
    }
    out
}

/// Encode bytes as a base91 string over the filesystem alphabet.
pub fn encode(src: &[u8]) -> String {
    let mut out: Vec<u8> = Vec::with_capacity(encoded_len_upper_bound(src.len()));
    let mut acc: u32 = 0;
    let mut num_bits: u32 = 0;

    for &x in src {
        acc |= (x as u32) << num_bits;
        num_bits += 8;
        if num_bits > 13 {
            let mut v = acc & 0x1fff;
            if v > 88 {
                acc >>= 13;
                num_bits -= 13;
            } else {
                v = acc & 0x3fff;
                acc >>= 14;
                num_bits -= 14;
            }
            out.push(ALPHABET[(v % 91) as usize]);
            out.push(ALPHABET[(v / 91) as usize]);
        }
    }

    if num_bits > 0 {
        out.push(ALPHABET[(acc % 91) as usize]);
        if num_bits > 7 || acc > 90 {
            out.push(ALPHABET[(acc / 91) as usize]);
        }
    }

    String::from_utf8(out).expect("alphabet is ASCII")
}

/// Decode a base91 string produced by [`encode`].
pub fn decode(src: &str) -> Result<Vec<u8>, Error> {
    let mut out: Vec<u8> = Vec::with_capacity(src.len());
    let mut acc: Option<u32> = None;
    let mut b: u32 = 0;
    let mut num_bits: u32 = 0;

    for &x in src.as_bytes() {
        let idx = INVERSE[x as usize];
        if idx == NONE {
            return Err(Error::InvalidCharacter);
        }
        match acc {
            Some(acc_v) => {
                let a = acc_v + idx as u32 * 91;
                b |= a << num_bits;
                num_bits += if (a & 0x1fff) > 88 { 13 } else { 14 };
                loop {
                    out.push((b & 0xff) as u8);
                    b >>= 8;
                    num_bits -= 8;
                    if num_bits <= 7 {
                        break;
                    }
                }
                acc = None;
            }
            None => acc = Some(idx as u32),
        }
    }

    if let Some(a) = acc {
        let last = b | (a << num_bits);
        if last > 0xff {
            return Err(Error::InvalidPadding);
        }
        out.push((last & 0xff) as u8);
    } else if b != 0 {
        return Err(Error::InvalidPadding);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alphabet_is_well_formed() {
        assert_eq!(ALPHABET.len(), 91);
        let mut seen = [false; 256];
        for &c in ALPHABET {
            assert!(c != 0 && c < 128, "alphabet must be ASCII and non-NUL");
            assert!(!seen[c as usize], "alphabet has a duplicate character");
            seen[c as usize] = true;
        }
        // The filesystem variant must drop '/' (path separator) for '\''.
        assert!(!ALPHABET.contains(&b'/'));
        assert!(ALPHABET.contains(&b'\''));
    }

    #[test]
    fn empty_round_trips() {
        assert_eq!(encode(&[]), "");
        assert_eq!(decode("").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn round_trips_across_lengths() {
        // Deterministic xorshift so any failure reproduces.
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for len in 0..=300usize {
            let input: Vec<u8> = (0..len).map(|_| (next() & 0xff) as u8).collect();
            let encoded = encode(&input);
            assert!(
                encoded.len() <= encoded_len_upper_bound(len),
                "encoded length exceeds the upper bound at len {len}"
            );
            assert!(
                !encoded.as_bytes().contains(&b'/'),
                "encoded output contains '/' at len {len}"
            );
            let decoded = decode(&encoded).expect("decode");
            assert_eq!(decoded, input, "round-trip mismatch at len {len}");
        }
    }

    #[test]
    fn full_byte_range_is_filesystem_safe() {
        let input: Vec<u8> = (0..=255).collect();
        let encoded = encode(&input);
        for &c in encoded.as_bytes() {
            assert!(c != b'/' && c != 0);
        }
        assert_eq!(decode(&encoded).unwrap(), input);
    }

    #[test]
    fn rejects_characters_outside_the_alphabet() {
        assert_eq!(decode("AB/CD"), Err(Error::InvalidCharacter));
        assert_eq!(decode("A B"), Err(Error::InvalidCharacter));
    }
}
