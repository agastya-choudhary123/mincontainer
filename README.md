# mincontainer

A small Linux container runtime in Rust. It does the usual isolation
(namespaces, cgroups v2, seccomp, capability dropping), and it can checkpoint a
running container to a single file and restore it, either on the same host or
on another machine over TCP.

```
$ mincontainer checkpoint web
checkpointed web — 64.21 MiB in 28 entries
  (preflight 2.1ms, freeze+dump 85.3ms, pack 300.0ms, total 396.0ms)

$ mincontainer migrate web node-b:7373 --token $TOKEN
migrated web to node-b:7373 — 64.21 MiB
  checkpoint 337ms, hash 129ms, transfer 126ms (510 MiB/s),
  remote restore 385ms (build 112ms), total 1120ms
  now pid 18 on node-b:7373
```

[CRIU](https://criu.org) does the actual process serialization. I wrote the
snapshot format, the restore ordering and rollback, and the migration protocol.
`src/criu.rs` (about 330 lines) is the only file that talks to CRIU.

The runtime is about 5,600 lines, plus about 1,100 lines of tests.

## Features

- PID, mount, UTS, IPC and network namespaces, `pivot_root`, cgroups v2
- seccomp-bpf filter and capability dropping
- `create` / `start` / `stop` / `ps` / `logs` / `rm` / `run`, foreground or detached
- `checkpoint` / `restore` / `inspect` for snapshots
- `migrate` / `serve` to move a container to another node
- `info` checks whether the host can checkpoint (and shows what `criu check` reports)

## Quick start

This only runs on Linux and needs root, so everything happens in the dev
image:

```bash
docker build -f Dockerfile.dev -t mincontainer-dev .
docker run --rm -v "$PWD":/work -w /work -v mc-target:/target \
    mincontainer-dev cargo build --release
```

Then, in a privileged container:

```bash
mincontainer create --rootfs /rootfs --id demo -- \
    /bin/sh -c 'i=0; while true; do i=$((i+1)); echo "tick $i"; sleep 0.3; done'
mincontainer start -d demo
mincontainer logs demo          # tick 1 ... tick 7

mincontainer checkpoint demo    # process tree is stopped and saved
mincontainer ps                 # demo   checkpointed   -   0   latest.mcsnap
mincontainer inspect ~/.mincontainer/containers/demo/snapshots/latest.mcsnap --verify

mincontainer restore demo
mincontainer logs demo          # ... tick 7, tick 8, tick 9
```

After the restore the counter picks up where it stopped instead of starting
over.

To migrate, run a receiver on the target and send the container to it:

```bash
# node B
mincontainer serve --listen 0.0.0.0:7373 --token hunter2

# node A
mincontainer migrate demo node-b:7373 --token hunter2
```

## How it works

### Checkpoint

The steps are ordered so the cheap checks run before the container is frozen:

1. Check that the container is running and its pid is real.
2. Pre-flight checks: is the state checkpointable at all? (For example, open
   TCP connections are refused unless you pass `--allow-tcp`.)
3. Read namespaces, mounts, fds, and cgroup limits from procfs and sysfs.
4. Mark the container as `checkpointing`.
5. CRIU freezes and dumps the process tree.
6. Pack the images, logs, and metadata into a single `.mcsnap` file.
7. Mark the container as `checkpointed`.

Checkpoint only works on detached containers (`start -d`). CRIU refuses to
dump a process whose session leader is outside its PID namespace, so detached
containers call `setsid()`. Foreground containers can't do that without losing
the terminal and Ctrl-C.

The container's stdout and stderr go to a directory that is bind-mounted in
before `pivot_root`. That way CRIU can record the fds, and the logs travel
inside the snapshot.

### Snapshot format

```
magic        "MCSNAP\0" + format byte            8 bytes
manifest_len u32 LE                              4 bytes
manifest     JSON
entry[i]     name_len u32 | name | data_len u64 | data
trailer      "MCSNAPED" + crc32 of everything before it
```

The manifest has the container config, its cgroup limits as read back from
`/sys/fs/cgroup`, namespace inodes, the mount table, the fd table, and an
index of entries with a CRC for each.

The trailer CRC lets a damaged file be rejected before a restore touches
anything on the host. The per-entry CRCs catch damage during unpacking.
Entry names are checked for path traversal, since snapshots can come in over
the network.

### Migration

The protocol is length-prefixed frames over plain TCP:

```
frame := type:u8 | len:u32 BE | payload[len]

HELLO {proto, id, bytes, crc, token}   ->
                                       <- HELLO_ACK {accept, host, arch, reason}
CHUNK ... (snapshot bytes)             ->
EOF {bytes, crc32}                     ->
                                       <- RESULT {ok, pid, restore timings}
```

The container is frozen before anything is sent, so it is never running in
two places. The sender only marks it `migrated` after `RESULT` confirms that a
process is running on the other end. If something goes wrong partway, both
nodes may hold a snapshot, but only one has a live process.

### Restore and failure handling

- Restore never modifies the snapshot. Everything is unpacked into a scratch
  directory, so a failed restore can just be run again.
- Every host resource (cgroup, veth, process) is registered with a `Rollback`
  before it's created, and an error path undoes all of them. This is an
  explicit call rather than a `Drop` impl, because I didn't want process kills
  and network teardown happening from a destructor.
- If the restore is `SIGKILL`ed, `reconcile()` cleans up the leftover state on
  the next command (`ps` runs it too).
- Errors say how to recover. For example, a failed migration tells you to run
  `restore` locally or retry with `--use-existing`.

### Design choices

- **Networking stays with the runtime.** Dump and restore both pass
  `--empty-ns net`, and restore re-runs the same network setup code as
  `start`. The container keeps its IP but gets a new veth.
- **Packing takes an extra pass.** The manifest goes at the front, but the
  per-entry CRCs aren't known until the data has streamed through. So the
  payload goes to a scratch file first and then gets copied. That caps
  packing at about 213 MiB/s. Putting the manifest in the trailer would fix
  it.
- **CRIU runs as a subprocess** instead of through libcriu. The CLI is
  stable, and when a dump fails CRIU's log can be shown to the user directly.
  Most bugs in this project were diagnosed from that log.
- **Migration auth is a shared token over plaintext.** Accepting a migration
  means running whatever process image arrives, so a real deployment would
  need mutual TLS.
- **The receiver is single-threaded**, which avoids races over cgroups and
  network interfaces.

## Measurements

These were measured on Docker Desktop on Apple silicon (Linux 6.12.76-linuxkit,
aarch64), with CRIU 3.17.1 and an Alpine 3.20 rootfs. The workload writes to
every page it allocates; untouched pages never get faulted in and would make
the snapshot look smaller than it really is.

Checkpoint and restore on the same host (median of 8 runs):

| touched | snapshot | freeze+dump | checkpoint total | restore total |
|--------:|---------:|------------:|-----------------:|--------------:|
| 0 MiB | 0.18 MiB | 36.4 ms | 45.9 ms | 109.2 ms |
| 16 MiB | 16.20 MiB | 37.3 ms | 119.1 ms | 176.9 ms |
| 64 MiB | 64.21 MiB | 48.4 ms | 336.5 ms | 384.7 ms |
| 128 MiB | 128.22 MiB | 53.9 ms | 615.8 ms | 648.1 ms |
| 256 MiB | 256.24 MiB | 70.6 ms | 1185.8 ms | 1199.8 ms |

The container is only actually stopped during freeze+dump, which goes from
36 to 71 ms across this range. Most of the rest of the checkpoint time is the
packing step described above.

Migration between two containers on a Docker bridge network (median of 5):

| touched | snapshot | checkpoint | hash | transfer | remote restore | total |
|--------:|---------:|-----------:|-----:|---------:|---------------:|------:|
| 0 MiB | 0.19 MiB | 59 ms | 0.6 ms | 0.3 ms | 111 ms | 173 ms |
| 64 MiB | 64.22 MiB | 337 ms | 129 ms | 126 ms | 385 ms | 1120 ms |

All 5 runs at each size resumed correctly on the target. I checked that by
making sure the counter continued, not by trusting the transfer status.

To reproduce, run `scripts/bench-checkpoint.sh` and `scripts/bench-migrate.sh`.

## Bugs worth writing down

- **Namespace inode check.** Restore used to fail if the new process's
  namespace inodes matched the ones in the manifest, to catch two containers
  sharing a namespace. But the kernel reuses namespace inode numbers. Killing
  a restore partway and retrying it right away would reliably trip the check,
  and that's exactly the recovery path it was supposed to protect. Restore now
  checks that the process doesn't share namespaces with the runtime itself.
- **`--leave-running` left the state as `checkpointing`.** Reconcile would
  later treat that as an interrupted dump and kill the container.
- **Zombies counted as alive.** `pid_alive` only checked whether
  `/proc/<pid>` existed, so an unreaped supervisor made checkpoint wait out
  its 5 s timeout.
- **Supervisor cleanup raced the next restore.** The old supervisor could
  delete the cgroup after a new restore had already recreated it. It only
  showed up with `--net`, because veth teardown slowed the supervisor down
  enough to lose the race every time.

## Limitations

- Checkpointing requires `start --detach`.
- The rootfs isn't included in the snapshot, so the target needs it at the
  same path. The same goes for bind-mounted volumes; a missing volume is an
  error rather than being silently created empty.
- No pre-copy migration. Downtime covers the full checkpoint, transfer, and
  restore.
- Snapshots only restore on the same CPU architecture. A mismatch is refused
  at the handshake and again at restore.
- No image management. You provide the rootfs.
- Migration isn't encrypted or mutually authenticated.
- Restored containers are reparented to init instead of a new supervisor, so
  `stop` signals them directly.

## Tests

```bash
docker run --rm --privileged -v "$PWD":/work -w /work -v mc-target:/target \
    -e MC_TEST_STRICT=1 -e HOME=/root \
    mincontainer-dev cargo test --release -- --test-threads=1
```

There are 35 tests: 17 unit tests and 18 integration tests. The integration
tests run the real binary against real containers. Most of them check that
the counter continues after a restore, because a runtime that silently
restarted the container would still pass a plain "is it running" check.
`MC_TEST_STRICT=1` turns "this host can't run the test" into a failure
instead of a skip.

`tests/failure_recovery.rs` flips a bit in a snapshot's payload, truncates a
snapshot, and kills a restore partway through. In each case it checks that the
snapshot is intact, nothing is stuck in a transient state, no cgroups or
staging files are left behind, and a plain retry works.

## Layout

```
src/snapshot.rs     .mcsnap format: framing, manifest, CRC32
src/checkpoint.rs   validate, pre-flight, collect metadata, dump, pack
src/restore.rs      verify, unpack, rebuild, rollback, reconcile
src/transport.rs    TCP framing, sender and receiver
src/migrate.rs      migration ordering and ownership
src/criu.rs         CRIU adapter
src/container.rs, cgroups.rs, network.rs, seccomp.rs, capabilities.rs, state.rs
                    the base runtime
scripts/            benchmarks
tests/              integration tests
```
