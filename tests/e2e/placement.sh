#!/usr/bin/env bash
# Placement E2E test: VirtualMachine spec.placement is translated into
# Kubernetes scheduling constraints of the VM's pods, the Kubernetes
# scheduler places them, and VMs that cannot be placed say why.
#
# Placement is steered only with node labels and taints set by this test
# (administrator policy), and runtime health only by stopping baryon-virtd
# on a node. Labels, taints, and cordons are removed on exit.
#
# Cases:
#   selector        nodeSelector places the VM on the labeled node
#   labels          VM labels reach the Anchor Pod and follow changes without
#                   a restart; labels others set on the pod stay
#   affinity        required node affinity with two eligible nodes
#   spread          topologySpreadConstraints put two VMs created together
#                   on two nodes
#   tolerations     a tainted node takes only the VM that tolerates the taint
#   none            no eligible node: Unschedulable with the scheduler's
#                   message; recovers when a node becomes eligible
#   cordon          a cordoned eligible node does not take a new VM
#   runtime         baryon-virtd stopped on the only eligible node: the VM
#                   waits (Unschedulable) and starts when it is back; a VM
#                   already running there keeps its Anchor Pod
#   rejected        a pod the API server refuses (unknown PriorityClass) is
#                   reported as PodRejected
#   changed         changing placement reports RestartRequired
#
# Requires at least three Baryon nodes and NODE_SHELL (see lib.sh).
# Environment: see lib.sh, and STORAGE_CLASS (default StorageClass when
# unset).
set -euo pipefail
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
[ -n "${NODE_SHELL:-}" ] || { echo "placement.sh needs NODE_SHELL" >&2; exit 2; }

run="$RANDOM"
key="e2e-placement-$run"
mapfile -t nodes < <(kubectl "${ctx[@]}" get nodes -l virtualization.baryon.io/host=true \
  -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}')
[ "${#nodes[@]}" -ge 3 ] || { echo "need at least 3 Baryon nodes" >&2; exit 2; }
a="${nodes[0]}"; b="${nodes[1]}"; c="${nodes[2]}"
vms=()
pairs=()
virtd_stopped=""

cleanup() {
  local n v
  for v in "${vms[@]}"; do "${k[@]}" delete virtualmachine "$v" --ignore-not-found --wait=false >/dev/null 2>&1 || true; done
  for n in "${nodes[@]}"; do
    kubectl "${ctx[@]}" label node "$n" "$key-" "$key-group-" "$key-none-" >/dev/null 2>&1 || true
    kubectl "${ctx[@]}" taint node "$n" "$key-" >/dev/null 2>&1 || true
    kubectl "${ctx[@]}" uncordon "$n" >/dev/null 2>&1 || true
  done
  [ -n "$virtd_stopped" ] && host "$virtd_stopped" "systemctl start baryon-virtd" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# vm <name> <placement-yaml> [labels-yaml]: create a VM with that placement
# (indented under spec.placement) and labels.
vm() {
  local name="$1" placement="$2" labels="${3:-}" sha_line="" sc_line=""
  [ -n "${IMAGE_SHA256:-}" ] && sha_line="        sha256: ${IMAGE_SHA256}"
  [ -n "${STORAGE_CLASS:-}" ] && sc_line="    storageClassName: $STORAGE_CLASS"
  vms+=("$name")
  "${k[@]}" apply -f - >/dev/null <<EOF
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata:
  name: $name
  labels:
    e2e: "$run"
$labels
spec:
  className: general-small
  rootDisk:
    size: 4Gi
$sc_line
    source:
      http:
        url: $url
$sha_line
  placement:
$placement
EOF
}
ready() { [ "$("${k[@]}" get virtualmachine "$1" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')" = True ]; }
node_of() { "${k[@]}" get virtualmachine "$1" -o jsonpath='{.status.nodeName}' 2>/dev/null || true; }
cond() { "${k[@]}" get virtualmachine "$1" -o jsonpath="{.status.conditions[?(@.type==\"$2\")].$3}"; }
reason_is() { [ "$(cond "$1" Ready reason)" = "$2" ]; }
anchor_uid() { "${k[@]}" get pod "$1-anchor" -o jsonpath='{.metadata.uid}' 2>/dev/null || true; }
check() { if "$@"; then ok "$label"; else bad "$label"; fi; }
# retire <vm>...: delete VMs whose case is done, keeping their Anchor Pods
# for the leftover check.
retire() {
  local v u n
  for v in "$@"; do
    u="$(anchor_uid "$v")"; n="$(node_of "$v")"
    [ -n "$u" ] && [ -n "$n" ] && pairs+=("$n=$u")
    "${k[@]}" delete virtualmachine "$v" --wait=false >/dev/null
  done
}

log "case selector: nodeSelector on node $a"
kubectl "${ctx[@]}" label node "$a" "$key=a" >/dev/null
vm "pl-sel-$run" "    nodeSelector: {$key: a}" "    app: pl-$run"
wait_for "selector: VM Ready" ready "pl-sel-$run"
label="selector: VM placed on $a"; check [ "$(node_of "pl-sel-$run")" = "$a" ]

log "case labels: VM labels on the Anchor Pod, changed without a restart"
pod_label() { "${k[@]}" get pod "pl-sel-$run-anchor" -o jsonpath="{.metadata.labels.$1}"; }
label="labels: VM label copied to the Anchor Pod"; check [ "$(pod_label app)" = "pl-$run" ]
before="$(anchor_uid "pl-sel-$run")"
"${k[@]}" label virtualmachine "pl-sel-$run" --overwrite app=pl-$run-v2 tier=front >/dev/null
synced() { [ "$(pod_label app)" = "pl-$run-v2" ] && [ "$(pod_label tier)" = front ]; }
wait_for "labels: changed labels reach the Anchor Pod" synced
"${k[@]}" label pod "pl-sel-$run-anchor" other.example/owner=keep >/dev/null
"${k[@]}" label virtualmachine "pl-sel-$run" tier- >/dev/null
removed() { [ -z "$(pod_label tier)" ]; }
wait_for "labels: removed label leaves the Anchor Pod" removed
label="labels: same Anchor Pod (no restart)"; check [ "$(anchor_uid "pl-sel-$run")" = "$before" ]
label="labels: a label set by someone else stays"; check [ "$(pod_label 'other\.example/owner')" = keep ]
"${k[@]}" label virtualmachine "pl-sel-$run" --overwrite "virtualization.baryon.io/vm-name=forged" >/dev/null 2>&1 || true
sleep 5
label="labels: reserved label keys are not copied"
check [ "$(pod_label 'virtualization\.baryon\.io/vm-name')" = "pl-sel-$run" ]
"${k[@]}" label virtualmachine "pl-sel-$run" "virtualization.baryon.io/vm-name-" >/dev/null 2>&1 || true

log "case affinity: two eligible nodes ($b, $c)"
kubectl "${ctx[@]}" label node "$b" "$key-group=yes" >/dev/null
kubectl "${ctx[@]}" label node "$c" "$key-group=yes" >/dev/null
group_affinity="    affinity:
      nodeAffinity:
        requiredDuringSchedulingIgnoredDuringExecution:
          nodeSelectorTerms:
            - matchExpressions: [{key: $key-group, operator: In, values: [\"yes\"]}]"
vm "pl-aff-$run" "$group_affinity"
wait_for "affinity: VM Ready" ready "pl-aff-$run"
n="$(node_of "pl-aff-$run")"
on_b_or_c() { [ "$n" = "$b" ] || [ "$n" = "$c" ]; }
label="affinity: VM on an eligible node ($n)"; check on_b_or_c

log "case spread: two VMs over $b and $c"
spread="$group_affinity
    topologySpreadConstraints:
      - maxSkew: 1
        topologyKey: kubernetes.io/hostname
        whenUnsatisfiable: DoNotSchedule
        labelSelector: {matchLabels: {spread: \"$run\"}}"
# Created together: with node-local storage the import Pods decide the
# nodes, so they must count in the spread too.
vm "pl-sp1-$run" "$spread" "    spread: \"$run\""
vm "pl-sp2-$run" "$spread" "    spread: \"$run\""
wait_for "spread: first VM Ready" ready "pl-sp1-$run"
wait_for "spread: second VM Ready" ready "pl-sp2-$run"
label="spread: the VMs run on different nodes"; check [ "$(node_of "pl-sp1-$run")" != "$(node_of "pl-sp2-$run")" ]
retire "pl-sp1-$run" "pl-sp2-$run"

log "case tolerations: node $c tainted"
kubectl "${ctx[@]}" taint node "$c" "$key=only:NoSchedule" >/dev/null
kubectl "${ctx[@]}" label node "$c" "$key=c" >/dev/null
vm "pl-notol-$run" "    nodeSelector: {$key: c}"
unsched() { [ "$(cond "$1" Scheduled reason)" = Unschedulable ]; }
wait_for "tolerations: VM without the toleration is Unschedulable" unsched "pl-notol-$run"
label="tolerations: the reason reaches Ready"; check reason_is "pl-notol-$run" Unschedulable
msg="$(cond "pl-notol-$run" Scheduled message)"
log "message: $msg"
label="tolerations: the message names the taint"; check grep -q -i taint <<<"$msg"
vm "pl-tol-$run" "    nodeSelector: {$key: c}
    tolerations: [{key: $key, operator: Equal, value: only, effect: NoSchedule}]"
wait_for "tolerations: VM with the toleration Ready" ready "pl-tol-$run"
label="tolerations: placed on $c"; check [ "$(node_of "pl-tol-$run")" = "$c" ]
retire "pl-notol-$run" "pl-tol-$run"

log "case none: no eligible node, then one becomes eligible"
vm "pl-none-$run" "    nodeSelector: {$key-none: x}"
wait_for "none: Unschedulable" unsched "pl-none-$run"
label="none: Ready says Unschedulable"; check reason_is "pl-none-$run" Unschedulable
log "message: $(cond "pl-none-$run" Scheduled message)"
kubectl "${ctx[@]}" label node "$b" "$key-none=x" >/dev/null
wait_for "none: VM Ready once a node is eligible" ready "pl-none-$run"
label="none: placed on $b"; check [ "$(node_of "pl-none-$run")" = "$b" ]
retire "pl-none-$run"

log "case cordon: the only eligible node is cordoned"
kubectl "${ctx[@]}" cordon "$a" >/dev/null
vm "pl-cord-$run" "    nodeSelector: {$key: a}"
wait_for "cordon: Unschedulable" unsched "pl-cord-$run"
kubectl "${ctx[@]}" uncordon "$a" >/dev/null
wait_for "cordon: VM Ready after uncordon" ready "pl-cord-$run"
retire "pl-cord-$run"

log "case rejected: the API server refuses the VM's pod"
vm "pl-rej-$run" "    priorityClassName: e2e-missing-$run"
rejected() { reason_is "pl-rej-$run" PodRejected; }
wait_for "rejected: VM reports PodRejected" rejected
log "message: $(cond "pl-rej-$run" Ready message)"
retire "pl-rej-$run"

log "case runtime: baryon-virtd stopped on $a"
running_before="$(anchor_uid "pl-sel-$run")"
host "$a" "systemctl stop baryon-virtd"
virtd_stopped="$a"
vnode_not_ready() { [ "$(kubectl "${ctx[@]}" get virtualizationnode "$a" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')" != True ]; }
wait_for "runtime: $a is not a ready VirtualizationNode" vnode_not_ready
vm "pl-rt-$run" "    nodeSelector: {$key: a}"
wait_for "runtime: new VM Unschedulable on the unhealthy node" unsched "pl-rt-$run"
log "message: $(cond "pl-rt-$run" Scheduled message)"
label="runtime: the running VM kept its Anchor Pod"; check [ "$(anchor_uid "pl-sel-$run")" = "$running_before" ]
host "$a" "systemctl start baryon-virtd"
virtd_stopped=""
wait_for "runtime: new VM Ready once the runtime is back" ready "pl-rt-$run"
wait_for "runtime: running VM Ready" ready "pl-sel-$run"
label="runtime: the running VM still has the same Anchor Pod"; check [ "$(anchor_uid "pl-sel-$run")" = "$running_before" ]

log "case changed: placement change reports RestartRequired"
"${k[@]}" patch virtualmachine "pl-aff-$run" --type merge -p "{\"spec\":{\"placement\":{\"nodeSelector\":{\"$key\":\"a\"}}}}" >/dev/null
restart_required() { [ "$(cond "pl-aff-$run" RestartRequired reason)" = PlacementChanged ]; }
wait_for "changed: RestartRequired PlacementChanged" restart_required

log "deleting"
retire "pl-sel-$run" "pl-aff-$run" "pl-rt-$run"
gone() { [ -z "$("${k[@]}" get virtualmachine -l "e2e=$run" -o name 2>/dev/null)" ]; }
wait_for "all VMs removed" gone
host_leftovers "after delete" "${pairs[@]}"

cleanup
trap - EXIT
summary
