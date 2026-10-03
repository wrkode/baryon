#!/usr/bin/env bash
# Hostile disk E2E test: a claim whose disk.img is a block device node that
# a pod created (mknod) must not be opened by the VMM. baryon-virtd accepts
# only a regular file at the filesystem-mode disk path and device nodes only
# on the container's /dev.
#
# Needs a namespace that admits a root pod with the container runtime's
# default capabilities (CAP_MKNOD), a filesystem-mode StorageClass (the
# default one, or STORAGE_CLASS), and NODE_SHELL for the host check.
# Environment: see lib.sh, and STORAGE_CLASS, PROBE_IMAGE (busybox:1.36).
set -euo pipefail
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

run="$RANDOM"
claim="e2e-hostile-$run"
maker="e2e-hostile-mknod-$run"
vm="e2e-hostile-vm-$run"

cleanup() {
  "${k[@]}" delete virtualmachine "$vm" --ignore-not-found --wait=false >/dev/null 2>&1 || true
  "${k[@]}" delete pod "$maker" --ignore-not-found --wait=false >/dev/null 2>&1 || true
  "${k[@]}" delete pvc "$claim" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}
trap cleanup EXIT

sc_line=""
[ -n "${STORAGE_CLASS:-}" ] && sc_line="  storageClassName: $STORAGE_CLASS"
"${k[@]}" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: $claim
spec:
$sc_line
  accessModes: [ReadWriteOnce]
  volumeMode: Filesystem
  resources:
    requests:
      storage: 1Gi
---
apiVersion: v1
kind: Pod
metadata:
  name: $maker
spec:
  restartPolicy: Never
  containers:
    - name: mknod
      image: ${PROBE_IMAGE:-busybox:1.36}
      securityContext:
        runAsUser: 0
      command: [sh, -c, "mknod /data/disk.img b 7 0 && chmod 0666 /data/disk.img && ls -l /data/disk.img"]
      volumeMounts:
        - name: data
          mountPath: /data
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: $claim
EOF
made() { [ "$("${k[@]}" get pod "$maker" -o jsonpath='{.status.phase}')" = Succeeded ]; }
wait_for "a pod created a block device node as disk.img" made
"${k[@]}" delete pod "$maker" --wait=true >/dev/null

"${k[@]}" apply -f - >/dev/null <<EOF
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata:
  name: $vm
spec:
  className: general-small
  rootDisk:
    source:
      persistentVolumeClaim:
        claimName: $claim
EOF
refused() { [ "$(vm_reason "$vm")" = StorageFailed ]; }
wait_for "VM refused with StorageFailed" refused
msg="$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.conditions[?(@.type=="Ready")].message}')"
log "message: $msg"
case "$msg" in
  *"expected a regular file"*) ok "refusal names the wrong disk kind" ;;
  *) bad "unexpected refusal message" ;;
esac
pod="$("${k[@]}" get pod "$vm-anchor" -o jsonpath='{.metadata.uid}' 2>/dev/null || true)"
node="$(node_of_pod "$vm-anchor")"
if [ -n "$pod" ] && [ -n "$node" ]; then
  host_wait "no VMM started for the hostile disk" no_vmm_for "$node" "$pod"
fi

cleanup
trap - EXIT
summary
