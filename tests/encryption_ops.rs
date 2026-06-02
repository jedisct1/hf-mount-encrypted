mod common;

#[cfg(all(feature = "encrypt", feature = "nfs"))]
type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[cfg(all(feature = "encrypt", feature = "nfs"))]
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[cfg(all(feature = "encrypt", feature = "nfs"))]
fn assert_bytes_eq(label: &str, actual: &[u8], expected: &[u8]) {
    if actual == expected {
        return;
    }
    let first_mismatch = actual
        .iter()
        .zip(expected.iter())
        .position(|(a, b)| a != b)
        .map(|i| format!("first mismatch at {i}: actual={} expected={}", actual[i], expected[i]))
        .unwrap_or_else(|| "one buffer is a prefix of the other".to_string());
    panic!(
        "{label}: byte mismatch: actual_len={} expected_len={}; {first_mismatch}",
        actual.len(),
        expected.len(),
    );
}

#[cfg(all(feature = "encrypt", feature = "nfs"))]
fn with_encrypted_nfs_mount<F>(
    bucket_id: &str,
    mount_point: &str,
    cache_dir: &str,
    key_file: &str,
    graceful_secs: u64,
    body: F,
) -> TestResult
where
    F: FnOnce(&str) -> TestResult,
{
    let child = common::mount_bucket_nfs(
        bucket_id,
        mount_point,
        cache_dir,
        &["--encryption-key-file", key_file, "--flush-debounce-ms", "100"],
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(mount_point)));
    common::unmount_nfs(mount_point, child, graceful_secs);
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Fail if any raw listing entry still carries one of the plaintext names the
/// test writes.
#[cfg(all(feature = "encrypt", feature = "nfs"))]
fn check_no_plaintext_leak(entries: &[hf_mount::hub_api::TreeEntry], context: &str) -> Result<(), String> {
    if entries
        .iter()
        .any(|e| e.path.contains("secret-dir") || e.path.contains("alpha.bin"))
    {
        return Err(format!("plaintext leaked in raw {context} listing: {entries:?}"));
    }
    Ok(())
}

#[cfg(all(feature = "encrypt", feature = "nfs"))]
async fn assert_raw_hub_is_encrypted(
    hub: &hf_mount::hub_api::HubApiClient,
    expected_plaintext_len: usize,
) -> Result<(), String> {
    use hf_mount::encryption::ENC_ROOT;

    let root_entries = hub.list_tree("").await.map_err(|e| format!("list root: {e}"))?;

    // The raw root listing must contain exactly one entry — a directory named .enc.
    if root_entries.len() != 1 {
        return Err(format!(
            "raw root listing should have exactly 1 entry ({ENC_ROOT}), got {}: {root_entries:?}",
            root_entries.len()
        ));
    }
    let enc_dir = &root_entries[0];
    if enc_dir.path != ENC_ROOT {
        return Err(format!(
            "raw root entry must be the {ENC_ROOT} directory, got {:?}: {root_entries:?}",
            enc_dir.path
        ));
    }
    if enc_dir.entry_type != "directory" {
        return Err(format!(
            "{ENC_ROOT} must be a directory, got {}: {root_entries:?}",
            enc_dir.entry_type
        ));
    }
    check_no_plaintext_leak(&root_entries, "root")?;

    let child_entries = hub
        .list_tree(ENC_ROOT)
        .await
        .map_err(|e| format!("list {ENC_ROOT} dir: {e}"))?;
    check_no_plaintext_leak(&child_entries, "child")?;

    // The only written file is secret-dir/alpha.bin and listings are
    // non-recursive, so .enc's direct child is the encrypted directory; the
    // encrypted file lives one level below it.
    let encrypted_dir = child_entries
        .iter()
        .find(|e| e.entry_type == "directory")
        .ok_or_else(|| format!("missing encrypted directory under {ENC_ROOT}: {child_entries:?}"))?;
    let file_entries = hub
        .list_tree(&encrypted_dir.path)
        .await
        .map_err(|e| format!("list encrypted dir: {e}"))?;
    check_no_plaintext_leak(&file_entries, "file")?;

    let file = file_entries
        .iter()
        .find(|e| e.entry_type == "file")
        .ok_or_else(|| format!("missing encrypted file in raw listing: {file_entries:?}"))?;
    let expected_cipher_size = hf_mount::encryption::content::ciphertext_size(expected_plaintext_len as u64);
    match file.size {
        Some(size) if size == expected_cipher_size => Ok(()),
        other => Err(format!(
            "raw file size should be ciphertext size {expected_cipher_size}, got {other:?}"
        )),
    }
}

#[cfg(all(feature = "encrypt", feature = "nfs"))]
#[tokio::test]
async fn test_nfs_encrypted_round_trip_live_bucket() {
    let guard = match common::setup_bucket("nfs-encrypt").await {
        Some(g) => g,
        None => return,
    };

    let pid = std::process::id();
    let mount_point = format!("/tmp/hf-mount-nfs-enc-mnt-{pid}");
    let cache_dir = format!("/tmp/hf-mount-nfs-enc-cache-{pid}");
    let key_file = format!("/tmp/hf-mount-nfs-enc-key-{pid}.bin");
    std::fs::write(&key_file, [0x42u8; 32]).expect("write encryption key");

    let original = payload(90_000);
    let file_rel = "secret-dir/alpha.bin";

    let first = with_encrypted_nfs_mount(&guard.bucket_id, &mount_point, &cache_dir, &key_file, 30, |mp| {
        let dir = format!("{mp}/secret-dir");
        std::fs::create_dir(&dir)?;
        let path = format!("{mp}/{file_rel}");
        std::fs::write(&path, &original)?;
        assert_eq!(std::fs::metadata(&path)?.len(), original.len() as u64);
        assert_bytes_eq("first mount readback", &std::fs::read(&path)?, &original);

        let root_names: Vec<String> = std::fs::read_dir(mp)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(root_names.contains(&"secret-dir".to_string()));
        let child_names: Vec<String> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(child_names.contains(&"alpha.bin".to_string()));
        Ok(())
    });
    std::fs::remove_dir_all(&mount_point).ok();
    if let Err(e) = first {
        std::fs::remove_dir_all(&cache_dir).ok();
        std::fs::remove_file(&key_file).ok();
        panic!("encrypted first mount failed: {e}");
    }

    assert_raw_hub_is_encrypted(&guard.hub, original.len()).await.unwrap();

    let mut edited = original.clone();
    let patch = b"patched through encrypted existing-file rewrite";
    edited[1234..1234 + patch.len()].copy_from_slice(patch);
    edited.truncate(70_000);

    let second = with_encrypted_nfs_mount(&guard.bucket_id, &mount_point, &cache_dir, &key_file, 30, |mp| {
        let path = format!("{mp}/{file_rel}");
        assert_bytes_eq("second mount initial read", &std::fs::read(&path)?, &original);

        use std::io::{Seek, SeekFrom, Write};
        let mut file = std::fs::OpenOptions::new().read(true).write(true).open(&path)?;
        file.seek(SeekFrom::Start(1234))?;
        file.write_all(patch)?;
        file.set_len(70_000)?;
        drop(file);

        assert_eq!(std::fs::metadata(&path)?.len(), edited.len() as u64);
        assert_bytes_eq("second mount edited readback", &std::fs::read(&path)?, &edited);
        Ok(())
    });
    std::fs::remove_dir_all(&mount_point).ok();
    if let Err(e) = second {
        std::fs::remove_dir_all(&cache_dir).ok();
        std::fs::remove_file(&key_file).ok();
        panic!("encrypted rewrite mount failed: {e}");
    }

    assert_raw_hub_is_encrypted(&guard.hub, edited.len()).await.unwrap();

    let third = with_encrypted_nfs_mount(&guard.bucket_id, &mount_point, &cache_dir, &key_file, 30, |mp| {
        let path = format!("{mp}/{file_rel}");
        assert_eq!(std::fs::metadata(&path)?.len(), edited.len() as u64);
        assert_bytes_eq("third mount final read", &std::fs::read(&path)?, &edited);
        Ok(())
    });

    std::fs::remove_dir_all(&mount_point).ok();
    std::fs::remove_dir_all(&cache_dir).ok();
    std::fs::remove_file(&key_file).ok();

    if let Err(e) = third {
        panic!("encrypted final remount failed: {e}");
    }
}

#[cfg(not(all(feature = "encrypt", feature = "nfs")))]
#[test]
fn encryption_ops_requires_nfs_and_encrypt_features() {
    eprintln!("Skipping encryption_ops: requires --features nfs,encrypt");
}
