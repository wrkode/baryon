#!/usr/bin/env bash
# Cloud Hypervisor guest policy E2E test: the options that Baryon writes
# explicitly because they affect security, compatibility, or performance
# reach the running VMM and the guest, and the guest network is not slowed
# down by a lost offload.
#
# Checks:
# - With NODE_SHELL: the VMM of the VM is the pinned Cloud Hypervisor
#   release (EXPECT_CH_VERSION) and runs with `nested=off` and the
#   virtio-net offloads `offload_csum=on` and `offload_tso=on`.
# - With GUEST_SHELL: the guest sees no `vmx` or `svm` CPU flag, and its
#   virtio-net device negotiated checksum offload and TSO in both
#   directions (feature bits CSUM, GUEST_CSUM, GUEST_TSO4, GUEST_TSO6,
#   HOST_TSO4, HOST_TSO6).
# - With GUEST_SHELL and MIN_MBPS: iperf3 from a pod on the VM's node to the
#   guest reaches at least MIN_MBPS (best of three 10 s runs). This is a
#   broad floor to catch a lost offload or a similar large regression, not
#   a benchmark: choose MIN_MBPS well below a known-good baseline of the
#   same environment. The guest image must contain iperf3.
#
# Environment, in addition to lib.sh:
#   GUEST_SHELL        command that runs a shell command in the guest:
#                      invoked as `$GUEST_SHELL <guest-address> sh -c <cmd>`
#                      (for example a wrapper around ssh from a pod). Guest
#                      checks are skipped when unset; the default Debian
#                      cloud image has no login, so IMAGE_URL must name an
#                      image the wrapper can log in to.
#   MIN_MBPS           throughput floor in Mbit/s; the check is skipped when
#                      unset
#   IPERF_IMAGE        client image with iperf3 (default: alpine:3.22, which
#                      installs iperf3 with apk and needs a package mirror)
#   EXPECT_CH_VERSION  expected Cloud Hypervisor version (default: the pin
#                      in packaging/versions.env)
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=tests/e2e/lib.sh
. "$here/lib.sh"

vm=e2e-guest-policy
iperf_pod=e2e-guest-policy-iperf
want_ch="${EXPECT_CH_VERSION:-$(sed -n 's/^CLOUD_HYPERVISOR_VERSION=//p' "$here/../../packaging/versions.env")}"

cleanup() {
  "${k[@]}" delete pod "$iperf_pod" --ignore-not-found --wait=false >/dev/null 2>&1 || true
  "${k[@]}" delete virtualmachine "$vm" --ignore-not-found --wait=true --timeout=300s >/dev/null 2>&1 || true
}
trap cleanup EXIT

guest() {
  # shellcheck disable=SC2086
  $GUEST_SHELL "$addr" sh -c "$1"
}

vm_ready() { [ "$(vm_reason "$vm")" = GuestBooted ]; }

log "VM $vm"
cleanup
apply_vm "$vm"
wait_for "VM is Ready" vm_ready
anchor="$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.anchorPod}')"
anchor_uid="$("${k[@]}" get pod "$anchor" -o jsonpath='{.metadata.uid}')"
node="$(node_of_pod "$anchor")"
addr="$("${k[@]}" get virtualmachine "$vm" -o jsonpath='{.status.addresses[0].address}')"
log "anchor $anchor on $node, guest address $addr"

# Host: the VMM's binary and command line.
if [ -n "${NODE_SHELL:-}" ]; then
  pid="$(host "$node" "for d in /proc/[0-9]*; do case \"\$(tr '\\0' ' ' < \$d/cmdline 2>/dev/null)\" in *cloud-hypervisor*$anchor_uid*) echo \${d#/proc/};; esac; done | head -1")"
  if [ -z "$pid" ]; then
    bad "host: no VMM process found for the Anchor Pod"
  else
    cmdline="$(host "$node" "tr '\\0' ' ' < /proc/$pid/cmdline")"
    version="$(host "$node" "/proc/$pid/exe --version")"
    case "$version" in
      *"$want_ch"*) ok "host: the VMM is Cloud Hypervisor $want_ch" ;;
      *) bad "host: the VMM reports '$version', expected $want_ch" ;;
    esac
    for opt in nested=off offload_csum=on offload_tso=on; do
      case "$cmdline" in
        *"$opt"*) ok "host: the VMM runs with $opt" ;;
        *) bad "host: the VMM command line lacks $opt" ;;
      esac
    done
  fi
else
  log "SKIP host: VMM version and options (NODE_SHELL unset)"
fi

# Guest: CPU flags and negotiated virtio-net features.
if [ -n "${GUEST_SHELL:-}" ]; then
  wait_for "guest shell" guest true
  flags="$(guest "grep -c -w -E 'vmx|svm' /proc/cpuinfo || true")"
  if [ "${flags:-1}" = 0 ]; then
    ok "guest: no vmx or svm CPU flag (nested virtualization off)"
  else
    bad "guest: $flags CPU entries with vmx or svm"
  fi
  # The negotiated feature bits of the first virtio device of type 1
  # (network), as a string of 0 and 1, bit 0 first.
  # shellcheck disable=SC2016 # expanded in the guest
  features="$(guest 'for d in /sys/bus/virtio/devices/*; do [ "$(cat $d/device)" = 0x0001 ] && { cat $d/features; break; }; done')"
  if [ -z "$features" ]; then
    bad "guest: no virtio-net device found"
  else
    for bit in CSUM:0 GUEST_CSUM:1 GUEST_TSO4:7 GUEST_TSO6:8 HOST_TSO4:11 HOST_TSO6:12; do
      if [ "${features:${bit#*:}:1}" = 1 ]; then
        ok "guest: virtio-net negotiated ${bit%%:*}"
      else
        bad "guest: virtio-net did not negotiate ${bit%%:*}"
      fi
    done
  fi
else
  log "SKIP guest: CPU flags and virtio-net features (GUEST_SHELL unset)"
fi

# Throughput floor from a pod on the same node.
if [ -n "${GUEST_SHELL:-}" ] && [ -n "${MIN_MBPS:-}" ]; then
  guest 'pkill iperf3 || true; iperf3 -s -D' >/dev/null
  "${k[@]}" run "$iperf_pod" --image="${IPERF_IMAGE:-alpine:3.22}" --restart=Never \
    --overrides="{\"spec\":{\"nodeName\":\"$node\"}}" -- \
    sh -c 'command -v iperf3 >/dev/null || apk add -q --no-cache iperf3 >/dev/null; sleep 3600' >/dev/null
  wait_for "iperf3 client pod" "${k[@]}" exec "$iperf_pod" -- sh -c 'command -v iperf3'
  best=0
  for _ in 1 2 3; do
    r="$("${k[@]}" exec "$iperf_pod" -- iperf3 -c "$addr" -t 10 -O 2 -f m 2>/dev/null |
      awk '/receiver$/ { print int($(NF-2)) }')"
    [ "${r:-0}" -gt "$best" ] && best="$r"
  done
  if [ "$best" -ge "$MIN_MBPS" ]; then
    ok "throughput: pod to guest at least $MIN_MBPS Mbit/s"
  else
    bad "throughput: pod to guest $best Mbit/s, below the floor of $MIN_MBPS Mbit/s"
  fi
  log "throughput: best of three runs $best Mbit/s"
else
  log "SKIP throughput floor (GUEST_SHELL or MIN_MBPS unset)"
fi

summary
