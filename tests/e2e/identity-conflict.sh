#!/usr/bin/env bash
# E2E: a host identity conflict between two VMs on one node, and how the
# VM that meets it recovers (docs/api.md, Host identity; ADR 0011).
#
# A real conflict needs two VM UIDs whose identities collide, which nobody
# can arrange, so the script injects one as a cluster administrator: it
# pauses baryon-controller, replaces the Anchor Pod of VM B with a copy that
# claims another VM UID whose identity equals that of a running VM A on the
# same node (found by search; the script mirrors the internal identity
# function of crates/runtime-api, which may change), and resumes
# baryon-controller. The pod is created as baryon-controller
# (impersonation), as the admission policies require. VM B's root disk is a
# raw block device, so the copy's fsGroup changes nothing on it.
#
# Cases:
# - With OLD_ANCHOR_IMAGE (an anchor image of a release whose anchor exits
#   with code 1 on a conflict, such as v0.1.0-rc1): baryon-controller does
#   not act on the conflict; VM B stays not ready on the node with reason
#   IdentityConflict and RestartRequired. The documented recovery, deleting
#   the Anchor Pod, gives VM B a new Anchor Pod and it becomes Ready.
# - With the current anchor image (exit code 3): baryon-controller records
#   the node in status.identityConflictNodes, emits an IdentityConflict
#   event, and replaces the Anchor Pod on another node, where VM B becomes
#   Ready. Stopping VM B clears the list.
# In both cases VM A keeps running with the same Anchor Pod.
#
# Environment (besides lib.sh):
#   STORAGE_CLASS     storage class of VM B's root disk; its claims must be
#                     attachable on another node (such as Ceph RBD) for the
#                     move check, else that check is skipped
#   OLD_ANCHOR_IMAGE  optional anchor image of an earlier release, by digest
#   COLLIDE           optional command that prints a UUID with the identity
#                     given as its argument; the default is a python3
#                     search, which can take several minutes
# Needs python3, and permission to scale baryon-controller and to
# impersonate its service account (a cluster administrator).
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=tests/e2e/lib.sh
. "$here/lib.sh"

a=e2e-idc-a
b=e2e-idc-b
sys=(kubectl "${ctx[@]}" -n baryon-system)
as_ctl=(--as=system:serviceaccount:baryon-system:baryon-controller)
replicas="$("${sys[@]}" get deploy baryon-controller -o jsonpath='{.spec.replicas}')"
work="$(mktemp -d)"

resume_controller() {
  "${sys[@]}" scale deploy/baryon-controller --replicas="$replicas" >/dev/null 2>&1 || true
}
cleanup() {
  resume_controller
  "${k[@]}" delete virtualmachine "$a" "$b" --ignore-not-found --wait=true --timeout=600s >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

sc_line=""
[ -n "${STORAGE_CLASS:-}" ] && sc_line="    storageClassName: ${STORAGE_CLASS}"
sha_line=""
[ -n "${IMAGE_SHA256:-}" ] && sha_line="        sha256: ${IMAGE_SHA256}"
vm() { # vm <name> <volumeMode>
  "${k[@]}" apply -f - >/dev/null <<EOF
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata:
  name: $1
spec:
  className: general-small
  rootDisk:
    size: 4Gi
$sc_line
    volumeMode: $2
    source:
      http:
        url: $url
$sha_line
EOF
}
cond() { # cond <vm> <type>: "<status>/<reason>"
  "${k[@]}" get virtualmachine "$1" -o jsonpath="{.status.conditions[?(@.type==\"$2\")].status}/{.status.conditions[?(@.type==\"$2\")].reason}" 2>/dev/null
}
ready() { [ "$(cond "$1" Ready)" = True/GuestBooted ]; }
pod_uid() { "${k[@]}" get pod "$1-anchor" -o jsonpath='{.metadata.uid}' 2>/dev/null; }
conflict_nodes() { "${k[@]}" get virtualmachine "$b" -o jsonpath='{.status.identityConflictNodes}' 2>/dev/null; }
identity() { # identity <vm-uid>: the VM's host identity; mirrors VmIdentity::for_vm in crates/runtime-api/src/identity.rs, change both together
  python3 -c 'import hashlib, sys; d = hashlib.sha256(sys.argv[1].encode()).digest(); print(1000000000 + int.from_bytes(d[:4], "big") % 1000000000)' "$1"
}
collide() { # collide <identity>: a UUID string with that identity
  if [ -n "${COLLIDE:-}" ]; then
    $COLLIDE "$1"
    return
  fi
  python3 - "$1" <<'PY'
import hashlib, multiprocessing as mp, sys
want = int(sys.argv[1]) - 1000000000
def search(start):
    step = mp.cpu_count()
    i = start
    while True:
        s = "%08x-%04x-4%03x-8%03x-%012x" % (0x1dc0ffee, (i >> 32) & 0xffff, (i >> 20) & 0xfff, (i >> 8) & 0xfff, i & 0xffffffffffff)
        if int.from_bytes(hashlib.sha256(s.encode()).digest()[:4], "big") % 1000000000 == want:
            return s
        i += step
with mp.Pool(mp.cpu_count()) as pool:
    for r in pool.imap_unordered(search, range(mp.cpu_count())):
        print(r)
        pool.terminate()
        break
PY
}
# not_started <label> <node> <pod-uid>: baryon-virtd started nothing for a
# pod that still exists (no runtime directory, no VMM, no DHCP responder).
not_started() {
  if [ -z "${NODE_SHELL:-}" ]; then log "SKIP $1: host checks (NODE_SHELL unset)"; return 0; fi
  local procs
  if host "$2" "test -e /run/baryon/vms/$3"; then bad "$1: runtime directory on $2"; else ok "$1: no runtime directory on $2"; fi
  # shellcheck disable=SC2016 # expanded on the node
  procs="$(host "$2" 'for c in /proc/[0-9]*/cmdline; do tr "\0" " " < "$c" 2>/dev/null; echo; done')"
  if printf '%s\n' "$procs" | grep "[c]loud-hypervisor" | grep -qF "$3"; then bad "$1: VMM process on $2"; else ok "$1: no VMM process on $2"; fi
  if printf '%s\n' "$procs" | grep "^baryon-dhcp " | grep -qF "$3"; then bad "$1: DHCP responder on $2"; else ok "$1: no DHCP responder on $2"; fi
}
# inject <image>: pause baryon-controller, replace VM B's Anchor Pod with a
# copy on VM A's node that claims the colliding UID, resume.
inject() {
  local image="$1"
  "${sys[@]}" scale deploy/baryon-controller --replicas=0 >/dev/null
  wait_for "baryon-controller paused" bash -c "[ -z \"\$(kubectl ${ctx[*]} -n baryon-system get pods -l app.kubernetes.io/name=baryon-controller --field-selector=status.phase=Running -o name)\" ]"
  "${k[@]}" delete pod "$b-anchor" --ignore-not-found --wait=true --timeout=300s >/dev/null
  python3 - "$work/template.json" "$image" "$fake_uid" "$id" "$node" > "$work/forged.json" <<'PY'
import json, sys
path, image, fake, ident, node = sys.argv[1:]
p = json.load(open(path))
p.pop("status", None)
for f in ("uid", "resourceVersion", "creationTimestamp", "managedFields", "generation", "deletionTimestamp", "deletionGracePeriodSeconds"):
    p["metadata"].pop(f, None)
s = p["spec"]
s["nodeName"] = node
s["securityContext"].update(runAsUser=int(ident), runAsGroup=int(ident), fsGroup=int(ident))
c = s["containers"][0]
c["image"] = image
for e in c["env"]:
    if e["name"] == "BARYON_VM_SPEC":
        spec = json.loads(e["value"])
        spec["vmUid"] = fake
        e["value"] = json.dumps(spec, separators=(",", ":"))
print(json.dumps(p))
PY
  "${k[@]}" "${as_ctl[@]}" create -f "$work/forged.json" >/dev/null
  forged_uid="$(pod_uid "$b")"
  resume_controller
}

log "VM A on its node, VM B (block root disk) on the same node"
vm "$a" Filesystem
wait_for "VM A Ready" ready "$a"
node="$(node_of_pod "$a-anchor")"
a_pod="$(pod_uid "$a")"
id="$(identity "$("${k[@]}" get virtualmachine "$a" -o jsonpath='{.metadata.uid}')")"
vm "$b" Block
wait_for "VM B Ready" ready "$b"
"${k[@]}" get pod "$b-anchor" -o json > "$work/template.json"
log "VM A on $node; searching a UUID with VM A's host identity"
fake_uid="$(collide "$id")"
if [ "$(identity "$fake_uid")" = "$id" ]; then ok "found a colliding UUID"; else bad "collision search"; fi
current_image="$("${sys[@]}" get deploy baryon-controller -o jsonpath='{.spec.template.spec.containers[0].env[?(@.name=="BARYON_ANCHOR_IMAGE")].value}')"

if [ -n "${OLD_ANCHOR_IMAGE:-}" ]; then
  log "case: an anchor of an earlier release meets the conflict"
  inject "$OLD_ANCHOR_IMAGE"
  wait_for "the old anchor exits with code 1" bash -c "[ \"\$(kubectl ${ctx[*]} -n $ns get pod $b-anchor -o jsonpath='{.status.containerStatuses[0].lastState.terminated.exitCode}')\" = 1 ]"
  msg="$("${k[@]}" get pod "$b-anchor" -o jsonpath='{.status.containerStatuses[0].lastState.terminated.message}')"
  case "$msg" in IdentityConflict:*) ok "its termination message starts with IdentityConflict:";; *) bad "termination message: $msg";; esac
  wait_for "VM B not ready with reason IdentityConflict" bash -c "[ \"\$(kubectl ${ctx[*]} -n $ns get virtualmachine $b -o jsonpath='{.status.conditions[?(@.type==\"Ready\")].reason}')\" = IdentityConflict ]"
  log "VM B: Ready $(cond "$b" Ready), Running $(cond "$b" Running), RestartRequired $(cond "$b" RestartRequired)"
  sleep 30
  if [ -z "$(conflict_nodes)" ]; then ok "baryon-controller does not act on exit code 1 (no node recorded)"; else bad "node recorded for exit code 1: $(conflict_nodes)"; fi
  if [ "$(pod_uid "$b")" = "$forged_uid" ]; then ok "the Anchor Pod was not replaced"; else bad "the Anchor Pod was replaced"; fi
  if [ "$(node_of_pod "$b-anchor")" = "$node" ]; then ok "VM B stays on the node"; else bad "VM B left the node"; fi
  if [ "$(cond "$b" RestartRequired)" = True/AnchorImageChanged ]; then ok "RestartRequired True AnchorImageChanged"; else bad "RestartRequired: $(cond "$b" RestartRequired)"; fi
  not_started "old anchor" "$node" "$forged_uid"
  old_uid="$forged_uid"
  log "recovery: delete the Anchor Pod"
  "${k[@]}" delete pod "$b-anchor" --wait=true --timeout=300s >/dev/null
  wait_for "VM B Ready after the Anchor Pod was deleted" ready "$b"
  if [ "$(pod_uid "$b")" != "$forged_uid" ]; then ok "VM B has a new Anchor Pod"; else bad "same Anchor Pod"; fi
  if [ "$("${k[@]}" get pod "$b-anchor" -o jsonpath='{.spec.containers[0].image}')" = "$current_image" ]; then ok "the new Anchor Pod runs the current anchor image"; else bad "anchor image of the new pod"; fi
  if [ "$(cond "$b" RestartRequired)" = False/UpToDate ]; then ok "RestartRequired False"; else bad "RestartRequired: $(cond "$b" RestartRequired)"; fi
  host_leftovers "old anchor after its deletion" "$node=$old_uid"
fi

log "case: the current anchor meets the conflict"
"${k[@]}" get pod "$b-anchor" -o json > "$work/template.json"
inject "$current_image"
wait_for "baryon-controller records the node" bash -c "kubectl ${ctx[*]} -n $ns get virtualmachine $b -o jsonpath='{.status.identityConflictNodes}' | grep -q '\"$node\"'"
if "${k[@]}" get events --field-selector "involvedObject.name=$b,reason=IdentityConflict" -o name | grep -q .; then ok "IdentityConflict event"; else bad "no IdentityConflict event"; fi
wait_for "the Anchor Pod was replaced" bash -c "[ \"\$(kubectl ${ctx[*]} -n $ns get pod $b-anchor -o jsonpath='{.metadata.uid}' 2>/dev/null)\" != $forged_uid ]"
if "${k[@]}" get pod "$b-anchor" -o jsonpath='{.spec.affinity.nodeAffinity}' | grep -q "\"$node\""; then ok "the new Anchor Pod avoids the node"; else bad "no node affinity for the recorded node"; fi
if [ -n "${STORAGE_CLASS:-}" ]; then
  wait_for "VM B Ready on another node" ready "$b"
  if [ "$(node_of_pod "$b-anchor")" != "$node" ]; then ok "VM B runs on another node"; else bad "VM B on the recorded node"; fi
else
  log "SKIP move check (STORAGE_CLASS unset)"
fi

host_leftovers "conflicting anchor after its replacement" "$node=$forged_uid"
if [ "$(pod_uid "$a")" = "$a_pod" ]; then ok "VM A kept its Anchor Pod"; else bad "VM A's Anchor Pod changed"; fi
if ready "$a"; then ok "VM A Ready"; else bad "VM A not ready: $(cond "$a" Ready)"; fi

log "stop VM B: the recorded nodes are cleared"
"${k[@]}" patch virtualmachine "$b" --type merge -p '{"spec":{"runPolicy":"Stopped"}}' >/dev/null
wait_for "identityConflictNodes cleared on stop" bash -c "[ -z \"\$(kubectl ${ctx[*]} -n $ns get virtualmachine $b -o jsonpath='{.status.identityConflictNodes}')\" ]"

summary
