#!/usr/bin/env bash
# Import integrity E2E test for a block-mode root disk (ADR 0006):
#
# - stale data: the claim's device is filled with a marker before the
#   import; after the import and a guest boot, no byte of the device holds
#   the marker;
# - import record: a namespace user (a service account bound to the
#   Kubernetes `edit` role) removes the claim's metadata and cannot write
#   the PersistentVolume, the VM status, or the disk source; Anchor Pod
#   replacement, a baryon-controller restart, and VM restarts then reuse the
#   disk without a second import.
#
# Needs NODE_SHELL (see lib.sh), a namespace that admits root pods (the
# filler and scanner pods read and write the raw device), and
# STORAGE_CLASS, a StorageClass that provisions block volumes.
# Environment: see lib.sh, and STORAGE_CLASS, SCAN_IMAGE (default
# python:3.13-alpine).
set -euo pipefail
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
[ -n "${NODE_SHELL:-}" ] || { echo "import-integrity.sh needs NODE_SHELL" >&2; exit 2; }
[ -n "${STORAGE_CLASS:-}" ] || { echo "STORAGE_CLASS is required" >&2; exit 2; }

run="$RANDOM"
vm="e2e-integrity-$run"
claim="$vm-root"
sa="e2e-tenant-$run"
scan_image="${SCAN_IMAGE:-python:3.13-alpine}"
marker="BARYON-STALE-DATA-MARKER"
tenant=(kubectl "${ctx[@]}" -n "$ns" --as="system:serviceaccount:$ns:$sa")

cleanup() {
  "${k[@]}" delete virtualmachine "$vm" --ignore-not-found --wait=false >/dev/null 2>&1 || true
  "${k[@]}" delete pod "$vm-filler" "$vm-scan" "$vm-approval" --ignore-not-found --wait=false >/dev/null 2>&1 || true
  "${k[@]}" delete rolebinding "$sa" --ignore-not-found >/dev/null 2>&1 || true
  "${k[@]}" delete serviceaccount "$sa" --ignore-not-found >/dev/null 2>&1 || true
}
trap cleanup EXIT

ready_is() {
  [ "$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')" = "$1" ]
}
pod_absent() { ! "${k[@]}" get pod "$1" >/dev/null 2>&1; }
pod_done() {
  local phase
  phase="$("${k[@]}" get pod "$1" -o jsonpath='{.status.phase}' 2>/dev/null)"
  [ "$phase" = Succeeded ] || [ "$phase" = Failed ]
}
events() {
  "${k[@]}" get events --field-selector "involvedObject.name=$vm,reason=$1" -o jsonpath='{range .items[*]}{.count}{"\n"}{end}' 2>/dev/null |
    awk '{ s += ($1 == "" ? 1 : $1) } END { print s + 0 }'
}
anchor_uid() { "${k[@]}" get pod "$vm-anchor" -o jsonpath='{.metadata.uid}' 2>/dev/null; }
host_keys_generated() {
  host "$1" "grep -a -c 'Generating public/private' /var/log/baryon/vms/${ns}_${vm}_$2/console.log 2>/dev/null || true"
}
cloud_init_done() {
  host "$1" "grep -a -q 'Cloud-init v. .* finished' /var/log/baryon/vms/${ns}_${vm}_$2/console.log"
}
# check_reused <label>: the current boot ran from the existing disk.
check_reused() {
  local node uid n
  node="$(node_of_pod "$vm-anchor")"; uid="$(anchor_uid)"
  wait_for "$1: cloud-init finished" cloud_init_done "$node" "$uid" || return 0
  n="$(host_keys_generated "$node" "$uid")"
  if [ "${n:-0}" -eq 0 ]; then ok "$1: guest booted from the existing disk"; else bad "$1: the disk was imported again"; fi
}
check_one_import() {
  local i
  i="$(events Importing)"
  if [ "$i" -eq 1 ]; then ok "$1: exactly one import"; else bad "$1: $i imports"; fi
  if pod_absent "$vm-import"; then ok "$1: no import Pod"; else bad "$1: an import Pod exists"; fi
}

# run_device_pod <name> <python>: run Python as root against the claim's
# device at /dev/blk and print its output.
run_device_pod() {
  "${k[@]}" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $1
spec:
  restartPolicy: Never
  containers:
    - name: c
      image: $scan_image
      securityContext:
        runAsUser: 0
      command: [python3, -c]
      args:
        - |
$(printf '%s\n' "$2" | sed 's/^/          /')
      volumeDevices:
        - name: disk
          devicePath: /dev/blk
  volumes:
    - name: disk
      persistentVolumeClaim:
        claimName: $claim
EOF
  wait_for "$1 finished" pod_done "$1" >/dev/null
  "${k[@]}" logs "$1" 2>&1 | tail -1
  "${k[@]}" delete pod "$1" --wait=true >/dev/null
}
fill_py="m=b'$marker'*43690
n=0
f=open('/dev/blk','r+b',buffering=0)
while True:
    try:
        w=f.write(m)
    except OSError:
        break
    if not w:
        break
    n+=w
f.close()
print(n)"
scan_py="m=b'$marker'
n=0;t=0;tail=b''
f=open('/dev/blk','rb',buffering=0)
while True:
    b=f.read(8<<20)
    if not b:
        break
    s=tail+b
    n+=s.count(m)
    tail=s[-(len(m)-1):]
    t+=len(b)
print(t,n)"

log "namespace user $sa with the edit role"
"${k[@]}" create serviceaccount "$sa" >/dev/null
"${k[@]}" create rolebinding "$sa" --clusterrole=edit --serviceaccount="$ns:$sa" >/dev/null

sha_line=""
[ -n "${IMAGE_SHA256:-}" ] && sha_line="        sha256: ${IMAGE_SHA256}"
created() {
  "${tenant[@]}" apply -f - >/dev/null 2>&1 <<EOF
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata:
  name: $vm
spec:
  className: general-small
  runPolicy: Stopped
  rootDisk:
    size: 4Gi
    volumeMode: Block
    storageClassName: $STORAGE_CLASS
    source:
      http:
        url: $url
$sha_line
EOF
}
# The aggregated role reaches the edit role shortly after installation.
wait_for "namespace user creates the VirtualMachine" created
claim_exists() { "${k[@]}" get pvc "$claim" >/dev/null 2>&1; }
wait_for "root disk claim created" claim_exists

log "filling the device with stale data"
filled="$(run_device_pod "$vm-filler" "$fill_py")"
log "filler wrote $filled bytes"
read -r total found <<<"$(run_device_pod "$vm-scan" "$scan_py")"
if [ "${found:-0}" -gt 0 ]; then ok "device holds the stale marker before the import ($found times in $total bytes)"; else bad "the filler did not write the marker"; fi

log "a namespace user's pod named like the import Pod is not trusted"
vm_uid="$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.metadata.uid}')"
"${tenant[@]}" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $vm-import
  labels:
    app.kubernetes.io/managed-by: baryon
  ownerReferences:
    - apiVersion: virtualization.baryon.io/v1alpha1
      kind: VirtualMachine
      name: $vm
      uid: $vm_uid
      controller: true
spec:
  restartPolicy: Never
  containers:
    - name: import
      image: ${PROBE_IMAGE:-busybox:1.36}
      command: ["true"]
EOF
"${tenant[@]}" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $vm-anchor
  labels:
    app.kubernetes.io/managed-by: baryon
  ownerReferences:
    - apiVersion: virtualization.baryon.io/v1alpha1
      kind: VirtualMachine
      name: $vm
      uid: $vm_uid
      controller: true
spec:
  restartPolicy: Never
  initContainers:
    - name: import
      image: ${PROBE_IMAGE:-busybox:1.36}
      command: ["true"]
  containers:
    - name: anchor
      image: ${PROBE_IMAGE:-busybox:1.36}
      command: [sleep, "3600"]
      volumeDevices:
        - name: disk
          devicePath: /dev/blk
  volumes:
    - name: disk
      persistentVolumeClaim:
        claimName: $claim
EOF
"${tenant[@]}" patch virtualmachine "$vm" --type merge -p '{"spec":{"runPolicy":"Running"}}' >/dev/null
anchor_conflict() { [ "$(vm_reason "$vm")" = AnchorPodConflict ]; }
wait_for "VM reports AnchorPodConflict for the user's Anchor-named pod" anchor_conflict
storage_ready="$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.conditions[?(@.type=="StorageReady")].status}')"
if [ "$storage_ready" != True ]; then ok "the user's init container does not make the disk ready"; else bad "StorageReady=True from the user's pod"; fi
"${tenant[@]}" delete pod "$vm-anchor" --wait=true >/dev/null
conflict() { [ "$(vm_reason "$vm")" = ImportPodConflict ]; }
wait_for "VM reports ImportPodConflict" conflict
if "${k[@]}" get pod "$vm-anchor" >/dev/null 2>&1; then bad "an Anchor Pod was created"; else ok "no Anchor Pod while the conflict exists"; fi
pv_now="$("${k[@]}" get pvc "$claim" -o jsonpath='{.spec.volumeName}')"
rec_now="$(kubectl "${ctx[@]}" get pv "$pv_now" -o jsonpath='{.metadata.annotations.virtualization\.baryon\.io/disk-import}' 2>/dev/null || true)"
if [ -z "$rec_now" ]; then ok "the user's pod produced no import record"; else bad "an import record was written: $rec_now"; fi

log "starting the VM (import)"
"${tenant[@]}" delete pod "$vm-import" --wait=true >/dev/null
wait_for "VM Ready after the import" ready_is True
pv="$("${k[@]}" get pvc "$claim" -o jsonpath='{.spec.volumeName}')"
claim_uid="$("${k[@]}" get pvc "$claim" -o jsonpath='{.metadata.uid}')"
record="$(kubectl "${ctx[@]}" get pv "$pv" -o jsonpath='{.metadata.annotations.virtualization\.baryon\.io/disk-import}')"
case "$record" in
  *"\"claimUID\":\"$claim_uid\""*) ok "import recorded on the PersistentVolume for this claim" ;;
  *) bad "no import record for this claim on the PersistentVolume: $record" ;;
esac
node="$(node_of_pod "$vm-anchor")"; uid="$(anchor_uid)"
wait_for "first boot: cloud-init finished" cloud_init_done "$node" "$uid"
n="$(host_keys_generated "$node" "$uid")"
if [ "${n:-0}" -gt 0 ]; then ok "first boot ran from the imported image"; else bad "first boot did not run from a fresh image"; fi

log "stopping the VM and scanning the whole device"
"${tenant[@]}" patch virtualmachine "$vm" --type merge -p '{"spec":{"runPolicy":"Stopped"}}' >/dev/null
wait_for "Anchor Pod gone" pod_absent "$vm-anchor"
read -r total found <<<"$(run_device_pod "$vm-scan" "$scan_py")"
if [ "${found:-1}" -eq 0 ] && [ "${total:-0}" -gt 0 ]; then
  ok "no stale data on the device after the import ($total bytes scanned)"
else
  bad "stale marker found $found times in $total bytes after the import"
fi

log "namespace user edits the claim metadata and tries the import record"
"${tenant[@]}" annotate pvc "$claim" virtualization.baryon.io/imported-from- >/dev/null 2>&1 || true
"${tenant[@]}" annotate pvc "$claim" --overwrite example.org/tenant=edited >/dev/null
ok "namespace user removed and changed claim annotations"
if "${tenant[@]}" annotate pv "$pv" --overwrite virtualization.baryon.io/disk-import- >/dev/null 2>&1; then
  bad "namespace user changed the PersistentVolume"
else
  ok "namespace user cannot change the PersistentVolume"
fi
"${tenant[@]}" run "$vm-approval" --image="${PROBE_IMAGE:-busybox:1.36}" --restart=Never -- sleep 60 >/dev/null
if "${tenant[@]}" annotate pod "$vm-approval" virtualization.baryon.io/import-approved="$claim_uid" >/dev/null 2>&1; then
  bad "namespace user set an import approval"
else
  ok "namespace user cannot set an import approval"
fi
"${tenant[@]}" delete pod "$vm-approval" --wait=false >/dev/null
if "${tenant[@]}" patch virtualmachine "$vm" --subresource=status --type merge -p '{"status":{"conditions":[]}}' >/dev/null 2>&1; then
  bad "namespace user wrote the VM status"
else
  ok "namespace user cannot write the VM status"
fi
if "${tenant[@]}" patch virtualmachine "$vm" --type merge -p '{"spec":{"rootDisk":{"source":{"http":{"url":"https://example.org/other.raw"}}}}}' >/dev/null 2>&1; then
  bad "namespace user changed the disk source"
else
  ok "the disk source cannot be changed"
fi

log "starting again"
"${tenant[@]}" patch virtualmachine "$vm" --type merge -p '{"spec":{"runPolicy":"Running"}}' >/dev/null
wait_for "VM Ready after start" ready_is True
check_one_import "after start"
check_reused "after start"

log "Anchor Pod replacement"
old="$(anchor_uid)"
"${tenant[@]}" delete pod "$vm-anchor" --wait=false >/dev/null
replaced() { local u; u="$(anchor_uid)"; [ -n "$u" ] && [ "$u" != "$old" ]; }
wait_for "Anchor Pod replaced" replaced
wait_for "VM Ready after replacement" ready_is True
check_one_import "after Anchor Pod replacement"
check_reused "after Anchor Pod replacement"

log "baryon-controller restart, then VM restart"
kubectl "${ctx[@]}" -n baryon-system delete pod -l app.kubernetes.io/name=baryon-controller --wait=true >/dev/null
kubectl "${ctx[@]}" -n baryon-system rollout status deploy/baryon-controller --timeout="${timeout}s" >/dev/null
ok "baryon-controller restarted"
old="$(anchor_uid)"
"${tenant[@]}" annotate virtualmachine "$vm" --overwrite "virtualization.baryon.io/restartedAt=$(date -u +%FT%TZ)" >/dev/null
wait_for "Anchor Pod replaced" replaced
wait_for "VM Ready after controller restart and VM restart" ready_is True
check_one_import "after controller restart"
check_reused "after controller restart"

log "a lost volume record does not make the used disk look new"
status_uid="$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.rootDiskImport.claimUID}')"
if [ "$status_uid" = "$claim_uid" ]; then ok "VM status records the import into this claim"; else bad "VM status import record: '$status_uid'"; fi
"${tenant[@]}" patch virtualmachine "$vm" --type merge -p '{"spec":{"runPolicy":"Stopped"}}' >/dev/null
wait_for "Anchor Pod gone" pod_absent "$vm-anchor"
saved="$(kubectl "${ctx[@]}" get pv "$pv" -o jsonpath='{.metadata.annotations.virtualization\.baryon\.io/disk-import}')"
kubectl "${ctx[@]}" annotate pv "$pv" virtualization.baryon.io/disk-import- >/dev/null
"${tenant[@]}" patch virtualmachine "$vm" --type merge -p '{"spec":{"runPolicy":"Running"}}' >/dev/null
refused() { [ "$(vm_reason "$vm")" = ImportStateUnknown ]; }
wait_for "VM refused with ImportStateUnknown" refused
check_one_import "with the volume record removed"
kubectl "${ctx[@]}" annotate pv "$pv" "virtualization.baryon.io/disk-import=$saved" >/dev/null
wait_for "VM Ready after the record is restored" ready_is True
check_one_import "after the record is restored"
check_reused "after the record is restored"

log "deleting"
pods_left="$(anchor_uid)"; node="$(node_of_pod "$vm-anchor")"
"${tenant[@]}" delete virtualmachine "$vm" --wait=false >/dev/null
vm_absent() { ! "${k[@]}" get virtualmachine "$vm" >/dev/null 2>&1; }
wait_for "VirtualMachine removed" vm_absent
host_leftovers "after delete" "$node=$pods_left"

cleanup
trap - EXIT
summary
