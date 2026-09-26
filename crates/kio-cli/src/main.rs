mod args;
mod release_binding;

use std::io::{BufRead, IsTerminal, Read};
use std::process;
use std::sync::Arc;

use clap::Parser;

use crate::args::Cli;
use kio_app::context::{AppContext, Interaction};
use kio_core::{ExitCode, KioError, Result};
use serde_json::{Value, json};

fn main() {
    release_binding::retain();
    kio_index::vec::ensure_registered();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => exit_from_clap_error(error),
    };
    let json_mode = cli.json;
    let working_directory = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => exit_with_error(KioError::io(error.to_string(), "."), json_mode),
    };
    let context = AppContext {
        working_directory,
        interaction: Arc::new(TerminalInteraction { json_mode }),
    };
    let exit_code = match kio_app::execute(&context, cli.command.into()) {
        Ok(mut output) => {
            let code = take_exit_override(&mut output).unwrap_or(ExitCode::Success);
            if code != ExitCode::Success {
                append_exit_override_error(&output, code);
            }
            print_output(output, json_mode);
            code
        }
        Err(error) => {
            let _ = kio_core::scope::append_error_log(&error);
            print_error(&error, json_mode);
            error.exit_code()
        }
    };
    process::exit(exit_code.code());
}

struct TerminalInteraction {
    json_mode: bool,
}

impl Interaction for TerminalInteraction {
    fn confirm(&self, summary: &str) -> Result<bool> {
        if !self.is_interactive() {
            return Ok(false);
        }
        eprintln!("{summary}");
        read_confirmation(std::io::stdin().lock())
    }
    fn read_input(&self, max_bytes: usize) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        std::io::stdin()
            .take((max_bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| KioError::io(error.to_string(), "stdin"))?;
        if bytes.len() > max_bytes {
            return Err(KioError::invalid_usage(
                "stdin input exceeds the command limit",
            ));
        }
        Ok(bytes)
    }
    fn is_interactive(&self) -> bool {
        !self.json_mode && std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }
    fn diagnostic(&self, message: &str) {
        eprintln!("{message}");
    }
}

fn read_confirmation(reader: impl BufRead) -> Result<bool> {
    const MAX_CONFIRMATION_BYTES: usize = 64;
    let mut line = String::new();
    reader
        .take((MAX_CONFIRMATION_BYTES + 1) as u64)
        .read_line(&mut line)
        .map_err(|error| KioError::io(error.to_string(), "stdin"))?;
    if line.len() > MAX_CONFIRMATION_BYTES {
        return Ok(false);
    }
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn take_exit_override(output: &mut Value) -> Option<ExitCode> {
    let code = output.as_object_mut()?.remove("__exit_code")?.as_u64()?;
    exit_code_from_override(code)
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

fn append_exit_override_error(output: &Value, code: ExitCode) {
    let error_code = output
        .get("error_code")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| exit_override_error_code(code).to_owned());
    let mut context = json!({ "exit_code": code.code() });
    if let Some(excluded) = output.get("excluded_scopes")
        && excluded.as_array().is_some_and(|array| !array.is_empty())
        && let Some(object) = context.as_object_mut()
    {
        object.insert("excluded_scopes".to_owned(), excluded.clone());
    }
    if let Some(failed) = output.get("failed_scopes")
        && failed.as_array().is_some_and(|array| !array.is_empty())
        && let Some(object) = context.as_object_mut()
    {
        object.insert("failed_scopes".to_owned(), failed.clone());
    }
    let _ = kio_core::scope::append_error_log(&KioError::new(
        error_code,
        "command completed with a non-success exit code",
        context,
        code,
    ));
}

fn exit_override_error_code(code: ExitCode) -> &'static str {
    match code {
        ExitCode::AuthError => "KIO-E-ADAPTER-AUTH-001",
        ExitCode::BudgetExceeded => "KIO-E-BUDGET-EXCEEDED-001",
        ExitCode::PartialFailure => "KIO-E-SEARCH-PARTIAL-001",
        ExitCode::PermanentFailure => "KIO-E-SEARCH-SCOPE-ALL-FAILED-001",
        _ => "KIO-E-INTERNAL-001",
    }
}

fn exit_with_error(error: KioError, json_mode: bool) -> ! {
    let _ = kio_core::scope::append_error_log(&error);
    print_error(&error, json_mode);
    process::exit(error.exit_code().code());
}

fn exit_from_clap_error(err: clap::Error) -> ! {
    use clap::error::ErrorKind;
    if matches!(
        err.kind(),
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
    ) {
        err.exit();
    }
    let exit_code = err.exit_code();
    let reason = clap_error_reason(&err);
    let error = KioError::invalid_usage(reason);
    let _ = kio_core::scope::append_error_log(&error);
    let wants_json = std::env::args().skip(1).any(|arg| arg == "--json");
    if wants_json {
        print_error(&error, true);
        process::exit(exit_code);
    }
    err.exit();
}

fn clap_error_reason(err: &clap::Error) -> String {
    let rendered = err.to_string();
    let first_line = rendered
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("invalid usage");
    first_line
        .strip_prefix("error: ")
        .unwrap_or(first_line)
        .to_string()
}

fn print_output(value: Value, json_mode: bool) {
    if json_mode {
        println!(
            "{}",
            serde_json::to_string(&value).expect("serializing command output cannot fail")
        );
        return;
    }

    // M2, narrowed by 05 §1.7.2 (2026-08-11): an Evidence Pointer `view`
    // resolution no longer carries a `text` field at all — it resolves to a
    // view path + span instead of the chunk body. This branch still fires for
    // the `object/chunk/<hash>` URI resolution path (`resolve_object_uri`),
    // which returns the CAS chunk object's own `text` verbatim — that case
    // still prints the body.
    if value.get("operation").and_then(Value::as_str) == Some("snapshot_auto") {
        // Scheduler output is a compact audit receipt in human mode as well as
        // JSON mode; do not collapse an important skipped reason to `skipped`.
        for key in [
            "status",
            "reason",
            "publication_status",
            "snapshot_status",
            "eligibility_reason",
            "change_count",
            "next_eligible_at",
            "idle_observed_since",
            "idle_observed_seconds",
            "idle_threshold_seconds",
            "idle_eligible",
            "recovery_pending",
            "commit_hash",
            "tree_hash",
        ] {
            let rendered = match value.get(key) {
                Some(Value::String(text)) => text.clone(),
                Some(other) => other.to_string(),
                None => "null".to_owned(),
            };
            println!("{}: {}", key, terminal_safe_text(&rendered, false));
        }
        if let Some(gc) = value.get("gc").and_then(Value::as_object)
            && let Some(status) = gc.get("status").and_then(Value::as_str)
        {
            let reason = gc
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let rendered = format!("{status} ({reason})");
            println!("gc: {}", terminal_safe_text(&rendered, false));
        }
    } else if let Some(text) = value.get("text").and_then(Value::as_str) {
        println!("{}", terminal_safe_text(text, true));
    } else if let Some(view_path) = value.get("view_path").and_then(Value::as_str) {
        // 05 §1.7.2: the path IS `view`'s output, so print it. Falling through
        // to the bare "viewed" status would leave a non-`--json` caller with
        // nothing to consume.
        println!("{}", terminal_safe_text(view_path, false));
    } else if value.get("status").and_then(Value::as_str) == Some("opened")
        && let Some(path) = value.get("path").and_then(Value::as_str)
    {
        // `open` resolves an original but intentionally does not launch an OS
        // application. Its human-mode result is therefore the resolved path;
        // `--json` retains the complete typed resolution record.
        println!("{}", terminal_safe_text(path, false));
    } else if value.get("operation").and_then(Value::as_str) == Some("unreachable_object_inventory")
    {
        // Milestone 8's report is the requested human artifact. Preserve the
        // complete deterministic classification instead of collapsing it to
        // the generic `dry_run` status line.
        let rendered =
            serde_json::to_string_pretty(&value).expect("serializing command output cannot fail");
        println!("{}", terminal_safe_text(&rendered, true));
    } else if value.get("status").and_then(Value::as_str) == Some("dry_run")
        && value.get("candidate_count").is_some()
        && value.get("object_kinds_planned").is_some()
    {
        // The GC dry-run result is itself the requested artifact. Printing only
        // its `status` would hide the candidate counts, policy, exclusions and
        // estimated bytes from human-mode users. Keep the ordinary structured
        // field order, rendered readably and through the terminal sanitizer.
        let rendered =
            serde_json::to_string_pretty(&value).expect("serializing command output cannot fail");
        println!("{}", terminal_safe_text(&rendered, true));
    } else if let Some(status) = value.get("status").and_then(Value::as_str) {
        println!("{}", terminal_safe_text(status, false));
    } else if let Some(commits) = value.get("commits").and_then(Value::as_array) {
        for commit in commits {
            println!(
                "{} {} {}",
                terminal_safe_text(commit["commit_hash"].as_str().unwrap_or_default(), false),
                terminal_safe_text(commit["created_at"].as_str().unwrap_or_default(), false),
                terminal_safe_text(commit["message"].as_str().unwrap_or_default(), false)
            );
        }
    } else if let Some(changes) = value.get("changes").and_then(Value::as_array) {
        for change in changes {
            println!(
                "{} {}",
                terminal_safe_text(change["change"].as_str().unwrap_or_default(), false),
                terminal_safe_text(change["relative_path"].as_str().unwrap_or_default(), false)
            );
        }
    } else if let Some(files) = value.get("files").and_then(Value::as_array) {
        for file in files {
            println!(
                "{} {}",
                terminal_safe_text(file["status"].as_str().unwrap_or_default(), false),
                terminal_safe_text(file["relative_path"].as_str().unwrap_or_default(), false)
            );
        }
    } else {
        let rendered =
            serde_json::to_string_pretty(&value).expect("serializing command output cannot fail");
        println!("{}", terminal_safe_text(&rendered, true));
    }
    if let Some(gc) = value.get("gc").and_then(Value::as_object) {
        let status = gc
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let reason = gc.get("reason").and_then(Value::as_str);
        let line = reason.map_or_else(
            || format!("gc: {status}"),
            |reason| format!("gc: {status} ({reason})"),
        );
        println!("{}", terminal_safe_text(&line, false));
    }
}

fn print_error(error: &KioError, json_mode: bool) {
    if json_mode {
        eprintln!(
            "{}",
            serde_json::to_string(&error.to_error_json())
                .expect("serializing command error cannot fail")
        );
    } else {
        eprintln!(
            "{}: {}",
            terminal_safe_text(error.error_code(), false),
            terminal_safe_text(error.message(), false)
        );
    }
}

/// Make lower-trust repository/provider text inert before writing it to a terminal.
/// Structured JSON output is serialized separately and retains the logical value.
fn terminal_safe_text(input: &str, allow_newlines: bool) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        if allow_newlines && ch == '\n' {
            output.push(ch);
            continue;
        }
        let code = ch as u32;
        let terminal_active = matches!(code, 0x00..=0x1f | 0x7f..=0x9f)
            || matches!(code, 0x061c | 0x200e..=0x200f | 0x2028..=0x202e | 0x2066..=0x2069);
        if terminal_active {
            if code <= 0xff {
                output.push_str(&format!("\\x{code:02x}"));
            } else {
                output.push_str(&format!("\\u{{{code:04x}}}"));
            }
        } else {
            output.push(ch);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{read_confirmation, terminal_safe_text};

    #[test]
    fn confirmation_reads_only_one_bounded_line() {
        for input in ["y\n", " YES\r\n", "yes"] {
            assert!(read_confirmation(input.as_bytes()).unwrap());
        }
        for input in ["", "no\n", "yesterday\n"] {
            assert!(!read_confirmation(input.as_bytes()).unwrap());
        }
        let mut input = std::io::Cursor::new(b"no\nyes\n");
        assert!(!read_confirmation(&mut input).unwrap());
        assert_eq!(input.position(), 3);
        assert!(read_confirmation(&mut input).unwrap());
    }

    #[test]
    fn oversized_confirmation_never_approves_a_yes_prefix() {
        let boundary = format!("yes{}\n", " ".repeat(60));
        assert_eq!(boundary.len(), 64);
        assert!(read_confirmation(boundary.as_bytes()).unwrap());
        let oversized = format!("yes{}\n", " ".repeat(1_000_000));
        let mut input = std::io::Cursor::new(oversized);
        assert!(!read_confirmation(&mut input).unwrap());
        assert_eq!(input.position(), 65);
    }

    #[test]
    fn document_body_preserves_only_newline_control() {
        assert_eq!(
            terminal_safe_text("line 1\nline\t2\r", true),
            "line 1\nline\\x092\\x0d"
        );
    }
}
