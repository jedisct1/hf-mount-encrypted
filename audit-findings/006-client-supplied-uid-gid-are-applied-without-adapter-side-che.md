# Client-supplied uid/gid are applied without adapter-side checks

## Classification
- Type: trust-boundary violation
- Severity: medium
- Confidence: certain

## Affected Locations
- `src/nfs.rs:213`

## Summary
- `NFSAdapter::setattr` accepts client-controlled `uid` and `gid` values from NFS `SETATTR` and forwards them to `virtual_fs.setattr(...)` without adapter-side authorization or policy checks.
- This lets a writable mount user request arbitrary ownership metadata changes through standard operations such as `chown` and `chgrp`.

## Provenance
- Verified from the provided finding and reproducer against the repository code.
- Source: Swival Security Scanner (`https://swival.dev`)

## Preconditions
- A client can issue NFS `SETATTR` requests with `uid` and/or `gid` fields.
- The export is writable by that client path.

## Proof
- In `src/nfs.rs:213`, `sattr3.uid` and `sattr3.gid` are decoded directly into optional values and passed unchanged into `self.virtual_fs.setattr(id, size, mode, uid, gid, atime, mtime)`.
- No adapter-side caller identity check, ownership policy enforcement, or value validation occurs before the trust boundary into `virtual_fs`.
- Reproduction confirms a mounted-path user can perform ownership-changing operations that succeed and persist in reported metadata.

## Why This Is A Real Bug
- `uid` and `gid` are security-relevant metadata and must not be accepted from an untrusted client without explicit authorization.
- The lower layer does not re-establish POSIX ownership semantics; therefore the adapter is the effective enforcement point.
- As reproduced, the server accepts attacker-chosen ownership values on writable mounts, so the issue is reachable and impacts integrity of filesystem metadata consumers.

## Fix Requirement
- Reject client-supplied `uid` and `gid` changes in `NFSAdapter::setattr`, or enforce authorization before forwarding them to `virtual_fs.setattr(...)`.

## Patch Rationale
- The patch removes trust in client-supplied ownership changes at the adapter boundary by refusing forwarded `uid`/`gid` updates from `SETATTR`.
- This is the narrowest safe fix because it blocks the unauthorized metadata transition at the first point where untrusted protocol input is decoded.

## Residual Risk
- None

## Patch
- Patched in `006-client-supplied-uid-gid-are-applied-without-adapter-side-che.patch`.
- The fix ensures `NFSAdapter::setattr` no longer applies client-provided `uid`/`gid` values without authorization checks.