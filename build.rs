fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Requires a system protoc on PATH. On macOS: `brew install protobuf`.
    // tonic 0.13+ split the prost-based codegen into tonic-prost-build;
    // tonic-build itself is now just the shared plumbing.
    //
    // `.bytes(".")` tells prost to generate `bytes::Bytes` (zero-copy) for
    // every `bytes` field in the proto, instead of the default `Vec<u8>`.
    // Matters most on ExecStream's Stdout/Stderr chunks and on the
    // workspace Import/Export streams, where we'd otherwise pay a copy
    // per frame from `Bytes` → `Vec<u8>`.
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .bytes(".")
        .compile_protos(&["proto/sandbox.proto"], &["proto"])?;

    println!("cargo:rerun-if-changed=proto/sandbox.proto");
    Ok(())
}
