fn main() {
    println!("cargo:rerun-if-changed=proto/");
    println!("cargo:rerun-if-changed=build.rs");

    // gRPC API generation
    let feature_enabled = std::env::var("CARGO_FEATURE_KRONI_API").is_ok();
    if feature_enabled {
        println!("cargo:rerun-if-changed=proto/kroni.proto");
        tonic_prost_build::configure()
            .build_client(true)
            .build_server(true)
            .compile_well_known_types(true)
            // Map well-known types to prost-types rather than generating a local google module
            .extern_path(".google.protobuf", "::prost_types")
            .compile_protos(&["proto/kroni.proto"], &["proto"]) // files, includes
            .expect("Failed to compile kroni.proto");
    }

    // macOS libproc bindings generation
    #[cfg(target_os = "macos")]
    {
        use std::env;
        use std::path::PathBuf;

        println!("cargo:rustc-link-lib=proc");

        let bindings = bindgen::Builder::default()
            .header_contents(
                "libproc.h",
                r#"
                #include <libproc.h>
                #include <sys/proc_info.h>
            "#,
            )
            .allowlist_function("proc_pid_rusage")
            .allowlist_type("rusage_info_v.*")
            .allowlist_var("RUSAGE_INFO_V4")
            .generate()
            .expect("Unable to generate libproc bindings");

        let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
        bindings
            .write_to_file(out_path.join("libproc_bindings.rs"))
            .expect("Couldn't write libproc bindings!");
    }
}
