//! Unprivileged, fixed Linux CI resource preflight; never provisions a host.
#[cfg(any(target_os = "linux", test))]
mod linux;

fn main() {
    #[cfg(target_os = "linux")]
    let result = linux::entry(std::env::args().skip(1).collect());
    #[cfg(not(target_os = "linux"))]
    let result: Result<(), String> = Err("requires a GitHub-hosted Linux runner".into());
    if let Err(error) = result {
        eprintln!("Linux resource preflight failed: {error}");
        std::process::exit(1);
    }
}
