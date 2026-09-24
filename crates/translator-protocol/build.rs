use std::path::PathBuf;

fn main() {
    // Resolve <workspace>/proto from <workspace>/crates/translator-protocol.
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let proto_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("proto"))
        .expect("workspace proto directory");

    let proto_file = proto_root.join("translator.proto");
    println!("cargo:rerun-if-changed={}", proto_file.display());

    prost_build::Config::new()
        .compile_protos(&[&proto_file], &[&proto_root])
        .expect("failed to compile translator.proto (is `protoc` on PATH?)");
}
