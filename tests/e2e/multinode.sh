#!/usr/bin/env bash
# Multi-node E2E test: Baryon VMs across several Baryon nodes, and the
# failure and maintenance paths that must not leak from one VM or node to
# another.
#
# Requires at least three schedulable Baryon nodes (label
# virtualization.baryon.io/host=true), NODE_SHELL (see lib.sh), and a
# StorageClass usable from every node (the default one, or STORAGE_CLASS).
# Placement is steered only with Kubernetes mechanisms (cordon/uncordon).
# The test cordons nodes temporarily and uncordons them on exit.
#
# Cases:
#   capability      every Baryon node publishes VirtualizationNode Ready
#   spread          one VM per node, all Ready, reachable from a probe pod
#   controller      baryon-controller restart: VMs untouched, new work reconciles
#   virtd           baryon-virtd restart on one node: same VMM, boot state kept,
#                   VMs on other nodes untouched
#   runtime-down    baryon-virtd stopped on one node: its VirtualizationNode
#                   goes not ready, VMs on other nodes untouched
#   vmm-crash       Cloud Hypervisor killed for one VM: reported and restarted
#   anchor          Anchor Pod deleted: replaced, VM Ready, Service follows
#   cordon          a cordoned node receives no new VM; uncordoned it does
#   delete-one      one VM deleted while the others keep running
#
# Environment: see lib.sh, and STORAGE_CLASS, PROBE_IMAGE (busybox:1.36).
set -euo pipefail
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
[ -n "${NODE_SHELL:-}" ] || { echo "multinode.sh needs NODE_SHELL" >&2; exit 2; }

run="$RANDOM"
probe="e2e-mn-probe-$run"
svc="e2e-mn-svc-$run"
mapfile -t nodes < <(kubectl "${ctx[@]}" get nodes -l virtualization.baryon.io/host=true \
  -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}')
[ "${#nodes[@]}" -ge 3 ] || { echo "need at least 3 Baryon nodes, found ${#nodes[@]}" >&2; exit 2; }
vms=()
cordoned=()

cleanup() {
  for n in "${cordoned[@]}"; do kubectl "${ctx[@]}" uncordon "$n" >/dev/null 2>&1 || true; done
  for v in "${vms[@]}"; do "${k[@]}" delete virtualmachine "$v" --ignore-not-found --wait=false >/dev/null 2>&1 || true; done
  "${k[@]}" delete pod "$probe" service "$svc" --ignore-not-found --wait=false >/dev/null 2>&1 || true
  for n in "${nodes[@]}"; do host "$n" "systemctl start baryon-virtd" >/dev/null 2>&1 || true; done
}
trap cleanup EXIT

cordon() { kubectl "${ctx[@]}" cordon "$1" >/dev/null; cordoned+=("$1"); }
uncordon() { kubectl "${ctx[@]}" uncordon "$1" >/dev/null; }
# Only `target` schedulable while the VM's Anchor Pod is placed.
only_on() {
  local target="$1" n
  for n in "${nodes[@]}"; do [ "$n" = "$target" ] || cordon "$n"; done
}
uncordon_all() { local n; for n in "${nodes[@]}"; do uncordon "$n"; done; }

ready() { [ "$(vm_reason "$1")" = GuestBooted ]; }
vm_node() { "${k[@]}" get virtualmachine "$1" -o jsonpath='{.status.nodeName}'; }
pod_uid_of() { "${k[@]}" get pod "$1-anchor" -o jsonpath='{.metadata.uid}' 2>/dev/null; }
restarts_of() { "${k[@]}" get pod "$1-anchor" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null; }
vmm_pid_of() {
  # The VMM of Anchor Pod $2 on node $1.
  # Anchored on the binary path, so the scanning shell (whose own command
  # line contains the pattern) never matches.
  host "$1" "for c in /proc/[0-9]*/cmdline; do l=\$(tr '\\0' ' ' < \$c 2>/dev/null); case \"\$l\" in /*/cloud-hypervisor\\ *$2*) d=\${c%/cmdline}; echo \${d#/proc/};; esac; done | head -1"
}
banner() {
  local addr
  addr="$("${k[@]}" get virtualmachine "$1" -o jsonpath='{.status.addresses[0].address}')"
  [ -n "$addr" ] && "${k[@]}" exec "$probe" -- sh -c "echo | nc -w 3 $addr 22" 2>/dev/null | grep -q '^SSH-'
}
service_banner() {
  "${k[@]}" exec "$probe" -- sh -c "echo | nc -w 3 $svc.$ns.svc.cluster.local 22" 2>/dev/null | grep -q '^SSH-'
}
vnode_ready_is() {
  [ "$(kubectl "${ctx[@]}" get virtualizationnode "$1" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')" = "$2" ]
}
# Fingerprint of a VM that must not change: Anchor Pod UID and container
# restarts.
fingerprint() { echo "$(pod_uid_of "$1")/$(restarts_of "$1")"; }

"${k[@]}" run "$probe" --image="${PROBE_IMAGE:-busybox:1.36}" --restart=Never -- sleep 7200 >/dev/null
"${k[@]}" wait --for=condition=Ready "pod/$probe" --timeout="${timeout}s" >/dev/null

log "case capability: every Baryon node is a ready VirtualizationNode"
for n in "${nodes[@]}"; do wait_for "capability: $n Ready" vnode_ready_is "$n" True; done

log "case spread: one VM per node"
declare -A vm_on
for n in "${nodes[@]}"; do
  v="e2e-mn-$run-$n"; vms+=("$v"); vm_on[$n]="$v"
  only_on "$n"
  apply_vm "$v" "general-small" Running
  # Hold the cordons until the Anchor Pod is bound to the target node.
  bound() { [ "$(node_of_pod "$1-anchor")" = "$2" ]; }
  wait_for "spread: $v bound to $n" bound "$v" "$n"
  uncordon_all
done
for n in "${nodes[@]}"; do
  wait_for "spread: ${vm_on[$n]} Ready on $n" ready "${vm_on[$n]}"
  wait_for "spread: ${vm_on[$n]} reachable" banner "${vm_on[$n]}"
done
distinct="$(for n in "${nodes[@]}"; do vm_node "${vm_on[$n]}"; echo; done | sort -u | sed '/^$/d' | wc -l)"
if [ "$distinct" -eq "${#nodes[@]}" ]; then ok "spread: VMs run on ${distinct} different nodes"; else bad "spread: VMs on only $distinct nodes"; fi

a="${nodes[0]}"; b="${nodes[1]}"; c="${nodes[2]}"
va="${vm_on[$a]}"; vb="${vm_on[$b]}"; vc="${vm_on[$c]}"

log "case controller: restart baryon-controller"
before_b="$(fingerprint "$vb")"; before_c="$(fingerprint "$vc")"
kubectl "${ctx[@]}" -n baryon-system rollout restart deployment/baryon-controller >/dev/null
kubectl "${ctx[@]}" -n baryon-system rollout status deployment/baryon-controller --timeout="${timeout}s" >/dev/null
ok "controller: restarted"
[ "$(fingerprint "$vb")" = "$before_b" ] && ok "controller: $vb untouched" || bad "controller: $vb changed"
[ "$(fingerprint "$vc")" = "$before_c" ] && ok "controller: $vc untouched" || bad "controller: $vc changed"
# New work after the restart still converges: restart one VM.
"${k[@]}" annotate virtualmachine "$va" --overwrite "virtualization.baryon.io/restartedAt=$(date -u +%FT%TZ)" >/dev/null
old_a="$(pod_uid_of "$va")"
replaced() { p="$(pod_uid_of "$1")"; [ -n "$p" ] && [ "$p" != "$2" ]; }
wait_for "controller: restart of $va reconciled" replaced "$va" "$old_a"
wait_for "controller: $va Ready again" ready "$va"
va_node="$(vm_node "$va")"

log "case virtd: restart baryon-virtd on $b"
pod_b="$(pod_uid_of "$vb")"; pid_before="$(vmm_pid_of "$b" "$pod_b")"
before_a="$(fingerprint "$va")"; before_c="$(fingerprint "$vc")"
host "$b" "systemctl restart baryon-virtd"
recovered() { host "$b" "journalctl -u baryon-virtd --since '-60s' --no-pager -o cat | grep -q 'anchor attached'"; }
wait_for "virtd: anchor of $vb reattached" recovered
pid_after="$(vmm_pid_of "$b" "$pod_b")"
if [ -n "$pid_before" ] && [ "$pid_before" = "$pid_after" ]; then ok "virtd: same VMM process ($pid_after)"; else bad "virtd: VMM changed ($pid_before -> $pid_after)"; fi
ready "$vb" && ok "virtd: $vb still reports GuestBooted" || bad "virtd: $vb reason $(vm_reason "$vb")"
[ "$(fingerprint "$vb")" = "$pod_b/$(restarts_of "$vb")" ] && ok "virtd: $vb Anchor Pod kept" || bad "virtd: $vb Anchor Pod changed"
wait_for "virtd: $vb reachable" banner "$vb"
[ "$(fingerprint "$va")" = "$before_a" ] && ok "virtd: $va on $va_node untouched" || bad "virtd: $va changed"
[ "$(fingerprint "$vc")" = "$before_c" ] && ok "virtd: $vc on $c untouched" || bad "virtd: $vc changed"

log "case runtime-down: stop baryon-virtd on $c"
before_a="$(fingerprint "$va")"; before_b="$(fingerprint "$vb")"
host "$c" "systemctl stop baryon-virtd"
wait_for "runtime-down: VirtualizationNode $c not Ready" vnode_ready_is "$c" False
[ "$(fingerprint "$va")" = "$before_a" ] && ok "runtime-down: $va untouched" || bad "runtime-down: $va changed"
[ "$(fingerprint "$vb")" = "$before_b" ] && ok "runtime-down: $vb untouched" || bad "runtime-down: $vb changed"
banner "$va" && ok "runtime-down: $va reachable" || bad "runtime-down: $va unreachable"
host "$c" "systemctl start baryon-virtd"
wait_for "runtime-down: VirtualizationNode $c Ready again" vnode_ready_is "$c" True
wait_for "runtime-down: $vc Ready again" ready "$vc"

log "case vmm-crash: kill Cloud Hypervisor of $vb"
pod_b="$(pod_uid_of "$vb")"; r0="$(restarts_of "$vb")"; before_a="$(fingerprint "$va")"
pid="$(vmm_pid_of "$b" "$pod_b")"
[ -n "$pid" ] || { bad "vmm-crash: no VMM found for $vb on $b"; summary; exit 1; }
host "$b" "kill -9 $pid"
restarted() { [ "$(restarts_of "$1")" -gt "$2" ]; }
wait_for "vmm-crash: Anchor container of $vb restarted" restarted "$vb" "$r0"
wait_for "vmm-crash: $vb Ready again" ready "$vb"
new_pid="$(vmm_pid_of "$b" "$pod_b")"
[ -n "$new_pid" ] && [ "$new_pid" != "$pid" ] && ok "vmm-crash: new VMM process" || bad "vmm-crash: VMM not replaced"
[ "$(fingerprint "$va")" = "$before_a" ] && ok "vmm-crash: $va untouched" || bad "vmm-crash: $va changed"

log "case anchor: replace the Anchor Pod of $vc behind a Service"
"${k[@]}" apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Service
metadata:
  name: $svc
spec:
  selector:
    virtualization.baryon.io/vm-name: $vc
  ports:
    - name: ssh
      port: 22
      targetPort: 22
YAML
wait_for "anchor: $vc reachable through the Service" service_banner
old_c="$(pod_uid_of "$vc")"
"${k[@]}" delete pod "$vc-anchor" --wait=false >/dev/null
wait_for "anchor: Anchor Pod of $vc replaced" replaced "$vc" "$old_c"
wait_for "anchor: $vc Ready again" ready "$vc"
wait_for "anchor: Service follows $vc to its new Anchor Pod" service_banner

log "case cordon: no new VM on a cordoned node"
cordon "$a"
v="e2e-mn-$run-cordon"; vms+=("$v")
apply_vm "$v" "general-small" Running
scheduled() { [ -n "$(node_of_pod "$1-anchor")" ]; }
wait_for "cordon: $v scheduled" scheduled "$v"
[ "$(node_of_pod "$v-anchor")" != "$a" ] && ok "cordon: $v avoided cordoned $a" || bad "cordon: $v placed on cordoned $a"
"${k[@]}" delete virtualmachine "$v" --wait=false >/dev/null
uncordon "$a"
v="e2e-mn-$run-uncordon"; vms+=("$v")
only_on "$a"
apply_vm "$v" "general-small" Running
wait_for "cordon: $v placed on uncordoned $a" bound "$v" "$a"
uncordon_all
wait_for "cordon: $v Ready on $a" ready "$v"

log "case delete-one: delete $v while the others run"
before_a="$(fingerprint "$va")"; before_b="$(fingerprint "$vb")"; before_c="$(fingerprint "$vc")"
pod_v="$(pod_uid_of "$v")"
"${k[@]}" delete virtualmachine "$v" --wait=false >/dev/null
gone() { ! "${k[@]}" get virtualmachine "$1" >/dev/null 2>&1; }
wait_for "delete-one: $v removed" gone "$v"
host_leftovers "delete-one" "$a=$pod_v"
for x in "$va" "$vb" "$vc"; do
  case "$x" in "$va") want="$before_a" ;; "$vb") want="$before_b" ;; *) want="$before_c" ;; esac
  [ "$(fingerprint "$x")" = "$want" ] && ok "delete-one: $x untouched" || bad "delete-one: $x changed"
  ready "$x" && ok "delete-one: $x Ready" || bad "delete-one: $x not Ready"
done

log "cleanup: delete all test VMs"
leftovers=()
for x in "$va" "$vb" "$vc"; do leftovers+=("$(vm_node "$x")=$(pod_uid_of "$x")"); "${k[@]}" delete virtualmachine "$x" --wait=false >/dev/null; done
for x in "$va" "$vb" "$vc"; do wait_for "cleanup: $x removed" gone "$x"; done
host_leftovers "cleanup" "${leftovers[@]}"

cleanup
trap - EXIT
summary
