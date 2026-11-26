//! Reject `gh api` endpoints that Git Bash (MSYS) rewrites as Windows paths.

use std::path::Path;

/// One-based line numbers of `gh api` commands whose endpoint argument starts
/// with `/`. Backslash continuations belong to the same command.
fn absolute_endpoint_lines(source: &str) -> Vec<usize> {
    let mut found = Vec::new();
    let mut start_line = 1;
    let mut logical = String::new();
    for (index, line) in source.lines().enumerate() {
        if logical.is_empty() {
            start_line = index + 1;
        }
        match line.strip_suffix('\\') {
            Some(continued) => {
                logical.push_str(continued);
                logical.push(' ');
            }
            None => {
                logical.push_str(line);
                if has_absolute_endpoint(&logical) {
                    found.push(start_line);
                }
                logical.clear();
            }
        }
    }
    if !logical.is_empty() && has_absolute_endpoint(&logical) {
        found.push(start_line);
    }
    found
}

fn has_absolute_endpoint(command: &str) -> bool {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    tokens.windows(2).enumerate().any(|(index, pair)| {
        pair[0].rsplit(['(', '`', '|', ';', '&']).next() == Some("gh")
            && pair[1] == "api"
            && tokens[index + 2..].iter().any(|token| {
                let unquoted = token.trim_start_matches(['"', '\'']);
                unquoted.len() > 1 && unquoted.starts_with('/')
            })
    })
}

fn workflow_violations(root: &Path) -> Vec<String> {
    let mut paths: Vec<_> = std::fs::read_dir(root.join(".github/workflows"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("yml" | "yaml")
            )
        })
        .collect();
    paths.sort();
    paths
        .iter()
        .flat_map(|path| {
            let source = std::fs::read_to_string(path).unwrap();
            absolute_endpoint_lines(&source)
                .into_iter()
                .map(move |line| format!("{}:{line}", path.display()))
        })
        .collect()
}

#[test]
fn rejects_quoted_unquoted_and_continued_endpoints() {
    for command in [
        "gh api /repos/a",
        "gh api '/repos/a'",
        "gh api \"/repos/a\"",
        "gh api --paginate \\\n  \"/repos/a\"",
        "x=$(gh api -H 'Accept: x' /repos/a)",
    ] {
        assert_eq!(absolute_endpoint_lines(command), [1], "{command}");
    }
}

#[test]
fn accepts_relative_endpoints_and_other_commands() {
    for command in [
        "gh api --paginate \"repos/a\"",
        "gh api repos/a >/dev/null",
        "curl /repos/a",
        "gh run view 1",
    ] {
        assert!(absolute_endpoint_lines(command).is_empty(), "{command}");
    }
}

#[test]
fn reports_the_first_line_of_each_command() {
    let source = "ok\ngh api \\\n  /repos/a\ngh api /repos/b\n";
    assert_eq!(absolute_endpoint_lines(source), [2, 4]);
}

#[test]
fn repository_workflows_use_relative_gh_api_endpoints() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let violations = workflow_violations(&root);
    assert!(
        violations.is_empty(),
        "gh api endpoint starts with /: {violations:?}"
    );
}
