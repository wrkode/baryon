#!/usr/bin/env bash
# Version skew E2E test: one VM on every Baryon node, whatever mix of
# cluster release and host image versions the cluster runs (ADR 0012).
#
# Run it at each step of a rolling upgrade: the cluster release upgraded
# before or after the hosts, and hosts upgraded one by one. It reports the
# runtime protocol version and baryon-virtd build of every node, starts one
# VM per node, restarts baryon-virtd on every node (reattach across
# versions), and deletes the VMs.
#
# Requires NODE_SHELL (see lib.sh) and a StorageClass usable from every node
# (the default one, or STORAGE_CLASS). Placement is steered with cordons,
# which are removed on exit.
#
# Environment: see lib.sh, and
#   STORAGE_CLASS  StorageClass of the root disks (default: cluster default)
#   EXPECT_MIXED   1: fail unless the nodes run at least two different
#                  baryon-virtd builds
set -euo pipefail
# shellcheck source=lib.sh
. "$(dirname "$0")/lib.sh"
[ -n "${NODE_SHELL:-}" ] || { echo "skew.sh needs NODE_SHELL" >&2; exit 2; }

run="$RANDOM"
mapfile -t nodes < <(kubectl "${ctx[@]}" get nodes -l virtualization.baryon.io/host=true \
  -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}')
[ "${#nodes[@]}" -ge 1 ] || { echo "no Baryon nodes" >&2; exit 2; }
declare -A vm_on
cordoned=()

cleanup() {
  local n v
  for n in "${cordoned[@]}"; do kubectl "${ctx[@]}" uncordon "$n" >/dev/null 2>&1 || true; done
  for v in "${vm_on[@]+"${vm_on[@]}"}"; do "${k[@]}" delete virtualmachine "$v" --ignore-not-found --wait=false >/dev/null 2>&1 || true; done
  for n in "${nodes[@]}"; do host "$n" "systemctl start baryon-virtd" >/dev/null 2>&1 || true; done
}
trap cleanup EXIT

vnode() { kubectl "${ctx[@]}" get virtualizationnode "$1" -o jsonpath="$2" 2>/dev/null; }
ready() { [ "$(vm_reason "$1")" = GuestBooted ]; }
bound() { [ "$(node_of_pod "$1-anchor")" = "$2" ]; }
pod_uid_of() { "${k[@]}" get pod "$1-anchor" -o jsonpath='{.metadata.uid}' 2>/dev/null; }
restarts_of() { "${k[@]}" get pod "$1-anchor" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null; }
vnode_ready() { [ "$(vnode "$1" '{.status.conditions[?(@.type=="Ready")].status}')" = True ]; }

anchor_image="$(kubectl "${ctx[@]}" -n baryon-system get deploy baryon-controller \
  -o jsonpath='{.spec.template.spec.containers[0].env[?(@.name=="BARYON_ANCHOR_IMAGE")].value}' 2>/dev/null || true)"
log "cluster release: anchor image ${anchor_image:-<unknown>}"
builds=()
for n in "${nodes[@]}"; do
  pv="$(vnode "$n" '{.status.runtime.protocolVersion}')"
  build="$(host "$n" "sed -n 's/.*\"gitCommit\": *\"\\([^\"]*\\)\".*/\\1/p' /usr/share/baryon/components.json" | head -1)"
  builds+=("$build")
  log "node $n: protocol ${pv:-1.0 (not reported)}, baryon-virtd $(vnode "$n" '{.status.runtime.virtdVersion}') build ${build:-unknown}"
done
distinct="$(printf '%s\n' "${builds[@]}" | sort -u | grep -c . || true)"
if [ "${EXPECT_MIXED:-0}" = 1 ]; then
  if [ "$distinct" -ge 2 ]; then ok "nodes run $distinct different baryon-virtd builds"; else bad "EXPECT_MIXED=1 but all nodes run one build"; fi
fi

for n in "${nodes[@]}"; do wait_for "$n is a ready VirtualizationNode" vnode_ready "$n"; done

sha_line=""
[ -n "${IMAGE_SHA256:-}" ] && sha_line="        sha256: ${IMAGE_SHA256}"
sc_line=""
[ -n "${STORAGE_CLASS:-}" ] && sc_line="    storageClassName: $STORAGE_CLASS"
i=0
for n in "${nodes[@]}"; do
  v="e2e-skew-$run-$i"; i=$((i + 1))
  vm_on[$n]="$v"
  for o in "${nodes[@]}"; do
    [ "$o" = "$n" ] || { kubectl "${ctx[@]}" cordon "$o" >/dev/null; cordoned+=("$o"); }
  done
  "${k[@]}" apply -f - >/dev/null <<EOF
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata:
  name: $v
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
  wait_for "$v bound to $n" bound "$v" "$n"
  for o in "${nodes[@]}"; do kubectl "${ctx[@]}" uncordon "$o" >/dev/null; done
done
for n in "${nodes[@]}"; do wait_for "${vm_on[$n]} Ready on $n" ready "${vm_on[$n]}"; done

log "restarting baryon-virtd on every node"
declare -A before
for n in "${nodes[@]}"; do before[$n]="$(pod_uid_of "${vm_on[$n]}")/$(restarts_of "${vm_on[$n]}")"; done
for n in "${nodes[@]}"; do host "$n" "systemctl restart baryon-virtd"; done
for n in "${nodes[@]}"; do
  v="${vm_on[$n]}"
  wait_for "$v Ready after the baryon-virtd restart" ready "$v"
  after="$(pod_uid_of "$v")/$(restarts_of "$v")"
  if [ "$after" = "${before[$n]}" ]; then ok "$v kept its Anchor Pod and container (reattached)"; else bad "$v Anchor Pod or container changed: ${before[$n]} -> $after"; fi
done

log "deleting"
pairs=()
for n in "${nodes[@]}"; do
  pairs+=("$n=$(pod_uid_of "${vm_on[$n]}")")
  "${k[@]}" delete virtualmachine "${vm_on[$n]}" --wait=false >/dev/null
done
gone() { ! "${k[@]}" get virtualmachine "$1" >/dev/null 2>&1; }
for n in "${nodes[@]}"; do
  wait_for "${vm_on[$n]} removed" gone "${vm_on[$n]}"
done
host_leftovers "after delete" "${pairs[@]}"

cleanup
trap - EXIT
summary
