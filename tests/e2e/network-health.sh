#!/usr/bin/env bash
# Guest network service E2E test: baryon-virtd supervises each VM's DHCP
# responder and the VM reports its state in the NetworkReady condition,
# separately from Ready.
#
# Failures are injected from the host only, by killing the responder
# process of a VM (no test hook in Baryon). Cases:
#   after-lease     responder killed once after the guest got its lease:
#                   restarted, NetworkReady True again, Ready stays True
#   failing         responder killed repeatedly: NetworkReady False
#                   (DhcpResponderFailing) while Ready stays True and the
#                   message names the lease; recovers when the kills stop
#   before-lease    responder killed from the VM's start: the guest has no
#                   address and NetworkReady is False; once the kills stop
#                   the guest gets its address and NetworkReady is True
#   anchor          Anchor Pod replaced: NetworkReady True for the new pod
#   virtd           baryon-virtd restarted: NetworkReady True again with a
#                   new responder
#   agent           the node agent of the VM's node removed for longer than
#                   the 90 s staleness bound: NetworkReady Unknown
#                   (RuntimeUnreachable), Ready unchanged; True again once
#                   the agent reports
#   reboot          the VM's node rebooted: NetworkReady True after the VM
#                   is back (skipped unless REBOOT=1)
#
# Requires NODE_SHELL (see lib.sh). Environment: see lib.sh, and
# STORAGE_CLASS (default StorageClass when unset), REBOOT.
set -euo pipefail
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
[ -n "${NODE_SHELL:-}" ] || { echo "network-health.sh needs NODE_SHELL" >&2; exit 2; }

run="$RANDOM"
vms=()
killers=()
cleanup() {
  local p v
  [ "$(type -t restore_agent)" = function ] && restore_agent
  for p in "${killers[@]}"; do kill "$p" >/dev/null 2>&1 || true; done
  for v in "${vms[@]}"; do "${k[@]}" delete virtualmachine "$v" --ignore-not-found --wait=false >/dev/null 2>&1 || true; done
}
trap cleanup EXIT

vm() {
  local sha_line="" sc_line=""
  [ -n "${IMAGE_SHA256:-}" ] && sha_line="        sha256: ${IMAGE_SHA256}"
  [ -n "${STORAGE_CLASS:-}" ] && sc_line="    storageClassName: $STORAGE_CLASS"
  vms+=("$1")
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
    source:
      http:
        url: $url
$sha_line
EOF
}
cond() { "${k[@]}" get virtualmachine "$1" -o jsonpath="{.status.conditions[?(@.type==\"$2\")].$3}"; }
net_is() { [ "$(cond "$1" NetworkReady status)" = "$2" ] && { [ -z "${3:-}" ] || [ "$(cond "$1" NetworkReady reason)" = "$3" ]; }; }
ready() { [ "$(cond "$1" Ready status)" = True ]; }
anchor_uid() { "${k[@]}" get pod "$1-anchor" -o jsonpath='{.metadata.uid}' 2>/dev/null || true; }
node_of() { "${k[@]}" get virtualmachine "$1" -o jsonpath='{.status.nodeName}' 2>/dev/null || true; }
# responder_pids <node> <pod-uid>: PIDs of the VM's DHCP responders.
responder_pids() {
  [ -n "$1" ] && [ -n "$2" ] || return 1
  host "$1" "for c in /proc/[0-9]*/cmdline; do l=\$(tr '\\0' ' ' < \$c 2>/dev/null); case \"\$l\" in baryon-dhcp\\ dhcp-responder*--pod-uid\\ $2*) d=\${c%/cmdline}; echo \${d#/proc/};; esac; done"
}
# kill_responders <node> <pod-uid>: SIGKILL the VM's responders, matching
# the command line again right before each kill.
kill_responders() {
  [ -n "$1" ] && [ -n "$2" ] || return 1
  host "$1" "for c in /proc/[0-9]*/cmdline; do l=\$(tr '\\0' ' ' < \$c 2>/dev/null); case \"\$l\" in baryon-dhcp\\ dhcp-responder*--pod-uid\\ $2\\ *|baryon-dhcp\\ dhcp-responder*--pod-uid\\ $2) d=\${c%/cmdline}; kill -9 \${d#/proc/};; esac; done" >/dev/null 2>&1
  return 0
}
# start_killer <node> <pod-uid>: kill the VM's responders every second in
# the background; prints the killer's PID.
start_killer() {
  ( while :; do kill_responders "$1" "$2"; sleep 1; done ) >/dev/null 2>&1 &
  echo $!
}
check() { if "$@"; then ok "$label"; else bad "$label"; fi; }

log "case after-lease: one responder kill after the guest's lease"
vm "nh-a-$run"
wait_for "after-lease: VM Ready" ready "nh-a-$run"
wait_for "after-lease: NetworkReady True" net_is "nh-a-$run" True AddressServiceRunning
n="$(node_of "nh-a-$run")"; u="$(anchor_uid "nh-a-$run")"
before="$(responder_pids "$n" "$u")"
label="after-lease: one responder runs"; check [ "$(wc -w <<<"$before")" -eq 1 ]
kill_responders "$n" "$u"
restarted() { local p; p="$(responder_pids "$n" "$u")"; [ -n "$p" ] && [ "$p" != "$before" ]; }
wait_for "after-lease: responder restarted" restarted
wait_for "after-lease: NetworkReady True" net_is "nh-a-$run" True
label="after-lease: Ready stayed True"; check ready "nh-a-$run"

log "case failing: responder killed repeatedly"
killer="$(start_killer "$n" "$u")"; killers+=("$killer")
TIMEOUT_SAVED="$timeout"; timeout=420
wait_for "failing: NetworkReady False DhcpResponderFailing" net_is "nh-a-$run" False DhcpResponderFailing
timeout="$TIMEOUT_SAVED"
label="failing: Ready stays True"; check ready "nh-a-$run"
msg="$(cond "nh-a-$run" NetworkReady message)"
log "message: $msg"
label="failing: the message gives the lease expiry"; check grep -q "lease valid until" <<<"$msg"
kill "$killer"
timeout=180
wait_for "failing: NetworkReady True once the kills stop" net_is "nh-a-$run" True AddressServiceRunning
timeout="$TIMEOUT_SAVED"

log "case anchor: Anchor Pod replaced"
old="$u"
"${k[@]}" delete pod "nh-a-$run-anchor" --wait=false >/dev/null
replaced() { local x; x="$(anchor_uid "nh-a-$run")"; [ -n "$x" ] && [ "$x" != "$old" ]; }
wait_for "anchor: new Anchor Pod" replaced
wait_for "anchor: VM Ready" ready "nh-a-$run"
wait_for "anchor: NetworkReady True for the new pod" net_is "nh-a-$run" True AddressServiceRunning

log "case virtd: baryon-virtd restarted"
n="$(node_of "nh-a-$run")"; u="$(anchor_uid "nh-a-$run")"
before="$(responder_pids "$n" "$u")"
host "$n" "systemctl restart baryon-virtd"
new_responder() { local p; p="$(responder_pids "$n" "$u")"; [ -n "$p" ] && [ "$p" != "$before" ]; }
wait_for "virtd: a new responder runs" new_responder
wait_for "virtd: NetworkReady True" net_is "nh-a-$run" True AddressServiceRunning
label="virtd: same Anchor Pod"; check [ "$(anchor_uid "nh-a-$run")" = "$u" ]

log "case agent: the node agent of the VM's node is down for longer than 90 s"
n="$(node_of "nh-a-$run")"
ds=(-n baryon-system)
agent_pod() { kubectl "${ctx[@]}" "${ds[@]}" get pods -l app.kubernetes.io/name=baryon-node-agent --field-selector "spec.nodeName=$n" -o name 2>/dev/null; }
restore_agent() { kubectl "${ctx[@]}" "${ds[@]}" patch daemonset baryon-node-agent --type json -p '[{"op":"remove","path":"/spec/template/spec/affinity"}]' >/dev/null 2>&1 || true; }
kubectl "${ctx[@]}" "${ds[@]}" patch daemonset baryon-node-agent --type json -p "[{\"op\":\"add\",\"path\":\"/spec/template/spec/affinity\",\"value\":{\"nodeAffinity\":{\"requiredDuringSchedulingIgnoredDuringExecution\":{\"nodeSelectorTerms\":[{\"matchExpressions\":[{\"key\":\"kubernetes.io/hostname\",\"operator\":\"NotIn\",\"values\":[\"$n\"]}]}]}}}}]" >/dev/null
no_agent() { [ -z "$(agent_pod)" ]; }
wait_for "agent: node agent gone from the VM's node" no_agent
timeout=240
wait_for "agent: NetworkReady Unknown RuntimeUnreachable" net_is "nh-a-$run" Unknown RuntimeUnreachable
timeout="$TIMEOUT_SAVED"
label="agent: Ready stays True"; check ready "nh-a-$run"
restore_agent
has_agent() { kubectl "${ctx[@]}" "${ds[@]}" get pods -l app.kubernetes.io/name=baryon-node-agent --field-selector "spec.nodeName=$n" 2>/dev/null | grep -q "1/1"; }
wait_for "agent: node agent back" has_agent
timeout=120
wait_for "agent: NetworkReady True again" net_is "nh-a-$run" True AddressServiceRunning
timeout="$TIMEOUT_SAVED"

log "case before-lease: responder killed from the VM's start"
vm "nh-b-$run"
scheduled() { [ -n "$(node_of "nh-b-$run")" ] && [ -n "$(anchor_uid "nh-b-$run")" ]; }
wait_for "before-lease: Anchor Pod scheduled" scheduled
nb="$(node_of "nh-b-$run")"; ub="$(anchor_uid "nh-b-$run")"
killer="$(start_killer "$nb" "$ub")"; killers+=("$killer")
timeout=420
wait_for "before-lease: NetworkReady False" net_is "nh-b-$run" False
timeout="$TIMEOUT_SAVED"
msg="$(cond "nh-b-$run" NetworkReady message)"
log "message: $msg"
label="before-lease: the message says the guest has no address"; check grep -q "has not acquired an address" <<<"$msg"
kill "$killer"
timeout=600
wait_for "before-lease: NetworkReady True once the kills stop" net_is "nh-b-$run" True AddressServiceRunning
wait_for "before-lease: VM Ready" ready "nh-b-$run"
timeout="$TIMEOUT_SAVED"

if [ "${REBOOT:-0}" = 1 ]; then
  log "case reboot: the VM's node reboots"
  n="$(node_of "nh-a-$run")"; u="$(anchor_uid "nh-a-$run")"
  host "$n" "systemd-run --on-active=2 systemctl reboot" >/dev/null || true
  gone_pod() { [ "$(anchor_uid "nh-a-$run")" != "$u" ]; }
  timeout=900
  wait_for "reboot: Anchor Pod replaced" gone_pod
  wait_for "reboot: VM Ready" ready "nh-a-$run"
  wait_for "reboot: NetworkReady True" net_is "nh-a-$run" True AddressServiceRunning
  timeout="$TIMEOUT_SAVED"
else
  log "SKIP reboot (REBOOT=1 to run)"
fi

log "deleting"
pairs=()
for v in "${vms[@]}"; do
  pairs+=("$(node_of "$v")=$(anchor_uid "$v")")
  "${k[@]}" delete virtualmachine "$v" --wait=false >/dev/null
done
gone() { [ -z "$("${k[@]}" get virtualmachine -o name 2>/dev/null | grep "nh-.-$run")" ]; }
wait_for "VMs removed" gone
host_leftovers "after delete" "${pairs[@]}"

cleanup
trap - EXIT
summary
