# baryon-node-agent image: one static binary.
# Build the binary first with hack/build-static.sh baryon-node-agent.
FROM scratch
COPY target/static/x86_64-unknown-linux-musl/release/baryon-node-agent /baryon-node-agent
ENTRYPOINT ["/baryon-node-agent"]
