//! Compiles `proto/` with protox, in Rust, so building needs no `protoc`.

const PROTOS: &[&str] = &[
    "proto/ukai/knowledge/v1/knowledge.proto",
    "proto/ukai/gateway/v1/agents.proto",
    "proto/ukai/gateway/v1/common.proto",
    "proto/ukai/gateway/v1/knowledge.proto",
    "proto/ukai/gateway/v1/ports.proto",
    "proto/ukai/gateway/v1/schedules.proto",
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    let files = protox::compile(PROTOS, ["proto"])?;
    tonic_prost_build::configure().compile_fds(files)?;
    Ok(())
}
