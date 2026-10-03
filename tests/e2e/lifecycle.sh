#!/usr/bin/env bash
# VirtualMachine lifecycle E2E test: create, stop, start, restart, delete,
# with host-side cleanup checks.
#
# Requires a cluster with Baryon installed (CRDs, controller, node agent) and
# at least one node running baryon-virtd with KVM.
#
# Environment: see lib.sh, and
#   PROBE_IMAGE    image with sh and nc for the guest network checks, run as a
#                  pod in NAMESPACE (default: busybox:1.36); set
#                  SKIP_NETWORK=1 to skip them
# The guest reset and the host checks need NODE_SHELL.
set -euo pipefail
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

vm="e2e-lifecycle-$RANDOM"
probe="$vm-probe"
svc="$vm-ssh"

ready_is() {
  [ "$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')" = "$1" ]
}
ready_reason_is() {
  [ "$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.conditions[?(@.type=="Ready")].reason}')" = "$1" ]
}
pod_absent() { ! "${k[@]}" get pod "$vm-anchor" >/dev/null 2>&1; }
vm_absent() { ! "${k[@]}" get virtualmachine "$vm" >/dev/null 2>&1; }
claim_absent() { ! "${k[@]}" get pvc "$vm-root" >/dev/null 2>&1; }
pod_uid() { "${k[@]}" get pod "$vm-anchor" -o jsonpath='{.metadata.uid}' 2>/dev/null; }
# The guest answers on TCP 22 at its reported address, from another pod.
ssh_banner() {
  local addr
  addr="$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.addresses[0].address}')"
  [ -n "$addr" ] || return 1
  "${k[@]}" exec "$probe" -- sh -c "echo | nc -w 3 $addr 22" 2>/dev/null | grep -q '^SSH-'
}
network_check() {
  [ "${SKIP_NETWORK:-0}" = 1 ] && return 0
  wait_for "$1" ssh_banner
}
# The guest answers through a ClusterIP Service selecting its Anchor Pod,
# addressed by DNS name: cluster DNS, kube-proxy DNAT, and the reply path
# through the tc redirect.
service_banner() {
  "${k[@]}" exec "$probe" -- sh -c "echo | nc -w 3 $svc.$ns.svc.cluster.local 22" 2>/dev/null | grep -q '^SSH-'
}
service_check() {
  [ "${SKIP_NETWORK:-0}" = 1 ] && return 0
  wait_for "$1" service_banner
}
guest_ping() {
  local addr
  addr="$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.addresses[0].address}')"
  [ -n "$addr" ] && "${k[@]}" exec "$probe" -- ping -c 1 -W 2 "$addr" >/dev/null 2>&1
}
# boots <node> <pod-uid>: kernel boots in the console log of an Anchor Pod.
boots() {
  host "$1" "cat /var/log/baryon/vms/${ns}_${vm}_$2/console.log 2>/dev/null | grep -c 'Linux version' || true"
}


cleanup() {
  "${k[@]}" delete virtualmachine "$vm" --ignore-not-found --wait=false >/dev/null 2>&1 || true
  "${k[@]}" delete pod "$probe" --ignore-not-found --wait=false >/dev/null 2>&1 || true
  "${k[@]}" delete service "$svc" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}
trap cleanup EXIT

if [ "${SKIP_NETWORK:-0}" != 1 ]; then
  "${k[@]}" run "$probe" --image="${PROBE_IMAGE:-busybox:1.36}" --restart=Never -- sleep 3600 >/dev/null
  "${k[@]}" wait --for=condition=Ready "pod/$probe" --timeout="${timeout}s" >/dev/null
fi

log "creating VirtualMachine $vm"
apply_vm "$vm"

wait_for "VM becomes Ready" ready_is True
first_pod="$(pod_uid)"
first_node="$(node_of_pod "$vm-anchor")"
"${k[@]}" get virtualmachine "$vm"
network_check "guest reachable over the pod network"
if [ "${SKIP_NETWORK:-0}" != 1 ]; then
  "${k[@]}" apply -f - >/dev/null <<YAML
apiVersion: v1
kind: Service
metadata:
  name: $svc
spec:
  selector:
    virtualization.baryon.io/vm-name: $vm
  ports:
    - name: ssh
      port: 22
      targetPort: 22
YAML
fi
service_check "guest reachable through a ClusterIP Service by DNS name"

log "stopping (runPolicy Stopped)"
"${k[@]}" patch virtualmachine "$vm" --type merge -p '{"spec":{"runPolicy":"Stopped"}}' >/dev/null
wait_for "Anchor Pod removed on stop" pod_absent
wait_for "Ready reason is Stopped" ready_reason_is Stopped
host_wait "host runtime state removed after stop" no_vmm_for "$first_node" "$first_pod"
"${k[@]}" get pvc "$vm-root" >/dev/null && ok "root disk claim kept while stopped"

log "starting (runPolicy Running)"
"${k[@]}" patch virtualmachine "$vm" --type merge -p '{"spec":{"runPolicy":"Running"}}' >/dev/null
wait_for "VM Ready again after start" ready_is True
second_pod="$(pod_uid)"
second_node="$(node_of_pod "$vm-anchor")"
[ "$second_pod" != "$first_pod" ] && ok "start created a new Anchor Pod"

log "restarting (restartedAt annotation)"
"${k[@]}" annotate virtualmachine "$vm" --overwrite "virtualization.baryon.io/restartedAt=$(date -u +%FT%TZ)" >/dev/null
new_pod() { p="$(pod_uid)"; [ -n "$p" ] && [ "$p" != "$second_pod" ]; }
wait_for "restart replaced the Anchor Pod" new_pod
wait_for "VM Ready after restart" ready_is True
third_pod="$(pod_uid)"
third_node="$(node_of_pod "$vm-anchor")"
host_wait "old VMM state removed after restart" no_vmm_for "$second_node" "$second_pod"
network_check "guest reachable after restart (new pod IP)"
service_check "Service follows the VM to its new Anchor Pod"

if [ -n "${NODE_SHELL:-}" ]; then
  # A guest reset re-creates the VM's devices inside the running VMM (disks
  # reopened, serial socket re-created) under Landlock. A hard reset can lose
  # recent guest writes, so only ping is checked afterwards.
  log "resetting the guest through the VMM API"
  before="$(boots "$third_node" "$third_pod")"
  code="$(host "$third_node" "curl -s -o /dev/null -w '%{http_code}' --unix-socket /run/baryon/vms/$third_pod/ctl/api.sock -X PUT http://localhost/api/v1/vm.reboot")"
  if [ "$code" = 204 ]; then ok "VMM accepted the reset"; else bad "VMM rejected the reset (HTTP $code)"; fi
  booted_again() { [ "$(boots "$third_node" "$third_pod")" -gt "$before" ]; }
  wait_for "guest booted again after the reset" booted_again
  [ "${SKIP_NETWORK:-0}" = 1 ] || wait_for "guest answers ping after the reset" guest_ping
fi

log "deleting"
"${k[@]}" delete virtualmachine "$vm" --wait=false >/dev/null
wait_for "VirtualMachine removed" vm_absent
pod_absent && ok "Anchor Pod gone"
wait_for "root disk claim garbage collected" claim_absent
host_wait "host runtime state removed after delete" no_vmm_for "$third_node" "$third_pod"
host_leftovers "after delete" "$first_node=$first_pod" "$second_node=$second_pod" "$third_node=$third_pod"

cleanup
trap - EXIT
summary
