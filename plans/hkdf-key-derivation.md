# Plan: HKDF Key Derivation

## Problem

The encryption key from the user's key file is used directly as the AEGIS key.
If two buckets share the same key file, they get the same encryption key -- a file
encrypted in bucket A can be decrypted in bucket B. There is no domain separation.

Additionally, the filesystem supports mixed-algorithm operation: reads use the
algorithm recorded in per-file metadata (`entry.file_algorithm`), not the CLI
default. Writes to existing encrypted files preserve the file's recorded algorithm.
Any key derivation scheme must account for this -- a single derived key computed
once at setup time is not sufficient.

## Goal

Derive per-(source, algorithm) encryption keys using HKDF-SHA256 (RFC 5869):
- Each source (bucket or repo) gets unique derived keys even from the same master key file.
- Derivation is bound to the AEGIS algorithm scheme, so a 128-bit file and a 256-bit file in the same bucket use different keys.
- The master key never touches AEGIS directly.
- Derivation happens at encrypt/decrypt call sites, not once at setup, to support per-file algorithm metadata.

## Design

### HKDF parameters

Following Soatok's guidance on correct HKDF usage (https://soatok.blog/2021/11/17/understanding-hkdf/):

| Parameter | Value |
|-----------|-------|
| **IKM** | 256-bit (32-byte) master key from the user's key file -- always 32 bytes regardless of algorithm |
| **Salt** | Fixed application-level constant: `b"hf-mount-v1"` (not randomized per call -- the security proof requires a single salt value) |
| **Info** | Structured context string (see below) |
| **Output length** | `algorithm.key_len()` bytes (16 or 32, derived from the same 32-byte master key) |

The PRK from HKDF-Extract depends only on (salt, IKM), both of which are fixed
for a given mount session. We compute it once at setup and cache it in
`EncryptionConfig`. Each encrypt/decrypt call then does only the cheap
HKDF-Expand step with the file's effective algorithm.

### Info string structure

To avoid canonicalization attacks (Soatok's warning about multi-part MAC inputs),
length-prefix every variable-length component. All lengths are big-endian u16:

```
"hf-mount-derive-v1" || len(source_context) || source_context || len(algorithm_scheme) || algorithm_scheme
```

Where `source_context` is a revision-free canonical source identifier:
- Buckets: `"bucket/<bucket_id>"` (e.g. `"bucket/user/my-bucket"`)
- Repos: `"<repo_type>/<repo_id>"` (e.g. `"model/user/my-model"`)

Revision is deliberately excluded. Encryption metadata does not record the
revision string used at encryption time, and the same file can be accessed
through different revision selectors (`main`, a commit SHA, a tag) that all
resolve to the same object. Including revision would make ciphertext unreadable
across mounts of the same repo with different revision strings.

We do NOT reuse `SourceKind::Display` (`hub_api.rs:95`) because it includes
revision. Instead, add a dedicated `EncryptionConfig::source_context_for`
helper (or a `SourceKind::encryption_context()` method) that formats
`"<kind>/<id>"` only. This prevents cross-namespace collisions (bucket
`user/foo` vs model repo `user/foo`) while keeping keys stable across
revisions.

The subfolder / path prefix is deliberately excluded from the info string.
The same underlying source object should produce the same keys regardless of
whether it's mounted at root or from a subfolder.

The algorithm scheme is included so that a 128-bit file and a 256-bit file
within the same source derive different keys.

Examples:

```
# Bucket user/my-bucket, aegis-128x2-raf:
hf-mount-derive-v1 | 00 12 | bucket/user/my-bucket | 00 0F | aegis-128x2-raf

# Model repo user/my-model (revision excluded), aegis-256-raf:
hf-mount-derive-v1 | 00 14 | model/user/my-model   | 00 0D | aegis-256-raf
```

### Source identity: use resolved, not raw

For repos, `HubApiClient::from_source` (`hub_api.rs:305`) resolves aliases
to a canonical `repo_id` from the API. The HKDF context must use this resolved
identity (available via `hub_client.source()`), not the raw CLI string from
`setup.rs:250`. This ensures the same repo mounted via alias or canonical name
derives the same key.

For buckets, the `bucket_id` is already canonical after `split_path_prefix()`.

### `SourceKind::encryption_context()` method

Add a method on `SourceKind` that produces the revision-free context string:

```rust
impl SourceKind {
    pub fn encryption_context(&self) -> String {
        match self {
            Self::Bucket { bucket_id } => format!("bucket/{bucket_id}"),
            Self::Repo { repo_id, repo_type, .. } => format!("{repo_type}/{repo_id}"),
        }
    }
}
```

This is the only way source context enters HKDF. Do not use `Display`.

### Crate

Use `hmac_sha256` (no_std-friendly, minimal deps):

```rust
use hmac_sha256::HKDF;

// Once at setup (cached in EncryptionConfig):
let prk = HKDF::extract(b"hf-mount-v1", &master_key);

// At each encrypt/decrypt call site:
let mut derived = vec![0u8; algorithm.key_len()];
HKDF::expand(&mut derived, &prk, &info);
```

## Data model changes

### `EncryptionConfig` stores master PRK, not a derived key

```rust
pub struct EncryptionConfig {
    pub prk: [u8; 32],           // HKDF-Extract output (cached)
    pub source_context: String,  // canonical SourceKind display string
    pub algorithm: Algorithm,    // CLI default for new files
    pub chunk_size: u32,
}
```

The raw master key is dropped after Extract (it is a stack-allocated `[u8; 32]`
that goes out of scope). We do not add a `zeroize` dependency -- the PRK is
equally sensitive and lives for the entire mount session, so zeroizing only the
IKM would be security theater. If we later want defense-in-depth against memory
scraping, that would be a separate change covering PRK and all derived keys too.

The `prk` plus `source_context` are sufficient to derive any per-algorithm key
on demand.

### New method: `EncryptionConfig::derive_key(&self, algorithm: Algorithm) -> Vec<u8>`

Performs HKDF-Expand using the cached PRK and builds the info string from
`self.source_context` and the given algorithm. This is the only place key
material is produced.

### `EncryptionConfig::key_for_default(&self) -> Vec<u8>` convenience

Calls `derive_key(self.algorithm)` for new file creation. Avoids repeating
the pattern at write call sites that use the mount default.

## Implementation steps

### Step 1: Add `hmac-sha256` dependency

In `Cargo.toml`, add as an optional dependency alongside `aegis` (`Cargo.toml:35`):

```toml
hmac-sha256 = { version = "1", optional = true }
```

And wire it into the `encrypt` feature (`Cargo.toml:41`):

```toml
encrypt = ["dep:aegis", "dep:hmac-sha256"]
```

### Step 2: Simplify `load_key` -> `load_master_key`

Rename and simplify: always expects exactly 32 bytes (raw or base64-encoded).
No longer takes an `Algorithm` parameter. Signature:

```rust
pub fn load_master_key(path: &Path) -> Result<[u8; 32]>
```

### Step 3: Add HKDF functions to `src/crypto.rs`

```rust
const HKDF_SALT: &[u8] = b"hf-mount-v1";
const INFO_PREFIX: &[u8] = b"hf-mount-derive-v1";

/// One-time Extract from master key. Result is cached in EncryptionConfig.
pub fn extract_prk(master_key: &[u8; 32]) -> [u8; 32] {
    hmac_sha256::HKDF::extract(HKDF_SALT, master_key)
}

/// Build the HKDF info string with length-prefixed fields.
fn build_info(source_context: &str, algorithm: Algorithm) -> Vec<u8> {
    let scheme = algorithm.scheme();
    let mut info = Vec::with_capacity(
        INFO_PREFIX.len() + 2 + source_context.len() + 2 + scheme.len()
    );
    info.extend_from_slice(INFO_PREFIX);
    info.extend_from_slice(&(source_context.len() as u16).to_be_bytes());
    info.extend_from_slice(source_context.as_bytes());
    info.extend_from_slice(&(scheme.len() as u16).to_be_bytes());
    info.extend_from_slice(scheme.as_bytes());
    info
}

/// Derive a per-(source, algorithm) key via HKDF-Expand.
pub fn derive_key(prk: &[u8; 32], source_context: &str, algorithm: Algorithm) -> Vec<u8> {
    let info = build_info(source_context, algorithm);
    let mut derived = vec![0u8; algorithm.key_len()];
    hmac_sha256::HKDF::expand(&mut derived, prk, &info);
    derived
}
```

Add `derive_key` method on `EncryptionConfig`:

```rust
impl EncryptionConfig {
    pub fn derive_key(&self, algorithm: Algorithm) -> Vec<u8> {
        derive_key(&self.prk, &self.source_context, algorithm)
    }
}
```

### Step 4: Update `EncryptionConfig` construction in `src/setup.rs`

Must happen **after** `HubApiClient::from_source` so we have the resolved
source identity. Current code at `setup.rs:330` runs before `hub_client` is
constructed at `setup.rs:269`. Reorder:

```rust
// After hub_client is built (line 279):
let source_context = hub_client.source().encryption_context();

// Then build encryption config:
let master_key = crate::crypto::load_master_key(key_path)?;
let prk = crate::crypto::extract_prk(&master_key);
// master_key goes out of scope here
Some(crate::crypto::EncryptionConfig {
    prk,
    source_context,
    algorithm,
    chunk_size: 65536,
})
```

### Step 5: Update decrypt call sites

Two decrypt sites in `src/virtual_fs/mod.rs` currently pass `&config.key`:

**`decrypt_and_open` (mod.rs:1243):**
```rust
// Before:
crate::crypto::decrypt_file(ciphertext_path, &plaintext_path, &config.key, &info)
// After:
let key = config.derive_key(algorithm);
crate::crypto::decrypt_file(ciphertext_path, &plaintext_path, &key, &info)
```

**`download_to_staging` (mod.rs:1292):**
```rust
// Before:
crate::crypto::decrypt_file(&ct_path, staging_path, &config.key, &info)
// After:
let key = config.derive_key(algorithm);
crate::crypto::decrypt_file(&ct_path, staging_path, &key, &info)
```

Both already have the file's effective `algorithm` from `fe.file_algorithm` /
`entry.file_algorithm`, so the derived key matches the file's actual algorithm.

### Step 6: Change `encrypt_file` and `create_raf` to take plain arguments

`encrypt_file` (`crypto.rs:315`) currently takes `config: &EncryptionConfig`.
Since `EncryptionConfig` no longer carries a derived key, change the signature
to take the pieces directly -- no throwaway struct:

```rust
pub fn encrypt_file(
    src: &Path,
    dst: &Path,
    key: &[u8],
    algorithm: Algorithm,
    chunk_size: u32,
) -> Result<()>
```

Similarly update `create_raf` (`crypto.rs:261`) to take `(key, algorithm, chunk_size)`
instead of `&EncryptionConfig`. `decrypt_file` and `open_raf` already take
`key: &[u8]` -- no change needed there.

### Step 7: Update encrypt call site in flush

In `flush.rs:315`, resolve the file's effective algorithm, derive the key,
and pass both to the updated `encrypt_file`:

```rust
let effective_algorithm = inodes.read().expect("inodes poisoned")
    .get(ino)
    .and_then(|e| e.file_algorithm)
    .unwrap_or(config.algorithm);
let derived_key = config.derive_key(effective_algorithm);
crate::crypto::encrypt_file(
    &item.staging_path, &p, &derived_key, effective_algorithm, config.chunk_size,
)
```

This mirrors the existing logic in `build_flush_content_type` (`flush.rs:268`)
which already reads `file_algorithm` with the same fallback. Both now use the
same effective algorithm for the same flush.

### Step 8: Update `open_raf` callers

`open_raf` (`crypto.rs:286`) already takes `key: &[u8]` -- callers just pass
the derived key instead of `config.key`. Already handled by steps 5 and 7.

### Step 9: Add tests

In `src/crypto.rs` tests:

1. **Determinism**: same (master_key, source_context, algorithm) always produces the same derived key.
2. **Source separation**: different source_contexts produce different keys.
3. **Namespace separation**: `"bucket/user/foo"` vs `"model/user/foo"` derive different keys -- the kind prefix prevents cross-namespace collisions.
4. **Revision stability**: `encryption_context()` for the same repo with different revision strings produces the same context (revision excluded).
5. **Algorithm separation**: same master key + same source but different algorithm produces different keys.
6. **Length correctness**: output is 16 bytes for 128-bit algorithms, 32 bytes for 256-bit.
7. **Non-identity**: derived key != master key bytes.
8. **Master key always 32 bytes**: `load_master_key` rejects files that aren't exactly 32 bytes.

In `src/virtual_fs/tests.rs` or integration tests:

9. **Mixed-algorithm read**: a file encrypted with algorithm A can be read when the mount default is algorithm B, because reads use `file_algorithm` from metadata and derive the correct per-algorithm key.
10. **Rewrite preserves algorithm**: rewriting an existing encrypted file whose `file_algorithm` differs from mount default still encrypts with the file's algorithm and the matching derived key.
11. **End-to-end roundtrip**: encrypt with derived key, decrypt with same derived key from same (prk, source_context, algorithm).

### Step 10: Migrate existing tests to new API

Existing tests construct the old `EncryptionConfig { key, algorithm, chunk_size }`
and pass it to `encrypt_file` / `create_raf`. These all need updating:

**`src/crypto.rs:378`** -- `test_config(algorithm)` helper builds the old struct.
Replace with a helper that calls `extract_prk` + `derive_key` to produce a
derived key, then passes `(key, algorithm, chunk_size)` to the updated
`encrypt_file` / `create_raf` signatures. Tests that only exercise RAF or
file-level encrypt/decrypt don't need a real source context -- use a fixed
test string like `"bucket/test"`.

**`src/virtual_fs/tests.rs:2550`** -- `test_config()` in the `encrypt` module
builds the old struct for VFS-level tests. Update to the new `EncryptionConfig`
shape (`prk`, `source_context`, `algorithm`, `chunk_size`). The `vfs_encrypted`
helper at line 2558 passes this into `TestOpts` -- it should construct the PRK
from a test master key and use a fixed source context.

### Step 11: Update key generation docs

Users should always generate a 32-byte key:
```
head -c 32 /dev/urandom > key.bin
```
Regardless of AEGIS variant. The old per-algorithm key size guidance no longer applies.

### Step 12: Verify and format

```bash
cargo +nightly fmt
cargo clippy --features fuse,nfs -- -D warnings
cargo test --lib --features fuse,nfs
```

## Backward compatibility

**This is a breaking change.** Files encrypted before this change used the raw
master key. After this change, the derived key will be different, so old
ciphertext becomes unreadable. Per project instructions, backward compatibility
is not a concern -- document in the commit message.

## Non-goals

- Password-based key derivation (Argon2/scrypt) -- the input is already a
  high-entropy key file, not a password. HKDF is the correct primitive here.
- Key versioning / rotation -- out of scope.
- Including subfolder in HKDF context -- deliberately excluded so the same
  source mounted at different subfolders produces the same keys.
