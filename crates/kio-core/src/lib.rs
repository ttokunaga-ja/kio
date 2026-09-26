//! Core types shared by Kio crates.

pub mod cas;
pub mod dag;
pub mod durability;
pub mod error;
pub mod exit_code;
pub mod gc;
pub mod history;
#[doc(hidden)]
pub mod identity_serde;
pub mod management;
pub mod portable;
pub mod private_fs;
pub mod purge;
pub mod schema;
pub mod scope;
pub mod store_dir;
#[cfg(debug_assertions)]
pub mod test_control;
pub mod xdg;

pub use error::{KioError, Result};
pub use exit_code::ExitCode;
