#!/bin/bash
#
# Proof of concept: parent-directory freshness cache
#
# Demonstrates that the optimization eliminates per-file HEAD requests
# on repeated file access in an actively polled directory.
#
# Setup: a HuggingFace bucket with 20 files, mounted via NFS with
# client-side caching disabled (noac) so every file access triggers
# a VFS lookup() call, making HEAD request counts directly observable.
#
set -euo pipefail

BUCKET="jedisct1/hf-mount-head-test-86701"
MOUNT=/tmp/hf-mount-poc
CACHE=/tmp/hf-mount-poc-cache
LOG=/tmp/hf-mount-poc.log

run_test() {
    local binary="$1"
    local label="$2"

    rm -rf "$CACHE"
    mkdir -p "$MOUNT" "$CACHE"

    # Start the NFS server. It picks a random port and prints it.
    # We'll capture the port from the log, then mount manually with noac.
    RUST_LOG=hf_mount=debug "$binary" \
        --hf-token "$HF_TOKEN" \
        --cache-dir "$CACHE" \
        --poll-interval-secs 2 \
        --metadata-ttl-ms 60000 \
        bucket "$BUCKET" "$MOUNT" \
        2>"$LOG" &
    local pid=$!

    # Wait for mount
    for i in $(seq 1 15); do
        if ls "$MOUNT" >/dev/null 2>&1; then break; fi
        sleep 1
    done

    # Remount with noac to disable NFS client caching entirely.
    # This forces the kernel to call LOOKUP on every name resolution.
    local port
    port=$(grep "NFS server listening" "$LOG" | grep -o '[0-9]*$')
    umount "$MOUNT" 2>/dev/null || true
    sleep 1
    mount_nfs -o "noac,nolocks,vers=3,tcp,rsize=1048576,port=${port},mountport=${port},wsize=1048576" \
        127.0.0.1:/ "$MOUNT"
    sleep 1

    echo "=== $label ==="

    # 1) Prime: first access loads the directory listing.
    > "$LOG"
    for f in $(seq -w 1 20); do
        stat "$MOUNT/file_${f}.txt" > /dev/null 2>&1
    done
    sleep 1
    local h1=$(grep -c "HEAD " "$LOG" 2>/dev/null || echo 0)
    local l1=$(grep -c "lookup:.*file_" "$LOG" 2>/dev/null || echo 0)
    echo "  1st pass (load dir):    $l1 lookups, $h1 HEADs"

    # 2) Immediate re-access: with noac, kernel re-LOOKUPs every file.
    > "$LOG"
    for f in $(seq -w 1 20); do
        stat "$MOUNT/file_${f}.txt" > /dev/null 2>&1
    done
    sleep 1
    local h2=$(grep -c "HEAD " "$LOG" 2>/dev/null || echo 0)
    local l2=$(grep -c "lookup:.*file_" "$LOG" 2>/dev/null || echo 0)
    echo "  2nd pass (re-access):   $l2 lookups, $h2 HEADs"

    # 3) Third pass for consistency.
    > "$LOG"
    for f in $(seq -w 1 20); do
        stat "$MOUNT/file_${f}.txt" > /dev/null 2>&1
    done
    sleep 1
    local h3=$(grep -c "HEAD " "$LOG" 2>/dev/null || echo 0)
    local l3=$(grep -c "lookup:.*file_" "$LOG" 2>/dev/null || echo 0)
    echo "  3rd pass (re-access):   $l3 lookups, $h3 HEADs"

    umount "$MOUNT" 2>/dev/null || true
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    echo ""
}

run_test /tmp/hf-mount-nfs-old "BEFORE (no parent-freshness cache)"
run_test /tmp/hf-mount-nfs-new "AFTER  (with parent-freshness cache)"
