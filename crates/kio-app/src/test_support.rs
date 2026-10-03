use std::path::{Path, PathBuf};

use kio_core::store_dir::StoreDirectory;

/// A private directory created through the production retained-parent path.
/// Keep the outer temporary directory alive so it also cleans up the child.
pub(crate) struct PrivateTempDir {
    _outer: tempfile::TempDir,
    path: PathBuf,
}

impl PrivateTempDir {
    pub(crate) fn new() -> Self {
        let temporary_root = std::env::temp_dir().canonicalize().unwrap();
        let outer = tempfile::tempdir_in(temporary_root).unwrap();
        let parent = StoreDirectory::open(&outer.path().canonicalize().unwrap()).unwrap();
        let private = parent.create_directory(Path::new("private")).unwrap();
        let path = outer.path().join("private").canonicalize().unwrap();
        let private = StoreDirectory::from_retained(private, path.clone()).unwrap();
        kio_core::private_fs::verify_private_directory_handle(&private).unwrap();
        Self {
            _outer: outer,
            path,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}
