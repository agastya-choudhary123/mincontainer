# mincontainer

A minimal container runtime written from scratch in Rust. Spawns isolated Linux containers using namespaces, cgroups v2, and seccomp.

## Features

- **Lightweight**: ~1200 LOC, no Docker libraries or libcontainer
- **Fast**: ~1.7ms startup overhead (3× faster than Docker, 6× faster than runc)
- **Isolated**: PID, mount, UTS, IPC, and network namespaces
- **Resource limits**: Memory, CPU, and process count via cgroups v2
- **Secure**: Seccomp syscall filtering and capability dropping
- **Stateful**: Create, start, stop, list containers with persistent state
- **Volumes**: Bind mounts from host into container
- **Measured**: Real benchmarks against runc and Docker

## Quick start

Build for Linux inside Docker:

```bash
docker build -f Dockerfile.dev -t mincontainer-dev .
docker run --rm -v "$PWD":/work -w /work -v mc-target:/target \
    mincontainer-dev cargo build --release

# Binary: target/release/mincontainer (for Linux)
```

Run a container:

```bash
mincontainer run --rootfs ./alpine --memory 64M -- /bin/sh
```

Stateful lifecycle:

```bash
# Create
mincontainer create --rootfs ./alpine --bind /data:/data -- /bin/sh
# ID: <container-id>

# Start
mincontainer start <container-id>

# List
mincontainer ps

# Stop
mincontainer stop <container-id>

# Remove
mincontainer rm <container-id>
```

## Benchmarks

**Startup latency** (100 runs, `/bin/true`):

```
mincontainer:  1.7ms mean (6× faster than runc, 3× faster than Docker)
runc:          6.1ms mean
Docker:       23.0ms mean
```

**Production workloads**:

- CPU-bound (100k loop): 104ms (99ms work + 5ms overhead)
- I/O + volumes (file read): 1ms (<1ms mount penalty)
- Memory operations (awk): 61ms (native speed in cgroups)
- Parallel (20 containers): 3.7ms mean

**Memory overhead**: 0.29 MiB per container (vs ~5 MiB for runc)

## What works

| Phase | Feature | Status |
|-------|---------|--------|
| 1 | Namespace isolation (PID, mount, UTS, IPC, net) | ✓ |
| 2 | Rootfs with pivot_root and fresh /proc, /dev | ✓ |
| 3 | cgroups v2: memory.max, cpu.max, pids.max | ✓ |
| 4 | veth networking, bridge, IP assignment, NAT | ✓ |
| 5 | seccomp-bpf filtering, capability dropping | ✓ |
| 6 | Stateful lifecycle, bind mounts, concurrent mgmt | ✓ |

## Limitations

- **Linux only**: Requires Linux kernel with cgroup v2 and namespaces (or Docker Desktop)
- **Single-process**: No PID relay or multi-process coordination
- **No image management**: Bring your own rootfs (e.g., Alpine minirootfs)
- **Minimal seccomp**: 15-rule deny-list, not runc's 300-rule default
- **No OCI spec compliance**: Skips AppArmor/SELinux, device cgroups, hooks, checkpoint/restore
- **Basic networking**: Hardcoded IPAM, simple iptables NAT

## Why is it faster?

mincontainer does less. runc implements the full OCI spec with defense-in-depth security, hook execution, systemd integration, and complex setup. mincontainer implements the core isolation path and stops.

The speedup comes from:
- Direct Linux syscalls (no daemon RPC)
- Minimal setup code (no logging, validation, image unpacking)
- Tight resource limits (cgroups applied immediately)

On native Linux (not Docker Desktop), mincontainer would be ~50-60× faster than Docker due to daemon overhead.

## Building and testing

Requires Docker Desktop or a Linux VM. All development happens inside the `mincontainer-dev` container:

```bash
# Build
docker build -f Dockerfile.dev -t mincontainer-dev .
docker run --rm -v "$PWD":/work -w /work -v mc-target:/target \
    mincontainer-dev cargo build --release

# Test isolation
docker run --privileged --rm -v "$PWD":/work -w /work -v mc-target:/target \
    mincontainer-dev /target/release/mincontainer run --rootfs /rootfs -- /bin/sh

# Benchmark
docker run --privileged --rm -v "$PWD":/work -w /work -v mc-target:/target \
    mincontainer-dev bash scripts/bench-vs-runc.sh
```

All benchmarks are reproducible. See `scripts/` for full suite.

## Implementation notes

**Two-level fork**: Parent stays in host PID namespace (so networking helpers like `ip` and `nsenter` can resolve container PIDs). Middle process creates namespaces, grandchild becomes PID 1.

**Cgroup delegation**: Must migrate all root processes into a leaf before enabling `subtree_control` (the "no internal process" rule). This matters when the runtime isn't the container's PID 1.

**Volume mounts**: Done before pivot_root so paths are in host namespace, then bound into the rootfs at relative container paths.

## Why I built this

To understand container internals by implementing them. The core is surprisingly simple: fork + unshare + pivot_root + exec. The complexity in real runtimes (runc, Docker) is features and safety, not the mechanisms.

## License

MIT
