# Import Pod image: one static binary, no shell, no package manager. It is
# used only by import Pods; Anchor Pods run the anchor image, which does not
# contain the importer.
# Build the binary first with hack/build-static.sh baryon-importer.
FROM scratch
COPY target/static/x86_64-unknown-linux-musl/release/baryon-importer /baryon-importer
USER 65532:65532
ENTRYPOINT ["/baryon-importer"]
