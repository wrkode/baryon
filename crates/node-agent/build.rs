//! Compiles the kubelet device plugin API with protox (no protoc needed).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto = "proto/deviceplugin/v1beta1/api.proto";
    println!("cargo:rerun-if-changed={proto}");
    let fds = protox::compile([proto], ["proto"])?;
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_fds(fds)?;
    Ok(())
}
