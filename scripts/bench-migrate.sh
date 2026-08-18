#!/usr/bin/env bash
# Genuinely cross-host migration: two containers on a Docker network, each with
# its own kernel-visible namespaces, filesystem and state directory. Run from
# the repo root on the *host* (this script drives docker, it does not run
# inside the dev container).
#
#   bash scripts/bench-migrate.sh [runs] [touch_mb]
#
# Node A starts a container, lets it count, then migrates it to node B. The
# check that matters is not that the transfer succeeded but that the counter
# continues on B from where it stopped on A.
set -euo pipefail

RUNS=${1:-5}
TOUCH_MB=${2:-0}
NET=mc-migrate-bench
TOKEN=bench-token
IMAGE=mincontainer-dev

cleanup() {
    docker rm -f mc-node-a mc-node-b >/dev/null 2>&1 || true
    docker network rm "$NET" >/dev/null 2>&1 || true
}
trap cleanup EXIT
cleanup

docker network create "$NET" >/dev/null

common=(--privileged --network "$NET" -v "$PWD":/work -w /work
        -v mc-target:/target -e HOME=/root)

docker run -d --name mc-node-b "${common[@]}" "$IMAGE" \
    /target/release/mincontainer serve --listen 0.0.0.0:7373 --token "$TOKEN" >/dev/null
docker run -d --name mc-node-a "${common[@]}" "$IMAGE" sleep infinity >/dev/null

# Wait for the receiver to be listening. bash's /dev/tcp avoids depending on
# netcat being present in the image.
for _ in $(seq 1 50); do
    if docker exec mc-node-a bash -c \
        'exec 3<>/dev/tcp/mc-node-b/7373' >/dev/null 2>&1; then
        break
    fi
    sleep 0.2
done

echo "############################################################"
echo "# cross-host migration: node A -> node B"
echo "# $RUNS runs, workload touches ${TOUCH_MB} MiB"
echo "############################################################"
echo
printf "%-5s %10s %9s %8s %9s %11s %10s %8s\n" \
    "run" "size(MiB)" "ckpt(ms)" "hash(ms)" "xfer(ms)" "restore(ms)" "total(ms)" "MiB/s"

total_ok=0
for i in $(seq 1 "$RUNS"); do
    id="mig-$i"
    # Same doubling allocation as bench-cr: appending fixed chunks is quadratic
    # in the shell and never finishes at these sizes.
    if [ "$TOUCH_MB" -gt 0 ]; then
        script="target=\$(($TOUCH_MB * 1024 * 1024)); \
                s=\$(awk 'BEGIN{while(i++<1024) printf \"x\"}'); \
                while [ \${#s} -lt \$target ]; do s=\"\$s\$s\"; done; \
                n=0; while true; do n=\$((n+1)); echo \"tick \$n\"; sleep 0.2; done"
    else
        script="n=0; while true; do n=\$((n+1)); echo \"tick \$n\"; sleep 0.2; done"
    fi
    limit=$(( (TOUCH_MB * 4 + 128) * 1024 * 1024 ))

    docker exec mc-node-a /target/release/mincontainer \
        create --rootfs /rootfs --id "$id" --memory "$limit" \
        -- /bin/sh -c "$script" >/dev/null
    docker exec mc-node-a /target/release/mincontainer start -d "$id" >/dev/null 2>&1

    # Let it get far enough that "resumed" is distinguishable from "restarted".
    docker exec mc-node-a sh -c "
        for _ in \$(seq 1 100); do
            grep -q 'tick 5' /root/.mincontainer/containers/$id/io/stdout 2>/dev/null && exit 0
            sleep 0.1
        done; exit 1" || { echo "run $i: workload never started"; continue; }

    before=$(docker exec mc-node-a sh -c \
        "grep -c '^tick' /root/.mincontainer/containers/$id/io/stdout")

    report=$(docker exec mc-node-a /target/release/mincontainer \
        migrate "$id" mc-node-b:7373 --token "$TOKEN" --json 2>/dev/null) || {
        echo "run $i: migration failed"; continue; }

    read -r bytes ckpt hash xfer restore total tput <<<"$(echo "$report" | jq -r \
        '[.snapshot_bytes, .checkpoint_ms, .hash_ms, .transfer_ms,
          .remote_restore_ms, .total_ms, .throughput_mib_s] | @tsv')"

    printf "%-5s %10.2f %9.1f %8.1f %9.1f %11.1f %10.1f %8.0f\n" \
        "$i" "$(echo "$bytes" | awk '{print $1/1048576}')" \
        "$ckpt" "$hash" "$xfer" "$restore" "$total" "$tput"

    # The real assertion: the container is alive on B and its counter carried on.
    sleep 1
    after=$(docker exec mc-node-b sh -c \
        "grep -c '^tick' /root/.mincontainer/containers/$id/io/stdout" 2>/dev/null || echo 0)
    if [ "$after" -le "$before" ]; then
        echo "  run $i: FAILED — counter did not advance on node B ($before -> $after)"
    else
        total_ok=$((total_ok + 1))
    fi

    docker exec mc-node-b /target/release/mincontainer stop "$id" >/dev/null 2>&1 || true
    docker exec mc-node-b /target/release/mincontainer rm "$id" >/dev/null 2>&1 || true
    docker exec mc-node-a /target/release/mincontainer rm "$id" >/dev/null 2>&1 || true
done

echo
echo "$total_ok/$RUNS migrations resumed correctly on node B."
[ "$total_ok" -eq "$RUNS" ]
