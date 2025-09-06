fn main() {
    println!("cargo:rerun-if-changed=../../proto/kroni.proto");
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_well_known_types(true)
        .extern_path(".google.protobuf", "::prost_types")
        .compile_protos(&["../../proto/kroni.proto"], &["../../proto"]) // files, includes
        .expect("Failed to compile kroni.proto");
}
