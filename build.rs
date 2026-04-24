fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Requires a system protoc on PATH. On macOS: `brew install protobuf`.
    // tonic 0.13+ split the prost-based codegen into tonic-prost-build;
    // tonic-build itself is now just the shared plumbing.
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/sandbox.proto"], &["proto"])?;

    println!("cargo:rerun-if-changed=proto/sandbox.proto");
    Ok(())
}
