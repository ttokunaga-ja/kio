use crate::error::Result;
use crate::gc::{Identity, corrupt};
use cap_primitives::fs as cap_fs;
pub(crate) fn gc_metadata_identity(m: &cap_fs::Metadata) -> Result<Identity> {
    use cap_primitives::fs::MetadataExt;
    Ok(Identity(m.dev(), m.ino()))
}

pub(crate) fn is_canonical_gc_index_identity(value: &str) -> bool {
    let Some((platform, dev, ino)) = value
        .split_once(':')
        .and_then(|(platform, rest)| rest.split_once(':').map(|(dev, ino)| (platform, dev, ino)))
    else {
        return false;
    };
    platform == "unix"
        && dev.len() == 16
        && ino.len() == 16
        && dev
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && ino
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub(crate) fn validate_gc_file_links(m: &cap_fs::Metadata) -> Result<()> {
    use cap_primitives::fs::MetadataExt;
    if m.nlink() != 1 {
        return Err(corrupt("linked store entry"));
    }
    Ok(())
}
pub(crate) fn validate_gc_directory_metadata(_: &cap_fs::Metadata) -> Result<()> {
    Ok(())
}
pub(crate) fn validate_gc_direct_directory_metadata(_: &cap_fs::Metadata) -> Result<()> {
    Ok(())
}
