#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowsFileIdentity {
    volume_serial_number: u32,
    file_index: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowsFileInformation {
    identity: WindowsFileIdentity,
    file_size: u64,
    number_of_links: u32,
    last_write_time: u64,
    is_directory: bool,
    is_reparse_point: bool,
}

impl WindowsFileInformation {
    fn from_components(
        volume_serial_number: u32,
        file_index: (u32, u32),
        file_size: (u32, u32),
        number_of_links: u32,
        last_write_time: (u32, u32),
        is_directory: bool,
        is_reparse_point: bool,
    ) -> Self {
        Self {
            identity: WindowsFileIdentity {
                volume_serial_number,
                file_index: join_u32(file_index.0, file_index.1),
            },
            file_size: join_u32(file_size.0, file_size.1),
            number_of_links,
            last_write_time: join_u32(last_write_time.0, last_write_time.1),
            is_directory,
            is_reparse_point,
        }
    }

    pub(crate) fn same_identity(self, other: Self) -> bool {
        self.identity == other.identity
    }

    pub(crate) fn same_file_state(self, other: Self) -> bool {
        self.same_identity(other)
            && self.file_size == other.file_size
            && self.last_write_time == other.last_write_time
    }

    pub(crate) fn file_size(self) -> u64 {
        self.file_size
    }

    pub(crate) fn has_single_link(self) -> bool {
        self.number_of_links == 1
    }

    pub(crate) fn is_regular_file(self) -> bool {
        !self.is_directory && !self.is_reparse_point
    }

    pub(crate) fn is_real_directory(self) -> bool {
        self.is_directory && !self.is_reparse_point
    }
}

fn join_u32(high: u32, low: u32) -> u64 {
    (u64::from(high) << 32) | u64::from(low)
}

#[cfg(windows)]
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

#[cfg(windows)]
pub(crate) fn open_path_no_follow(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    kio_core::platform::open_metadata_nofollow(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_information(
        identity_low: u32,
        size_low: u32,
        write_low: u32,
    ) -> WindowsFileInformation {
        WindowsFileInformation::from_components(
            7,
            (0, identity_low),
            (0, size_low),
            1,
            (0, write_low),
            false,
            false,
        )
    }

    #[test]
    fn identity_requires_volume_and_full_file_index_to_match() {
        let base = synthetic_information(11, 20, 30);
        assert!(base.same_identity(synthetic_information(11, 99, 99)));
        assert!(!base.same_identity(synthetic_information(12, 20, 30)));

        let other_volume =
            WindowsFileInformation::from_components(8, (0, 11), (0, 20), 1, (0, 30), false, false);
        assert!(!base.same_identity(other_volume));
    }

    #[test]
    fn state_requires_identity_size_and_last_write_time_to_match() {
        let base = synthetic_information(11, 20, 30);
        assert!(base.same_file_state(base));
        assert!(!base.same_file_state(synthetic_information(12, 20, 30)));
        assert!(!base.same_file_state(synthetic_information(11, 21, 30)));
        assert!(!base.same_file_state(synthetic_information(11, 20, 31)));
    }

    #[test]
    fn type_and_link_checks_fail_closed() {
        let regular = synthetic_information(11, 20, 30);
        assert!(regular.is_regular_file());
        assert!(regular.has_single_link());

        let directory =
            WindowsFileInformation::from_components(7, (0, 11), (0, 0), 1, (0, 30), true, false);
        assert!(directory.is_real_directory());
        assert!(!directory.is_regular_file());

        let reparse =
            WindowsFileInformation::from_components(7, (0, 11), (0, 20), 1, (0, 30), false, true);
        assert!(!reparse.is_regular_file());
        assert!(!reparse.is_real_directory());

        let hard_linked =
            WindowsFileInformation::from_components(7, (0, 11), (0, 20), 2, (0, 30), false, false);
        assert!(!hard_linked.has_single_link());
    }

    #[test]
    fn high_and_low_words_are_combined_without_truncation() {
        let info = WindowsFileInformation::from_components(
            7,
            (0x0123_4567, 0x89ab_cdef),
            (0x7654_3210, 0xfedc_ba98),
            1,
            (0x1111_2222, 0x3333_4444),
            false,
            false,
        );
        assert_eq!(info.identity.file_index, 0x0123_4567_89ab_cdef);
        assert_eq!(info.file_size(), 0x7654_3210_fedc_ba98);
        assert_eq!(info.last_write_time, 0x1111_2222_3333_4444);
    }

    #[cfg(windows)]
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
