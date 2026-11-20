//! Access the named EA through the retained synchronous file or directory handle.
use std::{fs::File, io, os::windows::io::AsRawHandle, ptr};
use windows_sys::{
    Wdk::Storage::FileSystem::{NtQueryEaFile, NtSetEaFile},
    Win32::System::IO::IO_STATUS_BLOCK,
};

const NAME: &[u8] = b"KIO_STAGE_NONCE\0";
const NAME_LEN: u8 = (NAME.len() - 1) as u8;
const VALUE_LEN: usize = 32;
const VALUE_OFFSET: usize = 8 + NAME.len();
const RECORD_LEN: usize = VALUE_OFFSET + VALUE_LEN;

#[repr(C, align(4))]
struct EaBuffer<const N: usize>([u8; N]);

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) fn create(file: &File, nonce: &[u8; VALUE_LEN]) -> io::Result<()> {
    // FILE_FULL_EA_INFORMATION with a single entry and a NUL-terminated name.
    let mut buffer = EaBuffer([0; RECORD_LEN]);
    buffer.0[5] = NAME_LEN;
    buffer.0[6..8].copy_from_slice(&(VALUE_LEN as u16).to_le_bytes());
    buffer.0[8..VALUE_OFFSET].copy_from_slice(NAME);
    buffer.0[VALUE_OFFSET..].copy_from_slice(nonce);
    let mut io_status = IO_STATUS_BLOCK::default();
    // SAFETY: buffers remain valid for the synchronous call on the retained handle.
    let status = unsafe {
        NtSetEaFile(
            file.as_raw_handle(),
            &mut io_status,
            buffer.0.as_ptr().cast(),
            RECORD_LEN as u32,
        )
    };
    if status != 0 {
        return Err(super::store::nt_status_error(status));
    }
    if &read(file)? != nonce {
        return Err(invalid("filesystem did not persist the stage EA"));
    }
    Ok(())
}

pub(crate) fn read(file: &File) -> io::Result<[u8; VALUE_LEN]> {
    // FILE_GET_EA_INFORMATION limits the query to the nonce's name.
    let mut name = EaBuffer([0; 5 + NAME.len()]);
    name.0[4] = NAME_LEN;
    name.0[5..].copy_from_slice(NAME);
    let mut buffer = EaBuffer([0; RECORD_LEN]);
    let mut io_status = IO_STATUS_BLOCK::default();
    // SAFETY: both aligned EA buffers and the status block remain valid for
    // the synchronous call. The output buffer has room for one complete nonce.
    let status = unsafe {
        NtQueryEaFile(
            file.as_raw_handle(),
            &mut io_status,
            buffer.0.as_mut_ptr().cast(),
            RECORD_LEN as u32,
            true,
            name.0.as_ptr().cast(),
            name.0.len() as u32,
            ptr::null(),
            true,
        )
    };
    if status != 0 {
        return Err(super::store::nt_status_error(status));
    }
    let data = buffer
        .0
        .get(..io_status.Information)
        .ok_or_else(|| invalid("invalid stage EA byte count"))?;
    parse_ea(data)
}

fn parse_ea(data: &[u8]) -> io::Result<[u8; VALUE_LEN]> {
    if data.len() != RECORD_LEN
        || data[..4] != [0; 4]
        || data[5] != NAME_LEN
        || data[6..8] != (VALUE_LEN as u16).to_le_bytes()
        || data[8..VALUE_OFFSET] != *NAME
    {
        return Err(invalid("stage nonce EA is absent or malformed"));
    }
    Ok(data[VALUE_OFFSET..].try_into().unwrap())
}

pub(crate) fn storage_exhausted(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::StorageFull
        || matches!(error.raw_os_error(), Some(39 | 112 | 1816))
}

pub(crate) fn create_directory(
    parent: &crate::store_dir::StoreDirectory,
    leaf: &std::path::Path,
) -> crate::Result<File> {
    let mut parts = leaf.components();
    let Some(std::path::Component::Normal(name)) = parts.next() else {
        return Err(crate::KioError::invalid_usage(
            "stage directory must be a direct leaf",
        ));
    };
    if parts.next().is_some() {
        return Err(crate::KioError::invalid_usage(
            "stage directory must be a direct leaf",
        ));
    }
    super::private_fs::create_new_private_at(
        &parent.root_handle(),
        name,
        super::private_fs::NewPrivateObjectKind::StageDirectory,
    )
    .map_err(|e| {
        crate::KioError::io(
            e.to_string(),
            parent.path().join(leaf).display().to_string(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Seek, SeekFrom, Write};

    #[test]
    fn ea_survives_rename_and_hardlink_and_is_readable_without_data_access() {
        use std::{fs, os::windows::fs::OpenOptionsExt};
        use windows_sys::Win32::Storage::FileSystem::FILE_READ_EA;

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("stage");
        let renamed = directory.path().join("renamed");
        let linked = directory.path().join("linked");
        let mut file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&source)
            .unwrap();
        create(&file, &[23; 32]).unwrap();
        file.write_all(b"payload").unwrap();
        file.sync_all().unwrap();
        drop(file);

        fs::rename(&source, &renamed).unwrap();
        fs::hard_link(&renamed, &linked).unwrap();
        for path in [renamed, linked] {
            let file = File::options()
                .access_mode(FILE_READ_EA)
                .open(path)
                .unwrap();
            assert_eq!(read(&file).unwrap(), [23; 32]);
        }
    }

    #[test]
    fn ea_round_trip_preserves_stream_position_and_payload() {
        let mut file = tempfile::tempfile_in(std::env::temp_dir()).unwrap();
        file.write_all(b"payload").unwrap();
        create(&file, &[83; 32]).unwrap();
        assert_eq!(file.stream_position().unwrap(), 7);
        assert_eq!(read(&file).unwrap(), [83; 32]);
        assert_eq!(file.stream_position().unwrap(), 7);
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"payload");
    }

    #[test]
    fn malformed_ea_records_fail_closed() {
        for bytes in [vec![], vec![0; 8], vec![0xff; 128]] {
            assert!(parse_ea(&bytes).is_err());
        }
    }
}
