fn main() {
    // `cargo:rustc-link-arg` does not propagate from de-mls-ds, so every
    // crate producing runnable binaries (tests, bins) that link libwaku adds
    // the workspace `libs/` rpath itself. The dylib's install name is
    // `@rpath/libwaku.dylib`.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let libs_dir = std::path::Path::new(&manifest_dir).join("../../libs");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if matches!(target_os.as_str(), "macos" | "linux") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", libs_dir.display());
    }
}
