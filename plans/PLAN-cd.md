# Optional client-side encryption plan

## Goal

Add optional client-side encryption for file contents so local plaintext is encrypted before upload to Hugging Face and transparently decrypted on read. Use the `aegis` crate with RAF and the `AEGIS-128X2` algorithm only. Do not use the RAF merkle tree.

## Current data flow to preserve

- Read-only xet-backed files are opened lazily and served by `VirtualFs::open_readonly()` / `open_lazy()` / `read()` using `xet_sessions.download_stream_boxed()` and `PrefetchState`.
- Non-xet repo/LFS files are downloaded by `hub_client.download_file_http()` into the staging cache, then opened locally.
- Advanced writes use a plaintext staging file on disk, then `flush_one()` / `flush_batch()` upload that path through `xet_sessions.upload_files()` and commit metadata via `BatchOp::AddFile`.
- Simple writes stream bytes directly to CAS through `XetSessions::create_streaming_writer()` and `streaming_commit()` without a staging file.

## High-level design

1. Add an optional encryption config to mount setup.
2. Represent encrypted remote objects explicitly in Hub metadata so mounts can detect whether a file must be decrypted.
3. Keep local filesystem semantics plaintext: reads, writes, truncate, random I/O, and rename all operate on plaintext.
4. Perform encryption/decryption at the xet boundary, not in FUSE/NFS adapters.
5. Reuse RAF for all random-access encryption/decryption work; do not implement block math manually.

## User-facing configuration

Add mount options:

- `--encryption-key-file <path>` — enables encryption. The file contains exactly
  16 raw bytes (for 128-bit key variants) or 32 raw bytes (for 256-bit key
  variants). Alternatively, the same bytes base64-encoded (24 or 44 chars).
  Presence of this flag is sufficient to enable encryption — no separate
  `--encrypt` flag needed.
- `--encryption-algorithm <name>` — optional, defaults to `aegis-128x2`. Valid
  values: `aegis-128l`, `aegis-128x2`, `aegis-128x4`, `aegis-256`, `aegis-256x2`,
  `aegis-256x4`. The algorithm name is stored in the `content_type` metadata
  (`scheme=` field), so decryption automatically uses the correct variant
  regardless of the mount-time default.

Rules:

- Encryption is disabled by default (no key file = no encryption).
- Key size must match the algorithm: 16 bytes for `aegis-128*` variants, 32 bytes
  for `aegis-256*` variants. Mismatch is a startup error.
- Mounted data remains plaintext locally; only remote stored content is encrypted.
- Reading encrypted files always uses the algorithm recorded in `content_type`,
  not the current `--encryption-algorithm` setting. This means a mount can read
  files encrypted with different algorithms as long as the key is correct and the
  key size matches.

## Metadata format

Store encryption metadata in Hub `content_type` on `BatchOp::AddFile` (hub_api.rs:119), since that field already exists and is committed with each file. All three commit sites already pass `content_type: None` (flush.rs:317, flush.rs:440, mod.rs:1785).

Format:

```
application/vnd.hf-mount+enc; scheme=<algorithm>-raf; chunk=65536; size=<plaintext_bytes>; stored=<ciphertext_bytes>
```

Example: `application/vnd.hf-mount+enc; scheme=aegis-128x2-raf; chunk=65536; size=1048576; stored=1049600`

Fields:
- `scheme=<algorithm>-raf` — identifies cipher + container format. The algorithm
  name matches the `--encryption-algorithm` values (`aegis-128x2`, `aegis-256`,
  etc.). On read, the parser extracts this to select the correct RAF variant.
- `chunk=65536` — RAF chunk size
- `size=<N>` — plaintext file size in bytes (used for `stat()`, EOF checks)
- `stored=<N>` — ciphertext file size in bytes (used for xet downloads via
  `XetFileInfo::new()` and `download_to_file()`). Redundant with the Hub's own
  `size` field on tree entries, but included so the metadata is self-contained
  and the fallback HEAD path works without a separate size lookup.

The plaintext size must be in the metadata because `file_info.file_size()` from xet-core reports ciphertext size, and `apply_commit()` (inode.rs:88) writes that directly into `entry.size`. Without the plaintext size in metadata, every `stat()`, truncation check, and flush bookkeeping operation uses the wrong value.

Parsing rules:
- `content_type` starting with `application/vnd.hf-mount+enc` → encrypted file
- Unknown or malformed encryption metadata → error, never fall back to plaintext
- No marker → plaintext (no decryption attempted)
- A mount without `--encrypt` encountering an encrypted file → EIO with log warning

This supports mixed plaintext/encrypted content in the same bucket.

### Implementation checkpoint: content_type in tree responses

Adding `content_type` to `TreeEntry` (hub_api.rs:128) assumes the Hub tree API
returns it. This is unverified. The bucket tree API (hub_api.rs:532) deserializes
directly into `TreeEntry` — if the JSON includes `content_type`, serde picks it
up automatically. The repo tree API (hub_api.rs:594) goes through `RepoTreeEntry`
first and would also need the field.

**Verification step** (implementation order step 2): after adding `content_type`
to `TreeEntry` and `RepoTreeEntry`, upload a test file with a custom content_type
via `BatchOp::AddFile` and confirm the tree API returns it. This is a blocking
checkpoint before proceeding with the read path.

**Fallback if tree responses lack content_type**: do a per-file HEAD request in
`open_readonly()` to fetch content_type before deciding the read path. This adds
one round-trip per encrypted file open but avoids blocking on an API change. The
HEAD response should include content_type since the resolve endpoint already
returns file metadata. Thread the result into the inode so subsequent opens skip
the probe.

## New module: `src/crypto.rs`

All `aegis` specifics live here. Also provides internal helper utilities that
wrap file-level encrypt/decrypt operations so call sites in flush.rs and
mod.rs stay concise.

Contents:

- `Algorithm` — enum matching the six AEGIS variants. Maps to/from the `scheme=`
  string in content_type and to the aegis `AlgorithmId`. Determines key size
  (16 bytes for 128-bit variants, 32 bytes for 256-bit variants).
- `EncryptionConfig { key: Vec<u8>, algorithm: Algorithm, chunk_size: u32 }` —
  mount-wide config. Key is `Vec<u8>` since length depends on algorithm.
- `EncryptedFileInfo { algorithm: Algorithm, plaintext_size: u64, ciphertext_size: u64, chunk_size: u32 }` — parsed metadata. The algorithm is per-file (from `scheme=`), not per-mount.
- `load_key(path: &Path, algorithm: Algorithm) -> Result<Vec<u8>>` — read key,
  validate length matches algorithm (16 or 32 bytes)
- `parse_content_type(ct: &str) -> Result<Option<EncryptedFileInfo>, Error>` —
  `Ok(None)` for non-encrypted, `Err` for malformed encrypted metadata
- `format_content_type(algorithm: Algorithm, plaintext_size: u64, ciphertext_size: u64, chunk_size: u32) -> String`
- `encrypt_file(src: &Path, dst: &Path, config: &EncryptionConfig) -> Result<()>`
- `decrypt_file(src: &Path, dst: &Path, key: &[u8], info: &EncryptedFileInfo) -> Result<u64>` —
  uses `info.algorithm` (from content_type) to select the RAF variant, not the
  mount-wide default. Returns plaintext size.
- `create_raf(path: &Path, config: &EncryptionConfig) -> Result<RafHandle>` — new empty RAF staging file
- `open_raf(path: &Path, key: &[u8], algorithm: Algorithm) -> Result<RafHandle>` — open existing

`RafHandle` is an enum wrapping `Raf<Aegis128X2>`, `Raf<Aegis256>`, etc. so the
caller doesn't need to be generic. It exposes `read()`, `write()`, `truncate()`,
`sync()`, `size()` methods that delegate to the inner variant. This keeps the
algorithm dispatch inside the crypto module.

The encrypt/decrypt helpers are internal utilities, not an architectural boundary.
They're called from `flush_one`/`flush_batch` (encrypt before upload) and
`open_readonly` (decrypt after download) — keeping those call sites to one or two
lines instead of inlining RAF setup each time.

## Dependency changes

In `Cargo.toml`:

```toml
[dependencies]
aegis = { version = "0.9", features = ["raf"], optional = true }

[features]
encrypt = ["aegis"]
```

The `raf` feature in the aegis crate re-exports `raf-core` + `getrandom`,
providing `Raf::create_file`/`open_file` convenience methods with `OsRng`.
Do not vendor or depend on the local checkout; use it only as reference.

## Read path changes

### 1) Read-only remote xet-backed files

Today `open_readonly()` opens a lazy xet stream when `xet_hash` exists. That path assumes remote bytes are directly readable plaintext.

For encrypted files:

- do not use `download_stream_boxed()` directly for plaintext reads
- instead, materialize/decrypt to a local cached plaintext file on first open, then serve with `open_local_readonly()`

Implementation approach for v1:

- download encrypted remote object to a cache path via
  `xet_sessions.download_to_file(xet_hash, remote_size, cache_path)` —
  note: must pass `entry.remote_size` (ciphertext size), not `entry.size`
  (plaintext size), since xet-core needs the real stored size for
  `XetFileInfo::new()` (xet.rs:119)
- open that downloaded file with RAF via `crypto::open_raf()` using the
  algorithm from the parsed `content_type`
- **verify RAF metadata against content_type**: compare `raf.size()` against
  `content_type`'s `size=` and the on-disk ciphertext length against `stored=`.
  If either disagrees, fail with EIO and log the mismatch. RAF metadata
  (embedded in the authenticated file header) is authoritative — treat
  `content_type` as a dispatch hint and pre-open size source, but never trust
  it over what the RAF layer reports after authentication.
- read/decrypt RAF contents into a plaintext cache file
- serve reads from the plaintext cache file

This is simpler and keeps `PrefetchState` unchanged. It sacrifices lazy remote reads for encrypted files initially, which is acceptable for a first implementation.

### 2) Read-only HTTP/LFS files

If encryption metadata is present on a non-xet file:

- after `download_file_http()`, treat the downloaded file as RAF ciphertext
- decrypt it to a plaintext cache file
- open the plaintext cache file locally

### 3) Future optimization (not v1)

Later, add a decrypted lazy reader backed by RAF range reads so encrypted xet files preserve lazy access. Do not block v1 on this.

## Write path changes

### Algorithm selection policy

- **New files**: use the mount's `--encryption-algorithm` (the mount default).
- **Rewrite of existing encrypted file** (open → modify → flush): preserve the
  file's recorded algorithm from `content_type`. The staging RAF is opened with
  the original algorithm, and the commit writes back the same `scheme=`. This
  avoids silent re-encryption and means a mount with `--encryption-algorithm
  aegis-256` won't re-encrypt files that were originally written with
  `aegis-128x2`.
- **Explicit re-encryption**: not in scope for v1. Future work could add a
  `--re-encrypt` flag or a separate tool.

To implement this, `InodeEntry` needs to store the per-file `Algorithm` (parsed
from `content_type`) so the staging open and flush paths can use it. For new
files (no existing `content_type`), fall back to the mount default.

### Inode encryption metadata consistency invariant

The fields `encrypted`, `file_algorithm`, `remote_size`, `size`, and the
`content_type` written to Hub are one logical unit. They must be updated
atomically at every commit site. This applies to all three transitions:

- **Plaintext → encrypted** (first write under encrypting mount): set
  `encrypted = true`, `file_algorithm = Some(mount_default)`,
  `remote_size = Some(ciphertext_size)`, `size = plaintext_size`, and
  `content_type` with matching values.
- **Encrypted → encrypted** (rewrite of existing encrypted file): update
  `remote_size` and `size` to reflect the new content, preserve
  `file_algorithm`, keep `encrypted = true`, write `content_type` with
  the preserved algorithm and new sizes.
- **Corruption check**: if `encrypted == true` but `file_algorithm` is
  `None`, fail with EIO. Never fall back to mount default.

Partial updates to any subset of these fields leave the inode in an
inconsistent state. Tests should assert this invariant after every commit
path: `flush_one`, `flush_batch`, `apply_commit`, and the plaintext →
encrypted transition in `open_advanced_write` + first flush.

### 1) Advanced writes

This is the best first target because it already uses local staging files and supports random I/O.

Change advanced-write staging files from plaintext files to RAF files when encryption is enabled:

- `open_advanced_write()`
  - for existing remote encrypted content: download ciphertext to staging path
    and open/update it via RAF using the **file's recorded algorithm**
  - for existing remote plaintext content with encryption enabled: download
    plaintext, create a new RAF file with the **mount default algorithm**,
    copy plaintext into RAF, then use RAF thereafter
  - for new/truncated files: create an empty RAF file with the **mount default
    algorithm**
- `write()` on `OpenFile::Local`
  - replace raw `libc::pwrite` with RAF `write()` when encrypted staging is active
- `read()` on `OpenFile::Local`
  - replace raw local file reads with RAF `read()` when encrypted staging is active
- `setattr(size)` / truncate paths
  - call RAF `truncate()` instead of `File::set_len()` for encrypted staging
- `flush`/`release`
  - call RAF `sync()` before enqueue/close if needed

This preserves plaintext semantics at the VFS layer while the on-disk staging object is ciphertext.

### 2) Simple streaming writes

Do not implement true encrypted streaming in v1.

Reason: current simple mode writes append-only plaintext directly into CAS via `SingleFileCleaner`; RAF is random-access-file oriented and easiest to apply on a file-backed object. For correctness and scope control:

- when encryption is enabled, force `advanced_writes = true` in setup for writable mounts
- document/log that encrypted mounts always use advanced writes

## Xet / Hub integration changes

### Xet

Add file-based helpers in `XetOps` / `XetSessions` as needed, but avoid changing the streaming interfaces unless necessary.

Likely additions:

- `download_xet_to_path(...)` can continue using existing `download_to_file()`
- no v1 changes to `download_stream_boxed()` or `create_streaming_writer()` if encrypted reads are cache-materialized and encrypted writes force advanced mode

### Hub metadata

`TreeEntry` (hub_api.rs:128) currently lacks `content_type`. Add it:

```rust
#[serde(default)]
pub content_type: Option<String>,
```

Wherever `TreeEntry` is converted into `InodeEntry`, if `content_type` parses as
encrypted via `crypto::parse_content_type()`:
- Set `entry.size` to the parsed plaintext size
- Set `entry.remote_size` to the parsed ciphertext size (or Hub's `size` field)
- Set `entry.encrypted = true`

On commit — all three sites (flush.rs:317, flush.rs:440, mod.rs:1785):
- Set `content_type` to `crypto::format_content_type(plaintext_size, ciphertext_size, chunk_size)`
- Pass plaintext size (not `file_info.file_size()`) to `apply_commit()`
- Store `file_info.file_size()` (ciphertext size) in `entry.remote_size`

## In-memory model changes

### Dual-size model

Encrypted files have two sizes: the plaintext size (what `stat()` reports, what
EOF checks use) and the ciphertext size (what xet-core needs to download from
CAS). Today `entry.size` is the single source of truth — used by `stat()`,
`read()` EOF capping, AND `XetFileInfo::new()` construction (mod.rs:1183,
xet.rs:119). With encryption these diverge.

Add a `remote_size` field to `InodeEntry`:

```rust
#[cfg(feature = "encrypt")]
pub remote_size: Option<u64>,
```

Rules:
- `entry.size` is always the plaintext size (used by `stat()`, `read()` EOF,
  truncation, `setattr`). This is what the user sees.
- `entry.remote_size` is the ciphertext size stored on the remote. Set from the
  Hub tree listing's `size` field when encryption metadata is present. Used when
  constructing `XetFileInfo` for `download_to_file()` (mod.rs:1007) and
  `download_stream_boxed()` (mod.rs:1183 via `PrefetchState.file_size`).
- For non-encrypted files, `remote_size` is `None` and `entry.size` is used
  everywhere (no behavioral change).

At `apply_commit` time (inode.rs:88): when encryption is active, store the
plaintext size in `entry.size` and the ciphertext size (`file_info.file_size()`)
in `entry.remote_size`.

In `open_readonly()` for encrypted xet-backed files: pass `entry.remote_size`
(not `entry.size`) to `download_to_file()`.

### Other additions

- `InodeEntry`: add `encrypted: bool` and `file_algorithm: Option<Algorithm>`
  fields (cfg-gated). Set from parsed `content_type` during inode population.
  `encrypted` is checked in `open_readonly()` to dispatch the read path.
  `file_algorithm` is used by `open_advanced_write()` and flush to preserve
  the file's original algorithm on rewrite (see algorithm selection policy).
- `OpenFile::Local`: add an optional `Raf<Aegis128X2>` handle (or a wrapper)
  for encrypted staging. When present, `read()`/`write()` use RAF instead of
  raw `pread`/`pwrite`.
- `VirtualFs`: add `encryption_config: Option<crypto::EncryptionConfig>`.
  Threaded into `FlushManager` for encrypt-before-upload.

## Testing plan

1. Unit tests for metadata parsing/formatting.
2. Unit tests for key loading and invalid key handling.
3. Unit tests around RAF helpers using `Aegis128X2`:
   - create/open roundtrip
   - random write/read at offsets
   - truncate
   - wrong key failure
4. `virtual_fs` unit tests with encryption enabled:
   - advanced write -> flush -> reopen -> plaintext read
   - rename dirty encrypted file still preserves `pending_deletes`
   - truncate encrypted file
   - read-only open of encrypted xet-backed file via decrypted cache path
5. Inode encryption metadata consistency invariant (assert after every commit path):
   - `flush_one()`: `encrypted`, `file_algorithm`, `size`, `remote_size` all
     consistent; `content_type` round-trips to matching values
   - `flush_batch()`: same invariant across multiple inodes in one batch
   - `apply_commit()` (inode.rs:88): verify `size`/`remote_size` never swapped
   - plaintext → encrypted transition: all five fields set atomically on first
     flush of a previously-plaintext file
   - encrypted → encrypted rewrite: `file_algorithm` preserved, sizes updated
   - rename of dirty encrypted file: all fields preserved through rename
   - corruption: `encrypted == true` with `file_algorithm == None` → EIO
   - `content_type` format → parse round-trip: same algorithm, sizes
   - RAF cross-check: `raf.size()` matches `size=`, ciphertext length matches
     `stored=`
7. Integration tests:
   - encrypted bucket write/read roundtrip on FUSE
   - encrypted bucket write/read roundtrip on NFS
   - verify remote metadata marks encrypted files
   - verify mounted reads remain plaintext
   - write with one algorithm, remount with different default algorithm,
     verify reads still work (content_type drives decryption, not mount default)

## Suggested implementation order

1. Add dependency and a small `crypto` module with key loading + metadata parser/formatter. Unit test the parser (including malformed input → error, not `None`).
2. **Checkpoint**: add `content_type` to `TreeEntry` and `RepoTreeEntry`. Upload a test file with a custom content_type via `BatchOp::AddFile`, list the tree, confirm the field comes back. If it doesn't, implement the HEAD-based fallback in `open_readonly()` before proceeding.
3. Thread encryption config through CLI/setup into `VirtualFs`; force advanced writes when enabled.
4. Extend inode/file metadata: add `encrypted: bool` and `remote_size: Option<u64>` to `InodeEntry`. Populate from parsed `content_type` during tree → inode conversion.
5. Implement RAF file helpers using `Raf<Aegis128X2>` with no merkle configuration.
6. Convert advanced-write local staging operations (`open`, `read`, `write`, `truncate`, `sync`) to use RAF when enabled.
7. Mark uploaded files as encrypted in `BatchOp::AddFile.content_type`. Pass plaintext size to `apply_commit()`, store ciphertext size in `remote_size`.
8. Implement read-only encrypted-file handling by download-then-decrypt-to-plaintext-cache. Pass `remote_size` (not `size`) to `download_to_file()`.
9. Add tests, then optimize only if needed.

## Non-goals for v1

- RAF merkle tree support
- preserving lazy remote range reads for encrypted files
- encrypting filenames, directory names, or metadata
- backwards-compatible automatic migration of existing plaintext remote files beyond on-write replacement
