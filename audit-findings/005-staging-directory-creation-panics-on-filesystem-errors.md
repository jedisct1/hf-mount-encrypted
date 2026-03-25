# Staging directory creation panics on filesystem errors

## Classification
- Type: error-handling bug
- Severity: medium
- Confidence: certain

## Affected Locations
- `src/xet.rs:173`

## Summary
`StagingDir::new` constructs `cache_dir/staging` and calls `std::fs::create_dir_all` with `unwrap_or_else`, panicking on filesystem errors instead of returning a recoverable error. This turns a setup-time directory creation failure into process abort/denial of service.

## Provenance
- Verified from provided reproducer and source inspection
- Reproduced against the current code path in `src/xet.rs`
- Scanner reference: https://swival.dev

## Preconditions
- An attacker or local user can influence the cache layout used for startup
- `cache_dir` itself is creatable, but `cache_dir/staging` cannot be created as a directory, such as when a regular file already exists at that path

## Proof
`StagingDir::new` joins caller-influenced `cache_dir` with `staging` and invokes directory creation through a panic-on-error path in `src/xet.rs:173`. If `cache_dir/staging` is blocked by a non-directory filesystem object, `create_dir_all` returns an error and the process aborts rather than propagating failure through `Result`.

This is practically reachable:
- setup already creates `cache_dir`, so making `cache_dir` itself uncreatable is not the relevant trigger
- placing a regular file at `cache_dir/staging` causes `create_dir_all(cache_dir)` to succeed earlier, while `create_dir_all(cache_dir/staging)` fails with `File exists`
- multiple binaries initialize staging from startup paths, so the panic is reachable during normal program initialization

## Why This Is A Real Bug
The function contract is inconsistent with the surrounding error-handling model: initialization APIs return `Result`, but this branch unconditionally aborts on an expected I/O failure mode. Filesystem layout conflicts are routine operational errors, not invariants. Because startup reaches this code before normal service, an attacker or local user who can shape the cache directory contents can reliably deny service.

## Fix Requirement
Change `StagingDir::new` to return `Result<Self>` and propagate `create_dir_all` failures as the crate error type instead of panicking.

## Patch Rationale
The patch removes the `unwrap_or_else` panic path and converts staging directory creation into ordinary error propagation. This preserves existing startup error reporting behavior, allows callers to handle initialization failure uniformly, and eliminates process abort on hostile or inconsistent cache layouts.

## Residual Risk
None

## Patch
`005-staging-directory-creation-panics-on-filesystem-errors.patch` updates `src/xet.rs` so `StagingDir::new` returns a `Result` and surfaces `create_dir_all` errors through normal error handling instead of panicking.