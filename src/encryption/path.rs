//! Filename path cipher.
//!
//! Each path component is encrypted independently with HCTR2-AES-128, a
//! length-preserving wide-block cipher, then base91-encoded into a single
//! filesystem-safe component. The plaintext path of the parent directory is
//! the HCTR2 tweak, so every component stays bound to the directory above it.
//!
//! Authentication comes from padding: before encryption a name is prefixed
//! with at least [`MIN_ZERO_PAD`] zero bytes (rounded up to a 32-byte block
//! when that still fits, to hide short-name lengths). Because HCTR2 is a
//! strong tweakable pseudorandom permutation over the whole block, a forged,
//! tampered, or relocated ciphertext decrypts to uniformly random bytes, so
//! the constant-time check that the first 12 bytes are zero passes with
//! probability 2^-96. Decryption strips leading NULs, which recovers both
//! the padded and exact-length cases without recording which one was used.
//!
//! The zero bytes go at the front rather than the tail because HCTR2 feeds the
//! first plaintext block straight through the AES block cipher (the
//! `E_K(MM)` step), while the rest of the message is masked by the CTR-mode
//! keystream. Anchoring the redundancy in that first block binds it through the
//! block cipher itself, which gives the construction stronger key-commitment
//! security than tail padding does.

use hctr2_rs::Hctr2_128;
use zeroize::ZeroizeOnDrop;

use super::PATH_KEY_LEN;
use crate::base91;

const PAD_BLOCK: usize = 32;
const MIN_ZERO_PAD: usize = 12;
const NAME_MAX: usize = 255;

// Every ciphertext is at least one pad block long, which must cover both the
// zero-pad MAC and HCTR2's minimum input.
const _: () = assert!(PAD_BLOCK >= MIN_ZERO_PAD && PAD_BLOCK >= Hctr2_128::MIN_INPUT_LENGTH);

/// Errors from name encryption. Decryption failures are reported as `None`
/// instead, so an undecryptable entry can be silently skipped.
#[derive(Debug, PartialEq, Eq)]
pub enum PathError {
    /// Empty component, or one containing NUL or '/'.
    InvalidName,
    /// The encrypted form would exceed `NAME_MAX`.
    NameTooLong,
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::InvalidName => f.write_str("invalid file name"),
            PathError::NameTooLong => f.write_str("file name too long once encrypted"),
        }
    }
}

impl std::error::Error for PathError {}

/// Encrypts and decrypts path components with the derived path key.
#[derive(ZeroizeOnDrop)]
pub struct PathCipher {
    path_key: [u8; PATH_KEY_LEN],
}

impl PathCipher {
    pub fn new(path_key: [u8; PATH_KEY_LEN]) -> Self {
        Self { path_key }
    }

    fn hctr2(&self) -> Hctr2_128 {
        Hctr2_128::new(&self.path_key)
    }

    /// Verify that `name` will fit in `NAME_MAX` once encrypted. Cheap enough to
    /// run before any local inode mutation.
    pub fn check_name_len(&self, name: &str) -> Result<(), PathError> {
        validate_name(name)?;
        encrypt_input_len(name.len()).map(|_| ()).ok_or(PathError::NameTooLong)
    }

    /// Encrypt a full plaintext path (bucket-relative, '/'-separated) into the
    /// stored path. Each component is bound to the plaintext path above it.
    pub fn encrypt_path(&self, plaintext_path: &str) -> Result<String, PathError> {
        if plaintext_path.is_empty() {
            return Ok(String::new());
        }
        let mut parent = String::new();
        let mut encoded: Vec<String> = Vec::new();
        for comp in plaintext_path.split('/') {
            encoded.push(self.encrypt_component(comp, &parent)?);
            push_component(&mut parent, comp);
        }
        Ok(encoded.join("/"))
    }

    /// Decrypt a stored path back to plaintext, or `None` if any component fails
    /// to decode, authenticate, or strip cleanly (the caller drops such entries).
    pub fn decrypt_path(&self, stored_path: &str) -> Option<String> {
        if stored_path.is_empty() {
            return Some(String::new());
        }
        let mut parent = String::new();
        let mut plain: Vec<String> = Vec::new();
        for comp in stored_path.split('/') {
            let name = self.decrypt_component(comp, &parent)?;
            push_component(&mut parent, &name);
            plain.push(name);
        }
        Some(plain.join("/"))
    }

    fn encrypt_component(&self, name: &str, parent: &str) -> Result<String, PathError> {
        validate_name(name)?;
        let input_len = encrypt_input_len(name.len()).ok_or(PathError::NameTooLong)?;
        let mut buf = vec![0u8; input_len];
        buf[input_len - name.len()..].copy_from_slice(name.as_bytes());
        let mut ciphertext = vec![0u8; input_len];
        self.hctr2()
            .encrypt(&buf, parent.as_bytes(), &mut ciphertext)
            .expect("input is at least one pad block, above the HCTR2 minimum");
        Ok(base91::encode(&ciphertext))
    }

    fn decrypt_component(&self, stored: &str, parent: &str) -> Option<String> {
        let ciphertext = base91::decode(stored).ok()?;
        if ciphertext.len() < PAD_BLOCK {
            return None;
        }
        let mut plaintext = vec![0u8; ciphertext.len()];
        self.hctr2()
            .decrypt(&ciphertext, parent.as_bytes(), &mut plaintext)
            .ok()?;
        // The zero padding is the MAC: accumulate the first MIN_ZERO_PAD bytes
        // branch-free and compare once, so the check runs in constant time.
        let pad = &plaintext[..MIN_ZERO_PAD];
        let acc = pad.iter().fold(0u8, |acc, &b| acc | b);
        if acc != 0 {
            return None;
        }
        let stripped = strip_leading_nuls(&plaintext);
        let name = std::str::from_utf8(stripped).ok()?;
        validate_name(name).ok()?;
        Some(name.to_owned())
    }
}

fn validate_name(name: &str) -> Result<(), PathError> {
    if name.is_empty() || name.as_bytes().contains(&0) || name.contains('/') {
        return Err(PathError::InvalidName);
    }
    Ok(())
}

/// The plaintext length to encrypt for a `name_len`-byte name: the name plus
/// mandatory zero padding, rounded up to a 32-byte multiple when that still
/// fits, otherwise the exact `name_len + MIN_ZERO_PAD` so long names use the
/// full `NAME_MAX`, or `None` when even that is too long.
fn encrypt_input_len(name_len: usize) -> Option<usize> {
    let min_len = name_len + MIN_ZERO_PAD;
    let padded = min_len.div_ceil(PAD_BLOCK) * PAD_BLOCK;
    if encoded_fits(padded) {
        Some(padded)
    } else if encoded_fits(min_len) {
        Some(min_len)
    } else {
        None
    }
}

fn encoded_fits(input_len: usize) -> bool {
    base91::encoded_len_upper_bound(input_len) <= NAME_MAX
}

fn strip_leading_nuls(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < bytes.len() && bytes[start] == 0 {
        start += 1;
    }
    &bytes[start..]
}

fn push_component(parent: &mut String, comp: &str) {
    if !parent.is_empty() {
        parent.push('/');
    }
    parent.push_str(comp);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher() -> PathCipher {
        let mut key = [0u8; PATH_KEY_LEN];
        for (i, b) in key.iter_mut().enumerate() {
            *b = i as u8;
        }
        PathCipher::new(key)
    }

    #[test]
    fn round_trips_paths() {
        let c = cipher();
        for path in ["a", "config.json", "checkpoints/run-7/model.safetensors", "x/y/z"] {
            let enc = c.encrypt_path(path).unwrap();
            assert!(!enc.contains('\0') && !enc.contains("//"));
            assert_eq!(c.decrypt_path(&enc).as_deref(), Some(path));
        }
    }

    #[test]
    fn deterministic() {
        let c = cipher();
        assert_eq!(c.encrypt_path("dir/file").unwrap(), c.encrypt_path("dir/file").unwrap());
    }

    #[test]
    fn ad_binding_rejects_relocated_component() {
        let c = cipher();
        let under_a = c.encrypt_path("a/secret").unwrap();
        let leaf = under_a.split('/').nth(1).unwrap().to_string();
        let other_parent = c.encrypt_path("b").unwrap();
        let forged = format!("{other_parent}/{leaf}");
        assert_eq!(c.decrypt_path(&forged), None);
    }

    #[test]
    fn padding_buckets_short_names() {
        let c = cipher();
        let one = c.encrypt_path("a").unwrap();
        let twenty = c.encrypt_path(&"x".repeat(20)).unwrap();
        assert_eq!(
            one.len(),
            twenty.len(),
            "names in the same 32-byte bucket must share a stored length"
        );
        let twenty_one = c.encrypt_path(&"y".repeat(21)).unwrap();
        assert_ne!(one.len(), twenty_one.len());
    }

    #[test]
    fn exact_block_multiple_round_trips() {
        let c = cipher();
        let name = "z".repeat(32);
        let enc = c.encrypt_path(&name).unwrap();
        assert_eq!(c.decrypt_path(&enc).as_deref(), Some(name.as_str()));
    }

    #[test]
    fn ciphertext_is_length_preserving() {
        let c = cipher();
        for len in [1, 20, 21, 32, 100] {
            let name = "m".repeat(len);
            let enc = c.encrypt_path(&name).unwrap();
            let decoded = base91::decode(&enc).unwrap();
            assert_eq!(decoded.len(), encrypt_input_len(len).unwrap());
        }
    }

    #[test]
    fn long_names_skip_padding_and_round_trip() {
        let c = cipher();
        let name = "n".repeat(194);
        let enc = c.encrypt_path(&name).unwrap();
        assert!(enc.len() <= NAME_MAX);
        let decoded = base91::decode(&enc).unwrap();
        assert_eq!(decoded.len(), 194 + MIN_ZERO_PAD, "exact-length branch, no bucketing");
        assert_eq!(c.decrypt_path(&enc).as_deref(), Some(name.as_str()));
    }

    #[test]
    fn name_length_limits() {
        let c = cipher();
        assert!(c.check_name_len(&"a".repeat(194)).is_ok());
        assert_eq!(c.check_name_len(&"a".repeat(195)), Err(PathError::NameTooLong));
        // The longest accepted name still encodes within NAME_MAX.
        assert!(c.encrypt_path(&"a".repeat(194)).unwrap().len() <= NAME_MAX);
    }

    #[test]
    fn rejects_invalid_names() {
        let c = cipher();
        assert_eq!(c.check_name_len(""), Err(PathError::InvalidName));
        assert_eq!(c.check_name_len("a\0b"), Err(PathError::InvalidName));
        assert_eq!(c.check_name_len("a/b"), Err(PathError::InvalidName));
    }

    #[test]
    fn drops_undecryptable_components() {
        let c = cipher();
        // Authentic ciphertext shape but random content: the zero-pad check fails.
        let garbage = base91::encode(&[0xa7u8; PAD_BLOCK]);
        assert_eq!(c.decrypt_path(&garbage), None);
    }

    #[test]
    fn rejects_short_or_garbage_inputs() {
        let c = cipher();
        // Decoded length below one pad block is rejected before decryption.
        let short = base91::encode(&[0x55u8; PAD_BLOCK - 1]);
        assert_eq!(c.decrypt_path(&short), None);
        // Invalid base91 is rejected by the decoder.
        assert_eq!(c.decrypt_path("not valid base91 \u{20ac}"), None);
        assert_eq!(c.decrypt_path("'"), None);
    }

    #[test]
    fn tampering_any_ciphertext_byte_is_detected() {
        let c = cipher();
        let enc = c.encrypt_path("important.txt").unwrap();
        let ciphertext = base91::decode(&enc).unwrap();
        for i in 0..ciphertext.len() {
            let mut tampered = ciphertext.clone();
            tampered[i] ^= 1;
            let stored = base91::encode(&tampered);
            assert_eq!(c.decrypt_path(&stored), None, "flipped byte {i} must not decrypt");
        }
    }

    /// Hand-encrypt one padded top-level component, bypassing
    /// encrypt_component's name validation, so decrypt-side rejection of names
    /// that could never have been produced legitimately can be exercised.
    fn encrypt_raw(c: &PathCipher, name: &[u8]) -> String {
        let mut buf = vec![0u8; PAD_BLOCK];
        buf[PAD_BLOCK - name.len()..].copy_from_slice(name);
        let mut ciphertext = vec![0u8; PAD_BLOCK];
        c.hctr2().encrypt(&buf, b"", &mut ciphertext).unwrap();
        base91::encode(&ciphertext)
    }

    #[test]
    fn interior_nul_is_rejected() {
        let c = cipher();
        assert_eq!(c.decrypt_path(&encrypt_raw(&c, b"ab\0cd")), None);
    }

    #[test]
    fn decrypted_slash_is_rejected() {
        // A component that decrypts to a name containing '/' would alter the
        // plaintext path structure; decrypt must drop it even though the
        // padding authenticates.
        let c = cipher();
        assert_eq!(c.decrypt_path(&encrypt_raw(&c, b"a/b")), None);
    }
}
