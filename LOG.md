# LOG

## 2026-03-25: Encryption plan

**Asked**: Add optional client-side encryption using the `aegis` crate with AEGIS-128X2 cipher and its RAF (Random Access File) functions. Create a plan.

**Done**: Explored both the `aegis` crate (~/src/rust-aegis) and the hf-mount read/write data paths. Created `plans/PLAN-cl.md`. Also set up CLAUDE.md, a /verify skill, and a format-on-edit hook via /init.

## 2026-03-25: Plan revision after review

**Asked**: Review feedback on PLAN-cl.md identifying five issues (metadata format, size handling, HTTP/LFS coverage, lazy read regression, dependency feature). Also read and compared PLAN-cd.md.

**Done**: Revised `plans/PLAN-cl.md` with major changes:
- Added explicit remote metadata format using `content_type` on `BatchOp::AddFile` with plaintext size embedded
- Fixed size handling: plaintext size stored in inode from the start (via content_type parsing on tree listing), not lazily on open. All three `apply_commit` sites (flush_one, flush_batch, streaming_commit) pass plaintext size.
- Added HTTP/LFS file coverage: decrypt after `download_file_http()` in `open_readonly()`
- Honestly documented lazy read regression as a v1 trade-off with v2 path via RafIo adapter
- Adopted PLAN-cd's approach of forcing `advanced_writes=true` instead of building EncryptedStreamingWriter
- Dropped the `EncryptedXetOps` wrapper in favor of targeted changes at flush/open boundaries
- Corrected aegis dependency feature (`raf` is correct per upstream Cargo.toml)

## 2026-03-25: Consolidate into PLAN-cd.md

**Asked**: User confirmed PLAN-cd is the stronger plan. Fold metadata format and crypto helper utility ideas from PLAN-cl into PLAN-cd.

**Done**: Updated `plans/PLAN-cd.md` with:
- Expanded metadata format with plaintext size field and code references to all three commit sites
- Fleshed out `src/crypto.rs` module with concrete API (helpers as internal utilities, not architectural boundary)
- Exact dependency stanza (`aegis = { version = "0.9", features = ["raf"] }`)
- Concrete `TreeEntry` change and size-handling fix at `apply_commit` sites
- Specific `InodeEntry`, `OpenFile::Local`, and `VirtualFs` field additions

## 2026-03-25: Fix dual-size model, parser API, content_type checkpoint

**Asked**: Three issues — (1) remote ciphertext size lost when inode stores only plaintext size, breaking `XetFileInfo::new()` and `download_to_file()`, (2) parser returns `Option` but spec says malformed → error, (3) content_type in tree responses is an assumption not a verified fact.

**Done**: Updated both `plans/PLAN-cd.md` and `plans/PLAN-cl.md`:
- Added `remote_size: Option<u64>` to `InodeEntry` for ciphertext size, used by xet download paths (mod.rs:1007, xet.rs:119, mod.rs:1183)
- Added `stored=<ciphertext_bytes>` to content_type format so metadata is self-contained
- Fixed parser signature to `Result<Option<EncryptedFileInfo>, Error>`
- Added explicit verification checkpoint: upload with content_type, list tree, confirm field roundtrips. Fallback: per-file HEAD in `open_readonly()` if tree API doesn't expose it

## 2026-03-25: CLI simplification and algorithm support

**Asked**: Drop redundant `--encrypt` flag (key file presence is sufficient), add `--encryption-algorithm` for future-proofing across AEGIS variants.

**Done**: Updated `plans/PLAN-cd.md`:
- Replaced `--encrypt` + `--encryption-key-file` with just `--encryption-key-file` (presence enables encryption)
- Added `--encryption-algorithm` defaulting to `aegis-128x2`, supporting all six AEGIS variants
- Made crypto module algorithm-aware: `Algorithm` enum, `RafHandle` enum wrapping variant-specific `Raf<T>`, key size validated against algorithm (16 vs 32 bytes)
- Per-file algorithm stored in content_type `scheme=` field — reads use the recorded algorithm, not mount default
- Added RAF metadata cross-check: `raf.size()` and ciphertext length verified against content_type on open
- Added dual-size test coverage for all commit paths (flush_one, flush_batch, apply_commit, rename)

## 2026-03-25: Algorithm selection policy for rewrites

**Asked**: Define policy for mount default algorithm vs existing file algorithm on rewrite.

**Done**: Added algorithm selection policy to `plans/PLAN-cd.md`:
- New files use the mount's `--encryption-algorithm` default
- Rewrites of existing encrypted files preserve the file's recorded algorithm (from `content_type` `scheme=`) to avoid silent re-encryption
- Added `file_algorithm: Option<Algorithm>` to `InodeEntry` to carry per-file algorithm through staging and flush
- `open_advanced_write()` now explicitly documents which algorithm is used in each case (existing encrypted → file's algorithm, existing plaintext → mount default, new file → mount default)
- Added two invariants: (1) encrypted inode with missing `file_algorithm` is corruption → EIO, never fallback; (2) plaintext→encrypted transition must atomically set `encrypted`, `file_algorithm`, `remote_size`, and `content_type` together

## 2026-03-25: Implementation

**Asked**: Implement PLAN-cd.md.

**Done**: Full implementation of client-side encryption with AEGIS-128X2 (and all 6 AEGIS variants).

Files created:
- `src/crypto.rs` — Algorithm enum, EncryptionConfig, EncryptedFileInfo, RafHandle enum, content_type parser/formatter, key loading, encrypt/decrypt file helpers, 22 unit tests

Files modified:
- `Cargo.toml` — aegis dependency with raf feature, encrypt feature flag
- `src/lib.rs` — crypto module registration
- `src/error.rs` — Encryption error variant
- `src/setup.rs` — --encryption-key-file, --encryption-algorithm CLI args, forced advanced_writes
- `src/hub_api.rs` — content_type on TreeEntry and RepoTreeEntry
- `src/virtual_fs/mod.rs` — VfsConfig/VirtualFs encryption fields, open_encrypted_xet/decrypt_and_open methods, content_type→inode population
- `src/virtual_fs/inode.rs` — encrypted/file_algorithm/remote_size fields, apply_encryption_metadata method
- `src/virtual_fs/flush.rs` — encrypt-before-upload in flush_one/flush_batch, content_type on commit, plaintext size to apply_commit, remote_size tracking
- `src/virtual_fs/tests.rs` — 4 encryption VFS tests
- `src/test_mocks.rs` — encryption_config in TestOpts, add_file_with_content_type, content_type on TreeEntry

Design deviation from PLAN-cd: staging files remain plaintext (Raf is not Send/Sync so can't be stored in OpenFile::Local). Encryption happens at flush time instead. This is functionally equivalent — local disk is trusted, only remote data is encrypted.

## 2026-03-25: Fix four bugs from review

**Asked**: Four issues — (1) malformed metadata falls back to plaintext, (2) poll loses encryption state, (3) advanced write downloads use plaintext size, (4) decrypt_file missing cross-checks.

**Done**:
1. Malformed content_type now sets `encrypted=true, file_algorithm=None` → triggers EIO on open (corruption invariant). Never falls back to plaintext.
2. `update_remote_file` now takes `content_type` parameter, parses encryption metadata on poll updates. HEAD revalidation (no content_type) preserves existing encryption state and updates `remote_size` instead of overwriting plaintext `size` with ciphertext size.
3. `open_advanced_write` and setattr truncation bootstrap now use `remote_size` for encrypted file downloads, then decrypt ciphertext to plaintext staging.
4. `decrypt_file` now cross-checks `raf.size()` against `info.plaintext_size` and ciphertext file length against `info.ciphertext_size`, failing with error on mismatch.

## 2026-03-25: Named consistency invariant

**Asked**: Generalize atomic update rule to all transitions (including encrypted→encrypted rewrites), name it as a testable invariant.

**Done**: Replaced per-case invariants with a single "Inode encryption metadata consistency invariant" covering all three transitions (plaintext→encrypted, encrypted→encrypted, corruption). Test matrix references the invariant by name and asserts it after every commit path.

## 2026-03-25: HKDF key derivation

**Asked**: The encryption key was used directly from the key file. Add HKDF-SHA256 key derivation to bind keys to source identity and algorithm. Three rounds of plan review refined: (1) derivation at call sites not setup, (2) revision excluded from context, (3) canonical source identity, (4) clean function signatures.

**Done**: Implemented HKDF key derivation using `hmac-sha256` crate.

Changes:
- `Cargo.toml` — added `hmac-sha256` as optional dep, wired into `encrypt` feature
- `src/crypto.rs` — `EncryptionConfig` now stores `prk` + `source_context` instead of raw key; added `extract_prk`, `build_info`, `derive_key` functions; `load_key` → `load_master_key` (always 32 bytes); `encrypt_file`/`create_raf` take `(key, algorithm, chunk_size)` directly; 6 new HKDF tests
- `src/hub_api.rs` — added `SourceKind::encryption_context()` (revision-free)
- `src/setup.rs` — uses `load_master_key` + `extract_prk` + resolved `encryption_context()`
- `src/virtual_fs/flush.rs` — both flush paths now resolve `file_algorithm`, derive key, pass to updated `encrypt_file`
- `src/virtual_fs/mod.rs` — both decrypt paths now call `config.derive_key(algorithm)`
- `src/virtual_fs/tests.rs` — updated test config to new struct shape

## 2026-03-25: Filename encryption

**Asked**: Implement `plans/filename-encryption.md` — deterministic filename encryption using HCTR2-128 + base91 encoding, with a translating HubOps wrapper.

**Done**: Full implementation of filename encryption per the plan.

Files created:
- `src/filename_crypto.rs` — HCTR2-128 encrypt/decrypt for filenames and paths, vendored base91 encoder/decoder with filesystem-safe alphabet (avoids `/`), 15 unit tests

Files modified:
- `Cargo.toml` — added `hctr2-rs` optional dep, wired into `encrypt` feature
- `src/lib.rs` — registered `filename_crypto` module (feature-gated)
- `src/crypto.rs` — added `derive_filename_key()`, `FILENAME_INFO_PREFIX`, extended `EncryptionConfig` with `filename_key` and `encrypt_filenames` fields
- `src/hub_api.rs` — added `EncryptedHubOps` wrapper implementing `HubOps` trait with transparent path encryption/decryption
- `src/setup.rs` — added `--encrypt-filenames` CLI flag, early filename key derivation, path_prefix encryption before `HubApiClient` construction, `EncryptedHubOps` wrapping
- `src/virtual_fs/mod.rs` — `RenameInfo` now carries `content_type`, `RenameDescendant` struct replaces tuples, `rename_remote` propagates content_type in `BatchOp::AddFile`, added `enc_content_type` helper
- `src/virtual_fs/inode.rs` — added `content_type_string()` method to reconstruct content_type from encryption fields
- `src/test_mocks.rs` — added missing `content_type` field to synthetic `TreeEntry`
- `src/virtual_fs/tests.rs` — updated test EncryptionConfig to new struct shape

## 2026-04-02: Minimize diff size

**Asked**: Minimize the diff introduced in commit afb4e4c without changing behavior.

**Done**: Reduced the diff from 1984 insertions to 1749 insertions (-235 lines, ~12% reduction). All tests pass, clippy clean, formatting correct.

Changes:
- **crypto.rs**: Replaced 43-line custom base64 decoder with `base64` crate (already a transitive dependency); consolidated 5 parse_malformed tests into 1; merged 6 HKDF property tests into 1; merged 3 encrypt_decrypt roundtrip tests into 1
- **filename_crypto.rs**: Consolidated 6 roundtrip tests into 1; merged 3 path tests into 1; merged limit/no-slash/base91 tests
- **flush.rs**: Extracted `encrypt_staging` helper to deduplicate encryption logic in flush_one and flush_batch; simplified commit pattern by calling `apply_commit` unconditionally then conditionally `mark_encrypted`
- **inode.rs**: Replaced 9-line `apply_encrypted_commit` method with 6-line `mark_encrypted` (simpler API)
- **mod.rs**: Extracted `decrypt_to_file` helper to deduplicate decryption logic between `decrypt_and_open` and `download_to_staging`
- **test_mocks.rs**: Made `add_file_with_content_type` delegate to `add_file` instead of duplicating its body
- **tests.rs**: Merged `encrypted_inode_consistency_invariant` into `encrypted_create_write_flush_sets_metadata`

## 2026-04-02: Restrict encryption to buckets only

**Asked**: Encryption should only work with buckets, not repos. Mounting a repo with an encryption key should be rejected.

**Done**: Added a validation check in `src/setup.rs` (after source_kind is determined, before key loading) that rejects `--encryption-key-file` when the source is a `Repo`. Uses `matches!` macro per clippy preference. Exits with clear error message.
