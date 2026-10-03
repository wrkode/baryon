#!/usr/bin/env bash
# Root disk storage E2E test for one StorageClass and volume mode: import,
# boot, Anchor Pod replacement, stop and start, a move to another node when
# the volume is not bound to a node, and deletion with claim, volume, and
# host cleanup.
#
# Disk persistence is checked on the guest console: the Debian cloud image
# generates its SSH host keys on the first boot only, so a later boot that
# generates them again ran from a fresh disk.
#
# Environment: see lib.sh, and
#   STORAGE_CLASS  StorageClass of the root disk claim (default: the
#                  cluster's default StorageClass)
#   VOLUME_MODE    Filesystem (default) or Block
# The console and host checks need NODE_SHELL.
set -euo pipefail
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

mode="${VOLUME_MODE:-Filesystem}"
sc="${STORAGE_CLASS:-}"
case "$mode" in
  Filesystem|Block) ;;
  *) echo "VOLUME_MODE must be Filesystem or Block" >&2; exit 2 ;;
esac
vm="e2e-storage-$(printf '%s' "$mode" | tr '[:upper:]' '[:lower:]')-$RANDOM"
log "StorageClass: ${sc:-<default>}, volume mode: $mode"

ready_is() {
  [ "$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')" = "$1" ]
}
pod_uid() { "${k[@]}" get pod "$vm-anchor" -o jsonpath='{.metadata.uid}' 2>/dev/null; }
pod_absent() { ! "${k[@]}" get pod "$vm-anchor" >/dev/null 2>&1; }
vm_absent() { ! "${k[@]}" get virtualmachine "$vm" >/dev/null 2>&1; }
claim_absent() { ! "${k[@]}" get pvc "$vm-root" >/dev/null 2>&1; }
claim_field() { "${k[@]}" get pvc "$vm-root" -o jsonpath="$1" 2>/dev/null; }
# new_pod <old-uid>: an Anchor Pod other than <old-uid> exists.
new_pod() { local p; p="$(pod_uid)"; [ -n "$p" ] && [ "$p" != "$1" ]; }
# host_keys_generated <node> <pod-uid>: SSH host keys generated on this boot.
host_keys_generated() {
  host "$1" "grep -a -c 'Generating public/private' /var/log/baryon/vms/${ns}_${vm}_$2/console.log 2>/dev/null || true"
}
# cloud_init_done <node> <pod-uid>: cloud-init finished on this boot.
cloud_init_done() {
  host "$1" "grep -a -q 'Cloud-init v. .* finished' /var/log/baryon/vms/${ns}_${vm}_$2/console.log"
}
# init_containers: names of the current Anchor Pod's init containers.
init_containers() {
  "${k[@]}" get pod "$vm-anchor" -o jsonpath='{.spec.initContainers[*].name}' 2>/dev/null
}
import_pod_absent() { ! "${k[@]}" get pod "$vm-import" >/dev/null 2>&1; }
# vmm_disk_types <node> <pod-uid>: counts of the file types the VMM of an
# Anchor Pod has open.
vmm_disk_types() {
  host "$1" "for p in /proc/[0-9]*; do tr '\\0' ' ' < \$p/cmdline 2>/dev/null | grep -q '^/[^ ]*cloud-hypervisor .*$2' || continue; for f in \$p/fd/*; do stat -L -c %F \$f 2>/dev/null; done; done | sort | uniq -c"
}

# check_persisted <label> <node> <pod-uid>: the boot reused the disk.
check_persisted() {
  [ -n "${NODE_SHELL:-}" ] || { log "SKIP $1: disk persistence (NODE_SHELL unset)"; return 0; }
  wait_for "$1: cloud-init finished on this boot" cloud_init_done "$2" "$3" || return 0
  local n; n="$(host_keys_generated "$2" "$3")"
  if [ "${n:-0}" -eq 0 ]; then ok "$1: guest booted from the existing disk"; else bad "$1: guest generated SSH host keys again (fresh disk)"; fi
}
# check_no_import <label>: no import Pod, and no import init container.
check_no_import() {
  if ! import_pod_absent; then bad "$1: an import Pod runs again"; return; fi
  case " $(init_containers) " in
    *" import "*) bad "$1: Anchor Pod imports again" ;;
    *) ok "$1: no import runs again" ;;
  esac
}

cleanup() {
  "${k[@]}" delete virtualmachine "$vm" "$vm-second" --ignore-not-found --wait=false >/dev/null 2>&1 || true
  [ -n "${cordoned:-}" ] && kubectl "${ctx[@]}" uncordon "$cordoned" >/dev/null 2>&1 || true
}
trap cleanup EXIT

sha_line=""
[ -n "${IMAGE_SHA256:-}" ] && sha_line="        sha256: ${IMAGE_SHA256}"
sc_line=""
[ -n "$sc" ] && sc_line="    storageClassName: $sc"
"${k[@]}" apply -f - >/dev/null <<EOF
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata:
  name: $vm
spec:
  className: general-small
  rootDisk:
    size: 4Gi
    volumeMode: $mode
$sc_line
    source:
      http:
        url: $url
$sha_line
EOF
log "created VirtualMachine $vm"

wait_for "VM becomes Ready" ready_is True
pods=()
p1="$(pod_uid)"; n1="$(node_of_pod "$vm-anchor")"; pods+=("$n1=$p1")
"${k[@]}" get virtualmachine "$vm"

[ "$(claim_field '{.spec.volumeMode}')" = "$mode" ] && ok "claim has volume mode $mode" || bad "claim volume mode is $(claim_field '{.spec.volumeMode}')"
if [ -n "$sc" ]; then
  [ "$(claim_field '{.spec.storageClassName}')" = "$sc" ] && ok "claim uses StorageClass $sc" || bad "claim StorageClass is $(claim_field '{.spec.storageClassName}')"
fi
[ "$(claim_field '{.status.phase}')" = Bound ] && ok "claim is bound" || bad "claim is not bound"
source_digest="sha256:$(printf '%s' "$url" | sha256sum | cut -d' ' -f1)"
# The completed import is recorded on the bound PersistentVolume.
recorded() {
  local pv uid rec
  pv="$(claim_field '{.spec.volumeName}')"; uid="$(claim_field '{.metadata.uid}')"
  rec="$(kubectl "${ctx[@]}" get pv "$pv" -o jsonpath='{.metadata.annotations.virtualization\.baryon\.io/disk-import}' 2>/dev/null)"
  case "$rec" in *"\"claimUID\":\"$uid\""*"\"source\":\"$source_digest\""*) return 0 ;; *) return 1 ;; esac
}
wait_for "completed import recorded on the PersistentVolume" recorded
wait_for "import Pod removed after the import" import_pod_absent
pv="$(claim_field '{.spec.volumeName}')"

if [ -n "${NODE_SHELL:-}" ]; then
  wait_for "cloud-init finished on the first boot" cloud_init_done "$n1" "$p1"
  n="$(host_keys_generated "$n1" "$p1")"
  if [ "${n:-0}" -gt 0 ]; then ok "first boot ran from the imported image"; else bad "first boot did not generate SSH host keys"; fi
  types="$(vmm_disk_types "$n1" "$p1")"
  if [ "$mode" = Block ]; then
    if printf '%s\n' "$types" | grep -q 'block special file'; then ok "VMM has the block device open"; else bad "VMM has no block device open: $types"; fi
  else
    if printf '%s\n' "$types" | grep -q 'regular file'; then ok "VMM has the disk image open"; else bad "VMM has no regular file open: $types"; fi
  fi
fi

log "a second VM on the same claim is refused"
other="$vm-second"
"${k[@]}" apply -f - >/dev/null <<EOF
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata:
  name: $other
spec:
  className: general-small
  rootDisk:
    source:
      persistentVolumeClaim:
        claimName: $vm-root
EOF
claim_in_use() { [ "$(vm_reason "$other")" = ClaimInUse ]; }
wait_for "second VM reports ClaimInUse" claim_in_use
if "${k[@]}" get pod "$other-anchor" >/dev/null 2>&1; then bad "second VM got an Anchor Pod"; else ok "second VM has no Anchor Pod"; fi
"${k[@]}" delete virtualmachine "$other" --wait=true >/dev/null

log "replacing the Anchor Pod (pod deleted)"
"${k[@]}" delete pod "$vm-anchor" --wait=false >/dev/null
wait_for "a new Anchor Pod is created" new_pod "$p1"
check_no_import "after pod replacement"
wait_for "VM Ready after pod replacement" ready_is True
p2="$(pod_uid)"; n2="$(node_of_pod "$vm-anchor")"; pods+=("$n2=$p2")
check_persisted "after pod replacement" "$n2" "$p2"

log "stopping and starting"
"${k[@]}" patch virtualmachine "$vm" --type merge -p '{"spec":{"runPolicy":"Stopped"}}' >/dev/null
wait_for "Anchor Pod removed on stop" pod_absent
"${k[@]}" patch virtualmachine "$vm" --type merge -p '{"spec":{"runPolicy":"Running"}}' >/dev/null
wait_for "VM Ready after start" ready_is True
check_no_import "after start"
p3="$(pod_uid)"; n3="$(node_of_pod "$vm-anchor")"; pods+=("$n3=$p3")
check_persisted "after start" "$n3" "$p3"

# A volume without node affinity can follow the VM to another node.
pinned="$(kubectl "${ctx[@]}" get pv "$pv" -o jsonpath='{.spec.nodeAffinity.required}' 2>/dev/null)"
if [ -n "$pinned" ]; then
  log "SKIP move to another node: the volume is bound to node $n3"
elif [ "$(kubectl "${ctx[@]}" get nodes -l virtualization.baryon.io/host=true --no-headers | wc -l)" -lt 2 ]; then
  log "SKIP move to another node: fewer than two Baryon nodes"
else
  log "moving the VM off $n3 (node cordoned, VM restarted)"
  kubectl "${ctx[@]}" cordon "$n3" >/dev/null
  cordoned="$n3"
  "${k[@]}" annotate virtualmachine "$vm" --overwrite "virtualization.baryon.io/restartedAt=$(date -u +%FT%TZ)" >/dev/null
  wait_for "a new Anchor Pod is created" new_pod "$p3"
  wait_for "VM Ready on another node" ready_is True
  p4="$(pod_uid)"; n4="$(node_of_pod "$vm-anchor")"; pods+=("$n4=$p4")
  [ "$n4" != "$n3" ] && ok "VM moved from $n3 to $n4" || bad "VM stayed on $n3"
  check_no_import "after the move"
  check_persisted "after the move" "$n4" "$p4"
  kubectl "${ctx[@]}" uncordon "$n3" >/dev/null
  cordoned=""
  host_wait "old node released the VM" no_vmm_for "$n3" "$p3"
fi

log "deleting"
"${k[@]}" delete virtualmachine "$vm" --wait=false >/dev/null
wait_for "VirtualMachine removed" vm_absent
wait_for "root disk claim garbage collected" claim_absent
pv_absent() { ! kubectl "${ctx[@]}" get pv "$pv" >/dev/null 2>&1; }
if [ "$(kubectl "${ctx[@]}" get pv "$pv" -o jsonpath='{.spec.persistentVolumeReclaimPolicy}' 2>/dev/null)" = Retain ]; then
  log "SKIP volume deletion: reclaim policy Retain"
else
  wait_for "persistent volume deleted" pv_absent
fi
host_leftovers "after delete" "${pods[@]}"

cleanup
trap - EXIT
summary
