# Plan: Client-side encryption with AEGIS-128X2

Optional, transparent encryption/decryption of file contents before they reach
Hugging Face. Uses the `aegis` crate's RAF (Random Access File) layer with the
AEGIS-128X2 cipher.

## Design overview

When encryption is enabled (`--encrypt --encryption-key-file <path>`), every file
written to the mount is encrypted locally before upload, and every file read from
the mount is decrypted transparently after download. The remote side only ever sees
ciphertext. Metadata (filenames, directory structure) is NOT encrypted — only file
content. Plaintext sizes are preserved in Hub metadata so `stat()` always reports
correct values.

### Key properties

- **Cipher**: AEGIS-128X2 (128-bit key, hardware-accelerated on AES-NI/ARM)
- **Key**: 16-byte symmetric key, read from a file at mount time
- **Chunk size**: 64 KiB (aegis RAF default), independently authenticated
- **No Merkle tree**: per the requirements
- **Stored size**: larger than plaintext (RAF header + per-chunk AEAD tags)
- **Reported size**: always plaintext size (stored in Hub `content_type` metadata)

## Remote metadata format

Encrypted files must be identifiable on the remote so that:
1. A mount with the key can detect which files need decryption
2. A mount without the key can refuse to serve garbage instead of silently returning ciphertext
3. Mixed plaintext/encrypted content in the same bucket is supported
4. Plaintext file size is available without downloading the file

Use `content_type` on `BatchOp::AddFile` — the field already exists and is
committed with each file (hub_api.rs:119, flush.rs:317, mod.rs:1785).

Format:

```
application/vnd.hf-mount+enc; scheme=aegis-128x2-raf; chunk=65536; size=<plaintext_bytes>; stored=<ciphertext_bytes>
```

Fields:
- `scheme=aegis-128x2-raf` — identifies the cipher and container format
- `chunk=65536` — RAF chunk size
- `size=<N>` — plaintext file size in bytes (used for `stat()`, EOF checks)
- `stored=<N>` — ciphertext file size in bytes (used for xet downloads via
  `XetFileInfo::new()` and `download_to_file()`)

Parsing rules:
- If `content_type` starts with `application/vnd.hf-mount+enc`, the file is encrypted
- Unknown or malformed encryption metadata is an error — never fall back to treating
  ciphertext as plaintext
- Files without this marker are plaintext (no decryption attempted)

This requires `TreeEntry` (hub_api.rs:128) to also parse `content_type` from the
Hub tree API response, so encryption status is known at directory listing time.

### Implementation checkpoint: content_type in tree responses

Adding `content_type` to `TreeEntry` assumes the Hub tree API returns it. This
is unverified. The bucket tree API (hub_api.rs:532) deserializes directly into
`TreeEntry` — if the JSON includes `content_type`, serde picks it up. The repo
tree API (hub_api.rs:594) uses `RepoTreeEntry` and would also need the field.

**Verification step**: upload a test file with a custom content_type via
`BatchOp::AddFile`, list the tree, confirm the field comes back. Blocking
checkpoint before the read path can rely on it.

**Fallback if tree responses lack content_type**: do a per-file HEAD request
in `open_readonly()` to fetch content_type before deciding the read path.
One extra round-trip per encrypted file open, but doesn't block on an API change.
Cache the result in the inode so subsequent opens skip the probe.

## Scope: which file types are encrypted

Both xet-backed files and plain HTTP/LFS files need encryption support because
both paths exist in the codebase:

- **xet-backed files**: downloaded via `XetOps` (`download_stream_boxed`, `download_to_file`)
- **HTTP/LFS files**: downloaded via `hub_client.download_file_http()` at mod.rs:1122

Wrapping only `XetOps` is not sufficient. The plan addresses both paths.

## Inode and size handling

Correct size tracking is critical. Today, `apply_commit()` (inode.rs:88) writes
`file_info.file_size()` directly into `entry.size`. With encryption, `file_info`
reports the ciphertext size (what xet-core sees), which would corrupt inode state.

The fix has two parts:

**On write (commit)**: every commit site that calls `apply_commit` must pass the
plaintext size, not the ciphertext size. There are three sites:
- `flush_one()` at flush.rs:303 — advanced writes
- `flush_batch()` at flush.rs:329 — batched advanced writes
- `streaming_commit()` at mod.rs:1800 — simple streaming writes

When encryption is active, the plaintext size is already known (it's what the user
wrote). Store it alongside the inode before commit and pass it to `apply_commit`
instead of `file_info.file_size()`.

**On read (directory listing / lookup)**: when populating the inode table from Hub
tree responses, parse `content_type` to detect encrypted files and extract the
plaintext size. Use the plaintext size for `entry.size`. The ciphertext size from
the Hub `size` field is only needed internally for download bookkeeping.

This means `stat()` is always correct, even before `open()`.

## New module: `src/crypto.rs`

All aegis-specific code lives here. ~300 lines.

```rust
use aegis::raf::{Raf, RafBuilder, FileIo, Aegis128X2};

pub struct EncryptionConfig {
    pub key: [u8; 16],
    pub chunk_size: u32,  // 65536
}

/// Parsed encryption metadata from Hub content_type.
pub struct EncryptedFileInfo {
    pub plaintext_size: u64,
    pub chunk_size: u32,
}
```

Public helpers:

1. **`parse_content_type(ct: &str) -> Result<Option<EncryptedFileInfo>, Error>`**
   Parse `application/vnd.hf-mount+enc; ...` into structured metadata.
   Returns `Ok(None)` for non-encrypted files, `Err` for malformed encrypted metadata.

2. **`format_content_type(plaintext_size: u64, chunk_size: u32) -> String`**
   Build the content_type string for `BatchOp::AddFile`.

3. **`encrypt_file(plaintext_path: &Path, ciphertext_path: &Path, key: &[u8; 16]) -> Result<u64>`**
   Encrypt a plaintext file into a RAF ciphertext file. Returns ciphertext size.

4. **`decrypt_file(ciphertext_path: &Path, plaintext_path: &Path, key: &[u8; 16]) -> Result<u64>`**
   Decrypt a RAF ciphertext file to plaintext. Returns plaintext size.

5. **`decrypt_range(ciphertext_path: &Path, key: &[u8; 16], offset: u64, len: u64) -> Result<Vec<u8>>`**
   Decrypt a byte range from a RAF file without materializing the whole plaintext.

6. **`load_key(path: &Path) -> Result<[u8; 16]>`**
   Read and validate a key file (16 raw bytes, or 24 base64 chars).

## Read path changes

### xet-backed files (mod.rs:1119)

Today: `open_lazy()` creates a `PrefetchState` and streams data on demand via
`fetch_data()` → `download_stream_boxed()`.

With encryption: the stream delivers ciphertext, but RAF needs random access to
decrypt (header + per-chunk nonces). Two approaches, in order of implementation:

**v1 — materialize-then-serve** (simple, correct):
When `open_readonly()` detects an encrypted xet-backed file (via parsed
`content_type` on the inode), take a different path:
1. Download ciphertext to a temp cache file via `xet_sessions.download_to_file()`
2. Decrypt via `crypto::decrypt_file()` to a plaintext cache file
3. Serve reads from the plaintext file via `open_local_readonly()`

This sacrifices lazy streaming for encrypted files in v1 — acceptable because
correctness comes first. The change is in `open_readonly()`, adding a branch
before the existing `open_lazy()` call.

**v2 — lazy encrypted reads** (future optimization):
Implement a `RafIo` adapter backed by on-demand xet range downloads, translating
plaintext offsets to ciphertext chunk offsets. Each RAF chunk is independently
decryptable, so only the needed ciphertext chunks are fetched. This restores
lazy access for encrypted files but requires understanding RAF's internal
chunk-to-offset mapping. Not in scope for v1.

### HTTP/LFS files (mod.rs:1122)

Today: `download_file_http()` writes the file to a staging cache path, then
`open_local_readonly()` serves reads.

With encryption: after `download_file_http()`, if the inode has encryption
metadata, treat the downloaded file as RAF ciphertext:
1. `crypto::decrypt_file(downloaded_path, plaintext_cache_path, key)`
2. `open_local_readonly(ino, plaintext_cache_path)`

This is a small addition to the existing HTTP download branch in `open_readonly()`.

## Write path changes

### Simple streaming writes

When encryption is enabled, force `advanced_writes = true` in setup. Rationale:
simple streaming mode pipes data append-only into CAS via `SingleFileCleaner`,
which has no concept of file-level encryption. RAF is file-oriented (needs
random access to write chunk headers). Rather than building an encrypted streaming
adapter that buffers the entire file anyway, just use the advanced-writes path
which already supports random I/O via staging files.

Log a message at mount time: "Encryption enabled, using advanced writes mode."

### Advanced writes

**Staging files remain plaintext** on disk. The user's local disk is trusted —
encryption protects data at rest on the remote. Random `pwrite()` / `pread()`
continue to work on plaintext staging files without modification.

**Encryption happens at flush time**, in `flush_one()` and `flush_batch()`:

1. Before calling `xet_sessions.upload_files(&staging_paths)`, encrypt each
   staging file to a temp RAF file via `crypto::encrypt_file()`
2. Upload the encrypted temp files instead
3. Set `content_type` on `BatchOp::AddFile` via `crypto::format_content_type()`
   with the original plaintext size
4. Pass the plaintext size (not `file_info.file_size()`) to `apply_commit()`
5. Clean up temp encrypted files

Code changes in `flush_one()` and `flush_batch()`:
- Before `xet_sessions.upload_files()`: if encryption config is present,
  encrypt staging files to `{staging_path}.enc` temp files
- Replace staging paths with encrypted paths for the upload call
- Set `content_type` on the `BatchOp::AddFile` to the encryption marker
- In `apply_commit()` calls, use the plaintext size from inode state

This requires threading `Option<EncryptionConfig>` into `FlushManager`.

### Streaming commit (mod.rs:1781)

This path is effectively disabled when encryption is on (forced advanced writes),
but for completeness: if it were ever reached, the same pattern applies — encrypt
before upload, set content_type, pass plaintext size to `apply_commit()`.

## Configuration and CLI

**`src/setup.rs`** — add to `MountOptions`:
```rust
#[cfg(feature = "encrypt")]
#[arg(long, default_value_t = false)]
pub encrypt: bool,

#[cfg(feature = "encrypt")]
#[arg(long)]
pub encryption_key_file: Option<PathBuf>,
```

**`src/virtual_fs/mod.rs`** — add to `VfsConfig`:
```rust
#[cfg(feature = "encrypt")]
pub encryption_config: Option<crypto::EncryptionConfig>,
```

**`VirtualFs`** — add field:
```rust
#[cfg(feature = "encrypt")]
encryption_config: Option<crypto::EncryptionConfig>,
```

**Setup logic**:
- Load key via `crypto::load_key()`
- If `--encrypt`, set `advanced_writes = true` and log why
- Pass `EncryptionConfig` through to VirtualFs and FlushManager

## Hub API changes

**`TreeEntry`** (hub_api.rs:128) — add field:
```rust
#[serde(default)]
pub content_type: Option<String>,
```

**`HeadFileInfo`** — if used for individual file metadata, also capture content_type.

**Inode population** — wherever `TreeEntry` is converted to `InodeEntry`, if
`content_type` parses as encrypted metadata, use the embedded plaintext size
for `entry.size` and store encryption status on the inode.

**`InodeEntry`** — add:
```rust
#[cfg(feature = "encrypt")]
pub encrypted: bool,
#[cfg(feature = "encrypt")]
pub remote_size: Option<u64>,
```

`encrypted` is checked in `open_readonly()` to dispatch to the encrypted read path.

`remote_size` holds the ciphertext size needed by `XetFileInfo::new()` (xet.rs:119)
and `download_to_file()` (mod.rs:1007). Today `entry.size` serves both `stat()`
and xet download construction — with encryption these diverge. `entry.size` is
always plaintext; `remote_size` is the ciphertext size from Hub or from
`file_info.file_size()` at commit time. For non-encrypted files, `remote_size`
is `None` and `entry.size` is used everywhere.

## Dependency

```toml
[dependencies]
aegis = { version = "0.9", features = ["raf"], optional = true }

[features]
encrypt = ["aegis"]
```

The `raf` feature in the aegis crate re-exports `raf-core` + `getrandom`
(Cargo.toml line 50 of the upstream crate). This provides `Raf::create_file` /
`open_file` convenience methods that use `OsRng` internally.

## Implementation order

1. **Dependency + crypto module**: `Cargo.toml` + `src/crypto.rs` with key loading,
   content_type parser/formatter, encrypt/decrypt file helpers. Unit tests for all.

2. **Hub metadata**: add `content_type` to `TreeEntry`, thread into inode population
   with plaintext size extraction. Add `encrypted` flag to `InodeEntry`.

3. **Setup + forced advanced writes**: CLI args, key loading, force advanced writes
   when encryption enabled.

4. **Write path (flush)**: encrypt staging files at flush time, set `content_type`
   on commit, fix `apply_commit` to use plaintext size. Thread `EncryptionConfig`
   into `FlushManager`.

5. **Read path (xet-backed)**: branch in `open_readonly()` for encrypted xet files —
   download-then-decrypt-to-cache.

6. **Read path (HTTP/LFS)**: branch in `open_readonly()` for encrypted HTTP files —
   decrypt after download.

7. **Tests**: unit tests for crypto module, VirtualFs tests with mock encrypted
   uploads/downloads, integration tests on FUSE/NFS.

## Files to create/modify

| File | Action | Description |
|------|--------|-------------|
| `Cargo.toml` | modify | Add `aegis` dep with `raf` feature, `encrypt` feature flag |
| `src/crypto.rs` | create | Key loading, content_type format, encrypt/decrypt helpers |
| `src/lib.rs` | modify | `#[cfg(feature = "encrypt")] pub mod crypto;` |
| `src/setup.rs` | modify | CLI args, key loading, force advanced writes |
| `src/hub_api.rs` | modify | Add `content_type` to `TreeEntry` |
| `src/virtual_fs/mod.rs` | modify | `VfsConfig` + `VirtualFs` encryption fields, `open_readonly()` branches, inode population with plaintext size |
| `src/virtual_fs/inode.rs` | modify | Add `encrypted` flag to `InodeEntry` |
| `src/virtual_fs/flush.rs` | modify | Encrypt staging files before upload, set content_type, fix apply_commit size |
| `src/virtual_fs/tests.rs` | modify | Encryption unit tests |
| `tests/encrypted_ops.rs` | create | Integration tests |

## Risks and mitigations

1. **Full-file download for encrypted reads (v1)**: encrypted xet-backed files lose
   lazy streaming — the full ciphertext is downloaded and decrypted on open. For
   large model files (multi-GB) this is a significant regression.
   *Mitigation*: v1 targets correctness. v2 adds a `RafIo` adapter for lazy
   chunk-by-chunk decryption. The RAF chunk size (64 KiB) maps well to xet range
   downloads.

2. **Staging file plaintext on disk**: advanced-writes staging files are unencrypted
   locally. Acceptable because encryption protects remote storage, not the local
   machine. Document this in `--help`.

3. **Key management**: v1 uses a raw 16-byte key file. Future: passphrase-based
   key derivation, key rotation, per-repo keys.

4. **Mixed content**: supported via per-file content_type. A mount with encryption
   enabled writes encrypted files; existing plaintext files are readable as-is.
   A mount without `--encrypt` encountering an encrypted file returns EIO with
   a log warning.

## Non-goals for v1

- Filename / directory name encryption
- Key rotation / re-encryption of existing files
- Per-file keys
- Merkle tree integrity (excluded per requirements)
- Lazy chunk-by-chunk decryption for streaming reads (v2 optimization)
- Encrypting metadata or directory structure
