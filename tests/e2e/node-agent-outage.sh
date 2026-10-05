#!/usr/bin/env bash
# Node agent outage and controller restart E2E test: baryon-controller must
# rebuild its view of the nodes after a restart without trusting a node
# whose node agent stopped reporting.
#
# Two VMs, pinned with spec.placement: one on the outage node (the node
# agent is removed there), one on another node (unrelated, must not be
# affected). Cases:
#   down-then-restart  node agent removed, its last heartbeat allowed to age
#                      past the 90 s staleness bound (the controller has
#                      already turned NetworkReady Unknown), then baryon-controller
#                      restarted: the outage VM never shows NetworkReady True
#                      after the restart and reports Unknown
#                      (RuntimeUnreachable); Ready stays True (the VM runs);
#                      the node's heartbeat does not advance; the unrelated
#                      VM stays Ready and NetworkReady True; all recover when
#                      the node agent returns
#   restart-then-down  baryon-controller restarted, then the node agent
#                      removed shortly after: the outage VM turns Unknown
#                      (RuntimeUnreachable) once the report is stale; the
#                      unrelated VM is unaffected; recovery as above
#
# The outage is made by excluding the node from the node agent DaemonSet
# with a node affinity, removed again on exit. The script refuses to run if
# the DaemonSet already has an affinity. Each change of the pod template
# rolls the node agents on every node, so run it on a cluster no other test
# uses at the same time. Requires two Baryon nodes and permission to patch
# the DaemonSet and restart the controller Deployment.
#
# Environment: see lib.sh, and STORAGE_CLASS (default StorageClass when
# unset).
set -euo pipefail
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

run="$RANDOM"
sys=(kubectl "${ctx[@]}" -n baryon-system)
mapfile -t nodes < <(kubectl "${ctx[@]}" get nodes -l virtualization.baryon.io/host=true \
  -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}')
[ "${#nodes[@]}" -ge 2 ] || { echo "node-agent-outage.sh needs two Baryon nodes" >&2; exit 2; }
out_node="${nodes[1]}"
other_node="${nodes[0]}"
vm_out="nao-out-$run"
vm_other="nao-other-$run"
watcher=""

patched=0
restore_agent() {
  [ "$patched" = 1 ] || return 0
  "${sys[@]}" patch daemonset baryon-node-agent --type json \
    -p '[{"op":"remove","path":"/spec/template/spec/affinity"}]' >/dev/null 2>&1 && patched=0
}
cleanup() {
  [ -n "$watcher" ] && kill "$watcher" >/dev/null 2>&1
  restore_agent
  "${k[@]}" delete virtualmachine "$vm_out" "$vm_other" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}
trap cleanup EXIT

vm() {
  local sha_line="" sc_line=""
  [ -n "${IMAGE_SHA256:-}" ] && sha_line="        sha256: ${IMAGE_SHA256}"
  [ -n "${STORAGE_CLASS:-}" ] && sc_line="    storageClassName: $STORAGE_CLASS"
  "${k[@]}" apply -f - >/dev/null <<EOF
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata:
  name: $1
spec:
  className: general-small
  placement:
    nodeSelector:
      kubernetes.io/hostname: $2
  rootDisk:
    size: 4Gi
$sc_line
    source:
      http:
        url: $url
$sha_line
EOF
}
cond() { "${k[@]}" get virtualmachine "$1" -o jsonpath="{.status.conditions[?(@.type==\"$2\")].$3}" 2>/dev/null; }
net_is() { [ "$(cond "$1" NetworkReady status)" = "$2" ] && { [ -z "${3:-}" ] || [ "$(cond "$1" NetworkReady reason)" = "$3" ]; }; }
ready() { [ "$(cond "$1" Ready status)" = True ]; }
check() { if "$@"; then ok "$label"; else bad "$label"; fi; }
heartbeat() { kubectl "${ctx[@]}" get virtualizationnode "$1" -o jsonpath='{.status.heartbeatTime}' 2>/dev/null; }
agent_running() { "${sys[@]}" get pods -l app.kubernetes.io/name=baryon-node-agent --field-selector "spec.nodeName=$1" 2>/dev/null | grep -q "1/1"; }
no_agent() { [ -z "$("${sys[@]}" get pods -l app.kubernetes.io/name=baryon-node-agent --field-selector "spec.nodeName=$out_node" -o name 2>/dev/null)" ]; }
remove_agent() {
  patched=1
  "${sys[@]}" patch daemonset baryon-node-agent --type json -p "[{\"op\":\"add\",\"path\":\"/spec/template/spec/affinity\",\"value\":{\"nodeAffinity\":{\"requiredDuringSchedulingIgnoredDuringExecution\":{\"nodeSelectorTerms\":[{\"matchExpressions\":[{\"key\":\"kubernetes.io/hostname\",\"operator\":\"NotIn\",\"values\":[\"$out_node\"]}]}]}}}}]" >/dev/null
}
restart_controller() {
  "${sys[@]}" rollout restart deployment/baryon-controller >/dev/null
  "${sys[@]}" rollout status deployment/baryon-controller --timeout=300s >/dev/null
}
# watch_true <vm> <file>: record every NetworkReady True seen for the VM.
watch_true() {
  while :; do
    [ "$(cond "$1" NetworkReady status)" = True ] && date -u +%T >> "$2"
    sleep 2
  done
}
both_healthy() {
  ready "$vm_out" && ready "$vm_other" \
    && net_is "$vm_out" True AddressServiceRunning && net_is "$vm_other" True AddressServiceRunning
}
heartbeat_older_than() {
  local hb; hb="$(heartbeat "$1")"
  [ -n "$hb" ] && [ $(( $(date -u +%s) - $(date -u -d "$hb" +%s) )) -gt "$2" ]
}

if [ -n "$("${sys[@]}" get daemonset baryon-node-agent -o jsonpath='{.spec.template.spec.affinity}')" ]; then
  echo "the node agent DaemonSet already has an affinity; not changing it" >&2
  exit 2
fi
log "outage node $out_node, unrelated node $other_node"
vm "$vm_out" "$out_node"
vm "$vm_other" "$other_node"
wait_for "both VMs Ready with NetworkReady True" both_healthy

log "case down-then-restart"
remove_agent
wait_for "down-then-restart: node agent gone from $out_node" no_agent
saved="$timeout"; timeout=240
wait_for "down-then-restart: outage VM Unknown before the restart" net_is "$vm_out" Unknown RuntimeUnreachable
wait_for "down-then-restart: heartbeat of $out_node older than 120 s" heartbeat_older_than "$out_node" 120
timeout="$saved"
hb_before="$(heartbeat "$out_node")"
seen="$(mktemp)"
# Watch from before the restart: the old controller already reports
# Unknown, so any True seen from now on is a false claim.
watch_true "$vm_out" "$seen" & watcher=$!
restart_controller
ok "down-then-restart: baryon-controller restarted"
wait_for "down-then-restart: outage VM NetworkReady Unknown RuntimeUnreachable" net_is "$vm_out" Unknown RuntimeUnreachable
sleep 30
kill "$watcher" >/dev/null 2>&1; watcher=""
# The controller before the restart had already turned the condition
# Unknown; a True after the restart would come from trusting the old
# report again.
label="down-then-restart: outage VM never NetworkReady True after the restart"; check [ "$(grep -c . "$seen" || true)" -eq 0 ]
label="down-then-restart: outage VM stays Ready (it runs)"; check ready "$vm_out"
label="down-then-restart: heartbeat of $out_node did not advance"; check [ "$(heartbeat "$out_node")" = "$hb_before" ]
label="down-then-restart: unrelated VM Ready"; check ready "$vm_other"
label="down-then-restart: unrelated VM NetworkReady True"; check net_is "$vm_other" True AddressServiceRunning
restore_agent
wait_for "down-then-restart: node agent back on $out_node" agent_running "$out_node"
wait_for "down-then-restart: both VMs healthy again" both_healthy
rm -f "$seen"

log "case restart-then-down"
restart_controller
ok "restart-then-down: baryon-controller restarted"
wait_for "restart-then-down: both VMs healthy after the restart" both_healthy
remove_agent
wait_for "restart-then-down: node agent gone from $out_node" no_agent
saved="$timeout"; timeout=240
wait_for "restart-then-down: outage VM NetworkReady Unknown RuntimeUnreachable" net_is "$vm_out" Unknown RuntimeUnreachable
timeout="$saved"
label="restart-then-down: outage VM stays Ready (it runs)"; check ready "$vm_out"
label="restart-then-down: unrelated VM Ready"; check ready "$vm_other"
label="restart-then-down: unrelated VM NetworkReady True"; check net_is "$vm_other" True AddressServiceRunning
restore_agent
wait_for "restart-then-down: node agent back on $out_node" agent_running "$out_node"
wait_for "restart-then-down: both VMs healthy again" both_healthy

log "deleting"
pairs=()
for v in "$vm_out" "$vm_other"; do
  pairs+=("$("${k[@]}" get virtualmachine "$v" -o jsonpath='{.status.nodeName}')=$("${k[@]}" get pod "$v-anchor" -o jsonpath='{.metadata.uid}')")
done
"${k[@]}" delete virtualmachine "$vm_out" "$vm_other" --wait=false >/dev/null
gone() { ! "${k[@]}" get virtualmachine "$vm_out" >/dev/null 2>&1 && ! "${k[@]}" get virtualmachine "$vm_other" >/dev/null 2>&1; }
wait_for "VMs removed" gone
host_leftovers "after delete" "${pairs[@]}"

cleanup
trap - EXIT
summary
