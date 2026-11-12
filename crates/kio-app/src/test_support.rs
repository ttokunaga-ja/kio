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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DiagnosticCall {
    Write(String),
    Flush,
    Confirm(String),
}

pub(crate) struct DiagnosticInteraction {
    pub(crate) fail_write_at: Option<usize>,
    pub(crate) fail_flush: bool,
    calls: std::sync::Mutex<Vec<DiagnosticCall>>,
}

impl DiagnosticInteraction {
    pub(crate) fn new(fail_write_at: Option<usize>, fail_flush: bool) -> Self {
        Self {
            fail_write_at,
            fail_flush,
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }
    pub(crate) fn calls(&self) -> Vec<DiagnosticCall> {
        self.calls.lock().unwrap().clone()
    }
    pub(crate) fn bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        for call in self.calls() {
            match call {
                DiagnosticCall::Write(text) => bytes.extend_from_slice(text.as_bytes()),
                DiagnosticCall::Confirm(summary) => {
                    bytes.extend_from_slice(summary.as_bytes());
                    bytes.push(b'\n');
                }
                DiagnosticCall::Flush => {}
            }
        }
        bytes
    }
    pub(crate) fn context(self: &std::sync::Arc<Self>) -> crate::context::AppContext {
        crate::context::AppContext {
            working_directory: PathBuf::from("."),
            interaction: self.clone(),
        }
    }
}

impl crate::context::Interaction for DiagnosticInteraction {
    fn confirm(&self, summary: &str) -> kio_core::Result<bool> {
        self.calls
            .lock()
            .unwrap()
            .push(DiagnosticCall::Confirm(summary.to_owned()));
        Ok(true)
    }
    fn read_input(&self, _: usize) -> kio_core::Result<Vec<u8>> {
        Ok(Vec::new())
    }
    fn is_interactive(&self) -> bool {
        true
    }
    fn diagnostic(&self, message: &str) {
        self.calls
            .lock()
            .unwrap()
            .push(DiagnosticCall::Write(format!("{message}\n")));
    }
    fn write_diagnostic(&self, text: &str) -> kio_core::Result<()> {
        let mut calls = self.calls.lock().unwrap();
        calls.push(DiagnosticCall::Write(text.to_owned()));
        let count = calls
            .iter()
            .filter(|call| matches!(call, DiagnosticCall::Write(_)))
            .count();
        if self.fail_write_at == Some(count) {
            return Err(kio_core::KioError::io(
                "injected diagnostic write failure",
                "stderr",
            ));
        }
        Ok(())
    }
    fn flush_diagnostics(&self) -> kio_core::Result<()> {
        self.calls.lock().unwrap().push(DiagnosticCall::Flush);
        if self.fail_flush {
            return Err(kio_core::KioError::io(
                "injected diagnostic flush failure",
                "stderr",
            ));
        }
        Ok(())
    }
}

pub(crate) fn assert_diagnostic_io_error(error: &kio_core::KioError, operation: &str) {
    let expected =
        kio_core::KioError::io(format!("injected diagnostic {operation} failure"), "stderr");
    assert_eq!(error.to_error_json(), expected.to_error_json());
    assert_eq!(error.exit_code(), expected.exit_code());
}

/// Create a fresh private fixture namespace without repairing an existing one.
pub(crate) fn create_private_test_directory(path: &Path) {
    assert!(!path.exists(), "private fixture directory must be new");
    let directory = StoreDirectory::open_or_create_directories(path).unwrap();
    kio_core::private_fs::verify_private_directory_handle(&directory).unwrap();
}

/// Seed raw malformed/corrupt test bytes in a legitimately protected file.
pub(crate) fn create_private_test_file(path: &Path) -> std::fs::File {
    let parent = StoreDirectory::open(path.parent().unwrap()).unwrap();
    kio_core::private_fs::verify_private_directory_handle(&parent).unwrap();
    parent
        .create_private_file(Path::new(path.file_name().unwrap()))
        .unwrap()
}
