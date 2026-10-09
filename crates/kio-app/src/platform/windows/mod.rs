mod gc;
pub(crate) use gc::recover_prepared_bound_gc_index;

use std::process::{Command, ExitStatus};

pub(crate) fn wait_supervised_child(
    command: &mut Command,
) -> Result<ExitStatus, kio_process::BoundedProcessError> {
    kio_process::platform::wait_supervised_child(command)
}
