#!/usr/bin/env bash
# Compare mincontainer startup latency against runc (the reference OCI runtime
# that Docker and Kubernetes use under the hood). Run inside the privileged
# mincontainer-dev container.
set -euo pipefail

RUNS=${1:-50}
BIN=/target/release/mincontainer
ROOTFS=/rootfs
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

stats() { # <label> < stream-of-microsecond-ints
    local label="$1"
    sort -n | awk -v label="$label" '
        {a[NR]=$1}
        END{
            n=NR
            if(n==0){ print label": no data" }
            else {
                s=0; for(i=1;i<=n;i++) s+=a[i]
                printf "%-26s n=%-4d mean=%8.3f  p50=%8.3f  p99=%8.3f  min=%8.3f  max=%8.3f\n",
                    label, n, (s/n)/1000, a[int(n*0.5)+1]/1000, a[int(n*0.99)]/1000, a[1]/1000, a[n]/1000
            }
        }'
}

echo "############################################################"
echo "# mincontainer  (this project)"
echo "############################################################"
$BIN bench --rootfs "$ROOTFS" --runs "$RUNS" -- /bin/true

echo
echo "############################################################"
echo "# runc  (reference OCI runtime used by Docker/Kubernetes)"
echo "############################################################"

# Build a minimal OCI bundle running /bin/true.
BUNDLE="$TMP/bundle"
mkdir -p "$BUNDLE/rootfs"
cp -a "$ROOTFS/." "$BUNDLE/rootfs/"
( cd "$BUNDLE" && runc spec )
# Point it at /bin/true and turn off the terminal so it runs non-interactively.
jq '.process.args=["/bin/true"] | .process.terminal=false' "$BUNDLE/config.json" > "$BUNDLE/c.json"
mv "$BUNDLE/c.json" "$BUNDLE/config.json"

# Warm up (first run pays one-time costs).
runc --root "$TMP/state" run --bundle "$BUNDLE" warmup >/dev/null 2>&1 || true

for i in $(seq 1 "$RUNS"); do
    s=$(date +%s%N)
    runc --root "$TMP/state" run --bundle "$BUNDLE" "b$i" >/dev/null 2>&1
    e=$(date +%s%N)
    echo $(( (e - s) / 1000 ))   # microseconds
done | stats "runc end-to-end wall (ms)"

echo
echo "(both run /bin/true in the same Alpine rootfs with cgroup v2 limits;"
echo " Docker would add its daemon + API + image-management overhead on top of runc.)"
