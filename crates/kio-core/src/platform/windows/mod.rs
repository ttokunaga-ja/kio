#[cfg(windows)]
pub(crate) mod atomic;
#[cfg(windows)]
pub(crate) mod atomic_semantics;
#[cfg(windows)]
pub(crate) mod cas_identity;
#[cfg(windows)]
pub(crate) mod cas_io;
#[cfg(windows)]
pub(crate) mod cas_semantics;
#[cfg(windows)]
pub(crate) mod durability;
#[cfg(windows)]
pub(crate) mod gc_exchange;
#[cfg(windows)]
pub(crate) mod gc_io;
#[cfg(windows)]
pub(crate) mod gc_metadata;
#[cfg(windows)]
pub(crate) mod gc_observation;
#[cfg(windows)]
pub(crate) mod gc_semantics;
#[cfg(windows)]
pub(crate) mod gc_snapshot_io;
#[cfg(windows)]
pub(crate) mod gc_snapshot_semantics;
#[cfg(windows)]
pub(crate) mod gc_tree;
pub(crate) mod identity_logic;
#[cfg(windows)]
pub(crate) mod lock;
#[cfg(windows)]
pub(crate) mod management;
#[cfg(windows)]
pub(crate) mod planned_removal;
#[cfg(windows)]
pub(crate) mod primitives;
#[cfg(windows)]
pub(crate) mod private_fs;
#[cfg(windows)]
pub(crate) mod profile;
#[cfg(windows)]
pub(crate) mod removal_semantics;
#[cfg(windows)]
pub(crate) mod scheduled;
#[cfg(windows)]
pub(crate) mod scope_io;
#[cfg(windows)]
pub(crate) mod scope_semantics;
pub(crate) mod snapshot_logic;
#[cfg(windows)]
pub(crate) mod storage_semantics;
#[cfg(windows)]
pub(crate) mod store;
#[cfg(windows)]
pub(crate) mod store_gate;
