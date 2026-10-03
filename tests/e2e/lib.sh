# Shared helpers for the E2E scripts in this directory. Source it after
# `set -euo pipefail`.
#
# Environment:
#   KUBE_CONTEXT   kubectl context (default: current)
#   NAMESPACE      namespace for the test VMs (default: default)
#   IMAGE_URL      raw disk image URL (default: Debian 13 genericcloud)
#   IMAGE_SHA256   expected SHA-256 of the image (required for plain http URLs)
#   NODE_SHELL     command that runs a shell command as root on a Baryon
#                  node: invoked as `$NODE_SHELL <node-name> sh -c <command>`,
#                  where <node-name> is the Kubernetes node name (for example
#                  a wrapper around ssh). Host checks are skipped when unset.
#   TIMEOUT        seconds to wait for each transition (default: 300)

ctx=()
[ -n "${KUBE_CONTEXT:-}" ] && ctx=(--context "$KUBE_CONTEXT")
ns="${NAMESPACE:-default}"
k=(kubectl "${ctx[@]}" -n "$ns")
url="${IMAGE_URL:-https://cloud.debian.org/images/cloud/trixie/latest/debian-13-genericcloud-amd64.raw}"
timeout="${TIMEOUT:-300}"
pass=0
fail=0

case "$url" in
  http://*)
    [ -n "${IMAGE_SHA256:-}" ] || { echo "IMAGE_SHA256 is required for a plain http IMAGE_URL" >&2; exit 2; }
    ;;
esac

log() { printf '%s %s\n' "$(date -u +%T)" "$*"; }
ok() { log "PASS $*"; pass=$((pass + 1)); }
bad() { log "FAIL $*"; fail=$((fail + 1)); }

# wait_for <description> <command...>: retry until the command succeeds.
wait_for() {
  local what="$1"; shift
  local start=$SECONDS
  until "$@" >/dev/null 2>&1; do
    if (( SECONDS - start > timeout )); then
      bad "$what (timed out after ${timeout}s)"
      return 1
    fi
    sleep 2
  done
  ok "$what ($(( SECONDS - start ))s)"
}

# host <node> <command>: run a shell command on a Baryon node.
host() {
  [ -n "${NODE_SHELL:-}" ] || return 0
  [ -n "$1" ] || { echo "host: no node given" >&2; return 1; }
  # shellcheck disable=SC2086
  $NODE_SHELL "$1" sh -c "$2"
}

# node_of_pod <pod>: the node an Anchor Pod is bound to (empty if none).
node_of_pod() {
  "${k[@]}" get pod "$1" -o jsonpath='{.spec.nodeName}' 2>/dev/null
}

# wait_for, but only when host checks are possible (NODE_SHELL set).
host_wait() {
  if [ -n "${NODE_SHELL:-}" ]; then
    wait_for "$@"
  else
    log "SKIP $1 (NODE_SHELL unset)"
  fi
}

# no_vmm_for <node> <pod-uid>
no_vmm_for() {
  ! host "$1" "test -e /run/baryon/vms/$2"
}

# apply_vm <name> [class] [runPolicy]: create a VirtualMachine importing
# IMAGE_URL.
apply_vm() {
  local name="$1" class="${2:-general-small}" policy="${3:-Running}" sha_line=""
  [ -n "${IMAGE_SHA256:-}" ] && sha_line="        sha256: ${IMAGE_SHA256}"
  "${k[@]}" apply -f - >/dev/null <<EOF
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata:
  name: $name
spec:
  className: $class
  runPolicy: $policy
  rootDisk:
    size: 4Gi
    source:
      http:
        url: $url
$sha_line
EOF
}

vm_reason() {
  "${k[@]}" get virtualmachine "$1" -o jsonpath='{.status.conditions[?(@.type=="Ready")].reason}' 2>/dev/null
}

# host_leftovers <label> <node>=<pod-uid>...: check that nothing of these
# Anchor Pods remains on their nodes: runtime directories, VMM and DHCP
# responder processes, and mounts (pod volumes, the VMM tmpfs). Other VMs may
# be running on the nodes; only the given pods are checked.
host_leftovers() {
  local label="$1"; shift
  if [ -z "${NODE_SHELL:-}" ]; then
    log "SKIP $label: host leftovers (NODE_SHELL unset)"
    return 0
  fi
  local pair node uid left procs vmms responders mounts
  for pair in "$@"; do
    node="${pair%%=*}"; uid="${pair#*=}"
    if [ -z "$node" ] || [ -z "$uid" ]; then
      bad "$label: missing node or pod UID in '$pair'"
      continue
    fi
    left="$(host "$node" "ls /run/baryon/vms" | grep -xF "$uid" || true)"
    if [ -n "$left" ]; then bad "$label: runtime directory left on $node: $left"; else ok "$label: no runtime directory left on $node"; fi
    # Scan /proc directly: busybox ps (Hadron) has no -o. Bracket the first
    # letter so the pattern does not match this shell's own command line.
    procs="$(host "$node" 'for c in /proc/[0-9]*/cmdline; do tr "\0" " " < "$c" 2>/dev/null; echo; done')"
    vmms="$(printf '%s\n' "$procs" | grep "[c]loud-hypervisor" | grep -cF "$uid" || true)"
    if [ "${vmms:-0}" -gt 0 ]; then bad "$label: VMM process left on $node"; else ok "$label: no VMM process left on $node"; fi
    responders="$(printf '%s\n' "$procs" | grep "^baryon-dhcp " | grep -cF "$uid" || true)"
    if [ "${responders:-0}" -gt 0 ]; then bad "$label: DHCP responder left on $node"; else ok "$label: no DHCP responder left on $node"; fi
    mounts="$(host "$node" "grep -cF '$uid' /proc/mounts || true")"
    if [ "${mounts:-0}" -gt 0 ]; then bad "$label: $mounts mounts left on $node"; else ok "$label: no mounts left on $node"; fi
  done
}

# summary: print the result and exit non-zero on failure.
summary() {
  log "result: $pass passed, $fail failed"
  [ "$fail" -eq 0 ]
}
