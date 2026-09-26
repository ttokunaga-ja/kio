//! Fixed-contract acceptance infrastructure, outside the shipped product CLI.
pub mod dispatch;
pub mod gpu_identity;
pub mod gpu_memory;
pub mod local_client;
pub mod ocr_proxy;
pub mod package_extract;
pub mod prepare_embedding;
pub mod provider_ledger;
pub mod route_preflight;

#[cfg(test)]
fn canonical_tempdir() -> tempfile::TempDir {
    // Resolve the safe enclosing root without following deliberate fixture symlinks.
    tempfile::tempdir_in(
        std::env::temp_dir()
            .canonicalize()
            .expect("canonical temporary root"),
    )
    .expect("fixture directory")
}
