mod export;
mod gc;
mod purge;
mod storage;
mod verification;

pub(crate) use export::*;
pub(crate) use gc::try_recover_marked_index_rotation;
pub(crate) use purge::*;
pub(crate) use storage::*;
pub(crate) use verification::verified_inventory_directory_native;

pub(super) mod watch_queue;

mod eg2_interrupt;
mod eg2_library;
pub(crate) mod eg2_worker;
pub(crate) use eg2_interrupt::Registration as Eg2InterruptRegistration;
pub(crate) use eg2_library::{Eg2Library, eg2_c_path, open_eg2_library};

pub(crate) fn open_model_import(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
}
