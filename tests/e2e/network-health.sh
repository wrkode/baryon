#!/usr/bin/env bash
# Guest network service E2E test: baryon-virtd supervises each VM's DHCP
# responder and the VM reports its state in the NetworkReady condition,
# separately from Ready.
#
# Failures are injected from the host only (no test hook in Baryon): an
# exit by killing the responder process of a VM, a hang by stopping it with
# SIGSTOP, which leaves the process in place but stops its serving loop.
# baryon-virtd probes the loop through the responder's control pipe and must
# not rely on guest DHCP traffic. Cases:
#   after-lease     responder killed once after the guest got its lease:
#                   restarted, NetworkReady True again, Ready stays True
#   hang            responder stopped once after the guest got its lease:
#                   baryon-virtd detects the hang, kills the stopped process,
#                   starts a new responder; NetworkReady True again, Ready
#                   stays True
#   hang-repeated   every new responder stopped: NetworkReady False with a
#                   message naming the missing probe answers, at least three
#                   hang restarts, Ready stays True; recovers when the stops
#                   end
#   failing         responder killed repeatedly: NetworkReady False
#                   (DhcpResponderFailing) while Ready stays True and the
#                   message names the lease; recovers when the kills stop
#   before-lease    responder stopped (hung) from the VM's start: the guest
#                   has no address and NetworkReady is False; once the stops
#                   end the guest gets its address and NetworkReady is True
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
# The agent case adds a node affinity to the node agent DaemonSet and
# removes it again; refuse to start if the DaemonSet already has one.
# Changing the pod template rolls the node agents on every node.
if [ -n "$(kubectl "${ctx[@]}" -n baryon-system get daemonset baryon-node-agent -o jsonpath='{.spec.template.spec.affinity}')" ]; then
  echo "the node agent DaemonSet already has an affinity; not changing it" >&2
  exit 2
fi

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
# kill_responders <node> <pod-uid> [signal]: send the signal (default KILL)
# to the VM's responders: processes whose command line matches, whose
# parent is baryon-virtd, and whose uid is not root, checked right before
# each signal.
# signal_cmd <pod-uid> <signal>: host shell command that sends the signal to
# the VM's responders: processes whose command line matches, whose parent
# is baryon-virtd, and whose uid is not root, checked right before each
# signal.
signal_cmd() {
  local sig="$2"
  set -- "" "$1"
  echo "m=\$(systemctl show -p MainPID --value baryon-virtd); [ \"\$m\" -gt 0 ] && for c in /proc/[0-9]*/cmdline; do l=\$(tr '\\0' ' ' < \$c 2>/dev/null); case \"\$l\" in baryon-dhcp\\ dhcp-responder*--pod-uid\\ $2\\ *|baryon-dhcp\\ dhcp-responder*--pod-uid\\ $2) d=\${c%/cmdline}; [ \"\$(awk '/^PPid:/{print \$2}' \$d/status)\" = \"\$m\" ] && [ \"\$(stat -c %u \$d)\" != 0 ] && kill -$sig \${d#/proc/};; esac; done"
}
kill_responders() {
  [ -n "$1" ] && [ -n "$2" ] || return 1
  host "$1" "$(signal_cmd "$2" "${3:-KILL}")" >/dev/null 2>&1
  return 0
}
# hold_responders <node> <pod-uid> <signal> <seconds>: signal the VM's
# responders every 0.2 s for that long, from a loop on the host, so a new
# responder runs only briefly. Runs in the foreground.
hold_responders() {
  host "$1" "end=\$(( \$(date +%s) + $4 )); while [ \$(date +%s) -lt \$end ]; do $(signal_cmd "$2" "$3"); sleep 0.2; done" >/dev/null 2>&1
  return 0
}
# start_killer <node> <pod-uid> [signal]: signal the VM's responders every
# second in the background; prints the killer's PID.
start_killer() {
  ( while :; do kill_responders "$1" "$2" "${3:-KILL}"; sleep 1; done ) >/dev/null 2>&1 &
  echo $!
}
# hangs_since <node> <pod-uid> <unix-seconds>: hang restarts baryon-virtd
# logged for the VM since then.
hangs_since() {
  host "$1" "journalctl -u baryon-virtd --since @$3 -o cat --no-pager | grep 'stopped answering liveness probes' | grep -c '$2'" 2>/dev/null || echo 0
}
gone_pid() { ! host "$1" "test -d /proc/$2"; }
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

log "case hang: responder stopped once after the guest's lease"
before="$(responder_pids "$n" "$u")"
t0="$(date +%s)"
kill_responders "$n" "$u" STOP
hung_restart() { [ "$(hangs_since "$n" "$u" "$t0")" -ge 1 ]; }
wait_for "hang: baryon-virtd logged the hang" hung_restart
wait_for "hang: the stopped responder is gone" gone_pid "$n" "$before"
wait_for "hang: a new responder runs" restarted
wait_for "hang: NetworkReady True" net_is "nh-a-$run" True AddressServiceRunning
label="hang: Ready stayed True"; check ready "nh-a-$run"

log "case hang-repeated: every new responder stopped"
t0="$(date +%s)"
killer="$(start_killer "$n" "$u" STOP)"; killers+=("$killer")
hung_msg() { net_is "nh-a-$run" False && grep -q "stopped answering liveness probes" <<<"$(cond "nh-a-$run" NetworkReady message)"; }
TIMEOUT_SAVED="$timeout"; timeout=300
wait_for "hang-repeated: NetworkReady False naming the probes" hung_msg
three_hangs() { [ "$(hangs_since "$n" "$u" "$t0")" -ge 3 ]; }
wait_for "hang-repeated: at least three hang restarts" three_hangs
timeout="$TIMEOUT_SAVED"
label="hang-repeated: Ready stays True"; check ready "nh-a-$run"
kill "$killer"
kill_responders "$n" "$u" KILL
timeout=180
wait_for "hang-repeated: NetworkReady True once the stops end" net_is "nh-a-$run" True AddressServiceRunning
timeout="$TIMEOUT_SAVED"

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
agent_patched=0
restore_agent() {
  [ "$agent_patched" = 1 ] || return 0
  kubectl "${ctx[@]}" "${ds[@]}" patch daemonset baryon-node-agent --type json -p '[{"op":"remove","path":"/spec/template/spec/affinity"}]' >/dev/null 2>&1 && agent_patched=0
}
agent_patched=1
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

log "case before-lease: responder hung from the VM's start"
# Every new responder is stopped from a loop on the host every 0.2 s for
# 150 s, so the guest has little chance of a lease while the responders
# hang; a lease obtained in one of those short windows is logged, not
# counted as a failure.
vm "nh-b-$run"
scheduled() { [ -n "$(node_of "nh-b-$run")" ] && [ -n "$(anchor_uid "nh-b-$run")" ]; }
wait_for "before-lease: Anchor Pod scheduled" scheduled
nb="$(node_of "nh-b-$run")"; ub="$(anchor_uid "nh-b-$run")"
hold_responders "$nb" "$ub" STOP 150 &
holder=$!; killers+=("$holder")
timeout=150
wait_for "before-lease: NetworkReady False while every responder hangs" net_is "nh-b-$run" False
timeout="$TIMEOUT_SAVED"
msg="$(cond "nh-b-$run" NetworkReady message)"
log "message: $msg"
if grep -q "has not acquired an address" <<<"$msg"; then
  log "before-lease: the guest has no address"
else
  log "before-lease: the guest obtained a lease in a short responder window"
fi
wait "$holder" 2>/dev/null || true
kill_responders "$nb" "$ub" KILL
timeout=600
wait_for "before-lease: NetworkReady True once the hangs end" net_is "nh-b-$run" True AddressServiceRunning
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
