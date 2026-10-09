use crate::unsupported::UnsupportedInputStore;
use crate::{IoResultExt, Result};
use std::fs;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
#[derive(Debug)]
pub(crate) struct StoreRootSnapshot {
    canonical: PathBuf,

    information: crate::platform::windows_file::WindowsFileInformation,

    _handle: fs::File,
}

impl UnsupportedInputStore {
    pub(crate) fn store_root_snapshot(&self) -> Result<StoreRootSnapshot> {
        {
            let handle =
                crate::platform::open_path_no_follow(&self.kio_dir).pipeline_io(&self.kio_dir)?;
            let information = crate::platform::information(&handle).pipeline_io(&self.kio_dir)?;
            if !information.is_real_directory() {
                return Err(self.corrupt("Kio store root is not a real directory"));
            }
            let canonical = self.kio_dir.canonicalize().pipeline_io(&self.kio_dir)?;
            Ok(StoreRootSnapshot {
                canonical,
                information,
                _handle: handle,
            })
        }
    }

    pub(crate) fn validate_opened_file(
        &self,
        file: &fs::File,
        opened_path: &Path,
        root: &StoreRootSnapshot,
    ) -> Result<()> {
        let opened = file.metadata().pipeline_io(opened_path)?;

        let (valid_identity_and_type, single_link) = {
            let opened_information = crate::platform::information(file).pipeline_io(opened_path)?;
            let listed_handle =
                crate::platform::open_path_no_follow(opened_path).pipeline_io(opened_path)?;
            let listed_information =
                crate::platform::information(&listed_handle).pipeline_io(opened_path)?;
            (
                opened.is_file()
                    && opened_information.is_regular_file()
                    && listed_information.is_regular_file()
                    && opened_information.same_identity(listed_information),
                opened_information.has_single_link() && listed_information.has_single_link(),
            )
        };

        if !valid_identity_and_type {
            return Err(self.corrupt(
                "unsupported-input store changed while it was opened or is not a regular file",
            ));
        }
        if !single_link {
            return Err(self.corrupt("unsupported-input store has an unexpected hard-link count"));
        }

        let current_root = self.store_root_snapshot()?;

        let same_root_identity = current_root.information.same_identity(root.information);

        if current_root.canonical != root.canonical || !same_root_identity {
            return Err(self.corrupt("Kio store root changed while the store was opened"));
        }
        let canonical_file = opened_path.canonicalize().pipeline_io(opened_path)?;
        if canonical_file.parent() != Some(root.canonical.as_path()) {
            return Err(self
                .corrupt("unsupported-input store resolves outside the canonical Kio directory"));
        }
        Ok(())
    }
}

pub(crate) fn configure_no_follow(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    options.custom_flags(0x0020_0000);
}
