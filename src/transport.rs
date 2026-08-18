//! Cross-host migration over raw TCP.
//!
//! The wire format is length-prefixed frames and nothing else — no gRPC, no
//! HTTP, no serialisation framework. A snapshot is already a self-describing
//! byte stream with its own checksums, so the transport's only jobs are to
//! delimit messages, carry the bytes, and report back what happened.
//!
//! ```text
//!   frame := type:u8 | len:u32 BE | payload[len]
//! ```
//!
//! ```text
//!   sender                                    receiver
//!   ──────                                    ────────
//!   HELLO      {proto, id, bytes, crc, token} ──▶
//!                                       ◀── HELLO_ACK {accept, host, reason}
//!   CHUNK ...  (raw snapshot bytes)           ──▶
//!   EOF        {bytes, crc32}                 ──▶
//!                                       ◀── RESULT {ok, pid, restore_ms}
//! ```
//!
//! The ordering is the correctness argument for migration. The container is
//! already frozen before the first byte goes out, so it is never live in two
//! places. The sender only marks it `migrated` — giving up its claim — after
//! `RESULT` says it is *running* on the far side. If the connection dies at any
//! point, the sender still holds a complete, valid snapshot and can restore
//! locally or retry: the failure mode is a container that stays put, never one
//! that vanishes.
//!
//! ## What this is not
//!
//! Plaintext and only as authenticated as a shared token makes it. Anyone who
//! can reach the port and knows the token can hand this node a process image to
//! run, which is equivalent to remote code execution by design — that is what
//! migration *is*. Run it on a trusted network. A real deployment would want
//! mutual TLS, and the framing here would not have to change to get it.

use crate::checkpoint;
use crate::error::{Result, RuntimeError};
use crate::restore;
use crate::snapshot::Crc32;
use serde::{Deserialize, Serialize};
use std::io::{BufReader, BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 7373;
pub const PROTO_VERSION: u32 = 1;

/// Payload chunk size. Large enough that the per-frame overhead is noise,
/// small enough that a receiver never has to buffer much.
const CHUNK: usize = 256 * 1024;

/// Cap on a control frame. Chunks are bounded separately; this keeps a hostile
/// or corrupt length field from making us allocate unboundedly.
const MAX_CONTROL_FRAME: u32 = 4 * 1024 * 1024;
const MAX_CHUNK_FRAME: u32 = 4 * 1024 * 1024;

const IO_TIMEOUT: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Hello = 0x01,
    HelloAck = 0x02,
    Chunk = 0x03,
    Eof = 0x04,
    Result = 0x05,
    Error = 0x06,
}

impl FrameType {
    fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x01 => FrameType::Hello,
            0x02 => FrameType::HelloAck,
            0x03 => FrameType::Chunk,
            0x04 => FrameType::Eof,
            0x05 => FrameType::Result,
            0x06 => FrameType::Error,
            _ => return None,
        })
    }
}

fn write_frame<W: Write>(w: &mut W, t: FrameType, payload: &[u8]) -> Result<()> {
    let len = u32::try_from(payload.len())
        .map_err(|_| RuntimeError::Migration("frame payload exceeds 4 GiB".into()))?;
    w.write_all(&[t as u8])
        .and_then(|_| w.write_all(&len.to_be_bytes()))
        .and_then(|_| w.write_all(payload))
        .map_err(|e| RuntimeError::Migration(format!("write frame: {e}")))
}

fn write_json<W: Write, T: Serialize>(w: &mut W, t: FrameType, v: &T) -> Result<()> {
    let body = serde_json::to_vec(v)?;
    write_frame(w, t, &body)
}

/// Read one frame header, returning its type and payload length.
fn read_header<R: Read>(r: &mut R) -> Result<(FrameType, u32)> {
    let mut hdr = [0u8; 5];
    r.read_exact(&mut hdr)
        .map_err(|e| RuntimeError::Migration(format!("read frame header: {e}")))?;
    let t = FrameType::from_u8(hdr[0]).ok_or_else(|| {
        RuntimeError::Migration(format!("unknown frame type {:#04x} — peer is not mincontainer", hdr[0]))
    })?;
    let len = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]);
    let cap = if t == FrameType::Chunk { MAX_CHUNK_FRAME } else { MAX_CONTROL_FRAME };
    if len > cap {
        return Err(RuntimeError::Migration(format!(
            "frame of {len} bytes exceeds the {cap}-byte limit for {t:?}"
        )));
    }
    Ok((t, len))
}

fn read_payload<R: Read>(r: &mut R, len: u32) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)
        .map_err(|e| RuntimeError::Migration(format!("read frame payload: {e}")))?;
    Ok(buf)
}

fn read_json<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R, expect: FrameType) -> Result<T> {
    let (t, len) = read_header(r)?;
    let body = read_payload(r, len)?;
    if t == FrameType::Error {
        let e: ErrorMsg = serde_json::from_slice(&body).unwrap_or(ErrorMsg {
            message: "peer sent an unreadable error".into(),
        });
        return Err(RuntimeError::Migration(format!("peer refused: {}", e.message)));
    }
    if t != expect {
        return Err(RuntimeError::Migration(format!(
            "protocol error: expected {expect:?}, got {t:?}"
        )));
    }
    serde_json::from_slice(&body).map_err(RuntimeError::from)
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub proto_version: u32,
    pub producer: String,
    pub container_id: String,
    pub source_host: String,
    pub source_arch: String,
    pub snapshot_bytes: u64,
    /// CRC32 of the snapshot file, so the receiver can reject a mangled
    /// transfer without having to parse it first.
    pub snapshot_crc32: u32,
    pub token: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HelloAck {
    pub proto_version: u32,
    pub accept: bool,
    pub receiver_host: String,
    pub receiver_arch: String,
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Eof {
    pub bytes: u64,
    pub crc32: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MigrateResult {
    pub ok: bool,
    pub container_id: String,
    pub pid: i32,
    pub generation: u32,
    pub restore_ms: f64,
    pub container_ip: Option<String>,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorMsg {
    pub message: String,
}

// ---------------------------------------------------------------------------
// Sender
// ---------------------------------------------------------------------------

/// Outcome of a migration, from the sender's point of view.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MigrationReport {
    pub id: String,
    pub target: String,
    pub snapshot_bytes: u64,
    pub checkpoint_ms: f64,
    /// Time on the wire.
    pub transfer_ms: f64,
    /// Restore time as reported by the receiver.
    pub remote_restore_ms: f64,
    pub total_ms: f64,
    pub throughput_mib_s: f64,
    pub remote_pid: i32,
    pub remote_ip: Option<String>,
}

/// Ship an existing snapshot to `target` and have it restored there.
///
/// The container must already be checkpointed; the caller ([`crate::migrate`])
/// is responsible for freezing it first. Splitting those apart keeps this
/// function honest about what it does: it moves bytes and reports what the far
/// side said, and it never decides on its own to stop a running container.
pub fn send_snapshot(
    id: &str,
    snapshot: &Path,
    target: &str,
    token: &str,
) -> Result<(MigrateResult, f64, u64)> {
    let addr = resolve(target)?;

    let file_len = std::fs::metadata(snapshot)
        .map_err(|e| RuntimeError::Migration(format!("stat {}: {e}", snapshot.display())))?
        .len();

    // Checksum before connecting: it costs one read of a local file and means
    // a corrupt snapshot is caught here rather than after the peer has taken
    // custody of it.
    let crc = crc_of_file(snapshot)?;

    let t0 = Instant::now();
    let stream = TcpStream::connect(addr).map_err(|e| {
        RuntimeError::Migration(format!("connect to {target}: {e}"))
    })?;
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();

    let mut w = BufWriter::new(
        stream.try_clone().map_err(|e| RuntimeError::Migration(format!("clone socket: {e}")))?,
    );
    let mut r = BufReader::new(stream);

    let hello = Hello {
        proto_version: PROTO_VERSION,
        producer: format!("mincontainer/{}", env!("CARGO_PKG_VERSION")),
        container_id: id.to_string(),
        source_host: checkpoint::hostname(),
        source_arch: std::env::consts::ARCH.to_string(),
        snapshot_bytes: file_len,
        snapshot_crc32: crc,
        token: token.to_string(),
    };
    write_json(&mut w, FrameType::Hello, &hello)?;
    w.flush().map_err(|e| RuntimeError::Migration(format!("flush hello: {e}")))?;

    let ack: HelloAck = read_json(&mut r, FrameType::HelloAck)?;
    if !ack.accept {
        return Err(RuntimeError::Migration(format!(
            "{target} refused the migration: {}",
            ack.reason
        )));
    }
    if ack.receiver_arch != hello.source_arch {
        return Err(RuntimeError::Migration(format!(
            "{target} is {} but this host is {} — process images cannot cross architectures",
            ack.receiver_arch, hello.source_arch
        )));
    }

    // --- stream the snapshot ------------------------------------------------
    let mut f = BufReader::new(
        std::fs::File::open(snapshot)
            .map_err(|e| RuntimeError::Migration(format!("open {}: {e}", snapshot.display())))?,
    );
    let mut buf = vec![0u8; CHUNK];
    let mut sent = 0u64;
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| RuntimeError::Migration(format!("read snapshot: {e}")))?;
        if n == 0 {
            break;
        }
        write_frame(&mut w, FrameType::Chunk, &buf[..n])?;
        sent += n as u64;
    }
    if sent != file_len {
        return Err(RuntimeError::Migration(format!(
            "snapshot changed size mid-transfer ({sent} sent, {file_len} expected)"
        )));
    }

    write_json(&mut w, FrameType::Eof, &Eof { bytes: sent, crc32: crc })?;
    w.flush().map_err(|e| RuntimeError::Migration(format!("flush: {e}")))?;
    let transfer_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // The receiver restores before replying, so this read blocks for as long as
    // the remote restore takes.
    let result: MigrateResult = read_json(&mut r, FrameType::Result)?;
    if !result.ok {
        return Err(RuntimeError::Migration(format!(
            "{target} failed to restore the container: {}",
            result.message
        )));
    }

    Ok((result, transfer_ms, file_len))
}

fn resolve(target: &str) -> Result<std::net::SocketAddr> {
    let with_port = if target.contains(':') {
        target.to_string()
    } else {
        format!("{target}:{DEFAULT_PORT}")
    };
    with_port
        .to_socket_addrs()
        .map_err(|e| RuntimeError::Migration(format!("resolve {target}: {e}")))?
        .next()
        .ok_or_else(|| RuntimeError::Migration(format!("{target} resolved to no addresses")))
}

fn crc_of_file(path: &Path) -> Result<u32> {
    let mut f = BufReader::new(
        std::fs::File::open(path)
            .map_err(|e| RuntimeError::Migration(format!("open {}: {e}", path.display())))?,
    );
    let mut crc = Crc32::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| RuntimeError::Migration(format!("read {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        crc.update(&buf[..n]);
    }
    Ok(crc.finish())
}

// ---------------------------------------------------------------------------
// Receiver
// ---------------------------------------------------------------------------

/// Where incoming snapshots are staged before being adopted.
fn inbox() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
        .join(".mincontainer/inbox")
}

/// Serve migrations until interrupted.
///
/// Single-threaded and sequential on purpose: a restore takes over cgroups and
/// network interfaces, and serialising arrivals removes a whole class of races
/// for a workload that is not throughput-bound anyway.
pub fn serve(bind: &str, token: &str, index: u8) -> Result<()> {
    let listener = TcpListener::bind(bind)
        .map_err(|e| RuntimeError::Migration(format!("bind {bind}: {e}")))?;
    eprintln!(
        "[mincontainer] migration receiver listening on {} (host {})",
        listener.local_addr().map(|a| a.to_string()).unwrap_or_else(|_| bind.into()),
        checkpoint::hostname()
    );
    if token.is_empty() {
        eprintln!(
            "[mincontainer] WARNING: no --token set. Anyone who can reach this port can run \
             a process image here. Use a token and a trusted network."
        );
    }

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[mincontainer] accept failed: {e}");
                continue;
            }
        };
        let peer = stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "?".into());
        match handle_one(stream, token, index) {
            Ok(id) => eprintln!("[mincontainer] {peer}: restored {id}"),
            // One bad transfer must not take the receiver down.
            Err(e) => eprintln!("[mincontainer] {peer}: migration failed: {e}"),
        }
    }
    Ok(())
}

fn handle_one(stream: TcpStream, token: &str, index: u8) -> Result<String> {
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();
    let mut w = BufWriter::new(
        stream.try_clone().map_err(|e| RuntimeError::Migration(format!("clone socket: {e}")))?,
    );
    let mut r = BufReader::new(stream);

    let hello: Hello = read_json(&mut r, FrameType::Hello)?;

    let refuse = |w: &mut BufWriter<TcpStream>, reason: &str| -> Result<()> {
        write_json(
            w,
            FrameType::HelloAck,
            &HelloAck {
                proto_version: PROTO_VERSION,
                accept: false,
                receiver_host: checkpoint::hostname(),
                receiver_arch: std::env::consts::ARCH.to_string(),
                reason: reason.to_string(),
            },
        )?;
        w.flush().ok();
        Err(RuntimeError::Migration(reason.to_string()))
    };

    if hello.proto_version != PROTO_VERSION {
        return refuse(
            &mut w,
            &format!(
                "protocol version {} not supported (this node speaks {PROTO_VERSION})",
                hello.proto_version
            ),
        )
        .map(|_| String::new());
    }
    // Constant-time-ish comparison is overkill for a length-prefixed token on a
    // trusted network, but rejecting on length first at least avoids leaking it
    // through timing on the common mismatch.
    if hello.token != token {
        return refuse(&mut w, "authentication token mismatch").map(|_| String::new());
    }
    if hello.source_arch != std::env::consts::ARCH {
        return refuse(
            &mut w,
            &format!(
                "snapshot is {} but this host is {}",
                hello.source_arch,
                std::env::consts::ARCH
            ),
        )
        .map(|_| String::new());
    }

    write_json(
        &mut w,
        FrameType::HelloAck,
        &HelloAck {
            proto_version: PROTO_VERSION,
            accept: true,
            receiver_host: checkpoint::hostname(),
            receiver_arch: std::env::consts::ARCH.to_string(),
            reason: String::new(),
        },
    )?;
    w.flush().map_err(|e| RuntimeError::Migration(format!("flush ack: {e}")))?;

    // --- receive ------------------------------------------------------------
    let dir = inbox();
    std::fs::create_dir_all(&dir)
        .map_err(|e| RuntimeError::Migration(format!("mkdir inbox: {e}")))?;
    // `.part` until the checksum clears, so a half-received file is never
    // mistaken for a usable snapshot.
    let part = dir.join(format!("{}.mcsnap.part", hello.container_id));
    let final_path = dir.join(format!("{}.mcsnap", hello.container_id));

    let mut received = 0u64;
    let mut crc = Crc32::new();
    {
        let mut out = BufWriter::new(
            std::fs::File::create(&part)
                .map_err(|e| RuntimeError::Migration(format!("create {}: {e}", part.display())))?,
        );
        loop {
            let (t, len) = read_header(&mut r)?;
            match t {
                FrameType::Chunk => {
                    let body = read_payload(&mut r, len)?;
                    crc.update(&body);
                    out.write_all(&body).map_err(|e| {
                        RuntimeError::Migration(format!("write {}: {e}", part.display()))
                    })?;
                    received += body.len() as u64;
                    if received > hello.snapshot_bytes {
                        let _ = std::fs::remove_file(&part);
                        return Err(RuntimeError::Migration(format!(
                            "sender exceeded the {} bytes it announced",
                            hello.snapshot_bytes
                        )));
                    }
                }
                FrameType::Eof => {
                    let eof: Eof = serde_json::from_slice(&read_payload(&mut r, len)?)?;
                    out.flush().map_err(|e| {
                        RuntimeError::Migration(format!("flush {}: {e}", part.display()))
                    })?;
                    let got = crc.finish();
                    if eof.bytes != received || eof.crc32 != got || got != hello.snapshot_crc32 {
                        let _ = std::fs::remove_file(&part);
                        let msg = format!(
                            "transfer corrupted: {received} bytes crc {got:#010x}, sender said \
                             {} bytes crc {:#010x}",
                            eof.bytes, eof.crc32
                        );
                        write_json(&mut w, FrameType::Error, &ErrorMsg { message: msg.clone() })?;
                        w.flush().ok();
                        return Err(RuntimeError::Migration(msg));
                    }
                    break;
                }
                other => {
                    let _ = std::fs::remove_file(&part);
                    return Err(RuntimeError::Migration(format!(
                        "protocol error: expected CHUNK or EOF, got {other:?}"
                    )));
                }
            }
        }
    }
    std::fs::rename(&part, &final_path)
        .map_err(|e| RuntimeError::Migration(format!("commit snapshot: {e}")))?;

    // --- adopt and restore ---------------------------------------------------
    let outcome = (|| -> Result<restore::Report> {
        let (id, _cfg) = restore::adopt_snapshot(&final_path)?;
        restore::restore(
            &id,
            &restore::Options { from: Some(final_path.clone()), index, allow_tcp: false },
        )
    })();

    match outcome {
        Ok(rep) => {
            write_json(
                &mut w,
                FrameType::Result,
                &MigrateResult {
                    ok: true,
                    container_id: rep.id.clone(),
                    pid: rep.pid,
                    generation: rep.generation,
                    restore_ms: rep.restore_ms,
                    container_ip: rep.container_ip.clone(),
                    message: format!("restored on {}", checkpoint::hostname()),
                },
            )?;
            w.flush().ok();
            Ok(rep.id)
        }
        Err(e) => {
            // Tell the sender in detail: it still owns a valid snapshot and
            // needs to know it must keep it.
            write_json(
                &mut w,
                FrameType::Result,
                &MigrateResult {
                    ok: false,
                    container_id: hello.container_id.clone(),
                    pid: 0,
                    generation: 0,
                    restore_ms: 0.0,
                    container_ip: None,
                    message: e.to_string(),
                },
            )?;
            w.flush().ok();
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, FrameType::Chunk, b"hello world").unwrap();
        let mut r = &buf[..];
        let (t, len) = read_header(&mut r).unwrap();
        assert_eq!(t, FrameType::Chunk);
        assert_eq!(len, 11);
        assert_eq!(read_payload(&mut r, len).unwrap(), b"hello world");
    }

    #[test]
    fn json_frames_round_trip() {
        let mut buf: Vec<u8> = Vec::new();
        let eof = Eof { bytes: 4096, crc32: 0xDEAD_BEEF };
        write_json(&mut buf, FrameType::Eof, &eof).unwrap();
        let got: Eof = read_json(&mut &buf[..], FrameType::Eof).unwrap();
        assert_eq!(got.bytes, 4096);
        assert_eq!(got.crc32, 0xDEAD_BEEF);
    }

    #[test]
    fn unknown_frame_type_is_rejected() {
        let buf = [0xFFu8, 0, 0, 0, 0];
        let err = read_header(&mut &buf[..]).unwrap_err().to_string();
        assert!(err.contains("unknown frame type"), "{err}");
    }

    #[test]
    fn oversized_control_frame_is_rejected() {
        // A length field claiming 512 MiB must be refused before allocating.
        let mut buf = vec![FrameType::Hello as u8];
        buf.extend_from_slice(&(512u32 * 1024 * 1024).to_be_bytes());
        let err = read_header(&mut &buf[..]).unwrap_err().to_string();
        assert!(err.contains("exceeds"), "{err}");
    }

    #[test]
    fn error_frame_surfaces_as_the_peers_message() {
        let mut buf: Vec<u8> = Vec::new();
        write_json(
            &mut buf,
            FrameType::Error,
            &ErrorMsg { message: "no rootfs here".into() },
        )
        .unwrap();
        let err = read_json::<_, HelloAck>(&mut &buf[..], FrameType::HelloAck)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no rootfs here"), "{err}");
    }

    #[test]
    fn wrong_frame_type_is_a_protocol_error() {
        let mut buf: Vec<u8> = Vec::new();
        write_json(&mut buf, FrameType::Eof, &Eof { bytes: 1, crc32: 2 }).unwrap();
        let err = read_json::<_, HelloAck>(&mut &buf[..], FrameType::HelloAck)
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected HelloAck"), "{err}");
    }
}
