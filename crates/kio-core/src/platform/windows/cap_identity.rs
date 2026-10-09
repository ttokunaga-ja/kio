use cap_primitives::fs::{_WindowsByHandle, Metadata};

pub fn same_cap_file_identity(left: &Metadata, right: &Metadata) -> bool {
    left.volume_serial_number() == right.volume_serial_number()
        && left.file_index() == right.file_index()
}
