//! Compiles the runtime protocol with protox, so no protoc binary is needed.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "proto/baryon/runtime/v1alpha1/runtime.proto";
    println!("cargo:rerun-if-changed={proto}");
    let fds = protox::compile([proto], ["proto"])?;
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_fds(fds)?;
    Ok(())
}
