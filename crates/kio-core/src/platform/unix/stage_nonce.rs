//! Descriptor-bound xattrs, with no pathname or sidecar fallback.
use std::{fs::File, io, os::fd::AsRawFd};

const NAME: &std::ffi::CStr = c"user.kio.stage-nonce";

pub(crate) fn create(file: &File, nonce: &[u8; 32]) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            NAME.as_ptr(),
            nonce.as_ptr().cast(),
            nonce.len(),
            libc::XATTR_CREATE,
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            NAME.as_ptr(),
            nonce.as_ptr().cast(),
            nonce.len(),
            0,
            libc::XATTR_CREATE,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn read(file: &File) -> io::Result<[u8; 32]> {
    let mut nonce = [0; 32];
    #[cfg(target_os = "linux")]
    let size = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            NAME.as_ptr(),
            nonce.as_mut_ptr().cast(),
            nonce.len(),
        )
    };
    #[cfg(target_os = "macos")]
    let size = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            NAME.as_ptr(),
            nonce.as_mut_ptr().cast(),
            nonce.len(),
            0,
            0,
        )
    };
    if size == -1 {
        return Err(io::Error::last_os_error());
    }
    if size != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stage nonce must be exactly 256 bits",
        ));
    }
    Ok(nonce)
}

pub(crate) fn storage_exhausted(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::StorageFull || error.raw_os_error() == Some(libc::EDQUOT)
}

pub(crate) fn create_directory(
    parent: &crate::store_dir::StoreDirectory,
    leaf: &std::path::Path,
) -> crate::Result<File> {
    let handle = parent.create_directory(leaf)?;
    super::store::normalize_directory_handle(handle, &parent.path().join(leaf))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptor_nonce_round_trip_and_create_only() {
        let file = tempfile::tempfile_in(std::env::temp_dir()).unwrap();
        assert!(read(&file).is_err());
        create(&file, &[19; 32]).unwrap();
        assert_eq!(read(&file).unwrap(), [19; 32]);
        assert!(create(&file, &[23; 32]).is_err());
        assert_eq!(read(&file).unwrap(), [19; 32]);
    }

    #[test]
    fn malformed_nonce_is_not_accepted() {
        let file = tempfile::tempfile_in(std::env::temp_dir()).unwrap();
        let bad = [9_u8; 31];
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                NAME.as_ptr(),
                bad.as_ptr().cast(),
                bad.len(),
                0,
            )
        };
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                NAME.as_ptr(),
                bad.as_ptr().cast(),
                bad.len(),
                0,
                0,
            )
        };
        assert_eq!(result, 0);
        assert_eq!(read(&file).unwrap_err().kind(), io::ErrorKind::InvalidData);
    }
}
