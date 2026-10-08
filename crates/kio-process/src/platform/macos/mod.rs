pub(super) mod confinement;
pub(super) mod monitor;
pub(crate) fn clear_errno() {
    unsafe {
        *libc::__error() = 0;
    }
}
