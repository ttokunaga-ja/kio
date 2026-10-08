#[cfg(windows)]
use super::cas_identity::{DirectoryHandleIdentity, RegularFileHandleIdentity};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowsFileInformation {
    volume_serial_number: u32,
    file_index: u64,
    number_of_links: u32,
    file_attributes: u32,
}

impl WindowsFileInformation {
    const DIRECTORY_ATTRIBUTE: u32 = 0x10;
    const REPARSE_POINT_ATTRIBUTE: u32 = 0x400;

    pub(crate) const fn new(
        volume_serial_number: u32,
        file_index: u64,
        number_of_links: u32,
        file_attributes: u32,
    ) -> Self {
        Self {
            volume_serial_number,
            file_index,
            number_of_links,
            file_attributes,
        }
    }

    fn same_identity(self, other: Self) -> bool {
        self.volume_serial_number == other.volume_serial_number
            && self.file_index == other.file_index
    }

    pub(crate) fn is_single_link(self) -> bool {
        self.number_of_links == 1
    }

    pub(crate) fn is_regular_file(self) -> bool {
        self.file_attributes & (Self::DIRECTORY_ATTRIBUTE | Self::REPARSE_POINT_ATTRIBUTE) == 0
    }

    fn is_real_directory(self) -> bool {
        self.file_attributes & Self::DIRECTORY_ATTRIBUTE != 0
            && self.file_attributes & Self::REPARSE_POINT_ATTRIBUTE == 0
    }

    #[cfg(windows)]
    pub(crate) fn directory_identity(self) -> Option<DirectoryHandleIdentity> {
        self.is_real_directory()
            .then_some(DirectoryHandleIdentity::new(
                self.volume_serial_number,
                self.file_index,
            ))
    }

    #[cfg(windows)]
    pub(crate) fn regular_file_identity(self) -> Option<RegularFileHandleIdentity> {
        (self.is_regular_file() && self.is_single_link()).then_some(RegularFileHandleIdentity::new(
            self.volume_serial_number,
            self.file_index,
        ))
    }
}

#[cfg(test)]
pub(crate) fn same_windows_file_identity_components(
    left: Option<WindowsFileInformation>,
    right: Option<WindowsFileInformation>,
) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left.same_identity(right))
}

pub(crate) fn same_windows_regular_file_components(
    left: Option<WindowsFileInformation>,
    right: Option<WindowsFileInformation>,
) -> bool {
    matches!(
        (left, right),
        (Some(left), Some(right))
            if left.same_identity(right) && left.is_regular_file() && right.is_regular_file()
    )
}

pub(crate) fn same_windows_cas_file_components(
    left: Option<WindowsFileInformation>,
    right: Option<WindowsFileInformation>,
) -> bool {
    matches!(
        (left, right),
        (Some(left), Some(right))
            if left.same_identity(right)
                && left.is_regular_file()
                && right.is_regular_file()
                && left.is_single_link()
                && right.is_single_link()
    )
}

#[cfg(test)]
fn same_windows_cas_file_triplet_components(
    before: Option<WindowsFileInformation>,
    opened: Option<WindowsFileInformation>,
    after: Option<WindowsFileInformation>,
) -> bool {
    same_windows_cas_file_components(before, opened)
        && same_windows_cas_file_components(opened, after)
}

pub(crate) fn same_windows_repair_quarantine_file_components(
    left: Option<WindowsFileInformation>,
    right: Option<WindowsFileInformation>,
) -> bool {
    matches!(
        (left, right),
        (Some(left), Some(right))
            if left.same_identity(right)
                && left.is_regular_file()
                && right.is_regular_file()
                && left.number_of_links == 2
                && right.number_of_links == 2
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_identity_components_require_complete_exact_match() {
        let identity = WindowsFileInformation::new(7, 11, 1, 0);
        assert!(same_windows_file_identity_components(
            Some(identity),
            Some(identity)
        ));
        assert!(!same_windows_file_identity_components(
            Some(identity),
            Some(WindowsFileInformation::new(7, 12, 1, 0))
        ));
        assert!(!same_windows_file_identity_components(
            Some(identity),
            Some(WindowsFileInformation::new(8, 11, 1, 0))
        ));
        assert!(!same_windows_file_identity_components(Some(identity), None));
        assert!(!same_windows_file_identity_components(None, None));
    }

    #[test]
    fn windows_cas_components_require_regular_single_link_handles() {
        let single_link = WindowsFileInformation::new(7, 11, 1, 0);
        let hard_linked = WindowsFileInformation::new(7, 11, 2, 0);
        let extra_link = WindowsFileInformation::new(7, 11, 3, 0);
        let directory =
            WindowsFileInformation::new(7, 11, 1, WindowsFileInformation::DIRECTORY_ATTRIBUTE);
        let reparse_point =
            WindowsFileInformation::new(7, 11, 1, WindowsFileInformation::REPARSE_POINT_ATTRIBUTE);

        assert!(same_windows_file_identity_components(
            Some(single_link),
            Some(hard_linked)
        ));
        assert!(same_windows_regular_file_components(
            Some(single_link),
            Some(hard_linked)
        ));
        assert!(!same_windows_regular_file_components(
            Some(single_link),
            Some(directory)
        ));
        assert!(!same_windows_regular_file_components(
            Some(single_link),
            Some(reparse_point)
        ));
        assert!(same_windows_cas_file_components(
            Some(single_link),
            Some(single_link)
        ));
        assert!(!same_windows_cas_file_components(
            Some(single_link),
            Some(hard_linked)
        ));
        assert!(!same_windows_cas_file_components(
            Some(hard_linked),
            Some(hard_linked)
        ));
        assert!(!same_windows_cas_file_components(
            Some(directory),
            Some(directory)
        ));
        assert!(!same_windows_cas_file_components(
            Some(reparse_point),
            Some(reparse_point)
        ));
        assert!(!same_windows_cas_file_components(Some(single_link), None));
        assert!(same_windows_cas_file_triplet_components(
            Some(single_link),
            Some(single_link),
            Some(single_link),
        ));
        assert!(!same_windows_cas_file_triplet_components(
            Some(single_link),
            Some(single_link),
            Some(WindowsFileInformation::new(7, 12, 1, 0)),
        ));
        assert!(same_windows_repair_quarantine_file_components(
            Some(hard_linked),
            Some(hard_linked)
        ));
        assert!(!same_windows_repair_quarantine_file_components(
            Some(single_link),
            Some(single_link)
        ));
        assert!(!same_windows_repair_quarantine_file_components(
            Some(hard_linked),
            Some(extra_link)
        ));
        assert!(!same_windows_repair_quarantine_file_components(
            Some(directory),
            Some(directory)
        ));
        assert!(directory.is_real_directory());
        assert!(!reparse_point.is_real_directory());
        assert!(!single_link.is_real_directory());
    }
}
