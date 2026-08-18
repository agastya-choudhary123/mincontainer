#!/usr/bin/env bash
# Checkpoint and restore cost as a function of how much memory the container
# actually touches. Run inside the privileged mincontainer-dev container:
#
#   docker run --privileged --rm -v "$PWD":/work -w /work -v mc-target:/target \
#       mincontainer-dev bash scripts/bench-checkpoint.sh
#
# The workload writes to every page it allocates. That matters: an untouched
# allocation is never faulted in, so checkpointing it would measure nothing but
# the runtime's own overhead and report a flatteringly small snapshot.
set -euo pipefail

RUNS=${RUNS:-8}
BIN=/target/release/mincontainer
ROOTFS=${ROOTFS:-/rootfs}
SIZES=${SIZES:-"0 16 64 128 256"}

export HOME=${HOME:-/root}

if [ ! -x "$BIN" ]; then
    echo "no binary at $BIN — run 'cargo build --release' first" >&2
    exit 1
fi

echo "############################################################"
echo "# checkpoint / restore vs. touched memory"
echo "# $(uname -srm), criu $($BIN info | awk '/engine/{print $NF}')"
echo "# $RUNS runs per size"
echo "############################################################"
echo
printf "%-10s %12s %12s %12s %12s %10s\n" \
    "touched" "snapshot" "freeze+dump" "ckpt total" "restore" "ratio"
printf "%-10s %12s %12s %12s %12s %10s\n" \
    "(MiB)" "(MiB)" "(ms)" "(ms)" "(ms)" "snap/mem"

for mb in $SIZES; do
    # Headroom over the workload's own footprint: the cgroup ceiling has to
    # cover the shell's doubling buffer (which briefly holds two copies) plus
    # the interpreter itself, or the container is OOM-killed before it can be
    # checkpointed and the run measures nothing.
    limit=$(( (mb * 4 + 128) * 1024 * 1024 ))

    json=$("$BIN" bench-cr --rootfs "$ROOTFS" --runs "$RUNS" \
        --touch-mb "$mb" --memory "$limit" --json 2>/dev/null)
    if [ -z "$json" ]; then
        echo "  ${mb} MiB: FAILED (no output)" >&2
        continue
    fi

    echo "$json" | jq -r --arg mb "$mb" '
        def mean: if length == 0 then 0 else (add / length) end;
        def p50: sort | .[(length / 2) | floor];
        [
            $mb,
            ((.snapshot_bytes | mean) / 1048576),
            (.checkpoint_dump_ms | p50),
            (.checkpoint_total_ms | p50),
            (.restore_total_ms | p50),
            (if ($mb | tonumber) > 0
             then ((.snapshot_bytes | mean) / 1048576) / ($mb | tonumber)
             else 0 end)
        ] | @tsv' | while IFS=$'\t' read -r m snap dump total restore ratio; do
        if [ "$m" = "0" ]; then
            printf "%-10s %12.2f %12.1f %12.1f %12.1f %10s\n" \
                "$m" "$snap" "$dump" "$total" "$restore" "-"
        else
            printf "%-10s %12.2f %12.1f %12.1f %12.1f %10.2f\n" \
                "$m" "$snap" "$dump" "$total" "$restore" "$ratio"
        fi
    done
done

echo
echo "freeze+dump is the window the container is unavailable."
echo "ckpt total adds pre-flight, packing, and waiting for the supervisor to"
echo "tear down; restore covers checksum verification, unpack and rebuild."
