//! Privileged bootstrap for ephemeral macOS CI runners only. No path overrides.
#[cfg(all(unix, any(target_os = "macos", test)))]
mod sdk;

fn main() {
    #[cfg(target_os = "macos")]
    let result = sdk::prepare();
    #[cfg(not(target_os = "macos"))]
    let result: Result<(), String> = Err("requires macOS root".into());
    if let Err(error) = result {
        eprintln!("SDK preparation failed: {error}");
        std::process::exit(1);
    }
}
