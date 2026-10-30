# baryon-controller image: one static binary.
# Build the binary first with hack/build-static.sh baryon-controller.
FROM scratch
# Release identity (the release workflow passes both; docs/release.md).
ARG BARYON_VERSION=dev
ARG BARYON_REVISION=unknown
LABEL org.opencontainers.image.title="baryon-controller" \
      org.opencontainers.image.version="${BARYON_VERSION}" \
      org.opencontainers.image.revision="${BARYON_REVISION}" \
      org.opencontainers.image.source="https://github.com/wrkode/baryon" \
      org.opencontainers.image.licenses="Apache-2.0"
COPY target/static/x86_64-unknown-linux-musl/release/baryon-controller /baryon-controller
USER 65532:65532
ENTRYPOINT ["/baryon-controller"]
