#!/usr/bin/env bash
# API server checks: CRD schema and CEL rules, and the admission policies.
# Run by CI against hack/ci/k0s.sh; works against any cluster through
# KUBECTL.
set -euo pipefail
# KUBECTL: the kubectl command to use (default: the CI k0s container).
read -r -a kc <<< "${KUBECTL:-docker exec -i ${K0S_CONTAINER:-baryon-ci-k0s} k0s kubectl}"
k() { "${kc[@]}" "$@"; }
k apply --server-side -f - < deploy/base/crds/virtualization.baryon.io.yaml
k wait --for condition=established --timeout=60s crd --all
k apply -f - < examples/classes.yaml
k apply --dry-run=server -f - < examples/debian-vm.yaml
reject() {
  if k apply --dry-run=server -f - >/dev/null 2>&1; then echo "accepted but must be rejected: $1" >&2; exit 1; fi
  echo "rejected as expected: $1"
}
reject "two disk sources" <<'YAML'
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata: {name: invalid, namespace: default}
spec:
  className: general-small
  rootDisk:
    source:
      http: {url: "https://example.org/d.raw"}
      persistentVolumeClaim: {claimName: x}
YAML
reject "plain http without sha256" <<'YAML'
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata: {name: nodigest, namespace: default}
spec:
  className: general-small
  rootDisk:
    size: 4Gi
    source:
      http: {url: "http://example.org/d.raw"}
YAML
reject "a toleration of every taint" <<'YAML'
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata: {name: tolerate-all, namespace: default}
spec:
  className: general-small
  rootDisk: {size: 4Gi, source: {http: {url: "https://example.org/d.raw"}}}
  placement:
    tolerations: [{operator: Exists}]
YAML
reject "a toleration of cordon" <<'YAML'
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata: {name: tolerate-cordon, namespace: default}
spec:
  className: general-small
  rootDisk: {size: 4Gi, source: {http: {url: "https://example.org/d.raw"}}}
  placement:
    tolerations: [{key: node.kubernetes.io/unschedulable, operator: Exists, effect: NoSchedule}]
YAML
reject "a toleration of control-plane nodes" <<'YAML'
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata: {name: tolerate-cp, namespace: default}
spec:
  className: general-small
  rootDisk: {size: 4Gi, source: {http: {url: "https://example.org/d.raw"}}}
  placement:
    tolerations: [{key: node-role.kubernetes.io/control-plane, operator: Exists}]
YAML
reject "a toleration of an out-of-service node" <<'YAML'
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata: {name: tolerate-oos, namespace: default}
spec:
  className: general-small
  rootDisk: {size: 4Gi, source: {http: {url: "https://example.org/d.raw"}}}
  placement:
    tolerations: [{key: node.kubernetes.io/out-of-service, operator: Exists, effect: NoExecute}]
YAML
reject "a system PriorityClass" <<'YAML'
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata: {name: system-priority, namespace: default}
spec:
  className: general-small
  rootDisk: {size: 4Gi, source: {http: {url: "https://example.org/d.raw"}}}
  placement:
    priorityClassName: system-node-critical
YAML
reject "cross-namespace pod anti-affinity" <<'YAML'
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata: {name: cross-ns, namespace: default}
spec:
  className: general-small
  rootDisk: {size: 4Gi, source: {http: {url: "https://example.org/d.raw"}}}
  placement:
    affinity:
      podAntiAffinity:
        requiredDuringSchedulingIgnoredDuringExecution:
          - {topologyKey: topology.kubernetes.io/zone, labelSelector: {matchLabels: {team: other}}, namespaceSelector: {}}
YAML
reject "cross-namespace preferred pod affinity" <<'YAML'
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata: {name: cross-ns-pref, namespace: default}
spec:
  className: general-small
  rootDisk: {size: 4Gi, source: {http: {url: "https://example.org/d.raw"}}}
  placement:
    affinity:
      podAffinity:
        preferredDuringSchedulingIgnoredDuringExecution:
          - {weight: 10, podAffinityTerm: {topologyKey: kubernetes.io/hostname, labelSelector: {matchLabels: {a: b}}, namespaces: [other]}}
YAML
k apply --dry-run=server -f - >/dev/null <<'YAML'
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata: {name: placed, namespace: default}
spec:
  className: general-small
  rootDisk: {size: 4Gi, source: {http: {url: "https://example.org/d.raw"}}}
  placement:
    nodeSelector: {disk: ssd}
    tolerations: [{key: dedicated, operator: Equal, value: vms, effect: NoSchedule}]
    topologySpreadConstraints:
      - {maxSkew: 1, topologyKey: kubernetes.io/hostname, whenUnsatisfiable: DoNotSchedule, labelSelector: {matchLabels: {app: web}}}
    affinity:
      nodeAffinity:
        requiredDuringSchedulingIgnoredDuringExecution:
          nodeSelectorTerms: [{matchExpressions: [{key: zone, operator: In, values: [a]}]}]
      podAntiAffinity:
        requiredDuringSchedulingIgnoredDuringExecution:
          - {topologyKey: kubernetes.io/hostname, labelSelector: {matchLabels: {app: web}}}
    priorityClassName: vm-high
YAML
echo "accepted as expected: placement with a selector, a toleration, a spread constraint, node affinity, same-namespace pod anti-affinity, and a PriorityClass"
reject "dotted name" <<'YAML'
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata: {name: web.prod, namespace: default}
spec:
  className: general-small
  rootDisk:
    size: 4Gi
    source:
      http: {url: "https://example.org/d.raw"}
YAML
k apply -f - >/dev/null <<'YAML'
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata: {name: immutable-disk, namespace: default}
spec:
  className: general-small
  runPolicy: Stopped
  rootDisk:
    size: 4Gi
    source:
      http: {url: "https://example.org/a.raw"}
YAML
if k patch virtualmachine immutable-disk -n default --type merge \
  -p '{"spec":{"rootDisk":{"source":{"http":{"url":"https://example.org/b.raw"}}}}}' >/dev/null 2>&1; then
  echo "accepted but must be rejected: rootDisk change" >&2; exit 1
fi
echo "rejected as expected: rootDisk change"
k patch virtualmachine immutable-disk -n default --type merge -p '{"spec":{"runPolicy":"Running"}}' >/dev/null
echo "accepted as expected: runPolicy change"
k delete virtualmachine immutable-disk -n default --wait=false >/dev/null
k apply -f - < deploy/base/namespace.yaml
for f in deploy/base/policy/*.yaml; do k apply -f - < "$f"; done
vmpod='apiVersion: v1
kind: Pod
metadata: {name: vm-pod, namespace: default}
spec:
  containers:
    - name: c
      image: busybox:1.36
      resources:
        limits: {virtualization.baryon.io/vm: "1"}'
for i in $(seq 1 30); do
  if out="$(printf '%s\n' "$vmpod" | k apply --dry-run=server -f - 2>&1)"; then sleep 2; continue; fi
  echo "$out" | grep -q baryon-vm-resource && break
  sleep 2
done
echo "$out" | grep -q baryon-vm-resource || { echo "pod requesting the vm resource was not denied" >&2; exit 1; }
echo "vm resource pod denied as expected"
importpod="$(printf '%s\n' "$vmpod" | sed 's/name: vm-pod/name: import-pod/; s#virtualization.baryon.io/vm#virtualization.baryon.io/import#')"
out="$(printf '%s\n' "$importpod" | k apply --dry-run=server -f - 2>&1 || true)"
echo "$out" | grep -q baryon-vm-resource || { echo "pod requesting the import resource was not denied: $out" >&2; exit 1; }
echo "import resource pod denied as expected"
initpod='apiVersion: v1
kind: Pod
metadata: {name: init-pod, namespace: default}
spec:
  initContainers:
    - name: i
      image: busybox:1.36
      resources:
        limits: {virtualization.baryon.io/vm: "1"}
  containers:
    - name: c
      image: busybox:1.36'
out="$(printf '%s\n' "$initpod" | k apply --dry-run=server -f - 2>&1 || true)"
echo "$out" | grep -q baryon-vm-resource || { echo "init container requesting the vm resource was not denied: $out" >&2; exit 1; }
echo "init container vm resource pod denied as expected"
k run plain --image=busybox:1.36 --dry-run=server -o name

# baryon-controller may only add the import record to a PersistentVolume and
# the import approval to a Baryon pod (policy/controller-writes.yaml).
k apply -f - < deploy/base/controller/rbac.yaml >/dev/null
k apply -f - < deploy/base/policy/controller-writes.yaml >/dev/null
as_ctl=(--as=system:serviceaccount:baryon-system:baryon-controller)
k apply -f - >/dev/null <<'YAML'
apiVersion: v1
kind: PersistentVolume
metadata: {name: ci-import-pv}
spec:
  capacity: {storage: 1Gi}
  accessModes: [ReadWriteOnce]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: ci-none
  hostPath: {path: /tmp/ci-import-pv}
  claimRef: {namespace: default, name: ci-claim}
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: ci-claim, namespace: default}
spec:
  storageClassName: ci-none
  accessModes: [ReadWriteOnce]
  volumeName: ci-import-pv
  resources: {requests: {storage: 1Gi}}
---
apiVersion: v1
kind: Pod
metadata:
  name: ci-managed
  namespace: default
  labels: {app.kubernetes.io/managed-by: baryon}
spec:
  containers: [{name: c, image: busybox:1.36}]
---
apiVersion: v1
kind: Pod
metadata: {name: ci-unmanaged, namespace: default}
spec:
  containers: [{name: c, image: busybox:1.36}]
YAML
denied() {
  local what="$1"; shift
  if out="$(k "$@" 2>&1)"; then echo "allowed but must be denied: $what" >&2; exit 1; fi
  echo "$out" | grep -q baryon-controller-writes || { echo "denied for another reason: $what: $out" >&2; exit 1; }
  echo "denied as expected: $what"
}
# The policy takes effect after a short delay.
for i in $(seq 1 30); do
  out="$(k patch pv ci-import-pv "${as_ctl[@]}" --type merge -p '{"spec":{"persistentVolumeReclaimPolicy":"Delete"}}' 2>&1 || true)"
  echo "$out" | grep -q baryon-controller-writes && break
  sleep 2
done
denied "controller changes a PV spec" patch pv ci-import-pv "${as_ctl[@]}" --type merge -p '{"spec":{"persistentVolumeReclaimPolicy":"Delete"}}'
denied "controller adds another PV annotation" annotate pv ci-import-pv "${as_ctl[@]}" other.example/x=1
bound() { [ "$(k get pv ci-import-pv -o jsonpath='{.status.phase}')" = Bound ]; }
for i in $(seq 1 30); do bound && break; sleep 2; done
bound || { echo "the CI claim did not bind" >&2; exit 1; }
claimuid="$(k get pvc ci-claim -n default -o jsonpath='{.metadata.uid}')"
vmuid=aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee
digest="sha256:$(printf '%064d' 0)"
denied "controller records an import for another claim" annotate pv ci-import-pv "${as_ctl[@]}" "virtualization.baryon.io/disk-import={\"vmUID\":\"$vmuid\",\"claimUID\":\"99999999-2222-4333-8444-555555555555\",\"source\":\"$digest\"}"
k annotate pv ci-import-pv "${as_ctl[@]}" "virtualization.baryon.io/disk-import={\"vmUID\":\"$vmuid\",\"claimUID\":\"$claimuid\",\"source\":\"$digest\"}" >/dev/null
echo "allowed as expected: controller adds the import record for the bound claim"
denied "controller rewrites the import record" annotate pv ci-import-pv "${as_ctl[@]}" --overwrite "virtualization.baryon.io/disk-import={\"vmUID\":\"$vmuid\",\"claimUID\":\"$claimuid\",\"source\":\"sha256:$(printf '%064d' 1)\"}"
denied "controller removes the import record" annotate pv ci-import-pv "${as_ctl[@]}" virtualization.baryon.io/disk-import-
k annotate pod ci-managed -n default "${as_ctl[@]}" virtualization.baryon.io/import-approved=claim-uid >/dev/null
echo "allowed as expected: controller approves an import on a Baryon pod"
k label pod ci-managed -n default "${as_ctl[@]}" app=x app.kubernetes.io/name=x >/dev/null
k annotate pod ci-managed -n default "${as_ctl[@]}" virtualization.baryon.io/vm-label-keys=app,app.kubernetes.io/name >/dev/null
echo "allowed as expected: controller copies VM labels (also app.kubernetes.io/name) and records their keys on a Baryon pod"
denied "controller adds a node.kubernetes.io label to a Baryon pod" label pod ci-managed -n default "${as_ctl[@]}" node.kubernetes.io/x=y
denied "controller changes a reserved label of a Baryon pod" label pod ci-managed -n default "${as_ctl[@]}" --overwrite app.kubernetes.io/managed-by=other
denied "controller adds a reserved label to a Baryon pod" label pod ci-managed -n default "${as_ctl[@]}" virtualization.baryon.io/vm-name=x
denied "controller relabels a pod it does not manage" label pod ci-unmanaged -n default "${as_ctl[@]}" app=x
denied "controller annotates a pod it does not manage" annotate pod ci-unmanaged -n default "${as_ctl[@]}" virtualization.baryon.io/import-approved=x
# Nobody but baryon-controller may set or change the import approval.
denied_by() {
  local policy="$1" what="$2"; shift 2
  if out="$(k "$@" 2>&1)"; then echo "allowed but must be denied: $what" >&2; exit 1; fi
  echo "$out" | grep -q "$policy" || { echo "denied for another reason: $what: $out" >&2; exit 1; }
  echo "denied as expected: $what"
}
denied_by baryon-import-approval "another user changes the import approval" annotate pod ci-managed -n default --overwrite virtualization.baryon.io/import-approved=forged
denied_by baryon-import-approval "another user removes the import approval" annotate pod ci-managed -n default virtualization.baryon.io/import-approved-
denied_by baryon-import-approval "another user approves an import" annotate pod ci-unmanaged -n default virtualization.baryon.io/import-approved=forged
k annotate pod ci-managed -n default --overwrite other.example/x=1 >/dev/null
echo "allowed as expected: another user changes other annotations of a pod with an approval"
k delete pod ci-managed ci-unmanaged -n default --wait=false >/dev/null
k delete pvc ci-claim -n default --wait=false >/dev/null
k delete pv ci-import-pv --wait=false >/dev/null
echo "all API server checks passed"
