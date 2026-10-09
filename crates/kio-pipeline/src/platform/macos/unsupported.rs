use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;

pub(crate) fn configure_no_follow(options: &mut OpenOptions) {
    options.custom_flags(0x104);
    let _ = options;
}
