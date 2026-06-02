//! Encrypted-file container format and the plaintext/ciphertext size map.
//!
//! An encrypted object is a 6-byte HFEB header (magic, version, algorithm)
//! followed by an AEGIS-RAF container. The RAF container has its own 64-byte
//! header and splits the plaintext into 64 KiB chunks, each stored with a
//! 16-byte nonce and a 16-byte authentication tag. The size map converts
//! between the plaintext size the user sees and the ciphertext size the remote
//! stores. The RAF layout constants are verified against the crate by
//! `size_map_matches_real_raf`.

use std::fmt;

pub const HFEB_MAGIC: [u8; 4] = *b"HFEB";
pub const HFEB_VERSION: u8 = 1;
pub const HFEB_HEADER_LEN: usize = 6;

/// HFEB algorithm byte for AEGIS-128X2 (matches `aegis`'s `AlgorithmId`).
pub const ALG_AEGIS_128X2: u8 = 2;

const RAF_HEADER_LEN: u64 = 64;
/// Plaintext bytes per chunk (the RAF default).
pub const RAF_CHUNK_SIZE: u64 = 65536;
/// Per-chunk overhead: a 16-byte nonce plus a 16-byte tag.
const RAF_CHUNK_OVERHEAD: u64 = 32;
/// On-disk size of one chunk slot. The RAF stores fixed-size slots — the final
/// chunk is padded to a full slot — so a file's on-disk body is always a whole
/// number of these.
const RAF_CHUNK_RECORD: u64 = RAF_CHUNK_SIZE + RAF_CHUNK_OVERHEAD;
/// Full object header length: the HFEB prefix plus the RAF header.
pub const CONTAINER_HEADER_LEN: u64 = HFEB_HEADER_LEN as u64 + RAF_HEADER_LEN;

/// Failure modes when parsing an HFEB header.
#[derive(Debug, PartialEq, Eq)]
pub enum HeaderError {
    Truncated,
    BadMagic,
    UnsupportedVersion(u8),
    UnknownAlgorithm(u8),
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeaderError::Truncated => f.write_str("encrypted file header is truncated"),
            HeaderError::BadMagic => f.write_str("not an HFEB encrypted file"),
            HeaderError::UnsupportedVersion(v) => write!(f, "unsupported HFEB version {v}"),
            HeaderError::UnknownAlgorithm(a) => write!(f, "unknown HFEB algorithm {a}"),
        }
    }
}

impl std::error::Error for HeaderError {}

/// Write the 6-byte HFEB header for `algorithm` into `buf`.
pub fn write_header(buf: &mut [u8; HFEB_HEADER_LEN], algorithm: u8) {
    buf[..4].copy_from_slice(&HFEB_MAGIC);
    buf[4] = HFEB_VERSION;
    buf[5] = algorithm;
}

/// Parse an HFEB header, returning the algorithm byte on success.
pub fn parse_header(bytes: &[u8]) -> Result<u8, HeaderError> {
    if bytes.len() < HFEB_HEADER_LEN {
        return Err(HeaderError::Truncated);
    }
    if bytes[..4] != HFEB_MAGIC {
        return Err(HeaderError::BadMagic);
    }
    if bytes[4] != HFEB_VERSION {
        return Err(HeaderError::UnsupportedVersion(bytes[4]));
    }
    match bytes[5] {
        ALG_AEGIS_128X2 => Ok(ALG_AEGIS_128X2),
        other => Err(HeaderError::UnknownAlgorithm(other)),
    }
}

/// Ciphertext size of the full HFEB container holding `plaintext` plaintext bytes.
pub fn ciphertext_size(plaintext: u64) -> u64 {
    CONTAINER_HEADER_LEN + plaintext.div_ceil(RAF_CHUNK_SIZE) * RAF_CHUNK_RECORD
}

/// Number of chunk slots in a container of `ciphertext` bytes, or `None` if that
/// is not a valid container size. Note this does **not** give the plaintext size:
/// the final chunk is padded to a full slot, so a container only reveals how many
/// chunks it holds, bounding the plaintext to `((n-1)*CHUNK, n*CHUNK]`. The exact
/// plaintext size must come from the RAF header (`probe`) or from our own write.
pub fn container_chunk_count(ciphertext: u64) -> Option<u64> {
    let body = ciphertext.checked_sub(CONTAINER_HEADER_LEN)?;
    (body % RAF_CHUNK_RECORD == 0).then_some(body / RAF_CHUNK_RECORD)
}

/// What a single decrypting read needs to fetch from the remote object.
///
/// A plaintext read touches only the chunk slots that cover it, so we never
/// download the whole file. `header_object` is the RAF header byte range (fetch
/// once and cache); `slots_object` is the byte range of the covering chunk slots
/// (may be empty for a read at or past EOF). Both ranges are in *object* space,
/// i.e. they include the 6-byte HFEB prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPlan {
    pub header_object: std::ops::Range<u64>,
    pub slots_object: std::ops::Range<u64>,
    /// RAF-space offset of the first byte of `slots_object`.
    pub slot_base_raf: u64,
    /// Total RAF container size (object size minus the HFEB header).
    pub raf_size: u64,
}

/// Plan the fetch for a plaintext read of `len` bytes at `off`, given the remote
/// object size. Returns `None` if `object_size` is not a valid container size.
pub fn plan_read(object_size: u64, off: u64, len: u64) -> Option<ReadPlan> {
    let chunks = container_chunk_count(object_size)?;
    let raf_size = object_size - HFEB_HEADER_LEN as u64;
    let hfeb = HFEB_HEADER_LEN as u64;
    let header_object = hfeb..hfeb + RAF_HEADER_LEN;

    let first = off / RAF_CHUNK_SIZE;
    let end = off.saturating_add(len);
    // No slots to fetch for an empty file, a zero-length read, or a read that
    // starts at or past the last chunk.
    if len == 0 || chunks == 0 || first >= chunks {
        return Some(ReadPlan {
            header_object,
            slots_object: 0..0,
            slot_base_raf: RAF_HEADER_LEN,
            raf_size,
        });
    }
    let last = ((end - 1) / RAF_CHUNK_SIZE).min(chunks - 1);
    let slot_base_raf = RAF_HEADER_LEN + first * RAF_CHUNK_RECORD;
    let slot_end_raf = RAF_HEADER_LEN + (last + 1) * RAF_CHUNK_RECORD;
    Some(ReadPlan {
        header_object,
        slots_object: hfeb + slot_base_raf..hfeb + slot_end_raf,
        slot_base_raf,
        raf_size,
    })
}

/// A read-only [`RafIo`](aegis::raf::RafIo) that serves the RAF a cached header
/// plus one fetched slice of chunk slots, reporting the file's true size. This
/// lets [`decrypt_read`] decrypt a ranged read without the whole container in
/// memory. All offsets are RAF-container space (the 6-byte HFEB prefix is already
/// stripped by the caller).
pub struct ReadIo {
    raf_size: u64,
    header: Vec<u8>,
    slot_base: u64,
    slots: Vec<u8>,
}

impl ReadIo {
    /// `header` is the RAF header (offsets `0..header.len()`); `slots` covers RAF
    /// offsets `slot_base..slot_base + slots.len()`; `raf_size` is the full
    /// container size.
    pub fn new(raf_size: u64, header: Vec<u8>, slot_base: u64, slots: Vec<u8>) -> Self {
        Self {
            raf_size,
            header,
            slot_base,
            slots,
        }
    }
}

impl aegis::raf::RafIo for ReadIo {
    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        let end = offset
            .checked_add(buf.len() as u64)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "offset overflow"))?;
        if end <= self.header.len() as u64 {
            let start = offset as usize;
            buf.copy_from_slice(&self.header[start..start + buf.len()]);
            Ok(())
        } else if offset >= self.slot_base && end <= self.slot_base + self.slots.len() as u64 {
            let start = (offset - self.slot_base) as usize;
            buf.copy_from_slice(&self.slots[start..start + buf.len()]);
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "read outside the fetched ciphertext range",
            ))
        }
    }

    fn write_at(&mut self, _buf: &[u8], _offset: u64) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "ReadIo is read-only",
        ))
    }

    fn get_size(&mut self) -> std::io::Result<u64> {
        Ok(self.raf_size)
    }

    fn set_size(&mut self, _size: u64) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "ReadIo is read-only",
        ))
    }
}

/// Decrypt a plaintext read into `out`, returning the number of bytes produced
/// (fewer than `out.len()` at end of file). `io` must already hold the header and
/// the chunk slots covering `[plaintext_off, plaintext_off + out.len())`, as
/// planned by [`plan_read`]. Authentication failure (tampering) surfaces as an
/// error.
pub fn decrypt_read(
    key: &[u8; 16],
    io: ReadIo,
    plaintext_off: u64,
    out: &mut [u8],
) -> Result<usize, aegis::raf::Error> {
    let mut raf = aegis::raf::RafBuilder::<aegis::raf::Aegis128X2>::new().open(io, key)?;
    raf.read(out, plaintext_off)
}

/// Recover the plaintext size from a file's RAF header bytes (the 64 bytes after
/// the HFEB prefix) and its full ciphertext object size, without the key. This
/// is the authoritative plaintext size — it cannot be derived from the object
/// size alone. `raf_header` must be at least [`RAF_HEADER_LEN`] bytes.
pub fn probe_plaintext_size(raf_header: &[u8], ciphertext_size: u64) -> Option<u64> {
    let raf_size = ciphertext_size.checked_sub(HFEB_HEADER_LEN as u64)?;
    let mut io = ReadIo::new(raf_size, raf_header.to_vec(), RAF_HEADER_LEN, Vec::new());
    let info = aegis::raf::probe(&mut io).ok()?;
    // Only accept the exact format we produce: AEGIS-128X2 at the default chunk
    // size (the size map assumes both).
    if info.algorithm != aegis::raf::AlgorithmId::Aegis128X2 || info.chunk_size as u64 != RAF_CHUNK_SIZE {
        return None;
    }
    Some(info.file_size)
}

/// A file-backed [`RafIo`](aegis::raf::RafIo) windowed past the 6-byte HFEB
/// prefix, so the RAF container lives at file offsets `[6, ..)` while the HFEB
/// header occupies `[0, 6)`. Used for the local staging file, which must be a
/// ciphertext container from creation onward (no plaintext ever hits disk).
struct StagingRafIo(std::fs::File);

impl aegis::raf::RafIo for StagingRafIo {
    fn read_at(&mut self, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.0.read_exact_at(buf, offset + HFEB_HEADER_LEN as u64)
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        self.0.write_all_at(buf, offset + HFEB_HEADER_LEN as u64)
    }

    fn get_size(&mut self) -> std::io::Result<u64> {
        Ok(self.0.metadata()?.len().saturating_sub(HFEB_HEADER_LEN as u64))
    }

    fn set_size(&mut self, size: u64) -> std::io::Result<()> {
        self.0.set_len(size + HFEB_HEADER_LEN as u64)
    }

    fn sync(&mut self) -> std::io::Result<()> {
        self.0.sync_all()
    }
}

/// Create a fresh, empty encrypted staging container at `path`: an HFEB header
/// followed by an empty AEGIS-RAF container. Overwrites any existing file.
pub fn create_staging_container(path: &std::path::Path, key: &[u8; 16]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    // Write the HFEB header first so the RAF view (offset 6) starts on a file
    // that is already large enough for the windowing arithmetic.
    let mut header = [0u8; HFEB_HEADER_LEN];
    write_header(&mut header, ALG_AEGIS_128X2);
    file.write_all(&header)?;
    let mut raf = aegis::raf::RafBuilder::<aegis::raf::Aegis128X2>::new()
        .truncate(true)
        .create(StagingRafIo(file), key)
        .map_err(|e| std::io::Error::other(format!("raf create: {e}")))?;
    raf.sync()
        .map_err(|e| std::io::Error::other(format!("raf sync: {e}")))?;
    Ok(())
}

/// Open the encrypted staging container at `path` for reading/writing. The
/// returned [`Raf`](aegis::raf::Raf) exposes plaintext `read`/`write`/`truncate`
/// at plaintext offsets; it is not `Send`, so callers use it within a single
/// operation and drop it (the `aegis` RAF context holds raw pointers).
pub fn open_staging_raf(
    path: &std::path::Path,
    key: &[u8; 16],
) -> std::io::Result<aegis::raf::Raf<aegis::raf::Aegis128X2>> {
    use std::os::unix::fs::FileExt;
    let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    // Validate the HFEB prefix before windowing into the RAF body — a
    // downloaded/reused staging file isn't necessarily one we created.
    let mut header = [0u8; HFEB_HEADER_LEN];
    file.read_exact_at(&mut header, 0)?;
    parse_header(&header).map_err(|e| std::io::Error::other(format!("invalid HFEB header: {e}")))?;
    aegis::raf::RafBuilder::<aegis::raf::Aegis128X2>::new()
        .open(StagingRafIo(file), key)
        .map_err(|e| std::io::Error::other(format!("raf open: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aegis::raf::{Aegis128X2, Algorithm, FileIo, Raf, probe};

    #[test]
    fn algorithm_byte_matches_crate() {
        assert_eq!(ALG_AEGIS_128X2, Aegis128X2::ALG_ID);
    }

    #[test]
    fn header_round_trips() {
        let mut buf = [0u8; HFEB_HEADER_LEN];
        write_header(&mut buf, ALG_AEGIS_128X2);
        assert_eq!(&buf[..4], b"HFEB");
        assert_eq!(parse_header(&buf), Ok(ALG_AEGIS_128X2));
    }

    #[test]
    fn header_rejects_garbage() {
        assert_eq!(parse_header(b"XXXX\x01\x02"), Err(HeaderError::BadMagic));
        assert_eq!(parse_header(b"HFEB\x09\x02"), Err(HeaderError::UnsupportedVersion(9)));
        assert_eq!(parse_header(b"HFEB\x01\x07"), Err(HeaderError::UnknownAlgorithm(7)));
        assert_eq!(parse_header(b"HFEB\x01"), Err(HeaderError::Truncated));
    }

    #[test]
    fn container_chunk_count_validates_sizes() {
        assert_eq!(container_chunk_count(0), None); // smaller than a header
        assert_eq!(container_chunk_count(CONTAINER_HEADER_LEN), Some(0)); // empty file
        assert_eq!(container_chunk_count(CONTAINER_HEADER_LEN + 10), None); // partial slot
        assert_eq!(container_chunk_count(ciphertext_size(1)), Some(1));
        assert_eq!(container_chunk_count(ciphertext_size(200000)), Some(4));
    }

    #[test]
    fn ciphertext_size_is_chunk_quantised() {
        // The final chunk is padded to a full slot, so size only reveals the
        // chunk count: a 1-byte and a full-chunk file are indistinguishable.
        assert_eq!(ciphertext_size(1), ciphertext_size(RAF_CHUNK_SIZE));
        assert_ne!(ciphertext_size(RAF_CHUNK_SIZE), ciphertext_size(RAF_CHUNK_SIZE + 1));
        assert_eq!(ciphertext_size(0), CONTAINER_HEADER_LEN);
    }

    // Build real AEGIS-RAF files and confirm the size map matches the crate's
    // on-disk output and that `probe` recovers the exact plaintext size.
    #[test]
    fn size_map_matches_real_raf() {
        let dir = tempfile::tempdir().unwrap();
        let key = [0u8; 16];
        for &plaintext in &[0u64, 1, 100, 65535, 65536, 65537, 131072, 200000] {
            let path = dir.path().join(format!("f{plaintext}"));
            {
                let mut raf = Raf::<Aegis128X2>::create_file(&path, &key).unwrap();
                if plaintext > 0 {
                    let data = vec![0xABu8; plaintext as usize];
                    assert_eq!(raf.write(&data, 0).unwrap() as u64, plaintext);
                }
                raf.sync().unwrap();
            }
            // The file holds the RAF container only; add the 6-byte HFEB prefix.
            let measured = std::fs::metadata(&path).unwrap().len();
            assert_eq!(
                measured + HFEB_HEADER_LEN as u64,
                ciphertext_size(plaintext),
                "size formula mismatch at plaintext {plaintext} (measured raf body {measured})"
            );
            // The exact plaintext size is only recoverable from the header.
            let mut io = FileIo::open(&path).unwrap();
            assert_eq!(probe(&mut io).unwrap().file_size, plaintext);
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn make_container(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c");
        {
            let mut raf = Raf::<Aegis128X2>::create_file(&path, key).unwrap();
            if !plaintext.is_empty() {
                assert_eq!(raf.write(plaintext, 0).unwrap(), plaintext.len());
            }
            raf.sync().unwrap();
        }
        std::fs::read(&path).unwrap()
    }

    // Decrypt `[off, off+len)` exactly as the VFS read path will: plan the fetch,
    // serve only the covering slots, decrypt. Returns the bytes and how many
    // ciphertext bytes the plan fetched.
    fn read_via_plan(container: &[u8], key: &[u8; 16], off: u64, len: usize) -> (Vec<u8>, u64) {
        let object_size = container.len() as u64 + HFEB_HEADER_LEN as u64;
        let plan = plan_read(object_size, off, len as u64).unwrap();
        let hfeb = HFEB_HEADER_LEN as u64;
        let header =
            container[(plan.header_object.start - hfeb) as usize..(plan.header_object.end - hfeb) as usize].to_vec();
        let slots =
            container[(plan.slots_object.start - hfeb) as usize..(plan.slots_object.end - hfeb) as usize].to_vec();
        let fetched = slots.len() as u64;
        let io = ReadIo::new(plan.raf_size, header, plan.slot_base_raf, slots);
        let mut out = vec![0u8; len];
        let n = decrypt_read(key, io, off, &mut out).unwrap();
        out.truncate(n);
        (out, fetched)
    }

    #[test]
    fn reads_whole_file_and_ranges() {
        let key = [7u8; 16];
        let plaintext = pattern(200000); // spans four chunks
        let container = make_container(&plaintext, &key);

        let (all, _) = read_via_plan(&container, &key, 0, plaintext.len());
        assert_eq!(all, plaintext);

        let (mid, _) = read_via_plan(&container, &key, 100, 50);
        assert_eq!(mid, &plaintext[100..150]);

        let (cross, _) = read_via_plan(&container, &key, 65500, 200);
        assert_eq!(cross, &plaintext[65500..65700]);
    }

    #[test]
    fn deep_read_fetches_only_covering_slot() {
        let key = [9u8; 16];
        let plaintext = pattern(200000);
        let container = make_container(&plaintext, &key);
        // A read inside chunk 2 must fetch exactly one slot, not chunks 0..2.
        let (data, fetched) = read_via_plan(&container, &key, 150000, 100);
        assert_eq!(data, &plaintext[150000..150100]);
        assert_eq!(fetched, RAF_CHUNK_RECORD);
    }

    #[test]
    fn reads_at_and_across_eof() {
        let key = [3u8; 16];
        let plaintext = pattern(1000);
        let container = make_container(&plaintext, &key);
        let (past, _) = read_via_plan(&container, &key, 1000, 50);
        assert!(past.is_empty());
        let (tail, _) = read_via_plan(&container, &key, 995, 100);
        assert_eq!(tail, &plaintext[995..1000]);
    }

    #[test]
    fn tampered_chunk_fails_authentication() {
        let key = [1u8; 16];
        let plaintext = pattern(1000);
        let mut container = make_container(&plaintext, &key);
        container[100] ^= 0xff; // a byte inside chunk 0's slot, past the 64-byte header
        let object_size = container.len() as u64 + HFEB_HEADER_LEN as u64;
        let plan = plan_read(object_size, 0, plaintext.len() as u64).unwrap();
        let hfeb = HFEB_HEADER_LEN as u64;
        let header =
            container[(plan.header_object.start - hfeb) as usize..(plan.header_object.end - hfeb) as usize].to_vec();
        let slots =
            container[(plan.slots_object.start - hfeb) as usize..(plan.slots_object.end - hfeb) as usize].to_vec();
        let io = ReadIo::new(plan.raf_size, header, plan.slot_base_raf, slots);
        let mut out = vec![0u8; plaintext.len()];
        assert!(decrypt_read(&key, io, 0, &mut out).is_err());
    }

    #[test]
    fn staging_container_holds_ciphertext_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("staging");
        let key = [9u8; 16];
        create_staging_container(&path, &key).unwrap();

        let plaintext: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        {
            let mut raf = open_staging_raf(&path, &key).unwrap();
            assert_eq!(raf.write(&plaintext, 0).unwrap(), plaintext.len());
            raf.sync().unwrap();
        }

        // On disk it is a ciphertext HFEB container, never the plaintext.
        let raw = std::fs::read(&path).unwrap();
        assert_eq!(&raw[..4], b"HFEB");
        assert_eq!(raw.len() as u64, ciphertext_size(plaintext.len() as u64));
        assert!(
            !raw.windows(16).any(|w| w == &plaintext[..16]),
            "plaintext must never appear in the staging file"
        );
        // The plaintext size is probeable without the key.
        assert_eq!(
            probe_plaintext_size(&raw[HFEB_HEADER_LEN..], raw.len() as u64),
            Some(plaintext.len() as u64)
        );

        // It decrypts back exactly.
        let mut raf = open_staging_raf(&path, &key).unwrap();
        assert_eq!(raf.size(), plaintext.len() as u64);
        let mut out = vec![0u8; plaintext.len()];
        raf.read(&mut out, 0).unwrap();
        assert_eq!(out, plaintext);
    }

    #[test]
    fn open_staging_raf_rejects_bad_hfeb_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("staging");
        let key = [1u8; 16];
        create_staging_container(&path, &key).unwrap();
        {
            let mut raf = open_staging_raf(&path, &key).unwrap();
            raf.write(b"hello", 0).unwrap();
            raf.sync().unwrap();
        }
        // Corrupt the HFEB magic; the RAF body underneath is still valid.
        use std::os::unix::fs::FileExt;
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.write_all_at(b"X", 0).unwrap();
        f.sync_all().unwrap();
        assert!(open_staging_raf(&path, &key).is_err());
    }

    #[test]
    fn staging_truncate_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("staging");
        let key = [4u8; 16];
        create_staging_container(&path, &key).unwrap();
        let plaintext: Vec<u8> = (0..70_000).map(|i| (i % 251) as u8).collect();
        {
            let mut raf = open_staging_raf(&path, &key).unwrap();
            raf.write(&plaintext, 0).unwrap();
            raf.truncate(100).unwrap();
            raf.sync().unwrap();
        }
        let mut raf = open_staging_raf(&path, &key).unwrap();
        assert_eq!(raf.size(), 100);
        let mut out = vec![0u8; 100];
        raf.read(&mut out, 0).unwrap();
        assert_eq!(out, &plaintext[..100]);
        assert_eq!(std::fs::read(&path).unwrap().len() as u64, ciphertext_size(100));
    }

    #[test]
    fn probe_recovers_plaintext_size_from_header() {
        let key = [5u8; 16];
        for &plaintext in &[0u64, 1, 100, 65536, 65537, 200000] {
            let container = make_container(&vec![0xCD; plaintext as usize], &key);
            let object_size = container.len() as u64 + HFEB_HEADER_LEN as u64;
            // The RAF header is the object's [6, 70) range, i.e. container[0..64].
            let header = &container[..RAF_HEADER_LEN as usize];
            assert_eq!(probe_plaintext_size(header, object_size), Some(plaintext));
        }
    }
}
