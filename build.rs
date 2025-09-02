fn main() {
    println!("cargo:rerun-if-changed=proto/");
    println!("cargo:rerun-if-changed=build.rs");
    let feature_enabled = std::env::var("CARGO_FEATURE_KRONI_API").is_ok();
    if !feature_enabled {
        return;
    }
    println!("cargo:rerun-if-changed=proto/kroni.proto");
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .type_attribute(".", "#[derive(serde::Serialize, serde::Deserialize)]")
        .compile_protos(&["proto/kroni.proto"], &["proto"]) // files, includes
        .expect("Failed to compile kroni.proto");
}
