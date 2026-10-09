use crate::{IoResultExt, Result};
use cap_primitives::fs as cap_fs;
use kio_core::management::DirectoryIdentity;
use kio_core::platform::same_cap_file_identity;
use std::fs::File;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

pub(crate) fn directory_identity(file: &File) -> Result<DirectoryIdentity> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };
    let information = kio_core::platform::file_information(file).map_err(|_| {
        crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            "discovery frontier is not a real directory",
        )
    })?;
    if information.file_attributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || information.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            "discovery frontier is not a real directory",
        ));
    }
    Ok(DirectoryIdentity::Windows {
        volume_serial_number: information.volume_serial_number,
        file_index: information.file_index,
    })
}

pub(crate) fn same_cap_directory_volume(left: &File, right: &File) -> Result<bool> {
    let left = cap_fs::Metadata::from_file(left).pipeline_io(Path::new("."))?;
    let right = cap_fs::Metadata::from_file(right).pipeline_io(Path::new("."))?;

    {
        use cap_fs::_WindowsByHandle;
        Ok(left.volume_serial_number() == right.volume_serial_number())
    }
}

pub(crate) fn same_bound_file_state(left: &cap_fs::Metadata, right: &cap_fs::Metadata) -> bool {
    same_cap_file_identity(left, right)
}

#[derive(Debug)]
pub(crate) struct OpenedFileState {
    information: crate::platform::windows_file::WindowsFileInformation,
}

impl OpenedFileState {
    pub(crate) fn len(&self) -> u64 {
        self.information.file_size()
    }
}

pub(crate) fn open_verified_regular_file(path: &Path) -> Result<(File, OpenedFileState)> {
    {
        let listed = crate::platform::open_path_no_follow(path).pipeline_io(path)?;
        let listed_information = crate::platform::information(&listed).pipeline_io(path)?;
        if !listed_information.is_regular_file() {
            return Err(crate::PipelineError::contract(
                "KIO-E-SCAN-FILE-IDENTITY-001",
                format!("scan candidate is not a regular file: {}", path.display()),
            ));
        }
        let file = File::open(path).pipeline_io(path)?;
        verify_opened_regular_file(path, listed_information, file)
    }
}

fn verify_opened_regular_file(
    path: &Path,
    listed: crate::platform::windows_file::WindowsFileInformation,
    file: File,
) -> Result<(File, OpenedFileState)> {
    let opened = file.metadata().pipeline_io(path)?;
    let information = crate::platform::information(&file).pipeline_io(path)?;
    if !opened.is_file() || !information.is_regular_file() || !listed.same_identity(information) {
        return Err(crate::PipelineError::contract(
            "KIO-E-SCAN-FILE-IDENTITY-001",
            format!(
                "scan candidate changed while it was being opened: {}",
                path.display()
            ),
        ));
    }
    Ok((file, OpenedFileState { information }))
}

pub(crate) fn ensure_file_unchanged(
    file: &File,
    before: &OpenedFileState,
    path: &Path,
) -> Result<()> {
    let unchanged = {
        let after = crate::platform::information(file).pipeline_io(path)?;
        after.is_regular_file() && before.information.same_file_state(after)
    };

    if !unchanged {
        return Err(crate::PipelineError::contract(
            "KIO-E-SCAN-INPUT-CHANGED-001",
            format!(
                "scan candidate changed while it was being read: {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

const MAX_CHILD_SCOPE_DISCOVERY_RELATIVE_UTF16: usize = 32_767;
pub(crate) fn validate_discovery_path_limit(path: &Path) -> Result<()> {
    if path.as_os_str().encode_wide().count() > MAX_CHILD_SCOPE_DISCOVERY_RELATIVE_UTF16 {
        return Err(crate::PipelineError::contract(
            "KIO-E-CHILD-DISCOVERY-CONTINUATION-STALE-001",
            "child discovery path exceeds the Windows supported limit",
        ));
    }

    Ok(())
}
pub(crate) fn reject_child_directory(child: &File) -> bool {
    kio_core::platform::directory_handle_identity(child).is_none()
}
