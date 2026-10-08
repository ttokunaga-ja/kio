fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");
    match std::env::var("CARGO_CFG_TARGET_OS") {
        Ok(target_os) if matches!(target_os.as_str(), "macos" | "linux" | "windows") => {}
        target_os => panic!(
            "kio-process supports only macOS, Linux, and Windows; unsupported CARGO_CFG_TARGET_OS: {target_os:?}"
        ),
    }
}
