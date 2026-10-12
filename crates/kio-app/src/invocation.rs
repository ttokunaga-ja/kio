use crate::commands::CommandRequest as Command;
use crate::context::AppContext;
use crate::multi_scope;
use crate::run_adapter_lifecycle;
use crate::run_ledger_lifecycle;
use crate::trust;
use crate::watch_command;
use kio_adapter::tool_lock::AdapterRuntimeSettings;
use kio_core::ExitCode;
use kio_core::Result;
use kio_core::control::ControlHandle;
use kio_core::scope::Repository;
use serde_json::Value;
use std::sync::Arc;

pub(crate) fn take_exit_override(output: &mut Value) -> Option<ExitCode> {
    let code = output.as_object_mut()?.remove("__exit_code")?.as_u64()?;
    exit_code_from_override(code)
}

pub(crate) fn peek_exit_override(output: &Value) -> Option<ExitCode> {
    output
        .get("__exit_code")
        .and_then(Value::as_u64)
        .and_then(exit_code_from_override)
}

fn exit_code_from_override(code: u64) -> Option<ExitCode> {
    match code {
        3 => Some(ExitCode::PartialFailure),
        4 => Some(ExitCode::PermanentFailure),
        5 => Some(ExitCode::AuthError),
        6 => Some(ExitCode::BudgetExceeded),
        _ => None,
    }
}

pub(crate) fn merged_index_exit_override(
    parent: Option<ExitCode>,
    children: impl IntoIterator<Item = ExitCode>,
    incomplete_child_discovery: bool,
) -> Option<ExitCode> {
    let mut auth = parent == Some(ExitCode::AuthError);
    let mut budget = parent == Some(ExitCode::BudgetExceeded);
    let mut partial = incomplete_child_discovery
        || parent.is_some_and(|code| {
            matches!(code, ExitCode::PartialFailure | ExitCode::PermanentFailure)
        });
    for code in children {
        match code {
            ExitCode::AuthError => auth = true,
            ExitCode::BudgetExceeded => budget = true,
            ExitCode::PartialFailure | ExitCode::PermanentFailure => partial = true,
            _ => {}
        }
    }
    if auth {
        Some(ExitCode::AuthError)
    } else if budget {
        Some(ExitCode::BudgetExceeded)
    } else if partial {
        Some(ExitCode::PartialFailure)
    } else {
        None
    }
}

pub(crate) fn exit_override_error_code(code: ExitCode) -> &'static str {
    match code {
        ExitCode::AuthError => "KIO-E-ADAPTER-AUTH-001",
        ExitCode::BudgetExceeded => "KIO-E-BUDGET-EXCEEDED-001",
        ExitCode::PartialFailure => "KIO-E-SEARCH-PARTIAL-001",
        ExitCode::PermanentFailure => "KIO-E-SEARCH-SCOPE-ALL-FAILED-001",
        _ => "KIO-E-INTERNAL-001",
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Invocation<'a> {
    pub(crate) context: &'a AppContext,
    pub(crate) settings: &'a Arc<AdapterRuntimeSettings>,
    pub(crate) control: &'a ControlHandle,
    pub(crate) locks: &'a kio_core::scope::LockSession,
}

pub(crate) struct InvocationRuntime {
    pub(crate) locks: kio_core::scope::LockSession,
    pub(crate) control: kio_core::control::ControlHandle,
    pub(crate) settings: Arc<AdapterRuntimeSettings>,
    pub(crate) _managed_trust: Option<trust::RuntimeTrust>,
}

impl std::ops::Deref for Invocation<'_> {
    type Target = AppContext;
    fn deref(&self) -> &Self::Target {
        self.context
    }
}
impl Invocation<'_> {
    pub(crate) fn open_read_only_repository(&self) -> Result<Repository> {
        Repository::open_read_only_in_session(self.cwd(), self.control.clone(), self.locks)
    }
    pub(crate) fn open_recovery_repository(&self) -> Result<Repository> {
        Repository::open_recovery_in_session(self.cwd(), self.control.clone(), self.locks)
    }
    pub(crate) fn open_search_repository(&self) -> Result<Repository> {
        Repository::open_search_in_session(self.cwd(), self.control.clone(), self.locks)
    }
    pub(crate) fn scope_delay_test_control(&self) -> multi_scope::ScopeDelayTestControl {
        #[cfg(debug_assertions)]
        {
            let control = self.control.debug();
            multi_scope::ScopeDelayTestControl::new(
                control.cli.scope_search_delay_scope_id.known().cloned(),
                control.cli.scope_search_delay_ms.known().copied(),
            )
        }
        #[cfg(not(debug_assertions))]
        multi_scope::ScopeDelayTestControl
    }
}

pub(crate) fn execute_lifecycle(
    ctx: &AppContext,
    control: &ControlHandle,
    command: Command,
) -> Result<Value> {
    match command {
        Command::Watch(args) => watch_command::run(ctx, args, control),
        Command::Adapter(args) => run_adapter_lifecycle(ctx, args.command, control),
        Command::Ledger(args) => run_ledger_lifecycle(args, control),
        _ => unreachable!("only lifecycle commands bypass adapter composition"),
    }
}
