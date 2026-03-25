## Workflow

- install: `curl -fsSL https://raw.githubusercontent.com/huggingface/hf-mount/main/install.sh | sh`
- install: `brew install macfuse`
- build: `cargo build --release --features fuse,nfs`
- test all: `cargo test --lib --features fuse,nfs`
- test file: `cargo test --lib --features fuse,nfs <module_path>:: -- --nocapture`
- test case: `cargo test --lib --features fuse,nfs <module_path>::<test_name> -- --nocapture`
- lint: `cargo clippy --features fuse,nfs -- -D warnings`
- format: `cargo +nightly fmt --check`
- after every edit: `cargo +nightly fmt --check && cargo clippy -- -D warnings && cargo clippy --features nfs -- -D warnings && cargo clippy --features fuse -- -D warnings && cargo clippy --features fuse,nfs -- -D warnings && cargo test --lib --features fuse,nfs`
- debug: `hf-mount-nfs repo gpt2 /tmp/gpt2`; `hf-mount-fuse --hf-token $HF_TOKEN bucket myuser/my-bucket /mnt/data`; `RUST_LOG=hf_mount=debug hf-mount-fuse repo gpt2 /mnt/gpt2`; `umount /tmp/data`; `hf-mount stop /tmp/data`; `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/$label.plist`; `launchctl bootout gui/$(id -u) ~/Library/LaunchAgents/$label.plist`

## Conventions

- Always pass explicit Cargo features when building, linting, or testing. The repo treats feature selection as part of normal development, not an optional variant.
- Repo mounts are always read-only even if `--read-only` is omitted. Do not add writable repo flows.
- Writable NFS mounts implicitly use advanced writes. Do not assume NFS matches default FUSE streaming-write behavior.
- On macOS, prefer `cargo test --lib --features fuse,nfs` for local validation. The checked-in integration harness still contains Linux-specific mount detection and unmount commands.
- Keep Hub/Xet access behind `HubOps` and `XetOps`. Tests rely on swapping in `MockHub` and `MockXet` instead of concrete clients.
- In `src/virtual_fs`, follow the documented lock ordering exactly. Treat it as a correctness constraint, not a comment.