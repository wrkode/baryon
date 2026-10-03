# baryon-controller image: one static binary.
# Build the binary first with hack/build-static.sh baryon-controller.
FROM scratch
COPY target/static/x86_64-unknown-linux-musl/release/baryon-controller /baryon-controller
USER 65532:65532
ENTRYPOINT ["/baryon-controller"]
