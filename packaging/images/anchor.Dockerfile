# VM Anchor Pod image: one static binary, no shell, no package manager.
# Build the binary first with hack/build-static.sh baryon-anchor.
FROM scratch
COPY target/static/x86_64-unknown-linux-musl/release/baryon-anchor /baryon-anchor
USER 65532:65532
ENTRYPOINT ["/baryon-anchor"]
