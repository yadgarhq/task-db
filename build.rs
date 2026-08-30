// Types are GENERATED from the vendored contract, never hand-written (D16). The
// protos come from proto/, vendored at the tag in PROTO_VERSION (D70), so this
// build reaches no network and needs no credentials.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(false)
        // Both files: prost generates only for the files it is given, and task.proto
        // merely IMPORTING common.proto does not produce a module for it.
        .compile_protos(
            &[
                "proto/yadgar/common/v1/common.proto",
                "proto/yadgar/task/v1/task.proto",
            ],
            &["proto"],
        )?;
    Ok(())
}
