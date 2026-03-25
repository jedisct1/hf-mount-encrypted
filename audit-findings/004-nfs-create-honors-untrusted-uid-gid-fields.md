# NFS CREATE trusts client uid/gid

## Classification
- Type: trust-boundary violation
- Severity: medium
- Confidence: certain

## Affected Locations
- `src/nfs.rs:261`

## Summary
`CREATE` accepted client-supplied `uid` and `gid` from `sattr3` and forwarded them unchanged into the virtual filesystem create path. A writable NFS client could therefore choose the created inode's reported owner/group for the lifetime of the current mount session, instead of using server-derived defaults.

## Provenance
- Verified from the supplied reproducer and code inspection
- Scanner source: https://swival.dev

## Preconditions
- Writable NFS client can send `CREATE` with `uid`/`gid` attributes

## Proof
At `src/nfs.rs:261`, `create` read `attr.uid` and `attr.gid` from client-controlled `sattr3` and assigned them directly to the `uid` and `gid` passed to `create_file`. That path then called `virtual_fs.create(dirid, name, mode, uid, gid, None)` without validation or remapping.

Reproduction confirmed the effect is session-scoped metadata spoofing:
- Ownership fields are not persisted to the Hub/Xet backend: `src/virtual_fs/flush.rs:201`, `src/virtual_fs/flush.rs:228`
- Reload/remount recreates remote entries with server defaults: `src/virtual_fs/mod.rs:443`, `src/virtual_fs/mod.rs:470`, `src/setup.rs:329`
- Access-control checks do not rely on inode owner/group on this path: `src/virtual_fs/mod.rs:1798`, `src/virtual_fs/mod.rs:1923`, `src/virtual_fs/mod.rs:2065`, `src/virtual_fs/mod.rs:2552`

## Why This Is A Real Bug
The server crossed a trust boundary by honoring unauthenticated ownership metadata from the client during file creation. Even though the effect is not durable across remount and does not directly bypass authorization in the reproduced code paths, it still lets a client forge file ownership as observed through NFS during the active mount. That creates false metadata, breaks server-side ownership invariants, and can mislead operators or dependent tooling.

## Fix Requirement
Ignore client-supplied `uid` and `gid` during `CREATE` unless they are derived from trusted server-side identity mapping. Use server defaults or authenticated credentials instead.

## Patch Rationale
The patch in `004-nfs-create-honors-untrusted-uid-gid-fields.patch` removes reliance on `sattr3.uid` and `sattr3.gid` in the `CREATE` path and uses server-derived ownership values instead. This directly closes the trust-boundary violation while preserving create behavior for mode and content.

## Residual Risk
None

## Patch
`004-nfs-create-honors-untrusted-uid-gid-fields.patch` updates `src/nfs.rs` so `CREATE` no longer propagates client-controlled `uid`/`gid` into `virtual_fs.create(...)`, ensuring created entries use trusted server ownership defaults.