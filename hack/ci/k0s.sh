#!/usr/bin/env bash
# Single-node k0s in a container, used by CI for API server checks (schema,
# CEL rules, admission policies). It is not a Baryon host: no KVM, no
# baryon-virtd. Baryon integration tests run on Hadron nodes (docs/testing.md).
#
# Usage: hack/ci/k0s.sh start | stop
set -euo pipefail
name="${K0S_CONTAINER:-baryon-ci-k0s}"
image="docker.io/k0sproject/k0s:${K0S_VERSION:-v1.36.4-k0s.1}"

case "${1:-}" in
  start)
    docker run -d --name "$name" --hostname k0s --privileged --cgroupns=host \
      -v /sys/fs/cgroup:/sys/fs/cgroup:rw -v /var/lib/k0s \
      "$image" k0s controller --enable-worker --no-taints >/dev/null
    for _ in $(seq 1 100); do
      docker exec "$name" k0s kubectl get --raw /readyz >/dev/null 2>&1 && exit 0
      sleep 3
    done
    echo "k0s API server did not become ready" >&2
    docker logs --tail 50 "$name" >&2
    exit 1
    ;;
  stop) docker rm -f "$name" >/dev/null ;;
  *) echo "usage: $0 start | stop" >&2; exit 2 ;;
esac
