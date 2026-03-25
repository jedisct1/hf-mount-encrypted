---
name: verify
description: Run clippy and unit tests with all features enabled to verify changes are correct
---

Run the following commands in sequence, stopping on first failure:

1. `cargo clippy --features fuse,nfs -- -D warnings` — lint check
2. `cargo test --lib --features fuse,nfs` — unit tests

Report results clearly. If clippy or tests fail, analyze the errors and suggest fixes.
