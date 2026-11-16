//! Regenerates src/proto/ from the pinned protos in protos/.
//! Run from the repository root:  cargo run --manifest-path tools/protogen/Cargo.toml
//! Requires `protoc` (only for regeneration; normal builds do not need it).
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "protos/build/bazel/remote/execution/v2/remote_execution.proto",
        "protos/google/bytestream/bytestream.proto",
        "protos/google/longrunning/operations.proto",
        // Build Event Stream: publish API (googleapis) and Bazel's event payloads.
        "protos/google/devtools/build/v1/publish_build_event.proto",
        "protos/src/main/java/com/google/devtools/build/lib/buildeventstream/proto/build_event_stream.proto",
    ];
    std::fs::create_dir_all("src/proto")?;
    tonic_build::configure()
        .build_server(false)
        .out_dir("src/proto")
        .include_file("mod.rs")
        .compile_protos(&protos, &["protos", "/usr/include"])?;
    Ok(())
}
