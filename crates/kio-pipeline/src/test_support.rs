//! Native private creators for valid test-owned storage, never permission repair.

use kio_core::store_dir::StoreDirectory;
use std::{
    fs::File,
    io::Write as _,
    path::{Path, PathBuf},
};

pub(crate) struct PrivateTempDir {
    _outer: tempfile::TempDir,
    path: PathBuf,
}

impl PrivateTempDir {
    pub(crate) fn new() -> Self {
        let outer = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let parent = StoreDirectory::open(outer.path()).unwrap();
        let handle = parent.create_directory(Path::new("private")).unwrap();
        let path = outer.path().join("private");
        let directory = StoreDirectory::from_retained(handle, path.clone()).unwrap();
        kio_core::private_fs::verify_private_directory_handle(&directory).unwrap();
        Self {
            _outer: outer,
            path,
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) fn create_private_file(path: &Path) -> File {
    StoreDirectory::open(path.parent().unwrap())
        .unwrap()
        .create_private_file(Path::new(path.file_name().unwrap()))
        .unwrap()
}

pub(crate) fn write_private_file(path: &Path, bytes: &[u8]) {
    create_private_file(path).write_all(bytes).unwrap();
}
