use crate::unsupported::UnsupportedInputStore;
use crate::{IoResultExt, Result};
use std::fs;
use std::path::{Path, PathBuf};
#[derive(Debug)]
pub(crate) struct StoreRootSnapshot {
    canonical: PathBuf,

    metadata: fs::Metadata,
}

impl UnsupportedInputStore {
    pub(crate) fn store_root_snapshot(&self) -> Result<StoreRootSnapshot> {
        let retained = kio_core::store_dir::StoreDirectory::open(&self.kio_dir)
            .map_err(|error| self.corrupt(error.to_string()))?;
        kio_core::private_fs::verify_private_directory_handle(&retained)
            .map_err(|error| self.corrupt(error.to_string()))?;
        {
            let metadata = fs::symlink_metadata(&self.kio_dir).pipeline_io(&self.kio_dir)?;
            if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
                return Err(self.corrupt("Kio store root is not a real directory"));
            }
            let canonical = self.kio_dir.canonicalize().pipeline_io(&self.kio_dir)?;
            Ok(StoreRootSnapshot {
                canonical,
                metadata,
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
            let listed = fs::symlink_metadata(opened_path).pipeline_io(opened_path)?;
            (
                opened.is_file()
                    && !listed.file_type().is_symlink()
                    && listed.file_type().is_file()
                    && same_file_identity(&opened, &listed),
                has_single_link(&opened) && has_single_link(&listed),
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

        let same_root_identity = same_file_identity(&current_root.metadata, &root.metadata);
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

fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn has_single_link(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink() == 1
}
