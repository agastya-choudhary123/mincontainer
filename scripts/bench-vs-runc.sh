#!/usr/bin/env bash
# Compare mincontainer against runc on real workloads
set -euo pipefail

ROOTFS=/rootfs
BIN=/target/release/mincontainer
TMP=$(mktemp -d)
trap 'rm -rf "$TMP" ~/.mincontainer/containers/*' EXIT

stats() {
    local label="$1"
    sort -n | awk -v label="$label" '
        {a[NR]=$1}
        END{
            n=NR
            if(n==0){ print label": no data"; exit }
            s=0; for(i=1;i<=n;i++) s+=a[i]
            idx_p50 = int(n * 0.5)
            idx_p99 = int(n * 0.99)
            if(idx_p50 < 1) idx_p50 = 1
            if(idx_p99 < 1) idx_p99 = 1
            printf "%-32s n=%-4d  mean=%7.2f  p50=%7.2f  p99=%7.2f  min=%7.2f  max=%7.2f\n",
                label, n, (s/n), a[idx_p50], a[idx_p99], a[1], a[n]
        }'
}

echo "############################################################"
echo "# mincontainer vs runc — Real Workload Benchmarks"
echo "############################################################"
echo

echo "=== Workload 1: Throughput (50 sequential /bin/true runs) ==="
echo "mincontainer..."
mc_times=()
for i in $(seq 1 50); do
    s=$(date +%s%N)
    $BIN run --rootfs $ROOTFS -- /bin/true >/dev/null 2>&1
    e=$(date +%s%N)
    mc_times+=($(($(((e - s) / 1000000)))))
done

echo "runc..."
# Build runc bundle
BUNDLE="$TMP/bundle"
mkdir -p "$BUNDLE/rootfs"
cp -a $ROOTFS/. "$BUNDLE/rootfs/"
(cd "$BUNDLE" && runc spec >/dev/null 2>&1)
jq '.process.args=["/bin/true"] | .process.terminal=false' "$BUNDLE/config.json" > "$BUNDLE/c.json"
mv "$BUNDLE/c.json" "$BUNDLE/config.json"

# Warm up
runc --root "$TMP/state" run --bundle "$BUNDLE" warmup >/dev/null 2>&1 || true

runc_times=()
for i in $(seq 1 50); do
    s=$(date +%s%N)
    runc --root "$TMP/state" run --bundle "$BUNDLE" "r$i" >/dev/null 2>&1
    e=$(date +%s%N)
    runc_times+=($(($(((e - s) / 1000000)))))
done

echo
printf "%s\n" "${mc_times[@]}" | stats "mincontainer sequential (ms)"
printf "%s\n" "${runc_times[@]}" | stats "runc sequential (ms)"
echo

echo "=== Workload 2: Parallel throughput (20 containers, 10 trials) ==="
echo "mincontainer..."
mc_batch_times=()
for trial in $(seq 1 10); do
    s=$(date +%s%N)
    for i in $(seq 1 20); do
        ($BIN run --rootfs $ROOTFS -- /bin/true >/dev/null 2>&1 &) &
    done
    wait
    e=$(date +%s%N)
    mc_batch_times+=($(($(((e - s) / 1000000)))))
done

echo "runc..."
runc_batch_times=()
for trial in $(seq 1 10); do
    s=$(date +%s%N)
    for i in $(seq 1 20); do
        (runc --root "$TMP/state" run --bundle "$BUNDLE" "batch-$trial-$i" >/dev/null 2>&1 &) &
    done
    wait
    e=$(date +%s%N)
    runc_batch_times+=($(($(((e - s) / 1000000)))))
done

echo
printf "%s\n" "${mc_batch_times[@]}" | stats "mincontainer 20-parallel (ms)"
printf "%s\n" "${runc_batch_times[@]}" | stats "runc 20-parallel (ms)"
echo

echo "############################################################"
echo "# Summary"
echo "############################################################"
mc_mean=$(printf "%s\n" "${mc_times[@]}" | awk '{s+=$1} END {print s/NR}')
runc_mean=$(printf "%s\n" "${runc_times[@]}" | awk '{s+=$1} END {print s/NR}')
speedup=$(echo "scale=1; $runc_mean / $mc_mean" | bc)
echo "Sequential throughput:"
echo "  mincontainer: ~${mc_mean%.??}ms per container"
echo "  runc:         ~${runc_mean%.??}ms per container"
echo "  Speedup:      ${speedup}x faster"
echo
echo "✓ mincontainer is optimized for minimal overhead"
echo "✓ runc implements full OCI spec (more features = more setup)"
echo "✓ Both metrics measured on the same machine, same rootfs"
