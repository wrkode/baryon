#!/usr/bin/env bash
# VirtualMachine deletion E2E test: delete in every phase before Ready and
# check that nothing is left in the cluster or on the node.
#
# Cases:
#   import    delete while the root disk is being imported (import Pod)
#   booting   delete after the VMM started, before the guest showed boot
#             evidence
#   pending   delete while the Anchor Pod cannot be scheduled
#   stopped   delete a VM with runPolicy Stopped
#
# Requires a cluster with Baryon installed and the VirtualMachineClass
# general-small (examples/classes.yaml). Environment: see lib.sh.
set -euo pipefail
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

run="$RANDOM"
huge_class="e2e-unschedulable-$run"
vms=()

cleanup() {
  for v in "${vms[@]}"; do
    "${k[@]}" delete virtualmachine "$v" --ignore-not-found --wait=false >/dev/null 2>&1 || true
  done
  kubectl "${ctx[@]}" delete virtualmachineclass "$huge_class" --ignore-not-found >/dev/null 2>&1 || true
}
trap cleanup EXIT

pod_uid_of() { "${k[@]}" get pod "$1-anchor" -o jsonpath='{.metadata.uid}' 2>/dev/null; }
importing() {
  [ -n "$("${k[@]}" get pod "$1-import" -o jsonpath='{.status.containerStatuses[0].state.running}' 2>/dev/null)" ]
}
reason_is() { [ "$(vm_reason "$1")" = "$2" ]; }
gone() {
  ! "${k[@]}" get virtualmachine "$1" >/dev/null 2>&1 \
    && ! "${k[@]}" get pod "$1-anchor" >/dev/null 2>&1 \
    && ! "${k[@]}" get pod "$1-import" >/dev/null 2>&1 \
    && ! "${k[@]}" get pvc "$1-root" >/dev/null 2>&1
}

# delete_and_check <case> <vm> <pod-uid or empty> <node or empty>
delete_and_check() {
  local name="$1" v="$2" uid="$3" node="${4:-}"
  "${k[@]}" delete virtualmachine "$v" --wait=false >/dev/null
  wait_for "$name: VirtualMachine, its pods, and root disk claim removed" gone "$v"
  if [ -n "$uid" ]; then
    host_leftovers "$name" "$node=$uid"
  fi
}

log "case import: delete while the disk is imported"
v="e2e-del-import-$run"; vms+=("$v")
apply_vm "$v"
wait_for "import: import Pod running" importing "$v"
limits="$("${k[@]}" get pod "$v-import" -o jsonpath='{.spec.containers[0].resources.limits}')"
case "$limits" in
  *'virtualization.baryon.io/import'*) case "$limits" in
    *'virtualization.baryon.io/vm'*) bad "import: import Pod requests a VM slot" ;;
    *) ok "import: import Pod requests only the placement slot" ;;
  esac ;;
  *) bad "import: import Pod does not request the placement slot" ;;
esac
if [ -n "${NODE_SHELL:-}" ]; then
  # The importer must see neither /dev/kvm nor the baryon-virtd anchor socket.
  inode="$(node_of_pod "$v-import")"
  seen="$(host "$inode" "for c in /proc/[0-9]*/cmdline; do case \"\$(tr '\\0' ' ' < \$c 2>/dev/null)\" in /baryon-importer\ import*|/baryon-anchor\ import*) r=\${c%/cmdline}/root; echo importer; ls -d \$r/dev/kvm \$r/run/baryon/anchor 2>/dev/null;; esac; done" || true)"
  paths="$(printf '%s\n' "$seen" | grep -v '^importer$' | grep . || true)"
  if ! printf '%s\n' "$seen" | grep -qx importer; then
    bad "import: importer process not found on $inode"
  elif [ -z "$paths" ]; then
    ok "import: importer has no /dev/kvm and no anchor socket"
  else
    bad "import: importer sees $paths"
  fi
fi
"${k[@]}" get pod "$v-anchor" >/dev/null 2>&1 && bad "import: an Anchor Pod exists before the import finished" || ok "import: no Anchor Pod while importing"
delete_and_check import "$v" "$("${k[@]}" get pod "$v-import" -o jsonpath='{.metadata.uid}')" "$(node_of_pod "$v-import")"

log "case booting: delete after the VMM started, before boot evidence"
v="e2e-del-boot-$run"; vms+=("$v")
apply_vm "$v"
# Poll every second: the window between Booting and Ready is short.
start=$SECONDS
until reason_is "$v" Booting; do
  if (( SECONDS - start > timeout )); then bad "booting: VM never reported Booting"; break; fi
  sleep 1
done
reason_is "$v" Booting && ok "booting: VM reported Booting ($(( SECONDS - start ))s)"
delete_and_check booting "$v" "$(pod_uid_of "$v")" "$(node_of_pod "$v-anchor")"

log "case pending: delete while the Anchor Pod cannot be scheduled"
kubectl "${ctx[@]}" apply -f - >/dev/null <<EOF
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachineClass
metadata:
  name: $huge_class
spec:
  cpu:
    vcpus: 1
  memory:
    size: 1Ti
EOF
v="e2e-del-pending-$run"; vms+=("$v")
apply_vm "$v" "$huge_class"
wait_for "pending: VM reports Unschedulable" reason_is "$v" Unschedulable
delete_and_check pending "$v" ""

log "case stopped: delete a stopped VM"
v="e2e-del-stopped-$run"; vms+=("$v")
apply_vm "$v" general-small Stopped
wait_for "stopped: VM reports Stopped" reason_is "$v" Stopped
delete_and_check stopped "$v" ""

cleanup
trap - EXIT
summary
