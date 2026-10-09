use cap_primitives::fs as cap_fs;

pub(crate) fn configure_cap_no_follow(options: &mut cap_fs::OpenOptions) {
    use cap_fs::OpenOptionsExt;
    options.custom_flags(0x104);
}
