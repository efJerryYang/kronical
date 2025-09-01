fn main() {
    println!("cargo:rerun-if-changed=proto/");
    println!("cargo:rerun-if-changed=build.rs");
    let feature_enabled = std::env::var("CARGO_FEATURE_KRONI_API").is_ok();
    if !feature_enabled {
        return;
    }
    println!("cargo:rerun-if-changed=proto/kroni.proto");
    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&["proto/kroni.proto"], &["proto"]) // files, includes
        .expect("Failed to compile kroni.proto");
}
