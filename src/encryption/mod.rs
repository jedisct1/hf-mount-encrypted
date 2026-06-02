//! Cryptographic core for client-side encryption.
//!
//! A 32-byte master key (supplied raw or as hex) is loaded once, and HKDF-SHA256
//! derives two domain-separated subkeys: a 16-byte path-encryption key for the
//! HCTR2-AES-128 filename cipher, and a 16-byte content key for AEGIS-128X2. All
//! key material is zeroized on drop.

use std::path::Path;

use ct_codecs::{Decoder, Hex};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub mod content;
pub mod path;
pub mod url;

/// Name of the bucket-root directory under which all encrypted objects are
/// stored. A directory name, not a path.
pub const ENC_ROOT: &str = ".enc";

pub const MASTER_KEY_LEN: usize = 32;
pub const PATH_KEY_LEN: usize = 16;
pub const CONTENT_KEY_LEN: usize = 16;

const HKDF_SALT: &[u8] = b"hf-mount/v1";
const PATH_KEY_INFO: &[u8] = b"hf-mount path-encryption-key hctr2-aes128 v1";
const CONTENT_KEY_INFO: &[u8] = b"hf-mount file-content-key aegis-128x2 v1";

/// Failure modes when loading the master key.
#[derive(Debug)]
pub enum KeyError {
    Io(std::io::Error),
    BadLength(usize),
    InvalidHex,
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::Io(e) => write!(f, "reading encryption key file: {e}"),
            KeyError::BadLength(n) => write!(
                f,
                "encryption key must be 32 raw bytes or 64 hex characters, got {n} bytes"
            ),
            KeyError::InvalidHex => f.write_str("encryption key is not valid hexadecimal"),
        }
    }
}

impl std::error::Error for KeyError {}

/// The 32-byte master secret. Raw bytes never leave this type except as derived
/// subkeys.
#[derive(ZeroizeOnDrop)]
pub struct MasterKey([u8; MASTER_KEY_LEN]);

impl MasterKey {
    /// Load the master key from a file containing either 32 raw bytes or 64 hex
    /// characters (surrounding whitespace is ignored).
    pub fn from_file(path: &Path) -> Result<Self, KeyError> {
        let raw = Zeroizing::new(std::fs::read(path).map_err(KeyError::Io)?);
        Self::from_bytes(&raw)
    }

    /// Parse the master key from in-memory bytes. Exactly 32 bytes are taken
    /// verbatim; otherwise the input is decoded as hexadecimal.
    pub fn from_bytes(raw: &[u8]) -> Result<Self, KeyError> {
        if raw.len() == MASTER_KEY_LEN {
            let mut key = [0u8; MASTER_KEY_LEN];
            key.copy_from_slice(raw);
            return Ok(MasterKey(key));
        }
        let mut key = [0u8; MASTER_KEY_LEN];
        let decoded = Hex::decode(&mut key, raw, Some(b" \t\r\n")).map_err(|_| KeyError::InvalidHex)?;
        if decoded.len() != MASTER_KEY_LEN {
            key.zeroize();
            return Err(KeyError::BadLength(raw.len()));
        }
        Ok(MasterKey(key))
    }

    /// Derive the path and content subkeys with HKDF-SHA256.
    pub fn derive_keys(&self) -> DerivedKeys {
        use hmac_sha256::HKDF;
        let prk = Zeroizing::new(HKDF::extract(HKDF_SALT, &self.0[..]));
        let prk_bytes: &[u8] = &prk[..];

        let mut path_key = [0u8; PATH_KEY_LEN];
        HKDF::expand(&mut path_key, prk_bytes, PATH_KEY_INFO);

        let mut content_key = [0u8; CONTENT_KEY_LEN];
        HKDF::expand(&mut content_key, prk_bytes, CONTENT_KEY_INFO);

        DerivedKeys { path_key, content_key }
    }
}

/// The two subkeys derived from the master key.
#[derive(ZeroizeOnDrop)]
pub struct DerivedKeys {
    pub path_key: [u8; PATH_KEY_LEN],
    pub content_key: [u8; CONTENT_KEY_LEN],
}

/// The runtime encryption handle: a ready-to-use path cipher for filenames plus
/// the content key and algorithm for file bodies. Built once at startup and
/// shared (via `Arc`) by the Hub client and the virtual filesystem.
pub struct Encryptor {
    pub path_cipher: std::sync::Arc<path::PathCipher>,
    pub content_key: Zeroizing<[u8; CONTENT_KEY_LEN]>,
    pub algorithm: u8,
}

impl Encryptor {
    /// Derive the subkeys from `master` and assemble the handle for `algorithm`
    /// (an HFEB algorithm byte, e.g. [`content::ALG_AEGIS_128X2`]).
    pub fn from_master_key(master: &MasterKey, algorithm: u8) -> Self {
        let keys = master.derive_keys();
        Encryptor {
            path_cipher: std::sync::Arc::new(path::PathCipher::new(keys.path_key)),
            content_key: Zeroizing::new(keys.content_key),
            algorithm,
        }
    }
}

/// Map an `--encryption-algorithm` CLI value to its HFEB algorithm byte. Only
/// AEGIS-128X2 is wired up for now; other names are reserved.
pub fn algorithm_byte(name: &str) -> Option<u8> {
    match name {
        "aegis-128x2" => Some(content::ALG_AEGIS_128X2),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ct_codecs::Encoder;

    #[test]
    fn raw_32_bytes_load_verbatim() {
        let raw: Vec<u8> = (0..32).collect();
        let mk = MasterKey::from_bytes(&raw).unwrap();
        assert_eq!(&mk.0[..], raw.as_slice());
    }

    #[test]
    fn hex_with_trailing_newline_loads() {
        let hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n";
        let mk = MasterKey::from_bytes(hex.as_bytes()).unwrap();
        let expect: Vec<u8> = (0..32).collect();
        assert_eq!(&mk.0[..], expect.as_slice());
    }

    #[test]
    fn rejects_short_hex() {
        assert!(matches!(
            MasterKey::from_bytes(b"00112233"),
            Err(KeyError::BadLength(_))
        ));
    }

    #[test]
    fn rejects_non_hex() {
        let bad = "zz".repeat(20);
        assert!(matches!(
            MasterKey::from_bytes(bad.as_bytes()),
            Err(KeyError::InvalidHex)
        ));
    }

    // Pinned against an independent Python HKDF-SHA256 implementation
    // (master key = bytes 0x00..0x1f).
    #[test]
    fn derive_keys_matches_reference_vector() {
        let master: Vec<u8> = (0..32).collect();
        let keys = MasterKey::from_bytes(&master).unwrap().derive_keys();
        assert_eq!(
            Hex::encode_to_string(keys.path_key).unwrap(),
            "0ed68542acb5efef937a12fc2f7ded66"
        );
        assert_eq!(
            Hex::encode_to_string(keys.content_key).unwrap(),
            "17df83bd274d533ca474fd675c5c5387"
        );
        // The two labels must produce different key material.
        assert_ne!(&keys.path_key[..CONTENT_KEY_LEN], &keys.content_key[..]);
    }
}
