fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Requires a system protoc on PATH. On macOS: `brew install protobuf`.
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/sandbox.proto"], &["proto"])?;

    println!("cargo:rerun-if-changed=proto/sandbox.proto");
    Ok(())
}
