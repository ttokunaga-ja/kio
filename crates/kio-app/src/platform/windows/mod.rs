mod storage;
pub(crate) use storage::*;
mod export;
mod purge;
mod verification;
pub(crate) use export::*;
pub(crate) use kio_index::platform::recover_prepared_bound_gc_index as try_recover_marked_index_rotation;
pub(crate) use purge::*;
pub(crate) use verification::*;

pub(crate) use kio_process::platform::wait_supervised_child;

mod watch_queue;
pub(crate) use watch_queue::{WatchQueueStorage, validate_watch_queue_directory};

mod eg2_interrupt;
mod eg2_library;
pub(crate) mod eg2_worker;
pub(crate) use eg2_interrupt::Registration as Eg2InterruptRegistration;
pub(crate) use eg2_library::{Eg2Library, eg2_c_path, open_eg2_library};

#[cfg(all(target_arch = "x86_64", target_env = "msvc"))]
pub(crate) const EG2_TARGET: &str = "x86_64-pc-windows-msvc";
#[cfg(not(all(target_arch = "x86_64", target_env = "msvc")))]
pub(crate) const EG2_TARGET: &str = "";

pub(crate) fn open_model_import(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}
