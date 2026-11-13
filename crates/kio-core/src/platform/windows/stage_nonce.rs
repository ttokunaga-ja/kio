//! EA access through Win32 backup streams (BACKUP_EA_DATA, never ADS).
//! https://learn.microsoft.com/windows/win32/api/winbase/ns-winbase-win32_stream_id
use std::{
    ffi::c_void,
    fs::File,
    io::{self, Seek, SeekFrom},
    os::windows::io::AsRawHandle,
    ptr,
};
use windows_sys::Win32::Storage::FileSystem::{
    BACKUP_EA_DATA, BackupRead, BackupSeek, BackupWrite,
};

const NAME: &[u8] = b"KIO_STAGE_NONCE";
const HEADER: usize = 20; // offsetof(WIN32_STREAM_ID, cStreamName)

struct Backup<'a> {
    file: &'a File,
    context: *mut c_void,
    writing: bool,
    position: Option<u64>,
}
impl<'a> Backup<'a> {
    fn new(mut file: &'a File, writing: bool) -> io::Result<Self> {
        Ok(Self {
            position: if file.metadata()?.is_file() {
                Some(file.stream_position()?)
            } else {
                None
            },
            file,
            context: ptr::null_mut(),
            writing,
        })
    }
    fn read_exact(&mut self, bytes: &mut [u8]) -> io::Result<()> {
        let mut count = 0;
        if unsafe {
            BackupRead(
                self.file.as_raw_handle(),
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                &mut count,
                0,
                0,
                &mut self.context,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if count as usize != bytes.len() {
            return Err(invalid("missing or truncated EA backup stream"));
        }
        Ok(())
    }
    fn skip(&mut self, bytes: u64) -> io::Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        let (mut low, mut high) = (0, 0);
        if unsafe {
            BackupSeek(
                self.file.as_raw_handle(),
                bytes as u32,
                (bytes >> 32) as u32,
                &mut low,
                &mut high,
                &mut self.context,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if u64::from(low) | (u64::from(high) << 32) != bytes {
            return Err(invalid("short EA stream seek"));
        }
        Ok(())
    }
}
impl Drop for Backup<'_> {
    fn drop(&mut self) {
        let mut count = 0;
        unsafe {
            if self.writing {
                BackupWrite(
                    self.file.as_raw_handle(),
                    ptr::null(),
                    0,
                    &mut count,
                    1,
                    0,
                    &mut self.context,
                );
            } else {
                BackupRead(
                    self.file.as_raw_handle(),
                    ptr::null_mut(),
                    0,
                    &mut count,
                    1,
                    0,
                    &mut self.context,
                );
            }
        }
        if let Some(position) = self.position {
            let _ = self.file.seek(SeekFrom::Start(position));
        }
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) fn create(file: &File, nonce: &[u8; 32]) -> io::Result<()> {
    // FILE_FULL_EA_INFORMATION: NextEntryOffset, Flags, EaNameLength,
    // EaValueLength, EaName + NUL, EaValue. Only a just-created file is passed.
    let payload_len = 8 + NAME.len() + 1 + nonce.len();
    let mut bytes = vec![0; HEADER + payload_len];
    bytes[..4].copy_from_slice(&BACKUP_EA_DATA.to_le_bytes());
    bytes[8..16].copy_from_slice(&(payload_len as u64).to_le_bytes());
    bytes[HEADER + 5] = NAME.len() as u8;
    bytes[HEADER + 6..HEADER + 8].copy_from_slice(&32_u16.to_le_bytes());
    bytes[HEADER + 8..HEADER + 8 + NAME.len()].copy_from_slice(NAME);
    bytes[HEADER + 9 + NAME.len()..].copy_from_slice(nonce);
    let mut backup = Backup::new(file, true)?;
    let mut written = 0;
    if unsafe {
        BackupWrite(
            file.as_raw_handle(),
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            0,
            0,
            &mut backup.context,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if written as usize != bytes.len() {
        return Err(invalid("short stage EA write"));
    }
    drop(backup);
    // BackupWrite can ignore unsupported streams. Never accept that as success.
    if &read(file)? != nonce {
        return Err(invalid(
            "filesystem did not persist stage EA; EA support required",
        ));
    }
    Ok(())
}

pub(crate) fn read(file: &File) -> io::Result<[u8; 32]> {
    let mut backup = Backup::new(file, false)?;
    // Skip data streams without reading payload, regardless of file length.
    for _ in 0..64 {
        let mut header = [0; HEADER];
        backup.read_exact(&mut header)?;
        let kind = u32::from_le_bytes(header[..4].try_into().unwrap());
        let size = u64::from_le_bytes(header[8..16].try_into().unwrap());
        let name_size = u32::from_le_bytes(header[16..20].try_into().unwrap());
        // Stream names are part of the header and cannot be BackupSeek'ed.
        if name_size > 64 * 1024 {
            return Err(invalid("oversized backup stream name"));
        }
        let mut name = vec![0; name_size as usize];
        if !name.is_empty() {
            backup.read_exact(&mut name)?;
        }
        if kind != BACKUP_EA_DATA {
            backup.skip(size)?;
            continue;
        }
        if size > 64 * 1024 {
            return Err(invalid("oversized stage EA data"));
        }
        let mut data = vec![0; size as usize];
        backup.read_exact(&mut data)?;
        return parse_ea(&data);
    }
    Err(invalid("stage EA not found within stream bound"))
}

fn parse_ea(mut data: &[u8]) -> io::Result<[u8; 32]> {
    loop {
        if data.len() < 8 {
            return Err(invalid("stage nonce EA is absent or malformed"));
        }
        let next = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
        let name_len = data[5] as usize;
        let value_len = u16::from_le_bytes(data[6..8].try_into().unwrap()) as usize;
        let end = 9 + name_len + value_len;
        if data.len() < end || data[8 + name_len] != 0 {
            return Err(invalid("truncated stage EA"));
        }
        if data[8..8 + name_len] == *NAME {
            return data[9 + name_len..end]
                .try_into()
                .map_err(|_| invalid("stage nonce must be exactly 256 bits"));
        }
        if next < end || next >= data.len() {
            return Err(invalid("stage nonce EA is absent"));
        }
        data = &data[next..];
    }
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
    use std::io::{Read, Write};

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
