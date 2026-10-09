use super::windows_file::WindowsFileInformation;

pub(crate) fn information(file: &std::fs::File) -> std::io::Result<WindowsFileInformation> {
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    };
    let raw = kio_core::platform::file_information(file)?;
    Ok(WindowsFileInformation::from_components(
        raw.volume_serial_number,
        ((raw.file_index >> 32) as u32, raw.file_index as u32),
        ((raw.file_size >> 32) as u32, raw.file_size as u32),
        raw.number_of_links,
        (
            (raw.last_write_time >> 32) as u32,
            raw.last_write_time as u32,
        ),
        raw.file_attributes & FILE_ATTRIBUTE_DIRECTORY != 0,
        raw.file_attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0,
    ))
}

pub(crate) fn open_path_no_follow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    kio_core::platform::open_metadata_nofollow(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_handle_information_distinguishes_files_and_reports_links() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("first.bin");
        let second_path = directory.path().join("second.bin");
        let linked_path = directory.path().join("linked.bin");
        std::fs::write(&first_path, b"same-size").unwrap();
        std::fs::write(&second_path, b"same-size").unwrap();
        std::fs::hard_link(&first_path, &linked_path).unwrap();

        let first = std::fs::File::open(&first_path).unwrap();
        let same = std::fs::File::open(&first_path).unwrap();
        let second = std::fs::File::open(&second_path).unwrap();
        assert!(
            information(&first)
                .unwrap()
                .same_identity(information(&same).unwrap())
        );
        assert!(
            !information(&first)
                .unwrap()
                .same_identity(information(&second).unwrap())
        );

        let linked = open_path_no_follow(&linked_path).unwrap();
        let linked_information = information(&linked).unwrap();
        assert!(
            information(&first)
                .unwrap()
                .same_identity(linked_information)
        );
        assert!(!linked_information.has_single_link());

        let directory_handle = open_path_no_follow(directory.path()).unwrap();
        assert!(information(&directory_handle).unwrap().is_real_directory());
    }
}
