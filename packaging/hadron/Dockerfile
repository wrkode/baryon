# Baryon-enabled Hadron/Kairos system image.
#
# Stages:
#   artifacts   pinned Cloud Hypervisor, CLOUDHV.fd, virtiofsd, verified by SHA-256
#   virtd       static musl build of baryon-virtd from this repository
#   host-layer  FROM scratch: every file Baryon adds to the host, under /usr only
#   image       Hadron base + host layer + kairos-init (run last, as upstream
#               Hadron and Kairos recommend)
#
# Build with hack/build-hadron-image.sh, which passes the pinned versions from
# packaging/versions.env. The host layer is usable on its own:
#   FROM <hadron or provider image>
#   COPY --from=<host-layer image> / /
#
# Build context: the repository root.

ARG HADRON_IMAGE
ARG KAIROS_INIT_IMAGE
ARG RUST_BUILDER_IMAGE=rust:1.99-alpine

FROM ${KAIROS_INIT_IMAGE} AS kairos-init

FROM alpine:3.22 AS artifacts
ARG CLOUD_HYPERVISOR_VERSION
ARG CLOUD_HYPERVISOR_URL
ARG CLOUD_HYPERVISOR_SHA256
ARG CLOUDHV_FIRMWARE_RELEASE
ARG CLOUDHV_FIRMWARE_URL
ARG CLOUDHV_FIRMWARE_SHA256
ARG VIRTIOFSD_VERSION
ARG VIRTIOFSD_ZIP_URL
ARG VIRTIOFSD_ZIP_SHA256
ARG VIRTIOFSD_SHA256
RUN apk add --no-cache curl unzip
WORKDIR /out
RUN set -eu; \
    curl -fsSL -o cloud-hypervisor "${CLOUD_HYPERVISOR_URL}"; \
    echo "${CLOUD_HYPERVISOR_SHA256}  cloud-hypervisor" | sha256sum -c -; \
    curl -fsSL -o CLOUDHV.fd "${CLOUDHV_FIRMWARE_URL}"; \
    echo "${CLOUDHV_FIRMWARE_SHA256}  CLOUDHV.fd" | sha256sum -c -; \
    curl -fsSL -o virtiofsd.zip "${VIRTIOFSD_ZIP_URL}"; \
    echo "${VIRTIOFSD_ZIP_SHA256}  virtiofsd.zip" | sha256sum -c -; \
    unzip -q -j virtiofsd.zip '*/x86_64-unknown-linux-musl/release/virtiofsd' -d .; \
    echo "${VIRTIOFSD_SHA256}  virtiofsd" | sha256sum -c -; \
    rm virtiofsd.zip; \
    chmod 0755 cloud-hypervisor virtiofsd; chmod 0644 CLOUDHV.fd

FROM ${RUST_BUILDER_IMAGE} AS virtd
ARG BARYON_GIT_COMMIT=unknown
ENV BARYON_GIT_COMMIT=${BARYON_GIT_COMMIT}
# Use the image's toolchain; rust-toolchain.toml would ask rustup to install
# extra components.
ENV RUSTUP_TOOLCHAIN=1.99.0
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml clippy.toml ./
COPY crates/ crates/
# COPY keeps the sources' modification times, and cargo decides by them
# whether an artifact in the shared target cache is current. A cache last
# filled from other sources (another checkout or commit) with newer times
# would be reused as is. Touch the workspace sources so that its crates are
# always rebuilt; dependencies stay cached.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    find crates -type f -exec touch {} + \
    && cargo build --locked --release --target x86_64-unknown-linux-musl -p baryon-virtd \
    && install -D -m 0755 target/x86_64-unknown-linux-musl/release/baryon-virtd /out/baryon-virtd

FROM alpine:3.22 AS manifest
ARG BARYON_VERSION
ARG BARYON_GIT_COMMIT=unknown
ARG HADRON_IMAGE
ARG KAIROS_INIT_IMAGE
ARG KUBERNETES_DISTRO
ARG KUBERNETES_VERSION
ARG CLOUD_HYPERVISOR_VERSION
ARG CLOUD_HYPERVISOR_SHA256
ARG CLOUDHV_FIRMWARE_RELEASE
ARG CLOUDHV_FIRMWARE_SHA256
ARG VIRTIOFSD_VERSION
RUN mkdir -p /out && printf '%s\n' \
    '{' \
    "  \"baryon\": {\"version\": \"${BARYON_VERSION}\", \"gitCommit\": \"${BARYON_GIT_COMMIT}\"}," \
    "  \"hadronImage\": \"${HADRON_IMAGE}\"," \
    "  \"kairosInitImage\": \"${KAIROS_INIT_IMAGE}\"," \
    "  \"kubernetes\": {\"distro\": \"${KUBERNETES_DISTRO}\", \"version\": \"${KUBERNETES_VERSION}\"}," \
    "  \"cloudHypervisor\": {\"version\": \"${CLOUD_HYPERVISOR_VERSION}\", \"sha256\": \"${CLOUD_HYPERVISOR_SHA256}\"}," \
    "  \"firmware\": {\"name\": \"CLOUDHV.fd\", \"release\": \"${CLOUDHV_FIRMWARE_RELEASE}\", \"sha256\": \"${CLOUDHV_FIRMWARE_SHA256}\"}," \
    "  \"virtiofsd\": {\"version\": \"${VIRTIOFSD_VERSION}\"}" \
    '}' > /out/components.json

FROM scratch AS host-layer
COPY --from=artifacts /out/cloud-hypervisor /usr/bin/cloud-hypervisor
COPY --from=artifacts /out/virtiofsd /usr/libexec/virtiofsd
COPY --from=artifacts /out/CLOUDHV.fd /usr/share/cloud-hypervisor/CLOUDHV.fd
COPY --from=virtd /out/baryon-virtd /usr/bin/baryon-virtd
COPY --from=manifest /out/components.json /usr/share/baryon/components.json
COPY --chmod=0644 packaging/hadron/files/usr/lib/systemd/system/baryon-virtd.service /usr/lib/systemd/system/baryon-virtd.service
COPY --chmod=0644 packaging/hadron/files/usr/lib/modules-load.d/baryon.conf /usr/lib/modules-load.d/baryon.conf
COPY --chmod=0644 packaging/hadron/files/usr/lib/tmpfiles.d/baryon.conf /usr/lib/tmpfiles.d/baryon.conf

FROM ${HADRON_IMAGE} AS image
ARG BARYON_VERSION
ARG KUBERNETES_DISTRO
ARG KUBERNETES_VERSION
COPY --from=host-layer / /
# Enable the unit the way an immutable image can: a relative symlink named
# after the unit, inside /usr. Then check the layout.
RUN set -eu; \
    mkdir -p /usr/lib/systemd/system/multi-user.target.wants; \
    ln -sfn ../baryon-virtd.service /usr/lib/systemd/system/multi-user.target.wants/baryon-virtd.service; \
    test "$(readlink /usr/lib/systemd/system/multi-user.target.wants/baryon-virtd.service)" = "../baryon-virtd.service"; \
    if grep -q '^\[Install\]' /usr/lib/systemd/system/baryon-virtd.service; then echo "unit must not have [Install]" >&2; exit 1; fi; \
    test ! -e /etc/systemd/system/baryon-virtd.service; \
    /usr/bin/cloud-hypervisor --version; \
    /usr/bin/baryon-virtd --version
# kairos-init last: it turns the Hadron image into a Kairos system and, with
# a provider, installs Kubernetes.
RUN --mount=type=bind,from=kairos-init,src=/kairos-init,dst=/kairos-init \
    set -eu; \
    provider=""; \
    if [ -n "${KUBERNETES_DISTRO}" ]; then \
      provider="-p ${KUBERNETES_DISTRO} --provider-${KUBERNETES_DISTRO}-version ${KUBERNETES_VERSION}"; \
    fi; \
    /kairos-init -l info -s install -m generic -t false ${provider} --version "${BARYON_VERSION}"; \
    /kairos-init -l info -s init -m generic -t false ${provider} --version "${BARYON_VERSION}"
LABEL org.opencontainers.image.title="Baryon-enabled Hadron" \
      org.opencontainers.image.source="https://github.com/wrkode/baryon"
