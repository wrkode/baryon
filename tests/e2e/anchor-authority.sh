#!/usr/bin/env bash
# Anchor authority E2E test (ADR 0015; findings M1, M2, M5, and M7 of
# 2026-10-27 and 2026-10-28), through the paths a namespace user has:
# pods/exec, ephemeral containers, and pod updates on the Anchor Pod and
# the import Pod.
#
# - M2: the Anchor container has no way to import or write the VM's disk:
#   `baryon-anchor import` does not exist, the importer binary is not in the
#   Anchor image, and the import Pod runs the importer image. An importer
#   started with kubectl exec in the import Pod refuses to run (it is not
#   the container's init process); the check fails when the import ends
#   before it could run. The image of the import Pod cannot be changed.
# - M1: a second `baryon-anchor run` started with kubectl exec refuses to
#   run (it is not the container's init process) and leaves the VM, the
#   Anchor Pod, and its readiness untouched; ephemeral containers cannot be
#   added to the Anchor Pod, and its image cannot be changed; with
#   NODE_SHELL, the anchor process is not dumpable (its /proc entries belong
#   to root) and Yama ptrace_scope is 1 or higher on its node.
# - With NODE_SHELL: after a baryon-virtd restart the same anchor process
#   is the VM's anchor again (same VMM, no refusals); with REBOOT=1, after a
#   node reboot the new Anchor Pod's anchor is.
# - Replacing the Anchor Pod never runs the import again, and the new
#   anchor, not the old one, is attached.
# - With NODE_SHELL: after the anchor container is killed and restarted in
#   the same pod, the restarted container's anchor holds the VM.
# - Replacing the Anchor Pod and deleting the VM stop the VM through its
#   anchor: baryon-virtd logs the anchor's final Stop, never a refused one,
#   and the anchor logs that it handled the runtime's SIGTERM, never that
#   it ignored one.
#
# The decisions of baryon-virtd for callers that are not the Anchor
# supervisor (the VMM, the DHCP responder, exec'd processes, nested
# namespaces, another pod) are covered by unit tests in crates/virtd.
#
# Environment: see lib.sh, and STORAGE_CLASS, VOLUME_MODE (Filesystem or
# Block), IMPORTER_IMAGE (expected import Pod image; default: the
# controller's BARYON_IMPORTER_IMAGE), REBOOT=1 to reboot the VM's node.
set -euo pipefail
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"

vm="e2e-authority-$RANDOM"
mode="${VOLUME_MODE:-Filesystem}"
cleanup() {
  "${k[@]}" delete virtualmachine "$vm" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}
trap cleanup EXIT

controller_env() {
  kubectl "${ctx[@]}" -n baryon-system get deploy baryon-controller \
    -o jsonpath="{.spec.template.spec.containers[0].env[?(@.name==\"$1\")].value}"
}
importer_image="${IMPORTER_IMAGE:-$(controller_env BARYON_IMPORTER_IMAGE)}"
anchor_image="$(controller_env BARYON_ANCHOR_IMAGE)"
log "anchor image $anchor_image, importer image $importer_image, volume mode $mode"

sha_line=""
[ -n "${IMAGE_SHA256:-}" ] && sha_line="        sha256: ${IMAGE_SHA256}"
sc_line=""
[ -n "${STORAGE_CLASS:-}" ] && sc_line="    storageClassName: ${STORAGE_CLASS}"
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

# The import Pod runs the importer image, never the anchor image. Wait for
# its container to run, so that the exec below reaches a live importer.
import_image=""
start=$SECONDS
while (( SECONDS - start < timeout )); do
  [ -n "$import_image" ] || import_image="$("${k[@]}" get pod "$vm-import" -o jsonpath='{.spec.containers[0].image}' 2>/dev/null || true)"
  [ -n "$("${k[@]}" get pod "$vm-import" -o jsonpath='{.status.containerStatuses[0].state.running}' 2>/dev/null || true)" ] && break
  [ "$(vm_reason "$vm")" = GuestBooted ] && break
  sleep 0.5
done
# M2: a second importer started with exec refuses to run.
itarget="--dir=/var/lib/baryon/disks/root"
[ "$mode" = Block ] && itarget="--device=/dev/baryon/root"
if out="$("${k[@]}" exec "$vm-import" -c import -- /baryon-importer import --url=http://127.0.0.1:9/disk.img \
    --size-bytes=4294967296 "$itarget" --approval-file=/etc/baryon/import/approved --claim-uid=e2e 2>&1)"; then
  bad "M2: an exec'd importer ran in the import Pod: $out"
elif grep -q "container's init process" <<<"$out"; then
  ok "M2: an exec'd importer refuses to run"
else
  bad "M2: the exec'd importer check did not run (the import Pod ended first): $(head -c 200 <<<"$out")"
fi
# M7: the import Pod's image cannot be replaced.
if out="$("${k[@]}" set image "pod/$vm-import" import=busybox:1.36 2>&1)"; then
  bad "M7: the import Pod's image was changed"
elif grep -q baryon-pod-immutable <<<"$out"; then
  ok "M7: the import Pod's image cannot be changed"
elif grep -q NotFound <<<"$out"; then
  bad "M7: the import Pod image check did not run (the import Pod is gone)"
else
  bad "M7: unexpected result of kubectl set image on the import Pod: $out"
fi
if [ -n "$import_image" ] && [ "$import_image" = "$importer_image" ] && [ "$import_image" != "$anchor_image" ]; then
  ok "import Pod runs the importer image"
else
  bad "import Pod image '$import_image' (expected '$importer_image', not the anchor image)"
fi

ready() { [ "$(vm_reason "$vm")" = GuestBooted ]; }
wait_for "VM Ready" ready || { summary; exit 1; }
anchor="$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.anchorPod}')"
anchor_uid="$("${k[@]}" get pod "$anchor" -o jsonpath='{.metadata.uid}')"
restarts() { "${k[@]}" get pod "$anchor" -o jsonpath='{.status.containerStatuses[0].restartCount}'; }
r0="$(restarts)"
claim="$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.rootDiskClaim}')"
pv="$("${k[@]}" get pvc "$claim" -o jsonpath='{.spec.volumeName}')"
record() { kubectl "${ctx[@]}" get pv "$pv" -o jsonpath='{.metadata.annotations.virtualization\.baryon\.io/disk-import}'; }
rec0="$(record)"
[ -n "$rec0" ] && ok "import record on the volume" || bad "no import record on the volume"

# M2: no import operation in the Anchor container.
target="--dir=/var/lib/baryon/disks/root"
[ "$mode" = Block ] && target="--device=/dev/baryon/root"
if out="$("${k[@]}" exec "$anchor" -c anchor -- /baryon-anchor import "--url=$url" --size-bytes=4294967296 "$target" 2>&1)"; then
  bad "M2: baryon-anchor import ran in the Anchor container: $out"
elif grep -qi "unrecognized subcommand" <<<"$out"; then
  ok "M2: the Anchor binary has no import subcommand"
else
  bad "M2: unexpected result of baryon-anchor import: $out"
fi
if out="$("${k[@]}" exec "$anchor" -c anchor -- /baryon-importer --help 2>&1)"; then
  bad "M2: the importer binary exists in the Anchor container"
else
  ok "M2: the importer binary is not in the Anchor image"
fi
if "${k[@]}" exec "$anchor" -c anchor -- /bin/sh -c true >/dev/null 2>&1; then
  bad "M2: the Anchor container has a shell"
else
  ok "M2: the Anchor container has no shell"
fi

# M1: a second supervisor started with exec refuses to run and changes nothing.
if out="$("${k[@]}" exec "$anchor" -c anchor -- /baryon-anchor run 2>&1)"; then
  bad "M1: an exec'd baryon-anchor run succeeded: $out"
elif grep -q "container's init process" <<<"$out"; then
  ok "M1: an exec'd baryon-anchor run refuses to run"
else
  bad "M1: unexpected result of an exec'd baryon-anchor run: $out"
fi
sleep 5
[ "$(vm_reason "$vm")" = GuestBooted ] && ok "M1: the VM stays Ready" || bad "M1: the VM changed: $(vm_reason "$vm")"
[ "$("${k[@]}" get pod "$anchor" -o jsonpath='{.status.containerStatuses[0].ready}')" = true ] \
  && ok "M1: the Anchor Pod stays ready" || bad "M1: the Anchor Pod is not ready"
[ "$(restarts)" = "$r0" ] && ok "M1: the anchor container did not restart" || bad "M1: the anchor container restarted"

# M1: no ephemeral containers in Baryon pods.
if out="$("${k[@]}" debug "$anchor" --image=busybox:1.36 -c "e2e-debug-$RANDOM" 2>&1)"; then
  bad "M1: an ephemeral container was added to the Anchor Pod"
elif grep -q baryon-no-ephemeral-containers <<<"$out"; then
  ok "M1: ephemeral containers are refused on the Anchor Pod"
else
  bad "M1: unexpected result of kubectl debug: $out"
fi

# M7: the Anchor Pod's image cannot be replaced.
if out="$("${k[@]}" set image "pod/$anchor" anchor=busybox:1.36 2>&1)"; then
  bad "M7: the Anchor Pod's image was changed"
elif grep -q baryon-pod-immutable <<<"$out"; then
  ok "M7: the Anchor Pod's image cannot be changed"
else
  bad "M7: unexpected result of kubectl set image on the Anchor Pod: $out"
fi
[ "$(restarts)" = "$r0" ] && ok "M7: the anchor container did not restart" || bad "M7: the anchor container restarted"

# M1: the anchor process is not dumpable: its /proc entries belong to root.
if [ -n "${NODE_SHELL:-}" ]; then
  node="$(node_of_pod "$anchor")"
  owners="$(host "$node" "for s in /proc/[0-9]*/status; do p=\${s%/status}; case \"\$(tr '\\0' ' ' < \$p/cmdline 2>/dev/null)\" in '/baryon-anchor run '*) grep -q ${anchor_uid//-/_} \$p/cgroup 2>/dev/null && echo \$(sed -n 's/^Uid:[[:space:]]*\([0-9]*\).*/\1/p' \$s) \$(stat -c %u \$p/environ);; esac; done")"
  log "anchor process (uid, owner of /proc/<pid>/environ): $owners"
  set -- $owners
  if [ "$#" -eq 2 ] && [ "$1" != 0 ] && [ "$2" = 0 ]; then
    ok "M1: the anchor process is not dumpable"
  else
    bad "M1: the anchor process looks dumpable or was not found"
  fi
  yama="$(host "$node" "cat /proc/sys/kernel/yama/ptrace_scope" 2>/dev/null | tail -1)"
  [ "${yama:-0}" -ge 1 ] 2>/dev/null && ok "M1: Yama ptrace_scope is $yama on the anchor's node" \
    || bad "M1: Yama ptrace_scope is '${yama}' on the anchor's node (1 or higher required)"
else
  log "SKIP M1: anchor dumpable check (NODE_SHELL unset)"
fi

# Anchor container log of <pod> until the container ends, in the background.
follow() { "${k[@]}" logs -f "$1" -c anchor >"$2" 2>&1 & }
# Final Stop calls of the anchor of <pod uid> on <node> since <epoch>.
final_stops() {
  local n
  n="$(host "$1" "journalctl -u baryon-virtd --since @$3 -o cat --no-pager | grep '\"message\":\"stop\"' | grep '$2' | grep -c '\"final_\":true'; true" 2>/dev/null | tail -1)"
  echo "${n:-0}"
}
graceful() {
  local node="$1" uid="$2" since="$3" logf="$4" what="$5"
  if grep -q "ignoring SIGTERM" "$logf"; then
    bad "$what: the anchor ignored the runtime's SIGTERM"
  elif grep -q "SIGTERM: shutting down the VM" "$logf"; then
    ok "$what: the anchor acted on the runtime's SIGTERM"
  else
    bad "$what: no SIGTERM handling in the anchor log"
  fi
  if [ -n "${NODE_SHELL:-}" ]; then
    [ "$(final_stops "$node" "$uid" "$since")" -ge 1 ] && ok "$what: baryon-virtd logged the anchor's final Stop" \
      || bad "$what: no final Stop from the anchor"
    [ "$(refusals "$node" "$uid" "$since")" = 0 ] && ok "$what: no refused attach or stop" || bad "$what: baryon-virtd refused the anchor"
  fi
}
# Refusals logged by baryon-virtd for this VM's pod since a point in time.
refusals() {
  local n
  n="$(host "$1" "journalctl -u baryon-virtd --since @$3 -o cat --no-pager | grep -E 'attach refused|stop refused' | grep -c '$2'; true" 2>/dev/null | tail -1)"
  echo "${n:-0}"
}
vmm_pid() {
  host "$1" "for d in /proc/[0-9]*; do grep -q '$2' \$d/cgroup 2>/dev/null && case \"\$(tr '\\0' ' ' < \$d/cmdline 2>/dev/null)\" in *cloud-hypervisor*) echo \${d#/proc/};; esac; done | head -1"
}

# baryon-virtd restart: the running anchor is the VM's anchor again.
if [ -n "${NODE_SHELL:-}" ]; then
  node="$(node_of_pod "$anchor")"
  since="$(date +%s)"
  v0="$(vmm_pid "$node" "${anchor_uid//-/_}")"
  host "$node" "systemctl restart baryon-virtd"
  reattached() { host "$node" "journalctl -u baryon-virtd --since @$since -o cat --no-pager | grep 'anchor attached' | grep -q '$anchor_uid'"; }
  wait_for "virtd restart: the anchor attached again" reattached
  [ -n "$v0" ] && [ "$(vmm_pid "$node" "${anchor_uid//-/_}")" = "$v0" ] \
    && ok "virtd restart: same VMM" || bad "virtd restart: the VMM changed or was not found ('$v0')"
  [ "$(restarts)" = "$r0" ] && ok "virtd restart: the anchor container did not restart" || bad "virtd restart: the anchor container restarted"
  [ "$(refusals "$node" "$anchor_uid" "$since")" = 0 ] && ok "virtd restart: no refused attach or stop" || bad "virtd restart: baryon-virtd refused the anchor"
  wait_for "virtd restart: VM Ready" ready
else
  log "SKIP virtd restart (NODE_SHELL unset)"
fi

# The anchor container is killed and restarted in the same pod.
if [ -n "${NODE_SHELL:-}" ]; then
  node="$(node_of_pod "$anchor")"
  since="$(date +%s)"
  host "$node" "for d in /proc/[0-9]*; do case \"\$(tr '\\0' ' ' < \$d/cmdline 2>/dev/null)\" in '/baryon-anchor run '*) grep -q '${anchor_uid//-/_}' \$d/cgroup 2>/dev/null && kill -KILL \${d#/proc/};; esac; done"
  restarted() { [ "$(restarts)" != "$r0" ] && ready; }
  wait_for "container restart: VM Ready in the same pod" restarted
  [ "$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.anchorPod}')" = "$anchor" ] \
    && ok "container restart: same Anchor Pod" || bad "container restart: the Anchor Pod changed"
  new_vmm() { local p; p="$(vmm_pid "$node" "${anchor_uid//-/_}")"; [ -n "$p" ] && [ "$p" != "$v0" ]; }
  wait_for "container restart: a new VMM runs" new_vmm
  wait_for "container restart: VM Ready again" ready
  [ "$(refusals "$node" "$anchor_uid" "$since")" = 0 ] && ok "container restart: no refused attach or stop" \
    || log "container restart: baryon-virtd refused an attach (expected only while it stopped the old VMM)"
  r0="$(restarts)"
else
  log "SKIP container restart (NODE_SHELL unset)"
fi

# Replacing the Anchor Pod never imports again.
since="$(date +%s)"; old_uid="$anchor_uid"; node="$(node_of_pod "$anchor")"
logf="$(mktemp)"; follow "$anchor" "$logf"
"${k[@]}" delete pod "$anchor" --wait=false >/dev/null
seen_import=0
new_ready() {
  local p
  p="$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.anchorPod}')"
  "${k[@]}" get pod "$vm-import" >/dev/null 2>&1 && seen_import=1
  [ -n "$p" ] && [ "$("${k[@]}" get pod "$p" -o jsonpath='{.metadata.uid}')" != "$anchor_uid" ] && ready
}
wait_for "VM Ready on a new Anchor Pod" new_ready
[ "$seen_import" = 0 ] && ok "no import Pod after the Anchor Pod replacement" || bad "an import Pod appeared after the Anchor Pod replacement"
[ "$(record)" = "$rec0" ] && ok "import record unchanged" || bad "import record changed"
graceful "$node" "$old_uid" "$since" "$logf" "replacement"
anchor="$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.anchorPod}')"
anchor_uid="$("${k[@]}" get pod "$anchor" -o jsonpath='{.metadata.uid}')"
if [ -n "${NODE_SHELL:-}" ]; then
  node="$(node_of_pod "$anchor")"
  # baryon-virtd logs an Attach before deciding on it: the new anchor holds
  # the VM when it attached, was not refused, and its pod runs the VMM.
  attached_new() {
    host "$node" "journalctl -u baryon-virtd --since '-15min' -o cat --no-pager | grep '\"message\":\"attach\"' | grep -q '$anchor_uid'" &&
      [ "$(refusals "$node" "$anchor_uid" "$(( $(date +%s) - 900 ))")" = 0 ] &&
      [ -n "$(vmm_pid "$node" "${anchor_uid//-/_}")" ]
  }
  wait_for "replacement: the new anchor is attached" attached_new
fi

# Node reboot: the Anchor Pod created after the reboot holds the VM.
if [ "${REBOOT:-0}" = 1 ] && [ -n "${NODE_SHELL:-}" ]; then
  node="$(node_of_pod "$anchor")"
  host "$node" "systemd-run --on-active=2 systemctl reboot" >/dev/null || true
  rebooted() {
    local p
    p="$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.anchorPod}')"
    [ -n "$p" ] && [ "$("${k[@]}" get pod "$p" -o jsonpath='{.metadata.uid}')" != "$anchor_uid" ] && ready
  }
  saved="$timeout"; timeout=900
  wait_for "reboot: VM Ready on a new Anchor Pod" rebooted
  timeout="$saved"
  anchor="$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.anchorPod}')"
  anchor_uid="$("${k[@]}" get pod "$anchor" -o jsonpath='{.metadata.uid}')"
  node="$(node_of_pod "$anchor")"
  wait_for "reboot: the new anchor is attached" attached_new
  [ "$(record)" = "$rec0" ] && ok "reboot: import record unchanged" || bad "reboot: import record changed"
else
  log "SKIP node reboot (REBOOT=1 and NODE_SHELL to run)"
fi

# Deleting the VM stops it through its anchor.
since="$(date +%s)"
logf="$(mktemp)"; follow "$anchor" "$logf"
"${k[@]}" delete virtualmachine "$vm" --wait=true --timeout=300s >/dev/null \
  && ok "VM deleted" || bad "VM deletion did not finish"
graceful "$node" "$anchor_uid" "$since" "$logf" "delete"

summary
