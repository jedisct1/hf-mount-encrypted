# Plan: Filename Encryption for hf-mount

## Goal

Encrypt filenames stored on the Hugging Face Hub so that an observer of the remote repository
sees only opaque ciphertext names, while local FUSE/NFS users see plaintext names as usual.
Encryption must be **deterministic** (same name + same key = same ciphertext) so that
lookups, deduplication, and directory listings work without storing extra state.

## Reference Design

Based on [zig-turbocrypt](~/src/zig-turbocrypt/src/filename_crypto.zig):

| Property | Value |
|----------|-------|
| Cipher | HCTR2-128 (length-preserving, wide-block, tweakable) |
| Key size | 16 bytes |
| Tweak | empty (deterministic) |
| Min plaintext | 16 bytes (pad shorter names with 0x00) |
| Encoding | base91 with filesystem-safe alphabet |
| Path handling | Encrypt each `/`-separated component independently |
| Special entries | `.` and `..` pass through unencrypted |

## Design

### 1. Key Derivation

**Prerequisite:** this plan builds on `plans/hkdf-key-derivation.md`, which introduces
HKDF-SHA256 key derivation for content encryption keys. That plan should be
implemented first. This section describes only the **extensions** needed for filename
encryption.

#### Relationship to the HKDF plan

The HKDF plan stores the PRK and `source_context` in `EncryptionConfig` and derives
content keys **on demand** per-algorithm via `EncryptionConfig::derive_key(algorithm)`.
This is necessary because different files may have been written with different AEGIS
algorithms, and the info string includes the algorithm scheme.

The filename key is different: it's always HCTR2-128 (16 bytes), independent of the
content encryption algorithm. It can be precomputed once at startup.

#### Filename key derivation

The filename key uses the same PRK from the HKDF plan but a different info string
that does **not** include the AEGIS algorithm scheme (since filenames aren't tied to
a content algorithm):

```
filename_info = b"hf-mount-filename-v1" || len(source_context) || source_context

filename_key = HKDF-Expand(PRK, info=filename_info, len=16)
```

The content key info uses the HKDF plan's existing format (with algorithm scheme).
The filename key info uses a distinct prefix (`"hf-mount-filename-v1"` vs
`"hf-mount-derive-v1"`) to ensure domain separation between the two purposes.

`source_context` is `SourceKind::encryption_context()` from the HKDF plan — canonical,
revision-free. `path_prefix` is deliberately excluded (see below).

#### Why path_prefix is excluded

`path_prefix` is a mount-time parameter, not a property of the data. Including it
would mean:

- The same physical file on the Hub gets a different key depending on which subfolder
  mount accessed it (`user/bucket` vs `user/bucket/subdir`).
- Renames and moves within a bucket (atomic Hub-side metadata operations) would
  require re-encryption because the key changed.

Two subfolder mounts of the same bucket deriving the same key is correct — they
access the same remote data.

#### Changes to `EncryptionConfig`

Extend the HKDF plan's `EncryptionConfig` with two new fields:

```rust
pub struct EncryptionConfig {
    pub prk: [u8; 32],           // from HKDF plan
    pub source_context: String,  // from HKDF plan
    pub algorithm: Algorithm,    // from HKDF plan
    pub chunk_size: u32,         // from HKDF plan
    // --- new fields ---
    pub filename_key: [u8; 16],  // precomputed HCTR2-128 key
    pub encrypt_filenames: bool, // CLI flag
}
```

The `filename_key` is derived once at startup:

```rust
// In setup.rs, after constructing EncryptionConfig per the HKDF plan:
let filename_key = crypto::derive_filename_key(&config.prk, &config.source_context);
```

Add to `src/crypto.rs`:

```rust
const FILENAME_INFO_PREFIX: &[u8] = b"hf-mount-filename-v1";

pub fn derive_filename_key(prk: &[u8; 32], source_context: &str) -> [u8; 16] {
    let mut info = Vec::with_capacity(FILENAME_INFO_PREFIX.len() + 2 + source_context.len());
    info.extend_from_slice(FILENAME_INFO_PREFIX);
    info.extend_from_slice(&(source_context.len() as u16).to_be_bytes());
    info.extend_from_slice(source_context.as_bytes());

    let mut key = [0u8; 16];
    hmac_sha256::HKDF::expand(&mut key, prk, &info);
    key
}
```

Content key derivation remains on-demand per-algorithm via the HKDF plan's
`EncryptionConfig::derive_key(algorithm)`. The two derivation paths use different
info prefixes and produce independent keys.

### 2. Filename Encryption Module

New file: `src/filename_crypto.rs` (gated behind `#[cfg(feature = "encrypt")]`).

**Dependencies:**
- `hctr2-rs` crate (https://docs.rs/hctr2-rs/latest/hctr2_rs/) — `Hctr2_128` for encryption
- `base91` crate — with a custom filesystem-safe alphabet (avoid `/`)

**Core functions:**

```rust
/// Encrypt a single filename component.
/// Pads to 16 bytes minimum, encrypts with HCTR2-128 (empty tweak),
/// encodes result as base91 with filesystem-safe alphabet.
pub fn encrypt_filename(name: &str, key: &[u8; 16]) -> Result<String>

/// Decrypt a single filename component.
/// Decodes base91, decrypts with HCTR2-128, strips null padding.
/// Returns Err on decoding/decryption failure (fail-closed).
pub fn decrypt_filename(name: &str, key: &[u8; 16]) -> Result<String>

/// Encrypt a full path (each component independently).
pub fn encrypt_path(path: &str, key: &[u8; 16]) -> Result<String>

/// Decrypt a full path (each component independently).
pub fn decrypt_path(path: &str, key: &[u8; 16]) -> Result<String>
```

**Algorithm (mirrors turbocrypt):**

1. Skip `.` and `..`
2. Pad plaintext name with `0x00` bytes to at least 16 bytes
3. `Hctr2_128::new(key).encrypt(padded, &[], &mut ciphertext)`
4. Encode ciphertext with base91 filesystem-safe alphabet
5. Validate encoded length <= 255 bytes (filesystem limit)

**Decryption:**

1. Skip `.` and `..`
2. Decode base91 → ciphertext bytes
3. `Hctr2_128::new(key).decrypt(ciphertext, &[], &mut padded)`
4. Strip trailing `0x00` bytes to recover original name
5. On failure, return `Err` — **not** the unchanged name (see section 4a)

### 3. Base91 Filesystem-Safe Alphabet

The standard base91 alphabet includes `/` which breaks paths. Turbocrypt uses a
filesystem-safe variant that replaces `/` with `'`.

**Options:**
- (a) Use the `base91` crate with a custom alphabet if it supports it.
- (b) Fork/vendor the base91 encoding (it's ~50 lines) with the filesystem alphabet.
- (c) Use base64url instead (simpler, 33% expansion vs base91's 23%, but well-tested).

**Decision:** Start with the `base91` crate. If it doesn't support custom alphabets,
vendor a minimal base91 encoder/decoder with the turbocrypt filesystem alphabet:
```
ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!#$%&()*+,.':;<=>?@[]^_`{|}~"
```
This avoids `/` and `-` (which could be confused with CLI flags in edge cases).

**Fallback:** If base91 proves problematic (crate quality, character issues on certain
filesystems), fall back to base64url-nopad encoding. Max plaintext filename length drops
from ~205 to ~189 bytes, which is still sufficient for real-world use.

### 4. Integration Points

**Architecture:** plaintext names live in VFS; encryption/decryption happens at the
`HubOps` trait boundary. Every call that crosses from VFS into the Hub API must translate
paths. The following is an exhaustive enumeration of those boundaries.

#### Hub API Boundary Inventory

The `HubOps` trait (`src/hub_api.rs:18`) defines four methods that accept paths:

| Method | Direction | Path param | Callers in VFS |
|--------|-----------|------------|----------------|
| `list_tree(prefix)` | Hub → VFS | prefix (outbound), returned `TreeEntry.path` (inbound) | `ensure_children_loaded`, `preload_tree_recursive` |
| `head_file(path)` | VFS → Hub | `full_path` (outbound) | `revalidate_file` (`mod.rs:293`) |
| `download_file_http(path, dest)` | VFS → Hub | `full_path` (outbound) | `open_for_read` (`mod.rs:1184`) |
| `batch_operations(ops)` | VFS → Hub | `BatchOp::AddFile.path`, `BatchOp::DeleteFile.path` (outbound) | `flush_one`, `flush_batch`, `rename_remote` |

All four must be covered. The cleanest approach is a **translating wrapper** around
`HubOps` rather than sprinkling encrypt/decrypt calls at each call site.

#### 4a. Encrypted path_prefix at construction time

`HubApiClient` stores a `path_prefix` (`hub_api.rs:430`) and transparently
prepends it to every outbound path via `prefixed_path()` (`hub_api.rs:434`) and
strips it from inbound paths via `strip_path_prefix()` (`hub_api.rs:446`). It also
validates the prefix exists via `validate_path_prefix()` (`hub_api.rs:458`).

If the wrapper only encrypts per-call paths, the client still prepends the
**plaintext** prefix. For a mount like `user/bucket/subdir`, the Hub would receive
`subdir/<encrypted-child>` instead of `<encrypted-subdir>/<encrypted-child>`.

**Solution:** when `encrypt_filenames` is enabled, encrypt the `path_prefix` before
passing it to `HubApiClient` at construction time in `setup.rs`:

```rust
let path_prefix = if encrypt_filenames {
    filename_crypto::encrypt_path(&raw_path_prefix, &filename_key)?
} else {
    raw_path_prefix
};
// Then construct HubApiClient with the (possibly encrypted) path_prefix
```

This makes the client's internal prefix handling work correctly:
- **Outbound:** client prepends encrypted prefix → wrapper encrypts per-call path →
  Hub receives `<enc-prefix>/<enc-path>`
- **Inbound:** Hub returns `<enc-prefix>/<enc-path>` → client strips encrypted
  prefix → wrapper decrypts remaining path → VFS gets plaintext
- **Validation:** `validate_path_prefix()` probes the encrypted prefix, which exists
  on the Hub because files were written with encrypted names

#### 4b. Translating HubOps wrapper

Introduce `EncryptedHubOps` that wraps an inner `Arc<dyn HubOps>` and transparently
encrypts outbound paths / decrypts inbound paths. The wrapper only handles per-call
paths — the `path_prefix` is already encrypted inside the inner client (see 4a).

```rust
struct EncryptedHubOps {
    inner: Arc<dyn HubOps>,
    filename_key: [u8; 16],
}

#[async_trait]
impl HubOps for EncryptedHubOps {
    async fn list_tree(&self, prefix: &str, recursive: bool) -> Result<Vec<TreeEntry>> {
        let enc_prefix = encrypt_path(prefix, &self.filename_key)?;
        let mut entries = self.inner.list_tree(&enc_prefix, recursive).await?;
        for entry in &mut entries {
            entry.path = decrypt_path(&entry.path, &self.filename_key)?;
        }
        Ok(entries)
    }

    async fn head_file(&self, path: &str) -> Result<Option<HeadFileInfo>> {
        let enc_path = encrypt_path(path, &self.filename_key)?;
        self.inner.head_file(&enc_path).await
    }

    async fn download_file_http(&self, path: &str, dest: &Path) -> Result<()> {
        let enc_path = encrypt_path(path, &self.filename_key)?;
        self.inner.download_file_http(&enc_path, dest).await
    }

    async fn batch_operations(&self, ops: &[BatchOp]) -> Result<()> {
        let enc_ops: Vec<BatchOp> = ops.iter().map(|op| match op {
            BatchOp::AddFile { path, xet_hash, mtime, content_type } => Ok(BatchOp::AddFile {
                path: encrypt_path(path, &self.filename_key)?,
                xet_hash: xet_hash.clone(),
                mtime: *mtime,
                content_type: content_type.clone(),
            }),
            BatchOp::DeleteFile { path } => Ok(BatchOp::DeleteFile {
                path: encrypt_path(path, &self.filename_key)?,
            }),
        }).collect::<Result<_>>()?;
        self.inner.batch_operations(&enc_ops).await
    }

    // Passthrough for non-path methods
    fn default_mtime(&self) -> SystemTime { self.inner.default_mtime() }
    fn source(&self) -> &SourceKind { self.inner.source() }
    fn is_repo(&self) -> bool { self.inner.is_repo() }
}
```

The wrapper is applied in `setup.rs` when `encrypt_filenames` is true. VFS already
holds `Arc<dyn HubOps>` (`mod.rs:79`), so the wrapper stores `inner: Arc<dyn HubOps>`
and is itself wrapped in an `Arc` before being passed to `VirtualFs::new()`. No VFS
code changes needed — all path translation is centralized.

Because the poll loop also calls `hub_client.list_tree("", true)` (`poll.rs:24`),
the wrapper automatically covers polling too — encrypted remote names are decrypted
before the poll diff logic compares them against the plaintext inode table.

#### 4c. Fail-closed decryption on tree load

`decrypt_path` in the `list_tree` wrapper returns `Err` if any component fails to
decode or decrypt. This surfaces as a visible error during tree loading rather than
silently poisoning the inode table with opaque ciphertext strings.

This is the correct behavior for the remote-listing path: if filenames can't be
decrypted, the key is wrong or data is corrupt, and the user should know immediately.

There is no "graceful passthrough" mode. If the user mounts without `--encrypt-filenames`,
the wrapper is not applied, and raw names pass through as-is. Mixed encrypted/unencrypted
directories within the same mount are not supported.

#### 4d. Rename preserves `content_type` metadata

Current rename logic in `rename_remote()` (`mod.rs:2524`, `mod.rs:2541`) creates
`BatchOp::AddFile` with `content_type: None`, which would lose encryption metadata
for already-encrypted files.

Fix: `rename_remote()` must read the existing `content_type` from the inode entry
and propagate it into the new `BatchOp::AddFile`. This is not strictly a filename-
encryption concern but a pre-existing bug that filename encryption work will expose.

```rust
// In rename_remote(), when constructing BatchOp::AddFile:
BatchOp::AddFile {
    path: info.new_full_path.clone(),
    xet_hash: hash.clone(),
    mtime: mtime_ms,
    content_type: info.content_type.clone(),  // preserve from source inode
}
```

The inode model stores encryption state as parsed fields (`encrypted`,
`file_algorithm`, `remote_size` in `inode.rs:62`), not a raw `content_type` string.
To reconstruct the outbound `content_type` for `BatchOp::AddFile`, add a helper
method on `InodeEntry`:

```rust
#[cfg(feature = "encrypt")]
pub fn content_type_string(&self) -> Option<String> {
    if self.encrypted {
        let alg = self.file_algorithm?;
        let remote_size = self.remote_size?;
        Some(crypto::format_content_type(&EncryptedFileInfo {
            algorithm: alg,
            plaintext_size: self.size,
            ciphertext_size: remote_size,
            chunk_size: chunk_size, // from EncryptionConfig or stored on InodeEntry
        }))
    } else {
        None
    }
}
```

`RenameInfo` gets an additional `content_type: Option<String>` field, populated via
this helper during rename preparation. For directory renames with descendant files,
each child's `content_type` is also collected — the descendant walk in `rename()`
already visits child inodes, so extend it to call `content_type_string()` alongside
collecting `xet_hash`.

**`chunk_size` source:** `InodeEntry` doesn't currently store `chunk_size`. Two options:
(a) add `chunk_size: Option<u32>` to the inode's encryption fields (populated from
`parse_content_type` which already parses it), or (b) pass `EncryptionConfig.chunk_size`
into `content_type_string()` as a parameter. Option (a) is more self-contained since
files could theoretically have been written with different chunk sizes; decide during
implementation.

### 5. CLI Interface

Add a `--encrypt-filenames` boolean flag to the CLI, only available when `--encryption-key-file`
is also provided.

**File:** `src/setup.rs`

```rust
#[arg(long, help = "Encrypt filenames on the remote (requires --encryption-key-file)")]
pub encrypt_filenames: bool,
```

If `--encrypt-filenames` is set without `--encryption-key-file`, emit an error.

### 6. Cargo Dependencies

The `hmac_sha256` dependency is added by the HKDF plan. This plan adds one more:

```toml
hctr2-rs = { version = "0.9", optional = true }
```

Update the `encrypt` feature:

```toml
encrypt = ["dep:aegis", "dep:hctr2-rs", "dep:hmac_sha256"]
```

### 7. Testing Strategy

#### Unit tests (in `filename_crypto.rs`):
- Roundtrip: `decrypt(encrypt(name)) == name` for various names
- Determinism: `encrypt(name)` called twice produces identical output
- Padding: names shorter than 16 bytes encrypt/decrypt correctly
- Special entries: `.` and `..` pass through unchanged
- Long names: validate 255-byte filesystem limit enforcement
- Wrong key: decryption with different key returns `Err` (fail-closed)
- Path roundtrip: `decrypt_path(encrypt_path("a/b/c")) == "a/b/c"`
- Empty components: paths with leading/trailing slashes handled correctly

#### Integration tests (in `virtual_fs/tests.rs`):
- Create a file with encryption + filename encryption enabled, verify the `BatchOp`
  path is encrypted (not plaintext)
- Load a tree with encrypted names, verify FUSE/NFS sees plaintext names
- Roundtrip: create file → flush → reload tree → verify name matches
- HEAD revalidation: verify `revalidate_file` sends encrypted path to Hub
- HTTP download: verify `download_file_http` receives encrypted path
- Wrong key during tree load: verify visible error, not silent passthrough
- Subfolder-scoped mounts: two mounts of same bucket with different prefixes derive
  the **same** keys (consistent encryption regardless of mount point)
- Cross-bucket isolation: same master key on different buckets derives different keys
- Encrypted path_prefix: subfolder mount with `encrypt_filenames` encrypts the prefix
  at construction, so `validate_path_prefix()` and all prefixed requests hit the
  correct encrypted remote path
- Rename of encrypted file: verify `content_type` metadata preserved in new `BatchOp`
- Rename of directory with encrypted children: verify all `content_type` values preserved
- Exercise both bucket and repo read paths under the wrapper (the underlying HTTP/tree
  plumbing differs: `hub_api.rs:482` list_tree, `hub_api.rs:631` head_file,
  `hub_api.rs:810` download_file_http)

### 8. Scope Boundaries

**In scope:**
- Filename encryption for bucket writes and bucket/repo reads
- All four `HubOps` path-bearing methods

**Out of scope (no changes needed):**
- `mkdir`, `rmdir`, `symlink` — these are local-only operations that don't emit Hub
  ops (`mod.rs:2101`, `mod.rs:2233`, `mod.rs:2301`). If they gain remote persistence
  later, the `EncryptedHubOps` wrapper will handle it automatically since all Hub
  traffic flows through the wrapper.
- Repo mounts are read-only, so filename encryption on the write path only matters
  for buckets. The read path (tree listing, HEAD, HTTP download) applies to both.

### 9. Implementation Order

**Prerequisite:** `plans/hkdf-key-derivation.md` must be implemented first. It
introduces the 32-byte master key, `hmac_sha256` dependency, and `derive_key()`
function. This plan extends that foundation.

1. **Add filename key derivation** — add `derive_filename_key()` to `crypto.rs`,
   add `filename_key` and `encrypt_filenames` fields to `EncryptionConfig`
2. **Add `hctr2-rs` dependency** to `Cargo.toml`
3. **`filename_crypto.rs`** — implement `encrypt_filename`, `decrypt_filename`,
   `encrypt_path`, `decrypt_path` with unit tests
4. **`EncryptedHubOps` wrapper** — implement the translating `HubOps` wrapper with
   fail-closed decryption
5. **CLI flag + encrypted path_prefix** — add `--encrypt-filenames` to `setup.rs`;
   when set, encrypt `path_prefix` before passing to `HubApiClient`, then wrap
   the client with `EncryptedHubOps`
6. **Fix rename `content_type` propagation** — extend `RenameInfo` and `rename_remote()`
   to preserve encryption metadata
7. **Integration tests** — all cases listed in section 7
8. **Verify** — `cargo clippy --features fuse,nfs,encrypt -- -D warnings` and
   `cargo test --lib --features fuse,nfs,encrypt`

### 10. Security Considerations

- **Deterministic encryption leaks equality**: an attacker can see when two files have
  the same name (across directories or over time). This is an inherent trade-off for
  enabling efficient lookups without server-side state.
- **Cross-bucket isolation**: HKDF info includes `SourceKind`, so the same master key
  used on different buckets/repos produces independent ciphertext. Subfolder mounts of
  the same bucket intentionally share the same key — they access the same remote data,
  and key consistency is required for atomic renames/moves without re-encryption.
- **Directory structure is visible**: number of files per directory and nesting depth are
  not hidden. Only the names themselves are encrypted.
- **File sizes visible**: encrypted content sizes are already stored in metadata; filename
  encryption doesn't change this.
- **No nonce reuse risk**: HCTR2 with empty tweak is deterministic by design — there is
  no nonce to mismanage.
- **Null-byte padding**: filenames containing embedded `0x00` bytes cannot be distinguished
  from padding on decryption. This is fine — POSIX filenames cannot contain null bytes.
- **Filesystem length limit**: encrypted names must fit in 255 bytes. With base91 encoding,
  the max plaintext filename is ~205 bytes. With base64url, ~189 bytes. Either is sufficient
  for real-world filenames.
- **Fail-closed decryption**: wrong key or corrupt data surfaces as an error during tree
  load, not as silent passthrough of ciphertext into the local namespace.
