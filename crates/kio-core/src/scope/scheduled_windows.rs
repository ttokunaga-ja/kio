//! Windows scheduled source identity; opaque interface is deferred.

use std::fs::File;

use crate::error::{KioError, Result};

pub(crate) fn source_identity(file: &File) -> Result<crate::cas::WindowsRegularFileIdentity> {
    crate::cas::windows_regular_file_handle_identity(file)
        .ok_or_else(|| KioError::schema("scheduled file is nonregular, reparse, or hardlinked"))
}
