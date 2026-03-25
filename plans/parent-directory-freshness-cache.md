# Parent-directory freshness cache to suppress per-file `HEAD`s

## Goal

Reduce redundant per-file `HEAD` requests on repeated `lookup()` calls by trusting a freshly fetched parent directory listing for a bounded time window, while preserving the current safety model and keeping code changes small.

## Current behavior

- `VirtualFs::lookup()` is the only VFS path that triggers per-file remote revalidation for clean files in already-loaded directories (`src/virtual_fs/mod.rs:731`).
- That revalidation goes through `revalidate_file()` and may call `hub_client.head_file(...)` unless the file inode's `last_revalidated` is still within `metadata_ttl` and `serve_lookup_from_cache` is enabled (`src/virtual_fs/mod.rs:275`).
- Parent directory contents are already fetched by `ensure_children_loaded(parent_ino)`, which calls `hub_client.list_tree(prefix)` and loads child metadata from `TreeEntry` (`src/virtual_fs/mod.rs:366`, `src/hub_api.rs:529`).
- `TreeEntry` already includes the metadata we use to detect remote file identity and content changes: `size`, `xet_hash`, `oid`, and `mtime` (`src/hub_api.rs:128`).
- Loaded-directory state already exists as `children_loaded` on each directory inode, and poll invalidation already clears it when a loaded directory may be stale (`src/virtual_fs/inode.rs:323`, `src/virtual_fs/poll.rs:221`).
- `ensure_children_loaded()` has an important short-circuit: if `children_loaded == true`, it returns without fetching anything (`src/virtual_fs/mod.rs:367`, `src/virtual_fs/mod.rs:387`, `src/virtual_fs/mod.rs:403`). That means repeated `lookup()` or `readdir()` calls do not refresh directory freshness on their own.

## Key insight

A successful `list_tree(parent)` call already proves the current state of that parent's direct children at a specific moment and already carries the same file identity fields that `HEAD` later compares.

So, for direct child files of a loaded parent directory, we can safely suppress per-file `HEAD`s when:

- the parent directory listing was fetched successfully,
- that listing is still fresh under the existing `metadata_ttl`,
- the parent has not been invalidated since,
- the child is still a clean file in that loaded parent.

This reuses the existing bounded-staleness contract instead of introducing a new one.

## Why poll refresh is required

A one-time directory freshness stamp at initial load is not enough.

Because `ensure_children_loaded()` short-circuits once `children_loaded == true`, directory freshness would otherwise behave like this:

1. directory loads and gets a freshness timestamp,
2. TTL expires,
3. lookups fall back to per-file `HEAD`,
4. the directory remains `children_loaded == true`, so later `ensure_children_loaded()` calls do not re-fetch,
5. parent freshness never advances again until some unrelated invalidation happens.

That would make the optimization only useful during the first TTL window after each directory load. For long-lived mounts, that is too weak. So poll-based directory freshness refresh is required, not optional.

## Minimal-change design

### 1. Reuse `InodeEntry.last_revalidated` for directory listing freshness

Do not add a new cache map.

Instead, treat `last_revalidated` as:

- file inode: last successful per-file `HEAD` revalidation,
- directory inode: last successful direct-child listing confirmation.

Why this is minimal:

- the field already exists on every inode (`src/virtual_fs/inode.rs:58`),
- it already uses the same `Instant` + `metadata_ttl` model we want,
- it avoids adding new locks, maps, config, or invalidation plumbing.

### 2. Stamp directory freshness on successful listing fetches

On the success path in `ensure_children_loaded()` (`src/virtual_fs/mod.rs:366`), after the fetched listing has been fully applied and after `children_loaded = true` is set, also set:

```rust
parent.last_revalidated = Some(Instant::now());
```

Meaning: "this directory's direct children are known fresh as of now."

Important details:

- stamp only after the listing has been successfully applied, never before,
- do not stamp on the short-circuit path where `children_loaded == true` and no fetch happens.

### 3. Clear directory freshness whenever children are invalidated

Extend `InodeTable::invalidate_children()` (`src/virtual_fs/inode.rs:323`) to also clear the directory freshness timestamp:

```rust
entry.children_loaded = false;
entry.last_revalidated = None;
```

That ensures `children_loaded == false` and `last_revalidated.is_none()` continue to move together after remote invalidation.

### 4. Teach `lookup()` to trust a fresh parent directory

In the fast path in `VirtualFs::lookup()` (`src/virtual_fs/mod.rs:734`), when the parent directory is already loaded and a child entry exists:

- if the child is not a clean file, keep current behavior,
- if the child is a clean file, first check whether the parent directory itself is fresh,
- if the parent is fresh, return the cached child attr immediately,
- otherwise fall back to the existing per-file `revalidate_file()` path.

Concretely, the decision should become:

- `serve_lookup_from_cache == true`
- `parent.children_loaded == true`
- `parent.last_revalidated.is_some()`
- `parent.last_revalidated.elapsed() < metadata_ttl`
- child is `InodeKind::File`
- child is not dirty

If all are true, return `FastResult::Hit(...)` instead of `FastResult::NeedsRevalidation { ... }`.

Locking note:

- compute `parent_fresh` and inspect the child under the same inode-table read lock already used by the fast path,
- do not drop and re-acquire between the parent freshness check and child attribute selection,
- this avoids a TOCTOU gap inside the optimization decision.

Important: this parent-freshness shortcut should only apply when `serve_lookup_from_cache` is true. In minimal mode, preserve the current eager-`HEAD` semantics.

### 5. Refresh directory freshness during successful poll confirmations

This step is required.

When `poll_remote_changes()` successfully fetches a loaded prefix and `apply_poll_diff()` does not invalidate that directory, refresh that directory inode's `last_revalidated` to `Instant::now()`.

Minimal implementation shape:

- keep `polled_prefixes` as the set of successfully fetched prefixes,
- after applying the diff, stamp `last_revalidated = Some(now)` for directory inodes whose prefix is in `polled_prefixes` and whose `children_loaded` is still true,
- skip any directory that was invalidated in the same diff, because its `children_loaded` will already be false.

Why this is required:

- it reuses already-paid-for poll traffic,
- it keeps active directories fresh across long-lived mounts,
- it prevents the optimization from collapsing back into per-file `HEAD`s once the first TTL window expires.

This does not weaken correctness, because the poll refresh only happens after a successful listing fetch for that exact directory prefix.

## Safety invariants

The implementation must preserve these invariants.

### Invariant 1: No suppression without a successful parent listing fetch

A child file may skip `HEAD` only if its parent directory has:

- `children_loaded == true`, and
- a non-expired directory freshness timestamp.

If the directory has never been loaded, was invalidated, was not successfully polled, or its freshness expired, the existing logic must run.

### Invariant 2: Dirty files never trust remote metadata over local state

Keep the current rule: dirty files do not use remote revalidation to overwrite local state.

The new parent-freshness shortcut must apply only to clean files, matching the current `lookup()` gate.

### Invariant 3: Parent invalidation must immediately disable child `HEAD` suppression

Whenever a directory may be stale because of remote discovery of new entries or any future invalidation path, both of these must become false together:

- `children_loaded`
- directory freshness timestamp validity

That is why `invalidate_children()` should clear both.

### Invariant 4: Local mutations remain authoritative for loaded parents

For loaded parents, local operations such as `create`, `mkdir`, `unlink`, `rename`, `rmdir`, and `symlink` already mutate the in-memory directory state after forcing `ensure_children_loaded()` where needed.

That means trusting a fresh loaded parent remains correct for local changes too; no extra invalidation step is needed just to preserve local correctness.

### Invariant 5: Bounded staleness must not exceed the existing TTL contract

This change must not create any new stale-data window longer than the one already accepted by:

- `serve_lookup_from_cache`,
- `metadata_ttl`,
- kernel metadata caching.

We are only moving the trust anchor from per-file `HEAD` timestamps to parent listing timestamps, using the same TTL budget.

### Invariant 6: Freshness remains direct-child only

A directory freshness stamp only proves the state of that directory's direct children, not the whole subtree.

Do not propagate directory freshness recursively.

## Why this remains correct

### Remote updates to existing files

A remote update to `parent/file.txt` after the parent listing was fetched can be missed until the TTL expires.
That is already consistent with the existing cache mode, where a recent file-level `last_revalidated` also suppresses new `HEAD`s until TTL expiry.

### Remote creation of sibling files

This is already handled by the poll invalidation path for loaded directories.
When poll observes new remote paths under a loaded directory, it calls `invalidate_children(dir_ino)`, which will force a refetch on the next lookup or `readdir()`.

### Remote deletion of the current child

If the parent listing freshness expires, lookup falls back to the existing per-file `HEAD` path and still detects deletion.
Within the TTL window, bounded staleness is unchanged from today's cache model.

### Negative cache safety

Negative-cache behavior does not need to change.
Misses in a loaded parent should still insert into the negative cache; local creates and renames already clear affected entries.
Remote additions under loaded parents are already handled by poll invalidation, which clears negative-cache entries for changed directories.

## Scope note: symlinks

This optimization intentionally targets clean direct child files only.

Reason:

- the current per-file `HEAD` path is only taken for `InodeKind::File` in `lookup()` (`src/virtual_fs/mod.rs:757`),
- symlinks already bypass `HEAD` in the fast path today.

So symlink handling does not need to change for v1. If symlink-specific remote revalidation is ever introduced later, parent-directory freshness could be reconsidered there too.

## Low-cost guardrails

Because `last_revalidated` now has dual semantics, add debug-only assertions to catch accidental cross-use during development:

- in `revalidate_file()`, assert the inode being revalidated is a file,
- in the new parent-freshness branch in `lookup()`, assert the parent inode is a directory.

This gives a type-like backstop without adding runtime cost in release builds.

## Concrete implementation steps

1. Update comments around `last_revalidated` in `src/virtual_fs/inode.rs` to state that it now applies to both file `HEAD` validation and directory listing freshness.
2. In `ensure_children_loaded()` (`src/virtual_fs/mod.rs:366`), stamp the parent directory inode's `last_revalidated = Some(Instant::now())` only after the listing is successfully applied and `children_loaded = true` is set.
3. In `InodeTable::invalidate_children()` (`src/virtual_fs/inode.rs:323`), clear both `children_loaded` and `last_revalidated`.
4. In `VirtualFs::lookup()` (`src/virtual_fs/mod.rs:731`), inspect the parent inode's freshness while holding the existing read lock and return `FastResult::Hit(...)` for a clean direct file when the parent directory is still fresh.
5. Keep the current `FastResult::NeedsRevalidation` fallback unchanged for all other cases.
6. In the poll path (`src/virtual_fs/poll.rs:12`), refresh directory freshness timestamps for successfully fetched loaded prefixes that remain `children_loaded == true` after diff application.
7. Add debug-only assertions around file-vs-directory use of `last_revalidated`.

## Suggested code shape

The `lookup()` fast-path check should stay local and simple. Avoid adding a new helper unless the branch becomes hard to read.

One straightforward pattern is:

- compute `parent_fresh` while reading `parent_entry`,
- inspect the child under the same read lock,
- if child is a clean file and `serve_lookup_from_cache && parent_fresh`, return `Hit`,
- else preserve current `NeedsRevalidation` behavior.

That keeps the lock scope and control flow nearly identical to today's code.

For poll refresh, prefer updating timestamps in one place after the diff has decided which directories remain loaded. That avoids duplicated "was this prefix confirmed fresh?" logic.

## Tests to add

Add focused unit tests in `src/virtual_fs/tests.rs`.

### New tests

- `lookup_fresh_parent_skips_head_for_clean_file`
  - load a file through parent listing,
  - mutate mock `head_file` response to a different size or hash,
  - perform another lookup within TTL,
  - assert the old cached metadata is returned because parent freshness suppresses `HEAD`.

- `lookup_stale_parent_falls_back_to_head`
  - load a file,
  - manually clear or age the parent directory freshness timestamp,
  - change mock `head_file` response,
  - assert lookup now observes the updated metadata.

- `lookup_parent_ttl_boundary`
  - load a file,
  - set the parent freshness timestamp to just inside the TTL boundary and verify lookup still skips `HEAD`,
  - then set it to just outside the boundary and verify lookup falls back to `HEAD`,
  - this protects the `<` versus `<=` comparison from regressions.

- `invalidate_children_clears_parent_freshness`
  - load a directory,
  - assert `children_loaded == true` and parent freshness is set,
  - call `invalidate_children(dir_ino)`,
  - assert `children_loaded == false` and `last_revalidated == None`.

- `poll_refreshes_loaded_directory_freshness`
  - load a directory,
  - age its freshness timestamp beyond TTL while leaving `children_loaded == true`,
  - simulate a successful poll for that prefix with no invalidation,
  - assert the directory freshness timestamp is refreshed and a subsequent lookup skips per-file `HEAD`.

- `dirty_file_does_not_use_parent_freshness_shortcut`
  - make a file dirty,
  - ensure lookup still follows the existing dirty-file path and never claims remote freshness from the parent.

### Existing tests that should still pass unchanged

- `lookup_ttl_skips_head_within_window` (`src/virtual_fs/tests.rs:779`)
- `revalidation_detects_hash_change` (`src/virtual_fs/tests.rs:1964`)
- `poll_detects_remote_deletion_via_revalidation` (`src/virtual_fs/tests.rs:1996`)
- poll invalidation tests around loaded directories (`src/virtual_fs/tests.rs:2056`, `src/virtual_fs/tests.rs:2102`, `src/virtual_fs/tests.rs:2271`)

## Risks and how to avoid them

### Risk: confusing file and directory semantics on one field

Mitigation:

- document `last_revalidated` clearly as "last remote freshness confirmation" with file-vs-directory meaning,
- guard call sites with kind checks,
- add debug-only assertions to catch accidental misuse in tests.

### Risk: parent freshness outliving invalidation

Mitigation:

- make `invalidate_children()` clear the timestamp in the same method that clears `children_loaded`,
- do not duplicate invalidation logic in multiple places.

### Risk: over-expanding scope into recursive freshness

Mitigation:

- define freshness only for a directory's direct children,
- do not attempt subtree freshness propagation,
- do not modify `readdir()` semantics beyond the same `ensure_children_loaded()` stamp and poll refresh.

### Risk: poll refresh stamping a directory that was actually invalidated

Mitigation:

- only refresh timestamps for prefixes whose directory inode still has `children_loaded == true` after diff application,
- never refresh timestamps before the diff has applied its invalidations.

## Non-goals

- No new config flag.
- No new global cache structure.
- No change to negative-cache TTL or capacity behavior.
- No attempt to eliminate all `HEAD`s; stale parents and non-fast-path cases should still use the existing logic.
- No recursive directory freshness or subtree-level trust model.

## Recommended implementation order

1. Reuse `last_revalidated` for directories and document the dual semantics.
2. Stamp it in `ensure_children_loaded()` after successful listing application.
3. Clear it in `invalidate_children()`.
4. Add the parent-freshness shortcut in `lookup()`.
5. Add the poll-based directory freshness refresh.
6. Add debug-only assertions.
7. Add tests for fresh-parent skip, stale-parent fallback, TTL boundary, invalidation clearing, and poll refresh.

## Expected outcome

After this change, repeated lookups of clean files inside actively used loaded directories should usually avoid per-file `HEAD`s, while preserving the current correctness envelope:

- loaded parent directories remain the trust boundary,
- invalidation still forces refetch,
- successful polls keep active directories fresh over long-lived mounts,
- dirty files remain protected,
- worst-case staleness remains bounded by the existing metadata TTL.
