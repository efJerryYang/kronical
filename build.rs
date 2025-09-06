fn main() {
    println!("cargo:rerun-if-changed=proto/");
    println!("cargo:rerun-if-changed=build.rs");

    // gRPC API generation moved to crates/rpc

    // macOS libproc bindings generation

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
