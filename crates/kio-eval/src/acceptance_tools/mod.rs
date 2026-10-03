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

#[cfg(test)]
fn private_fixture_directory(path: &std::path::Path) -> kio_core::store_dir::StoreDirectory {
    use kio_core::store_dir::StoreDirectory;

    let parent = StoreDirectory::open(path.parent().expect("fixture parent"))
        .expect("retained fixture parent");
    // Create only a new child with the store's current-user private policy.
    // Never repair the owner or permissions of an existing fixture directory.
    let handle = parent
        .create_directory(std::path::Path::new(
            path.file_name().expect("fixture leaf"),
        ))
        .expect("new private fixture directory");
    StoreDirectory::from_retained(handle, path.to_owned()).expect("retained private fixture")
}
