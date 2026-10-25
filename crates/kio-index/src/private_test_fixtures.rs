// Included only inside unit-test modules. docs/14 protects Kio-created objects;
// a TempDir itself is an external parent, not automatically private storage.
struct PrivateTestDirectory {
    _temporary: tempfile::TempDir,
    path: std::path::PathBuf,
}

impl PrivateTestDirectory {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

fn private_test_dir() -> PrivateTestDirectory {
    let temporary = tempfile::tempdir().expect("external fixture parent");
    let parent = temporary
        .path()
        .canonicalize()
        .expect("canonical fixture parent");
    // Source-index binding retains both the .kio-equivalent root and its
    // index-equivalent child, so both fixture levels must be Kio-private.
    let path = parent.join("private/store");
    private_test_subdirectory(&path);
    kio_core::private_fs::verify_private_directory(path.parent().expect("private fixture root"))
        .expect("fixture root must also satisfy source-index protection");
    PrivateTestDirectory {
        _temporary: temporary,
        path,
    }
}

fn private_test_subdirectory(path: &std::path::Path) {
    assert!(!path.exists(), "private fixture directory must be new");
    let directory = kio_core::store_dir::StoreDirectory::open_or_create_directories(path)
        .expect("create private fixture descendants");
    kio_core::private_fs::verify_private_directory_handle(&directory)
        .expect("fixture descendant must be private");
}

fn private_test_file(path: &std::path::Path) -> std::fs::File {
    let parent = kio_core::private_fs::verify_private_directory(
        path.parent().expect("fixture file parent"),
    )
    .expect("fixture file requires a private parent");
    parent
        .create_private_file(std::path::Path::new(path.file_name().expect("fixture file leaf")))
        .expect("create new private fixture file without adopting an existing object")
}

#[allow(dead_code)] // Only the registry module needs raw private sidecar bytes.
fn write_private_test_file(path: &std::path::Path, bytes: impl AsRef<[u8]>) {
    use std::io::Write;
    private_test_file(path)
        .write_all(bytes.as_ref())
        .expect("write private fixture bytes");
}
