use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

use kio_core::{ExitCode, KioError, Result};

pub trait Interaction: Send + Sync {
    fn confirm(&self, summary: &str) -> Result<bool>;
    fn read_input(&self, max_bytes: usize) -> Result<Vec<u8>>;
    fn is_interactive(&self) -> bool;
    fn diagnostic(&self, message: &str);
}

#[derive(Clone)]
pub struct AppContext {
    pub working_directory: PathBuf,
    pub interaction: Arc<dyn Interaction + Send + Sync>,
}

thread_local! {
    static CONTEXT: RefCell<Vec<AppContext>> = const { RefCell::new(Vec::new()) };
}

pub fn with_context<T>(context: &AppContext, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    CONTEXT.with(|slot| slot.borrow_mut().push(context.clone()));
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            CONTEXT.with(|slot| {
                slot.borrow_mut().pop();
            });
        }
    }
    let _guard = Guard;
    operation()
}

fn active() -> Result<AppContext> {
    CONTEXT
        .with(|slot| slot.borrow().last().cloned())
        .ok_or_else(|| {
            KioError::new(
                "KIO-E-APP-CONTEXT-001",
                "command execution requires an application context",
                serde_json::json!({}),
                ExitCode::Failure,
            )
        })
}

pub fn working_directory() -> Result<PathBuf> {
    Ok(active()?.working_directory)
}
pub fn confirm(summary: &str) -> Result<bool> {
    active()?.interaction.confirm(summary)
}
pub fn read_input(max_bytes: usize) -> Result<Vec<u8>> {
    active()?.interaction.read_input(max_bytes)
}
pub fn is_interactive() -> Result<bool> {
    Ok(active()?.interaction.is_interactive())
}
pub fn diagnostic(message: &str) {
    if let Ok(context) = active() {
        context.interaction.diagnostic(message);
    }
}
