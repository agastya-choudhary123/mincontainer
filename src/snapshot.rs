//! The on-disk snapshot format (`.mcsnap`).
//!
//! A snapshot is a single self-describing file: everything needed to rebuild a
//! container lives inside it, and a reader that has never seen this runtime can
//! still enumerate its contents from the manifest alone.
//!
//! ```text
//!   ┌────────────────────────────────────────────────────────────────┐
//!   │ magic        "MCSNAP\0" + format byte              8 bytes     │
//!   │ manifest_len u32 LE                                4 bytes     │
//!   │ manifest     JSON (Manifest)                       manifest_len│
//!   ├────────────────────────────────────────────────────────────────┤
//!   │ entry[0]     name_len u32 │ name │ data_len u64 │ data         │
//!   │ entry[1]     ...                                               │
//!   ├────────────────────────────────────────────────────────────────┤
//!   │ trailer      "MCSNAPED" + crc32 of all preceding   12 bytes    │
//!   └────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Integrity is checked at two granularities, which is what makes a corrupted
//! snapshot fail *before* we start mutating the host rather than halfway
//! through a restore: the trailer CRC covers the whole file, and every entry
//! carries its own CRC in the manifest. `verify()` walks both.
//!
//! Entries are streamed, never buffered whole — a memory-heavy container dumps
//! hundreds of megabytes of page images and we refuse to hold that in RAM.

use crate::config::ContainerConfig;
use crate::error::{Result, RuntimeError};
use crate::state::ContainerState;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const MAGIC: &[u8; 8] = b"MCSNAP\0\x01";
pub const TRAILER: &[u8; 8] = b"MCSNAPED";
pub const FORMAT_VERSION: u32 = 1;

/// Refuse manifests larger than this; a corrupt length field must not make us
/// allocate a gigabyte before we have checked anything.
const MAX_MANIFEST: u32 = 8 * 1024 * 1024;

// ---------------------------------------------------------------------------
// CRC32 (IEEE 802.3). Hand-rolled: a checksum is not worth a dependency.
// ---------------------------------------------------------------------------

fn crc_table() -> &'static [u32; 256] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    })
}

/// Rolling CRC32 so we can checksum a stream we are already copying.
#[derive(Debug, Clone, Copy)]
pub struct Crc32(u32);

impl Default for Crc32 {
    fn default() -> Self {
        Crc32(0xFFFF_FFFF)
    }
}

impl Crc32 {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, buf: &[u8]) {
        let t = crc_table();
        let mut c = self.0;
        for &b in buf {
            c = t[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    pub fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

pub fn crc32(buf: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(buf);
    c.finish()
}

/// A writer that CRCs everything passing through it.
struct CrcWriter<W: Write> {
    inner: W,
    crc: Crc32,
    bytes: u64,
}

impl<W: Write> CrcWriter<W> {
    fn new(inner: W) -> Self {
        CrcWriter { inner, crc: Crc32::new(), bytes: 0 }
    }
}

impl<W: Write> Write for CrcWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.crc.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// One file inside the snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryMeta {
    /// Path relative to the snapshot's logical root, e.g. `images/core-1.img`.
    pub name: String,
    pub len: u64,
    pub crc32: u32,
}

/// Resource limits as they actually were at checkpoint time, read back from the
/// cgroup rather than copied from the config — the two can differ if something
/// adjusted the cgroup after start, and the snapshot must record reality.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CgroupSnapshot {
    pub memory_max: String,
    pub memory_peak: u64,
    pub cpu_max: String,
    pub pids_max: String,
    pub cpu_usage_usec: u64,
}

/// Identity of each namespace the container occupied, by inode number. On
/// restore these must all differ from the recorded values: matching inodes mean
/// we somehow re-entered the *original* namespaces instead of building new
/// ones, which would silently corrupt the host.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NamespaceSnapshot {
    pub pid: Option<u64>,
    pub mnt: Option<u64>,
    pub net: Option<u64>,
    pub uts: Option<u64>,
    pub ipc: Option<u64>,
}

/// One line of `/proc/<pid>/mountinfo`, reduced to what a restore needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountEntry {
    pub mount_point: String,
    pub fs_type: String,
    pub source: String,
    pub options: String,
}

/// One entry of `/proc/<pid>/fd`, with the target it resolved to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FdEntry {
    pub fd: i32,
    pub target: String,
}

/// Everything about the checkpoint that is not raw image bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    /// What produced this snapshot, e.g. `mincontainer/0.2.0 criu/3.17.1`.
    pub producer: String,
    /// The checkpoint engine and its version, recorded so a restore on another
    /// node can refuse an image set its own engine cannot read.
    pub engine: String,
    pub engine_version: String,
    pub created_at: String,
    pub source_host: String,
    pub source_arch: String,
    pub source_kernel: String,

    pub container_id: String,
    pub config: ContainerConfig,
    pub lifecycle: ContainerState,

    pub cgroup: CgroupSnapshot,
    pub namespaces: NamespaceSnapshot,
    pub mounts: Vec<MountEntry>,
    pub open_fds: Vec<FdEntry>,

    /// Host-visible pid of the container's PID 1 when it was dumped. Purely
    /// informational — a restore always gets a fresh pid.
    pub checkpoint_pid: i32,
    /// Wall-clock milliseconds the freeze+dump took.
    pub dump_ms: f64,

    pub entries: Vec<EntryMeta>,
}

impl Manifest {
    /// Total size of the payload entries (excludes manifest and framing).
    pub fn payload_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.len).sum()
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Builds a snapshot file.
///
/// The manifest has to be written *before* the entries, but each entry's CRC is
/// only known after streaming it — so we make two passes: stream the payload to
/// a scratch file while accumulating metadata, then write the real file with a
/// complete manifest in front. The scratch file lives beside the destination so
/// the final rename stays on one filesystem.
pub struct SnapshotWriter {
    scratch_path: PathBuf,
    /// `None` once `finish` has taken it. Wrapped in an `Option` only because
    /// the type has a `Drop` impl, so the writer cannot be moved out directly.
    scratch: Option<BufWriter<File>>,
    entries: Vec<EntryMeta>,
}

impl SnapshotWriter {
    pub fn new(dest: &Path) -> Result<Self> {
        let scratch_path = dest.with_extension("mcsnap.part");
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                RuntimeError::Snapshot(format!("mkdir {}: {e}", parent.display()))
            })?;
        }
        let scratch = File::create(&scratch_path).map_err(|e| {
            RuntimeError::Snapshot(format!("create {}: {e}", scratch_path.display()))
        })?;
        Ok(SnapshotWriter {
            scratch_path,
            scratch: Some(BufWriter::new(scratch)),
            entries: Vec::new(),
        })
    }

    /// Append one entry, streaming from `src`.
    pub fn add_file(&mut self, name: &str, src: &Path) -> Result<()> {
        let f = File::open(src)
            .map_err(|e| RuntimeError::Snapshot(format!("open {}: {e}", src.display())))?;
        let len = f
            .metadata()
            .map_err(|e| RuntimeError::Snapshot(format!("stat {}: {e}", src.display())))?
            .len();
        self.write_entry(name, len, &mut BufReader::new(f))
    }

    /// Append one entry from an in-memory buffer.
    pub fn add_bytes(&mut self, name: &str, data: &[u8]) -> Result<()> {
        self.write_entry(name, data.len() as u64, &mut &data[..])
    }

    fn write_entry<R: Read>(&mut self, name: &str, len: u64, src: &mut R) -> Result<()> {
        if self.entries.iter().any(|e| e.name == name) {
            return Err(RuntimeError::Snapshot(format!("duplicate entry {name}")));
        }

        let scratch = self
            .scratch
            .as_mut()
            .ok_or_else(|| RuntimeError::Snapshot("snapshot writer already finished".into()))?;

        let name_bytes = name.as_bytes();
        scratch
            .write_all(&(name_bytes.len() as u32).to_le_bytes())
            .and_then(|_| scratch.write_all(name_bytes))
            .and_then(|_| scratch.write_all(&len.to_le_bytes()))
            .map_err(|e| RuntimeError::Snapshot(format!("write entry header {name}: {e}")))?;

        let mut crc = Crc32::new();
        let mut buf = vec![0u8; 256 * 1024];
        let mut copied = 0u64;
        loop {
            let n = src
                .read(&mut buf)
                .map_err(|e| RuntimeError::Snapshot(format!("read {name}: {e}")))?;
            if n == 0 {
                break;
            }
            crc.update(&buf[..n]);
            scratch
                .write_all(&buf[..n])
                .map_err(|e| RuntimeError::Snapshot(format!("write {name}: {e}")))?;
            copied += n as u64;
        }

        // A short read here means the source changed under us mid-dump. The
        // declared length is already on disk, so the frame would be unreadable;
        // fail rather than emit a snapshot we know is malformed.
        if copied != len {
            return Err(RuntimeError::Snapshot(format!(
                "{name}: declared {len} bytes but read {copied}"
            )));
        }

        self.entries.push(EntryMeta { name: name.to_string(), len, crc32: crc.finish() });
        Ok(())
    }

    pub fn entries(&self) -> &[EntryMeta] {
        &self.entries
    }

    /// Write the final file: magic, manifest, payload, trailer.
    ///
    /// `build_manifest` receives the entry table so the caller can fold it into
    /// the manifest it returns.
    pub fn finish<F>(mut self, dest: &Path, build_manifest: F) -> Result<Manifest>
    where
        F: FnOnce(Vec<EntryMeta>) -> Manifest,
    {
        let mut scratch = self
            .scratch
            .take()
            .ok_or_else(|| RuntimeError::Snapshot("snapshot writer already finished".into()))?;
        scratch
            .flush()
            .map_err(|e| RuntimeError::Snapshot(format!("flush scratch: {e}")))?;
        drop(scratch);

        let manifest = build_manifest(self.entries.clone());
        let manifest_json = serde_json::to_vec(&manifest)?;
        if manifest_json.len() as u32 > MAX_MANIFEST {
            return Err(RuntimeError::Snapshot(format!(
                "manifest too large: {} bytes",
                manifest_json.len()
            )));
        }

        let out = File::create(dest)
            .map_err(|e| RuntimeError::Snapshot(format!("create {}: {e}", dest.display())))?;
        let mut w = CrcWriter::new(BufWriter::new(out));

        w.write_all(MAGIC)
            .and_then(|_| w.write_all(&(manifest_json.len() as u32).to_le_bytes()))
            .and_then(|_| w.write_all(&manifest_json))
            .map_err(|e| RuntimeError::Snapshot(format!("write header: {e}")))?;

        let mut scratch = BufReader::new(File::open(&self.scratch_path).map_err(|e| {
            RuntimeError::Snapshot(format!("reopen scratch: {e}"))
        })?);
        std::io::copy(&mut scratch, &mut w)
            .map_err(|e| RuntimeError::Snapshot(format!("copy payload: {e}")))?;

        w.write_all(TRAILER)
            .map_err(|e| RuntimeError::Snapshot(format!("write trailer: {e}")))?;
        let crc = w.crc.finish();
        w.write_all(&crc.to_le_bytes())
            .map_err(|e| RuntimeError::Snapshot(format!("write crc: {e}")))?;
        w.flush()
            .map_err(|e| RuntimeError::Snapshot(format!("flush: {e}")))?;

        let _ = std::fs::remove_file(&self.scratch_path);
        Ok(manifest)
    }
}

impl Drop for SnapshotWriter {
    fn drop(&mut self) {
        // A writer abandoned mid-checkpoint must not leave a half-written part
        // file behind for the next run to trip over.
        let _ = std::fs::remove_file(&self.scratch_path);
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// Reads a snapshot file. Opening parses and validates the header only;
/// payload bytes are touched on `verify()` or `extract_*`.
pub struct SnapshotReader {
    file: BufReader<File>,
    manifest: Manifest,
    /// Byte offset of the first entry frame.
    payload_start: u64,
    /// Byte offset of the trailer.
    payload_end: u64,
    stored_crc: u32,
}

impl SnapshotReader {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .map_err(|e| RuntimeError::Snapshot(format!("open {}: {e}", path.display())))?;
        let total = file
            .metadata()
            .map_err(|e| RuntimeError::Snapshot(format!("stat {}: {e}", path.display())))?
            .len();
        if total < (MAGIC.len() + 4 + TRAILER.len() + 4) as u64 {
            return Err(RuntimeError::Snapshot(format!(
                "{}: too small to be a snapshot ({total} bytes)",
                path.display()
            )));
        }
        let mut file = BufReader::new(file);

        let mut magic = [0u8; 8];
        file.read_exact(&mut magic)
            .map_err(|e| RuntimeError::Snapshot(format!("read magic: {e}")))?;
        if &magic[..6] != b"MCSNAP" {
            return Err(RuntimeError::Snapshot(format!(
                "{}: not a mincontainer snapshot (bad magic)",
                path.display()
            )));
        }
        if magic != *MAGIC {
            return Err(RuntimeError::Snapshot(format!(
                "{}: snapshot format v{}, this build reads v{}",
                path.display(),
                magic[7],
                FORMAT_VERSION
            )));
        }

        let mut lenb = [0u8; 4];
        file.read_exact(&mut lenb)
            .map_err(|e| RuntimeError::Snapshot(format!("read manifest length: {e}")))?;
        let mlen = u32::from_le_bytes(lenb);
        if mlen > MAX_MANIFEST || mlen as u64 > total {
            return Err(RuntimeError::Snapshot(format!(
                "implausible manifest length {mlen} (file is {total} bytes) — snapshot is corrupt"
            )));
        }

        let mut mbuf = vec![0u8; mlen as usize];
        file.read_exact(&mut mbuf)
            .map_err(|e| RuntimeError::Snapshot(format!("read manifest: {e}")))?;
        let manifest: Manifest = serde_json::from_slice(&mbuf).map_err(|e| {
            RuntimeError::Snapshot(format!("manifest is not valid JSON — snapshot is corrupt: {e}"))
        })?;

        let payload_start = (MAGIC.len() + 4 + mlen as usize) as u64;
        let payload_end = total - (TRAILER.len() + 4) as u64;
        if payload_end < payload_start {
            return Err(RuntimeError::Snapshot(
                "truncated snapshot: payload ends before it begins".into(),
            ));
        }

        // Trailer.
        let mut f = file.into_inner();
        f.seek(SeekFrom::Start(payload_end))
            .map_err(|e| RuntimeError::Snapshot(format!("seek trailer: {e}")))?;
        let mut tr = [0u8; 8];
        let mut crcb = [0u8; 4];
        f.read_exact(&mut tr)
            .and_then(|_| f.read_exact(&mut crcb))
            .map_err(|e| RuntimeError::Snapshot(format!("read trailer: {e}")))?;
        if &tr != TRAILER {
            return Err(RuntimeError::Snapshot(
                "missing trailer — snapshot is truncated or corrupt".into(),
            ));
        }

        Ok(SnapshotReader {
            file: BufReader::new(f),
            manifest,
            payload_start,
            payload_end,
            stored_crc: u32::from_le_bytes(crcb),
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Whole-file integrity check. Cheap relative to a restore and it runs
    /// before we touch the host, so a corrupt snapshot is rejected rather than
    /// half-applied.
    pub fn verify(&mut self) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(0))
            .map_err(|e| RuntimeError::Snapshot(format!("seek: {e}")))?;

        let mut crc = Crc32::new();
        let mut remaining = self.payload_end + TRAILER.len() as u64;
        let mut buf = vec![0u8; 256 * 1024];
        while remaining > 0 {
            let want = buf.len().min(remaining as usize);
            let n = self
                .file
                .read(&mut buf[..want])
                .map_err(|e| RuntimeError::Snapshot(format!("read: {e}")))?;
            if n == 0 {
                return Err(RuntimeError::Snapshot("truncated snapshot".into()));
            }
            crc.update(&buf[..n]);
            remaining -= n as u64;
        }
        let got = crc.finish();
        if got != self.stored_crc {
            return Err(RuntimeError::Snapshot(format!(
                "checksum mismatch: file says {:#010x}, computed {:#010x} — snapshot is corrupt",
                self.stored_crc, got
            )));
        }
        Ok(())
    }

    /// Unpack every entry beneath `dir`, checking each entry's CRC as it lands.
    ///
    /// Entry names are validated against traversal: a snapshot arriving over
    /// the network is untrusted input, and `../../etc/shadow` must not be a
    /// writable path.
    pub fn extract_all(&mut self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)
            .map_err(|e| RuntimeError::Snapshot(format!("mkdir {}: {e}", dir.display())))?;

        self.file
            .seek(SeekFrom::Start(self.payload_start))
            .map_err(|e| RuntimeError::Snapshot(format!("seek payload: {e}")))?;

        let expected: Vec<EntryMeta> = self.manifest.entries.clone();
        for meta in &expected {
            let (name, len) = self.read_entry_header()?;
            if name != meta.name || len != meta.len {
                return Err(RuntimeError::Snapshot(format!(
                    "manifest/payload disagree: expected {} ({} bytes), found {name} ({len} bytes)",
                    meta.name, meta.len
                )));
            }
            let dest = safe_join(dir, &name)?;
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    RuntimeError::Snapshot(format!("mkdir {}: {e}", parent.display()))
                })?;
            }

            let mut out = BufWriter::new(File::create(&dest).map_err(|e| {
                RuntimeError::Snapshot(format!("create {}: {e}", dest.display()))
            })?);
            let mut crc = Crc32::new();
            let mut remaining = len;
            let mut buf = vec![0u8; 256 * 1024];
            while remaining > 0 {
                let want = buf.len().min(remaining as usize);
                let n = self
                    .file
                    .read(&mut buf[..want])
                    .map_err(|e| RuntimeError::Snapshot(format!("read {name}: {e}")))?;
                if n == 0 {
                    return Err(RuntimeError::Snapshot(format!("truncated entry {name}")));
                }
                crc.update(&buf[..n]);
                out.write_all(&buf[..n])
                    .map_err(|e| RuntimeError::Snapshot(format!("write {}: {e}", dest.display())))?;
                remaining -= n as u64;
            }
            out.flush()
                .map_err(|e| RuntimeError::Snapshot(format!("flush {}: {e}", dest.display())))?;

            let got = crc.finish();
            if got != meta.crc32 {
                return Err(RuntimeError::Snapshot(format!(
                    "entry {name}: checksum mismatch ({:#010x} != {:#010x}) — snapshot is corrupt",
                    got, meta.crc32
                )));
            }
        }
        Ok(())
    }

    fn read_entry_header(&mut self) -> Result<(String, u64)> {
        let mut nb = [0u8; 4];
        self.file
            .read_exact(&mut nb)
            .map_err(|e| RuntimeError::Snapshot(format!("read entry name length: {e}")))?;
        let nlen = u32::from_le_bytes(nb);
        if nlen == 0 || nlen > 4096 {
            return Err(RuntimeError::Snapshot(format!(
                "implausible entry name length {nlen} — snapshot is corrupt"
            )));
        }
        let mut name = vec![0u8; nlen as usize];
        self.file
            .read_exact(&mut name)
            .map_err(|e| RuntimeError::Snapshot(format!("read entry name: {e}")))?;
        let name = String::from_utf8(name)
            .map_err(|_| RuntimeError::Snapshot("entry name is not UTF-8".into()))?;
        let mut lb = [0u8; 8];
        self.file
            .read_exact(&mut lb)
            .map_err(|e| RuntimeError::Snapshot(format!("read entry length: {e}")))?;
        Ok((name, u64::from_le_bytes(lb)))
    }
}

/// Join `name` under `dir`, refusing anything that could escape it.
fn safe_join(dir: &Path, name: &str) -> Result<PathBuf> {
    use std::path::Component;
    let rel = Path::new(name);
    if rel.is_absolute() {
        return Err(RuntimeError::Snapshot(format!("absolute entry name {name}")));
    }
    for c in rel.components() {
        match c {
            Component::Normal(_) => {}
            _ => {
                return Err(RuntimeError::Snapshot(format!(
                    "entry name {name} escapes the snapshot root"
                )))
            }
        }
    }
    Ok(dir.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_vector() {
        // The canonical IEEE CRC32 of "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn rolling_crc_matches_oneshot() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let mut c = Crc32::new();
        for chunk in data.chunks(97) {
            c.update(chunk);
        }
        assert_eq!(c.finish(), crc32(&data));
    }

    #[test]
    fn safe_join_rejects_traversal() {
        let d = Path::new("/tmp/x");
        assert!(safe_join(d, "images/core.img").is_ok());
        assert!(safe_join(d, "../etc/shadow").is_err());
        assert!(safe_join(d, "/etc/shadow").is_err());
        assert!(safe_join(d, "a/../../b").is_err());
    }
}
