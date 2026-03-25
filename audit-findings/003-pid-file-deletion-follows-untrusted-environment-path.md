# PID file deletion trusts attacker-controlled environment path

## Classification
- Type: trust-boundary violation
- Severity: medium
- Confidence: certain

## Affected Locations
- `src/daemon.rs:126`

## Summary
`DaemonGuard::from_env()` accepted `HF_MOUNT_DAEMON_PID_FILE` from the process environment and stored it in `pid_file`. On normal guard drop, `Drop` unconditionally called `std::fs::remove_file(&self.pid_file)`. Because the value was not recomputed or validated against the daemon state directory, an attacker who controlled the daemon environment could cause deletion of an attacker-selected path reachable by the daemon process.

## Provenance
- Verified from the supplied reproducer and source review
- Reproduced against the affected code path described in the finding
- Scanner provenance: https://swival.dev

## Preconditions
- Attacker controls daemon process environment variables
- The daemon reaches `DaemonGuard::from_env()`
- The daemon later terminates via normal drop/unwind after a successful mount path; early mount failures using `std::process::exit(1)` do not trigger this deletion

## Proof
`DaemonGuard::from_env()` read `HF_MOUNT_DAEMON_PID_FILE` directly from the environment into `pid_file`. `Drop` later executed `std::fs::remove_file(&self.pid_file)` unconditionally. The reproducer confirmed a backend can be launched with attacker-chosen `_MOUNT_DAEMON_FD` and `HF_MOUNT_DAEMON_PID_FILE`, creating a guard whose drop deletes the supplied path with daemon privileges. `notify_ready()` only affects the readiness pipe byte and does not alter the later `remove_file` behavior. Reproduction also established the practical boundary: early mount-error exits in `src/bin/hf-mount-fuse.rs:12` and `src/bin/hf-mount-nfs.rs:16` use `std::process::exit(1)`, so the bug requires a successful mount followed by normal termination or another drop path.

## Why This Is A Real Bug
The environment is a lower-trust input boundary than internal daemon state. Using an untrusted environment path for later privileged file deletion creates a local arbitrary-file deletion primitive for any path the daemon process can remove. The deletion happens in a separate lifecycle phase from parsing, making the impact non-obvious but reliable on normal shutdown.

## Fix Requirement
Do not trust `HF_MOUNT_DAEMON_PID_FILE` for deletion. Recompute the PID file path internally from trusted state, or strictly validate that any supplied path resolves under `state_dir()` and matches the daemon's expected PID path before using it.

## Patch Rationale
The patch removes trust in the environment-supplied PID file path and derives the cleanup target from trusted internal state instead. That preserves daemon cleanup behavior while eliminating attacker control over the path passed to `remove_file`. This directly addresses the only unsafe trust transition needed for exploitation.

## Residual Risk
None

## Patch
- Patched in `003-pid-file-deletion-follows-untrusted-environment-path.patch`
- The fix aligns PID-file cleanup with a trusted internally derived path rather than `HF_MOUNT_DAEMON_PID_FILE`
- This prevents attacker-controlled environment values from influencing `Drop`-time file deletion