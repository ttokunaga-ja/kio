mod export;
mod file_options;

pub(crate) use export::is_platform_root_alias;
pub(crate) use file_options::configure_cap_no_follow;

mod watch_queue;
pub(crate) use watch_queue::{WatchQueueStorage, validate_watch_queue_directory};

#[cfg(target_arch = "aarch64")]
pub(crate) const EG2_TARGET: &str = "aarch64-apple-darwin";
#[cfg(not(target_arch = "aarch64"))]
pub(crate) const EG2_TARGET: &str = "";

mod eg2;
#[cfg(not(test))]
pub(crate) use eg2::EG2_SLOT_ROOT;
pub(crate) use eg2::{configure_eg2_worker_privacy, eg2_library_load_path};
