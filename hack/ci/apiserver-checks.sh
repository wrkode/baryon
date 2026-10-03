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
cat deploy/base/namespace.yaml deploy/base/policy/*.yaml | k apply -f -
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
k run plain --image=busybox:1.36 --dry-run=server -o name
echo "all API server checks passed"
