# Untrusted args file can redirect FUSE mount source

## Classification
- Type: trust-boundary violation
- Severity: medium
- Confidence: certain

## Affected Locations
- `src/bin/hf-mount-fuse-sidecar.rs:156`

## Summary
The sidecar trusts mount arguments loaded from `tmp_dir/.volumes/*/args`, a shared `emptyDir`, and uses them to build the FUSE mount configuration. An attacker who can write that args file can redirect the served backing source and alter mount options for a launched mount.

## Provenance
- Verified from repository source and reproducer evidence
- Scanner: https://swival.dev

## Preconditions
- Attacker can write the shared `tmp_dir` volume args file

## Proof
`discover_pending()` reads each `tmp_dir/.volumes/*/args` file with `read_to_string`, splits lines into argv-style tokens, and passes them directly into `MountArgs::try_parse_from`. The parsed values then flow into `run_mount()`, where `build()` consumes attacker-controlled fields such as `source` and `options`.

The args file is read from a shared temporary volume and no provenance check binds it to the CSI driver, validates file ownership, or authenticates contents before parsing. A valid file only needs the expected argv[0]-style leading token; an attacker modifying an existing legitimate file can preserve that token and replace the remaining arguments.

Reproduction confirmed that in sidecar mode the FUSE fd still comes from the CSI path, so this is somewhat narrower than arbitrary kernel mount creation. However, the attacker still controls what backing repo or bucket the daemon serves and which local paths or files it reads or creates.

## Why This Is A Real Bug
This crosses a trust boundary: untrusted data from a shared writable volume is treated as authoritative mount configuration. Even if the kernel mountpoint is already established via the CSI-provided FUSE fd, the sidecar still uses attacker-influenced configuration to decide the remote source and operational options exposed through that mount. That enables unauthorized data redirection and unintended filesystem interactions under the stated precondition.

## Fix Requirement
Reject unauthenticated args files before parsing. The sidecar must verify provenance for each pending mount configuration, such as enforcing trusted ownership and path constraints or requiring authenticated, driver-produced configuration material.

## Patch Rationale
The patch in `001-untrusted-args-file-controls-mount-configuration.patch` adds provenance enforcement before mount args are accepted. This closes the trust-boundary gap at the ingestion point, preventing shared-volume writers from steering `source` or `options` through forged or modified args files.

## Residual Risk
None

## Patch
- `001-untrusted-args-file-controls-mount-configuration.patch` validates args file provenance before parsing and rejects untrusted mount configurations.