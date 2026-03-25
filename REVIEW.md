I did a broader audit across the repository and fixed three logic bugs that I could prove with targeted tests.

- `src/hub_api.rs:279` and `src/hub_api.rs:433` now treat `--token-file` consistently as higher priority than `--hf-token`, both during initial client bootstrap and later request auth refreshes; they also clear stale cached file tokens when the token file disappears or becomes empty.
- `src/nfs.rs:283` now schedules a flush after `create_exclusive()`, matching `create()`. Without that, NFS exclusive creates of empty files could stay only in local staging forever because NFS v3 has no close/flush RPC to force a commit later.
- `src/cached_xet_client.rs:144` now cleans up single-flight state even if the leader task is cancelled while awaiting the CAS backend. Before this, followers could wait forever on a broadcast sender that would never be signaled and would never be removed from `inflight`.

How I audited the codebase:
- `src/hub_api.rs`: auth precedence, token refresh, path-prefix parsing/validation, HEAD/download logic, retry behavior.
- `src/setup.rs`: source parsing, repo/bucket path-prefix splitting, read-only/advanced-write selection, NFS/FUSE config handoff.
- `src/virtual_fs/mod.rs`, `src/virtual_fs/poll.rs`, `src/virtual_fs/flush.rs`, `src/virtual_fs/inode.rs`, `src/virtual_fs/tests.rs`: lookup/revalidation, directory loading, dirty-state transitions, rename/unlink semantics, poll diffing, flush scheduling, inode bookkeeping.
- `src/nfs.rs` plus `tests/nfs_ops.rs`: handle-pool behavior, read/write/create semantics, mount lifecycle, NFS-specific eventual flush behavior.
- `src/cached_xet_client.rs`: cache hit/miss/eviction paths, range derivation, single-flight concurrency.
- `src/xet.rs`: bounded/unbounded download logic, upload session use, staging-path generation.
- `src/daemon.rs` and binaries in `src/bin/`: daemon PID/log handling, start/stop flow, backend handoff.

Accepted findings and proof:
- `src/hub_api.rs:293` used the static token whenever both a static token and token file were supplied, contradicting the documented precedence rule. Proven by `src/hub_api.rs:1242` (`init_auth_get_prefers_token_file_over_static_token`) which inspects the built `Authorization` header.
- `src/hub_api.rs:444` kept an old cached token-file value forever if the token file later vanished. Proven by `src/hub_api.rs:1345` (`token_file_refresh_clears_stale_cache_and_falls_back_to_static`).
- `src/nfs.rs:283` omitted `schedule_flush()` in `create_exclusive()`, while `src/nfs.rs:264` correctly scheduled it in normal `create()`. Given the project’s own NFS comment that empty files need scheduled flushes, this was a real persistence bug. Proven by `src/nfs.rs:854` (`create_exclusive_schedules_flush_for_empty_file`) which waits past the flush debounce and checks that a batch commit happened.
- `src/cached_xet_client.rs:221` inserted an `inflight` marker before awaiting the backend, but cleanup happened only on the normal return path. If the leader future was aborted, the marker remained and later callers could block forever. Proven by `src/cached_xet_client.rs:719` (`cancelled_single_flight_leader_does_not_block_future_requests`) using `tokio::time::timeout`.

Rejected candidates after inspection:
- `src/setup.rs:403` returning `direct_io: options.direct_io` in `MountSetup` looks asymmetric with `VfsConfig.direct_io`, but it is intentional: the NFS binary ignores that field, while the FUSE binary is the only consumer.
- `src/virtual_fs/poll.rs:113` / `src/virtual_fs/mod.rs:453` using `size.unwrap_or(0)` looked suspicious, but the surrounding code distinguishes updates by hash/etag too; I did not find a proven regression from existing repo/bucket semantics or tests.
- `src/virtual_fs/mod.rs:2672` / `src/virtual_fs/mod.rs:932` use `unwrap_or_default()` for missing `xet_hash`, which at first looked like it might misclassify local files; after tracing `open_readonly()` and the existing tests around plain HTTP-backed repo files, the empty-string sentinel is used consistently there.
- `src/daemon.rs` path encode/decode and PID file handling had edge cases worth reviewing, but I did not find a reproducible correctness bug from the current logic.
- `src/xet.rs` bounded-range reconstruction and staging-path generation are conservative rather than incorrect; the over-fetch limitation is already documented in-code and not a hidden logic error in this repo.

Verification I ran:
- `cargo +nightly fmt --check`
- `cargo clippy --features nfs -- -D warnings`
- `cargo test --lib --features nfs`
- Focused regressions:
  - `cargo test --lib --features nfs init_auth_get_prefers_token_file_over_static_token`
  - `cargo test --lib --features nfs token_file_refresh_clears_stale_cache_and_falls_back_to_static`
  - `cargo test --lib --features nfs create_exclusive_schedules_flush_for_empty_file`
  - `cargo test --lib --features nfs cancelled_single_flight_leader_does_not_block_future_requests`

Results:
- `cargo test --lib --features nfs`: 220 passed
- `cargo clippy --features nfs -- -D warnings`: passed
- `cargo +nightly fmt --check`: passed

Files changed:
- `src/hub_api.rs`
- `src/nfs.rs`
- `src/cached_xet_client.rs`

Residual gaps:
- I could not run the full `fuse,nfs` matrix here because `fuser` fails to build without the system FUSE pkg-config entry (`fuse.pc`). So FUSE-specific runtime code was audited statically, not executed locally.
- I did not find additional proven bugs beyond the three fixes above after the broader inspection, but I’m being careful not to overclaim exhaustive formal correctness for every path in the repo.

<learned>`filename3` in `nfsserve` is a type alias to `nfsstring`, not a constructor. In the NFS test I needed to instantiate `nfsstring(...)` instead of calling `filename3(...)` like a tuple struct.</learned>

Natural next steps:
1. Install the FUSE system dependency and run `cargo clippy --features fuse,nfs -- -D warnings` plus `cargo test --lib --features fuse,nfs`.
2. If you want, I can do a second audit pass focused only on the FUSE adapter and daemon start/stop flows once the FUSE build dependency is available.
