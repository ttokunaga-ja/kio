use cap_primitives::fs::{Metadata, MetadataExt};

pub fn same_cap_file_identity(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}
