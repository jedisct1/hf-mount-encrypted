# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

This project uses Cargo feature flags for conditional compilation. Always specify features when building/testing:

```bash
# Format (requires nightly)
cargo +nightly fmt --check

# Lint (check all feature combos as CI does)
cargo clippy --features fuse,nfs -- -D warnings

# Unit tests
cargo test --lib --features fuse,nfs

# Integration tests (require HF_TOKEN env var, run single-threaded)
cargo test --release --features fuse,nfs --test fuse_ops -- --test-threads=1
cargo test --release --features fuse,nfs --test nfs_ops -- --test-threads=1
cargo test --release --features nfs --test repo_ops -- --test-threads=1  # public repo, no token needed
```

## Key Conventions

- Rust edition 2024 (requires rustc 1.85+)
- rustfmt max_width is 120 (`rustfmt.toml`)
- clippy treats all warnings as errors (`-D warnings`)
- Feature flags: `fuse`, `nfs`, `encrypt` — most code compiles with `fuse,nfs` enabled; `encrypt` adds AEGIS content encryption, HKDF key derivation, and HCTR2 filename encryption
- The `xet-core` dependency is pinned to a specific git revision, not on crates.io

## Lock Ordering

The virtual filesystem has strict lock ordering to prevent deadlocks:

```
staging_locks[ino] → inode_table → open_files → negative_cache
StreamingChannel.commit_hook → pending_commits
```

Locks are held briefly and never across `.await` points (except `tokio::sync::Mutex` for staging locks).

## Architecture

- `src/virtual_fs/` — core filesystem abstraction (inodes, prefetch, flush, polling)
- `src/fuse.rs` / `src/nfs.rs` — backend adapters implementing FUSE and NFS traits
- `src/hub_api.rs` — Hugging Face Hub API client (`HubOps` trait)
- `src/xet.rs` — xet-core file transfer (`XetOps` trait)
- `src/cached_xet_client.rs` — CAS reconstruction cache with single-flight dedup
- `src/crypto.rs` — client-side encryption: AEGIS RAF wrapper, HKDF-SHA256 key derivation, content-type metadata (feature-gated behind `encrypt`)
- `src/filename_crypto.rs` — deterministic filename encryption: HCTR2-128 cipher, base91 encoding, path component encryption (feature-gated behind `encrypt`)
- `src/test_mocks.rs` — mock implementations of `HubOps`/`XetOps` for unit tests
- Three binaries: `hf-mount` (daemon), `hf-mount-fuse`, `hf-mount-nfs`
