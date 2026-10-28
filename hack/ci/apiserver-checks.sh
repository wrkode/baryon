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
accept() {
  if ! out="$(k apply --dry-run=server -f - 2>&1)"; then echo "rejected but must be accepted: $1: $out" >&2; exit 1; fi
  echo "accepted as expected: $1"
}
vm_with_disk() {
  # vm_with_disk <name> <rootDisk as flow YAML>
  printf 'apiVersion: virtualization.baryon.io/v1alpha1\nkind: VirtualMachine\nmetadata: {name: %s, namespace: default}\nspec:\n  className: general-small\n  rootDisk: %s\n' "$1" "$2"
}
for bad in "0" "100m" "512Ki" "banana" "2Pi"; do
  vm_with_disk size-bad "{size: \"$bad\", source: {http: {url: \"https://example.org/d.raw\"}}}" | reject "rootDisk.size $bad"
done
vm_with_disk size-int "{size: 4294967296, source: {http: {url: \"https://example.org/d.raw\"}}}" | accept "rootDisk.size as an integer number of bytes"
vm_with_disk size-str "{size: 4Gi, source: {http: {url: \"https://example.org/d.raw\"}}}" | accept "rootDisk.size 4Gi"
vm_with_disk claim-empty "{source: {persistentVolumeClaim: {claimName: \"\"}}}" | reject "an empty claimName"
vm_with_disk claim-bad "{source: {persistentVolumeClaim: {claimName: \"Not_A_Name\"}}}" | reject "a claimName that is not a DNS-1123 subdomain"
vm_with_disk claim-size "{size: 4Gi, source: {persistentVolumeClaim: {claimName: disk}}}" | reject "size with an existing claim"
vm_with_disk claim-sc "{storageClassName: fast, source: {persistentVolumeClaim: {claimName: disk}}}" | reject "storageClassName with an existing claim"
vm_with_disk sc-empty "{size: 4Gi, storageClassName: \"\", source: {http: {url: \"https://example.org/d.raw\"}}}" | reject "an empty storageClassName"
vm_with_disk url-creds "{size: 4Gi, source: {http: {url: \"https://user:secret@example.org/d.raw\"}}}" | reject "a URL with credentials"
vm_with_disk url-nohost "{size: 4Gi, source: {http: {url: \"https:///d.raw\"}}}" | reject "a URL without a host"
vm_with_disk url-ftp "{size: 4Gi, source: {http: {url: \"ftp://example.org/d.raw\"}}}" | reject "a URL that is not http or https"
vm_with_disk claim-ok "{source: {persistentVolumeClaim: {claimName: disk.v1}}}" | accept "an existing claim"
vm_with_disk url-at-path "{size: 4Gi, source: {http: {url: \"https://example.org/a@b.raw\"}}}" | accept "an @ in the URL path"
vm_with_disk url-at-query "{size: 4Gi, source: {http: {url: \"https://example.org/d.raw?x=a@b\"}}}" | accept "an @ in the URL query"
vm_with_disk url-ipv6 "{size: 4Gi, source: {http: {url: \"https://[2001:db8::1]/d.raw\"}}}" | accept "an IPv6 literal host"
vm_with_disk url-opaque "{size: 4Gi, source: {http: {url: \"https:user@example.org/d.raw\"}}}" | reject "an opaque URL with userinfo"
vm_with_disk url-upper "{size: 4Gi, source: {http: {url: \"HTTPS://u@example.org/d.raw\"}}}" | reject "an uppercase scheme with userinfo"
vm_with_disk null-source "{size: 4Gi, source: {http: null}}" | reject "an explicit null source"
k apply --dry-run=server -f - <<'YAML' >/dev/null 2>&1 && { echo "accepted but must be rejected: a className that is not a DNS-1123 subdomain" >&2; exit 1; } || echo "rejected as expected: a className that is not a DNS-1123 subdomain"
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachine
metadata: {name: class-bad, namespace: default}
spec:
  className: Small_Class
  rootDisk: {size: 4Gi, source: {http: {url: "https://example.org/d.raw"}}}
YAML
reject "a class with 0 vCPUs" <<'YAML'
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachineClass
metadata: {name: zero}
spec: {cpu: {vcpus: 0}, memory: {size: 1Gi}}
YAML
for bad in "32Mi" "1500Ki" "1.5Gi" "banana"; do
  printf 'apiVersion: virtualization.baryon.io/v1alpha1\nkind: VirtualMachineClass\nmetadata: {name: mem-bad}\nspec: {cpu: {vcpus: 1}, memory: {size: "%s"}}\n' "$bad" | reject "class memory.size $bad"
done
reject "a change to an existing class" <<'YAML'
apiVersion: virtualization.baryon.io/v1alpha1
kind: VirtualMachineClass
metadata: {name: general-small}
spec: {cpu: {vcpus: 3}, memory: {size: 2Gi}}
YAML
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
      securityContext:
        allowPrivilegeEscalation: false
        capabilities: {drop: [ALL]}
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

# A Baryon pod keeps its own PID namespace (ADR 0015), even when
# baryon-controller creates it.
k apply -f - < deploy/base/controller/rbac.yaml >/dev/null
as_ctl=(--as=system:serviceaccount:baryon-system:baryon-controller)
printf '%s\n' "$vmpod" | k "${as_ctl[@]}" apply --dry-run=server -f - >/dev/null \
  || { echo "baryon-controller could not create a vm resource pod" >&2; exit 1; }
for field in 'shareProcessNamespace: true' 'hostPID: true' 'hostUsers: false'; do
  out="$(printf '%s\n' "$vmpod" | sed "s/^spec:$/spec:\n  $field/" | k "${as_ctl[@]}" apply --dry-run=server -f - 2>&1 || true)"
  echo "$out" | grep -q baryon-vm-resource || { echo "vm resource pod with $field was not denied: $out" >&2; exit 1; }
  echo "vm resource pod with $field denied as expected"
done

# Exactly one container and no init containers, even from baryon-controller.
twopod="$(printf '%s\n' "$vmpod" | sed 's/^spec:$/spec:\n  initContainers:\n    - {name: extra, image: busybox:1.36}/')"
out="$(printf '%s\n' "$twopod" | k "${as_ctl[@]}" apply --dry-run=server -f - 2>&1 || true)"
echo "$out" | grep -q baryon-vm-resource || { echo "vm resource pod with an init container was not denied: $out" >&2; exit 1; }
echo "vm resource pod with an init container denied as expected"
twopod="$(printf '%s\n' "$vmpod" | sed 's/^  containers:$/  containers:\n    - {name: extra, image: busybox:1.36}/')"
out="$(printf '%s\n' "$twopod" | k "${as_ctl[@]}" apply --dry-run=server -f - 2>&1 || true)"
echo "$out" | grep -q baryon-vm-resource || { echo "vm resource pod with two containers was not denied: $out" >&2; exit 1; }
echo "vm resource pod with two containers denied as expected"

# Every container drops all capabilities and cannot escalate, even from
# baryon-controller.
for sc in 's/drop: \[ALL\]/drop: [NET_RAW]/' 's/allowPrivilegeEscalation: false/allowPrivilegeEscalation: true/' \
    's/capabilities: {drop: \[ALL\]}/capabilities: {drop: [ALL], add: [NET_ADMIN]}/' 's/allowPrivilegeEscalation: false/privileged: true/' \
    '/securityContext:/,/capabilities:/d'; do
  out="$(printf '%s\n' "$vmpod" | sed "$sc" | k "${as_ctl[@]}" apply --dry-run=server -f - 2>&1 || true)"
  echo "$out" | grep -q baryon-vm-resource || { echo "vm resource pod with security context edit '$sc' was not denied: $out" >&2; exit 1; }
  echo "vm resource pod with security context edit '$sc' denied as expected"
done

# Nobody adds ephemeral containers to a Baryon pod.
printf '%s\n' "$vmpod" | sed 's/name: vm-pod/name: ci-anchor-pod/' | k "${as_ctl[@]}" apply -f - >/dev/null
for i in $(seq 1 30); do
  out="$(k debug -n default ci-anchor-pod --image=busybox:1.36 -c "dbg$i" 2>&1 || true)"
  echo "$out" | grep -q baryon-no-ephemeral-containers && break
  sleep 2
done
echo "$out" | grep -q baryon-no-ephemeral-containers || { echo "ephemeral container on a Baryon pod was not denied: $out" >&2; exit 1; }
echo "ephemeral container on a Baryon pod denied as expected"
k run ci-plain --image=busybox:1.36 >/dev/null
k debug -n default ci-plain --image=busybox:1.36 -c dbg >/dev/null \
  || { echo "ephemeral container on an ordinary pod was denied" >&2; exit 1; }

# Nobody changes the image of a Baryon pod; ordinary pods are unaffected.
out="$(k set image -n default pod/ci-anchor-pod c=busybox:1.37 2>&1 || true)"
echo "$out" | grep -q baryon-pod-immutable || { echo "image change on a Baryon pod was not denied: $out" >&2; exit 1; }
echo "image change on a Baryon pod denied as expected"
k label -n default pod/ci-anchor-pod ci-label=1 >/dev/null \
  || { echo "a label change on a Baryon pod was denied" >&2; exit 1; }
k set image -n default pod/ci-plain ci-plain=busybox:1.37 >/dev/null \
  || { echo "image change on an ordinary pod was denied" >&2; exit 1; }
k delete pod ci-anchor-pod ci-plain -n default --wait=false >/dev/null

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
