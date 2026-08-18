# mincontainer

A container runtime written from scratch in Rust. Namespaces, cgroups v2, seccomp —
and checkpoint/restore with cross-host live migration.

You can freeze a running container to a single file, ship that file to another
machine over a socket, and have the process resume there mid-execution: same
memory, same open files, same place in its loop.

```
$ mincontainer start -d web
$ mincontainer checkpoint web
checkpointed web — 64.21 MiB in 28 entries
  (preflight 2.1ms, freeze+dump 85.3ms, pack 300.0ms, total 396.0ms)

$ mincontainer migrate web node-b:7373 --token $TOKEN
migrated web to node-b:7373 — 64.21 MiB
  checkpoint 337ms, hash 129ms, transfer 126ms (510 MiB/s),
  remote restore 385ms (build 112ms), total 1120ms
  now pid 18 on node-b:7373
```

## Contents

- [What's here](#whats-here)
- [Quick start](#quick-start)
- [How checkpointing works](#how-checkpointing-works)
- [The snapshot format](#the-snapshot-format)
- [Migration](#migration)
- [Failure behaviour](#failure-behaviour)
- [Measurements](#measurements)
- [Design decisions and tradeoffs](#design-decisions-and-tradeoffs)
- [The hardest bug](#the-hardest-bug)
- [Limitations](#limitations)
- [Building and testing](#building-and-testing)

## What's here

| | |
|---|---|
| **Isolation** | PID, mount, UTS, IPC and network namespaces; `pivot_root`; cgroups v2 |
| **Hardening** | seccomp-bpf filter, capability dropping |
| **Lifecycle** | create / start / stop / ps / logs / rm, foreground or detached |
| **Checkpoint** | freeze a running container to a self-describing snapshot file |
| **Restore** | rebuild it on the same host, with rollback on failure |
| **Migration** | ship a snapshot to another node over raw TCP and restore it there |
| **Size** | ~5,600 lines of runtime, ~1,100 of tests |

The runtime was ~1,500 lines before this; checkpoint/restore and migration added
about 4,100 more.

CRIU does the process serialisation. The snapshot format, the restore
sequencing, the recovery behaviour and the migration transport are all this
project's own code — `src/criu.rs` is the entire surface that knows a checkpoint
engine exists, and it's 329 lines.

## Quick start

Everything runs inside the dev image, because this is Linux kernel work:

```bash
docker build -f Dockerfile.dev -t mincontainer-dev .
docker run --rm -v "$PWD":/work -w /work -v mc-target:/target \
    mincontainer-dev cargo build --release
```

Then, in a privileged container:

```bash
# Start something long-running, detached.
mincontainer create --rootfs /rootfs --id demo -- \
    /bin/sh -c 'i=0; while true; do i=$((i+1)); echo "tick $i"; sleep 0.3; done'
mincontainer start -d demo

mincontainer logs demo          # tick 1 ... tick 7

# Freeze it. The process tree is gone; the snapshot has everything.
mincontainer checkpoint demo
mincontainer ps                 # demo   checkpointed   -   0   latest.mcsnap

# Look inside the snapshot without restoring it.
mincontainer inspect ~/.mincontainer/containers/demo/snapshots/latest.mcsnap --verify

# Bring it back.
mincontainer restore demo
mincontainer logs demo          # ... tick 7, tick 8, tick 9 — not tick 1
```

That last line is the whole point. The counter continues; it does not restart.

For migration, run a receiver on the target node and hand it the container:

```bash
# node B
mincontainer serve --listen 0.0.0.0:7373 --token hunter2

# node A
mincontainer migrate demo node-b:7373 --token hunter2
```

`mincontainer info` reports whether the host can checkpoint at all, including
what `criu check` complains about.

## How checkpointing works

Checkpointing is ordered so that everything which can fail cheaply fails
*before* the container stops running:

```
1. validate       is this container running, and is its pid real?
2. pre-flight     is its state checkpointable at all?
3. metadata       namespaces, mounts, fds, cgroup — read from procfs
   ─────────────  everything above this line is a pure read
4. mark           status → checkpointing
5. dump           engine freezes and serialises the process tree
6. pack           images + logs + metadata → one file
7. commit         status → checkpointed, pid cleared
```

By the time the container actually stops, the only things left that can fail are
a full disk and an engine bug. The common failures — not running, stale pid,
missing rootfs, live TCP connection — all happen while the container is still
serving traffic.

Two things had to change in the runtime itself before any of this was possible.

**Containers needed to be able to outlive the CLI.** The original `run()` owned a
container for its entire lifetime and blocked until it exited, so there was never
a moment when a caller held a live container and could do anything else with it.
That split into `spawn()` returning a `Handle`, and `Handle::wait()`.

**The container needed to be its own session leader.** A detached container is
PID 1 of a new PID namespace, but without `setsid()` its session leader is still
the shell that launched it — a process in the *host* PID namespace, and therefore
not part of the tree being dumped. CRIU refuses that outright:

```
Error (criu/cr-dump.c:1618): A session leader of 11(1) is outside of its pid namespace
```

`setsid()` also drops the controlling terminal, which would make
`mincontainer run -- /bin/sh` unusable and break Ctrl-C. So it's applied only to
detached containers, and that's why `checkpoint` requires `start --detach`.

## The snapshot format

A snapshot is one file. Everything needed to rebuild the container is inside it,
and a reader that has never seen this runtime can still enumerate its contents:

```
┌────────────────────────────────────────────────────────────┐
│ magic        "MCSNAP\0" + format byte            8 bytes   │
│ manifest_len u32 LE                              4 bytes   │
│ manifest     JSON                                variable  │
├────────────────────────────────────────────────────────────┤
│ entry[0]     name_len u32 │ name │ data_len u64 │ data     │
│ entry[1]     ...                                           │
├────────────────────────────────────────────────────────────┤
│ trailer      "MCSNAPED" + crc32 of all preceding 12 bytes  │
└────────────────────────────────────────────────────────────┘
```

The manifest holds the container config, its lifecycle record, the cgroup limits
*as actually read back from `/sys/fs/cgroup`* rather than copied from config, the
namespace inodes, the parsed mount table, the open fd table, and an entry index
with a CRC per entry.

Integrity is checked at two granularities, and the reason is sequencing rather
than paranoia. The trailer CRC covers the whole file, so a damaged snapshot is
rejected before a restore touches the host; the per-entry CRCs then catch damage
to a specific image as it's unpacked. Entry names are validated against path
traversal, because a snapshot arriving over a socket is untrusted input.

`inspect` prints all of it, including the largest entries — which is what
actually explains where a snapshot's size went:

```
$ mincontainer inspect latest.mcsnap --verify
format       : v1
producer     : mincontainer/0.1.0
engine       : criu 3.17.1
created      : 2026-08-18T20:20:24Z
source       : 8e38f9d4030c (aarch64, kernel 6.12.76-linuxkit)
container    : demo
command      : ["/bin/sh", "-c", "i=0; while true; do i=$((i+1)); echo \"tick $i\"; sleep 0.3; done"]
rootfs       : /rootfs
network      : false
dumped pid   : 11 (freeze+dump 62.8ms)
cgroup       : memory.max=134217728 cpu.max=max 100000 pids.max=128 peak=1.00MiB cpu=8815us
namespaces   : pid=Some(4026532830) mnt=Some(4026532827) net=Some(4026532831) ...
mounts       : 4
               /                        overlay    overlay
               /.mcio                   overlay    overlay
               /proc                    proc       proc
               /dev                     devtmpfs   devtmpfs
open fds     : 3
               0    -> /dev/null
               1    -> /.mcio/stdout
               2    -> /.mcio/stderr
entries      : 28 (0.19 MiB payload)
               images/pages-1.img                77824 bytes  crc 0xb05a9a2c
               images/pages-2.img                65536 bytes  crc 0x09abd1c0
               images/dump.log                   41094 bytes  crc 0xd02a16c9
               images/tmpfs-dev-6.tar.gz.img      2501 bytes  crc 0x3615f85e
               ...
checksum     : OK (whole file and every entry)
```

Those `/.mcio` entries are the container's own stdout and stderr. They live in a
directory bind-mounted into the container before `pivot_root`, because a
checkpoint records an open regular file by its path *relative to the mount
namespace root* — a log file sitting only on the host isn't a dumpable fd. It
also means the logs travel inside the snapshot, so a migrated container's output
is continuous instead of restarting empty on the new node.

## Migration

The wire format is length-prefixed frames and nothing else — no gRPC, no HTTP, no
serialisation framework. A snapshot is already a self-describing byte stream with
its own checksums, so the transport only has to delimit messages, carry bytes,
and report back what happened.

```
frame := type:u8 | len:u32 BE | payload[len]

sender                                        receiver
──────                                        ────────
HELLO      {proto, id, bytes, crc, token}  ──▶
                                    ◀── HELLO_ACK {accept, host, arch, reason}
CHUNK ...  (raw snapshot bytes)             ──▶
EOF        {bytes, crc32}                   ──▶
                                    ◀── RESULT {ok, pid, restore timings}
```

The ordering *is* the correctness argument. The container is frozen before the
first byte goes out, so it is never live in two places. The sender gives up its
claim — marking the container `migrated` — only after `RESULT` confirms a running
process on the far side.

```
running here
     │  checkpoint                    ── downtime starts
     ▼
checkpointed here
     │  send + remote restore
     ▼
running there, checkpointed here      ── briefly true, and safe
     │  mark migrated
     ▼
running there                         ── ownership released
```

The middle state — two nodes holding a valid snapshot, one holding processes — is
recoverable. The state this ordering avoids is two nodes holding live processes,
which is not.

## Failure behaviour

Restore is the half that has to be paranoid. A failed checkpoint leaves a
container running and a snapshot unwritten; a failed restore has already created
a cgroup, moved a veth, and possibly spawned processes.

Two properties make that recoverable:

**The snapshot is never consumed.** Nothing in the restore path writes to, moves,
or deletes it. Everything is unpacked into a scratch directory that's safe to
delete. So a restore that fails for any reason — including the process being
killed outright — can simply be run again.

**Every host resource is registered before it's created.** A `Rollback` value
holds the undo list; error paths run it, success disarms it. It's deliberately
*not* a `Drop` impl: rollback kills processes and tears down networking, and
doing that implicitly from a destructor is far too easy to trigger on a path that
never intended it.

A `SIGKILL` outruns the guard, so `reconcile()` sweeps stale state on the next
command — `ps` calls it too, which means debris gets cleared by whatever an
operator happens to run next rather than needing a repair tool.

Failures say what to do about them:

```
$ mincontainer migrate demo node-b:7373
migration failed: connect to node-b:7373: Connection refused

The container is checkpointed on this node and was NOT handed over.
Recover with:
    mincontainer restore demo
or retry with:
    mincontainer migrate demo node-b:7373 --use-existing
```

And things that can't be checkpointed honestly are refused rather than faked:

```
$ mincontainer checkpoint web
checkpoint failed: container has 1 established TCP connection(s):
      10.66.0.2:39114 -> 10.66.0.1:8080

Freezing drops these on the floor: the peer is not part of the checkpoint
and will keep its half of the connection open until it times out.
Either close them first, or pass --allow-tcp to serialise them — which is
only correct if the container resumes at the same address, soon, and the
peer's timeout is longer than the freeze. It is never correct across a
migration to a different address.
```

## Measurements

All numbers from an 8-run sweep on Linux 6.12.76-linuxkit aarch64 (Docker Desktop
on Apple Silicon), CRIU 3.17.1, Alpine 3.20 rootfs. Reproduce with
`scripts/bench-checkpoint.sh` and `scripts/bench-migrate.sh`.

The workload writes to every page it allocates. That matters — an untouched
allocation is never faulted in, so checkpointing it would measure nothing but
runtime overhead and report a flatteringly small snapshot.

**Checkpoint and restore, same host** (median of 8):

| touched | snapshot | freeze+dump | checkpoint total | restore total | snap/mem |
|--------:|---------:|------------:|-----------------:|--------------:|---------:|
| 0 MiB | 0.18 MiB | 36.4 ms | 45.9 ms | 109.2 ms | — |
| 16 MiB | 16.20 MiB | 37.3 ms | 119.1 ms | 176.9 ms | 1.01 |
| 64 MiB | 64.21 MiB | 48.4 ms | 336.5 ms | 384.7 ms | 1.00 |
| 128 MiB | 128.22 MiB | 53.9 ms | 615.8 ms | 648.1 ms | 1.00 |
| 256 MiB | 256.24 MiB | 70.6 ms | 1185.8 ms | 1199.8 ms | 1.00 |

**freeze+dump is the number that matters for availability** — it's the window the
container isn't running. It grows slowly (36 → 71 ms across a 256 MiB range)
because CRIU writes pages out efficiently while the process is frozen.

**Checkpoint total grows linearly**, and almost all of the difference is my
packing step. The per-stage breakdown makes it explicit — a 64 MiB checkpoint
reports `preflight 2.1ms, freeze+dump 85.3ms, pack 300.0ms`, so packing is ~213
MiB/s and dominates everything else. That cost is a direct consequence of the
format; see the tradeoff below.

An idle container's snapshot floor is 0.18 MiB, and beyond that the snapshot
tracks touched memory to within 1%.

**Cross-host migration**, two containers on a Docker bridge network (median of 5):

| touched | snapshot | checkpoint | hash | transfer | remote restore | total |
|--------:|---------:|-----------:|-----:|---------:|---------------:|------:|
| 0 MiB | 0.19 MiB | 59 ms | 0.6 ms | 0.3 ms | 111 ms | 173 ms |
| 64 MiB | 64.22 MiB | 337 ms | 129 ms | 126 ms | 385 ms | 1120 ms |

Transfer throughput is ~510 MiB/s over the Docker bridge. 5/5 runs at each size
resumed correctly on node B, verified by the counter continuing rather than by
the transfer reporting success.

`hash` is the sender checksumming the snapshot before connecting, so a corrupt
file is caught before the peer takes custody of it. It's broken out separately
rather than charged to the network.

## Design decisions and tradeoffs

**The runtime owns networking; the engine never touches it.**
Both dump and restore pass `--empty-ns net`: give the container a bare network
namespace and leave its contents to the caller. On restore, the same
`Network::setup` that ran at start rewires the veth, address and route from
config. Asking CRIU to serialise a veth would duplicate logic that already
exists, and would make restore depend on the engine reproducing my addressing
scheme instead of on re-running the code that created it.

It's also the only thing that works here — see the hardest bug.

*Cost:* a restored container gets a new veth. Nothing outside the container that
was tracking the old interface survives. Since the address is reassigned from
config, the container keeps its IP, which is what actually matters.

**The snapshot format costs an extra pass over the data.**
The manifest has to be written *before* the entries, but each entry's CRC is only
known after streaming it. So packing writes the payload to a scratch file while
accumulating metadata, then writes the real file with a complete manifest in
front. That's two writes and one extra read of every byte, and it's the ~213 MiB/s
ceiling visible in the checkpoint-total column.

I took that deliberately: one file that's checksummed, self-describing, and
verifiable in a single pass is worth more than the throughput. The obvious fix if
it ever mattered would be to reserve a fixed-size manifest slot at the head, or
move the manifest into the trailer and seek — either removes the extra pass
without changing the format's guarantees.

**CRIU is driven as a subprocess, not through libcriu or RPC.**
The CLI is the interface CRIU documents and stabilises, it keeps the dependency at
"a binary on `$PATH`", and — the thing that actually mattered in practice — a
failed dump leaves a verbose log that can be quoted back to the user verbatim
instead of an opaque error code. Almost every bug in this project was diagnosed
from that log.

**Established TCP connections are refused unless explicitly allowed.**
CRIU *can* serialise them. But the peer isn't part of the checkpoint and can't be
told to wait, so the connection is only still valid if the container comes back at
the same address before the peer gives up. That's a judgement about the
deployment, not something a runtime can make on the operator's behalf.

**Migration is a shared token over plaintext.**
Anyone who can reach the port and knows the token can hand this node a process
image to run — which is what migration *is*; it's remote code execution by
design. The receiver warns when no token is set. A real deployment wants mutual
TLS, and the framing wouldn't have to change to get it.

**The receiver is single-threaded.**
A restore takes over cgroups and network interfaces, and serialising arrivals
removes a whole class of races for a workload that isn't throughput-bound.

## The hardest bug

Not the one that took longest, but the one where I had the wrong model.

Restore checked that the rebuilt container's namespaces were *new*, by comparing
the inode numbers behind `/proc/<pid>/ns/*` against the ones recorded in the
manifest at checkpoint time. The reasoning seemed solid: if a restored process
turned up in the same namespace as the container we'd checkpointed, we'd have two
containers sharing one namespace, which is silent corruption. Worth a hard failure.

It passed every test until the one that kills a restore halfway through and
retries it:

```
restore failed: restored process is in the *original* uts namespace
(inode 4026532828) — the checkpointed container is apparently still alive
```

Nothing was alive. The invariant was just false. **The kernel recycles namespace
inode numbers.** Once a namespace is destroyed its inode goes back in the pool,
and the very next namespace created can reuse it — which is exactly what happens
when you tear down a half-finished restore and immediately start another. The
check was near-certain to misfire on retry-after-interrupted-restore, and that is
*precisely* the recovery path it existed to protect. It would have refused to
bring back a container that was perfectly recoverable.

What makes it the hardest one is that the code was fine, the tests were fine, and
the failure was in the assumption underneath both. Any test that didn't
deliberately kill a restore mid-flight would have shipped it.

The replacement checks something that's actually true: the restored process must
not share the namespaces of *this* process, which lives in the host's. That
catches the real failure — an engine that didn't build the namespaces it was asked
for — without inventing an invariant the kernel never promised.

Three other bugs worth recording, all of them races or wrong assumptions rather
than logic errors:

- **`--leave-running` left the container marked `checkpointing`.** It was still
  running, so reconciliation would later mistake it for an interrupted dump and
  kill the processes that had been deliberately kept alive.

- **A zombie counted as a running process.** `pid_alive` tested `/proc/<pid>` for
  existence, but that entry survives until the parent reaps the child. An
  exited-but-unreaped supervisor read as alive, so checkpoint waited out its full
  5-second timeout — turning an 89 ms checkpoint into a 5.1-second one.

- **The supervisor's cleanup raced the next restore.** A dump kills the container,
  waking the supervisor to remove the cgroup. Nothing made checkpoint wait, so
  that `remove_dir` could land *after* a later restore had recreated the cgroup —
  and the restore then failed moving its process into a cgroup that had just
  vanished. Normally a microsecond-wide window; it only reproduced with `--net`,
  because deleting a veth makes the supervisor slow enough to lose the race every
  single time.

## Limitations

- **Checkpointing requires `start --detach`.** A foreground container shares the
  launching terminal's session and can't be dumped. See above.
- **The rootfs doesn't travel.** A snapshot carries the container's memory, not its
  filesystem. The target node needs the rootfs staged at the same path, and
  restore fails loudly if it isn't there. Bind-mounted volumes likewise — a
  missing volume source is a hard error, since silently creating it empty would
  bring the container back with its data gone.
- **No incremental or pre-copy migration.** Downtime is the full
  checkpoint→transfer→restore path. Pre-copy would need dirty-page tracking
  across iterations.
- **Same architecture only.** A snapshot is a register dump and a set of page
  images; an aarch64 checkpoint on an x86_64 host isn't a compatibility problem to
  work around, it's meaningless. Refused at both the handshake and the restore.
- **No image management.** Bring your own rootfs.
- **Migration isn't encrypted or mutually authenticated.**
- **Restored containers are orphaned to init** rather than re-parented under a new
  supervisor, so `stop` signals them directly instead of going through one.

## Building and testing

```bash
docker build -f Dockerfile.dev -t mincontainer-dev .

# Build
docker run --rm -v "$PWD":/work -w /work -v mc-target:/target \
    mincontainer-dev cargo build --release

# Full suite. MC_TEST_STRICT=1 turns "this host can't run the test" into a
# failure — a suite that quietly skips everything is worse than one that fails.
docker run --rm --privileged -v "$PWD":/work -w /work -v mc-target:/target \
    -e MC_TEST_STRICT=1 -e HOME=/root \
    mincontainer-dev cargo test --release -- --test-threads=1

# Benchmarks
docker run --rm --privileged -v "$PWD":/work -w /work -v mc-target:/target \
    -e HOME=/root mincontainer-dev bash scripts/bench-checkpoint.sh
bash scripts/bench-migrate.sh 5 64        # from the host — drives two nodes
```

35 tests: 17 unit, 18 integration across four suites.

The integration tests drive the real binary against real containers. There's no
mock engine and no in-process shortcut, because a test that never actually freezes
a process tells you nothing about whether checkpointing works. The central
assertion throughout is *counter continuity*, not liveness — a runtime that
quietly relaunched a container from scratch would sail through a "something is
running afterwards" check, and that's the exact failure mode being ruled out.

`tests/failure_recovery.rs` flips a bit in the middle of a snapshot's payload
(not its header, which parsing alone would catch), truncates a snapshot, and
`SIGKILL`s a restore mid-flight — then requires that the snapshot survived, the
container isn't stuck in a transient state, no cgroup or staging debris is left
behind, and a plain retry succeeds.

## Layout

```
src/snapshot.rs     the .mcsnap format — framing, manifest, CRC32
src/checkpoint.rs   freeze: validate, pre-flight, collect metadata, dump, pack
src/restore.rs      rebuild: verify, unpack, build, rollback, reconcile
src/transport.rs    raw-TCP framing, sender and receiver
src/migrate.rs      sequencing, and who owns the container when
src/criu.rs         the engine adapter — the only file that knows about CRIU
```

The original runtime (`container.rs`, `cgroups.rs`, `network.rs`, `seccomp.rs`,
`capabilities.rs`, `state.rs`) is unchanged except where checkpointing needed
something from it.

## License

MIT
