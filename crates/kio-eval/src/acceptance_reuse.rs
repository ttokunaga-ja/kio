//! Reuse of acceptance receipts across commits that cannot change the accepted
//! product, its evaluator, or the acceptance inputs.
//!
//! Receipts bind the commit that built and exercised the release archives (the
//! evidence commit). A later main commit (the verified commit) may reuse them
//! only when it descends from the evidence commit and the net diff touches
//! nothing but evidence-neutral paths. The release binary embeds its commit,
//! so binary digests cannot prove this; the diff is the proof.

use std::{fs::OpenOptions, io::Write, path::Path, process::Command};

use crate::acceptance::AcceptanceError;

/// Evidence and verified commits, and every path the net diff changes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EvidenceReuseReport {
    pub evidence_sha: String,
    pub verify_sha: String,
    pub changed_paths: Vec<String>,
}

/// Files whose change affects neither the release archives built at the
/// evidence commit nor the evaluator, fixtures, or acceptance workflows.
const NEUTRAL_FILES: &[&str] = &[
    ".github/workflows/ci.yml",
    ".gitignore",
    "CONTRIBUTING.md",
    "README.md",
    "SECURITY.md",
];

/// Directories with the same property. `kio-ci` and `scripts/ci` also prepare
/// acceptance and release runs, but receipts and archives were produced with
/// the evidence commit's version, so later edits cannot alter that evidence.
const NEUTRAL_DIRECTORIES: &[&str] = &["docs", "tasks", "strategy", "crates/kio-ci", "scripts/ci"];

/// Product crates whose `tests/` directories are compiled into neither the
/// release binary nor the evaluator.
const PRODUCT_CRATES: &[&str] = &[
    "kio-core",
    "kio-pipeline",
    "kio-adapter",
    "kio-index",
    "kio-search",
    "kio-app",
    "kio-cli",
    "kio-process",
];

const MAX_DIFF_BYTES: usize = 4 * 1024 * 1024;

/// Whether a repository-relative path can change without invalidating
/// receipts produced at an earlier commit.
#[must_use]
pub fn is_evidence_neutral_path(path: &str) -> bool {
    let segments = path.split('/').collect::<Vec<_>>();
    if path.contains('\\')
        || segments
            .iter()
            .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
    {
        return false;
    }
    NEUTRAL_FILES.contains(&path)
        || NEUTRAL_DIRECTORIES
            .iter()
            .any(|directory| is_strictly_under(&segments, directory))
        || PRODUCT_CRATES
            .iter()
            .any(|name| is_strictly_under(&segments, &format!("crates/{name}/tests")))
}

fn is_strictly_under(segments: &[&str], directory: &str) -> bool {
    let prefix = directory.split('/').collect::<Vec<_>>();
    segments.len() > prefix.len() && segments[..prefix.len()] == prefix[..]
}

/// Paths from `git diff --no-renames --name-status -z`. Rename and copy
/// records cannot appear without detection, so they fail closed.
pub(crate) fn parse_name_status_z(output: &[u8]) -> Result<Vec<String>, AcceptanceError> {
    if !output.is_empty() && !output.ends_with(&[0]) {
        return Err(malformed_diff());
    }
    let mut fields = output.split(|byte| *byte == 0);
    let mut paths = Vec::new();
    while let Some(status) = fields.next() {
        if status.is_empty() {
            if fields.next().is_some() {
                return Err(malformed_diff());
            }
            break;
        }
        if !matches!(status, b"A" | b"M" | b"D" | b"T") {
            return Err(malformed_diff());
        }
        let path = fields
            .next()
            .filter(|path| !path.is_empty())
            .ok_or_else(malformed_diff)?;
        let path = std::str::from_utf8(path).map_err(|_| malformed_diff())?;
        paths.push(path.to_owned());
    }
    Ok(paths)
}

fn malformed_diff() -> AcceptanceError {
    AcceptanceError::Invalid("git name-status output is malformed".into())
}

/// Prove that receipts bound to `evidence_sha` remain valid at `verify_sha`:
/// the verified commit descends from the evidence commit and the net diff
/// changes only evidence-neutral paths.
pub fn verify_evidence_reuse(
    repo: &Path,
    evidence_sha: &str,
    verify_sha: &str,
) -> Result<EvidenceReuseReport, AcceptanceError> {
    for (label, sha) in [("evidence", evidence_sha), ("verified", verify_sha)] {
        if sha.len() != 40
            || !sha
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(AcceptanceError::Invalid(format!(
                "{label} commit must be a full lowercase SHA"
            )));
        }
        let resolved = git(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{sha}^{{commit}}"),
            ],
        )?;
        if String::from_utf8_lossy(&resolved).trim() != sha {
            return Err(AcceptanceError::Invalid(format!(
                "{label} commit is not present in the repository"
            )));
        }
    }
    let ancestry = git_command(repo)
        .args(["merge-base", "--is-ancestor", evidence_sha, verify_sha])
        .status()
        .map_err(|error| AcceptanceError::Command(format!("git merge-base: {error}")))?;
    match ancestry.code() {
        Some(0) => {}
        Some(1) => {
            return Err(AcceptanceError::Invalid(
                "evidence commit is not an ancestor of the verified commit".into(),
            ));
        }
        _ => return Err(AcceptanceError::Command("git merge-base failed".into())),
    }
    let diff = git(
        repo,
        &[
            "diff",
            "--no-renames",
            "--no-ext-diff",
            "--name-status",
            "-z",
            evidence_sha,
            verify_sha,
            "--",
        ],
    )?;
    let mut changed_paths = parse_name_status_z(&diff)?;
    changed_paths.sort();
    changed_paths.dedup();
    let blocking = changed_paths
        .iter()
        .filter(|path| !is_evidence_neutral_path(path))
        .map(String::as_str)
        .collect::<Vec<_>>();
    if !blocking.is_empty() {
        return Err(AcceptanceError::Invalid(format!(
            "changes after the evidence commit require new receipts: {}",
            blocking.join(", ")
        )));
    }
    Ok(EvidenceReuseReport {
        evidence_sha: evidence_sha.to_owned(),
        verify_sha: verify_sha.to_owned(),
        changed_paths,
    })
}

/// Write the reuse report as canonical JSON to a new file.
pub fn write_evidence_reuse_report_create_only(
    path: &Path,
    report: &EvidenceReuseReport,
) -> Result<(), AcceptanceError> {
    let value =
        serde_json::to_value(report).map_err(|error| AcceptanceError::Json(error.to_string()))?;
    let bytes = kio_core::cas::canonical_json_bytes(&value)
        .map_err(|error| AcceptanceError::Json(error.to_string()))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| AcceptanceError::Io(error.to_string()))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| AcceptanceError::Io(error.to_string()))
}

/// Git without inherited repository, object, or configuration redirection,
/// and with replacement objects disabled so refs cannot rewrite history.
fn git_command(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env("LC_ALL", "C")
        .args(["--no-replace-objects", "--no-optional-locks"])
        .current_dir(repo);
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    command
}

#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>, AcceptanceError> {
    let output = git_command(repo)
        .args(args)
        .output()
        .map_err(|error| AcceptanceError::Command(format!("git {}: {error}", args[0])))?;
    if !output.status.success() {
        return Err(AcceptanceError::Command(format!("git {} failed", args[0])));
    }
    if output.stdout.len() > MAX_DIFF_BYTES {
        return Err(AcceptanceError::Invalid(format!(
            "git {} output exceeds its bound",
            args[0]
        )));
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, process::Command};

    use super::*;

    #[test]
    fn documentation_ci_helpers_and_integration_tests_are_neutral() {
        for path in [
            "docs/09-mvp-scope.md",
            "tasks/v1-closeout-execution-2026-10-03.md",
            "strategy/cloud-competitive-advantage.md",
            "crates/kio-ci/src/linux.rs",
            "crates/kio-ci/Cargo.toml",
            "scripts/ci/prepare-linux-confinement.sh",
            "crates/kio-cli/tests/v1_watch.rs",
            "crates/kio-core/tests/contract_vectors.rs",
            "crates/kio-process/tests/support/mod.rs",
            ".github/workflows/ci.yml",
            "README.md",
            "CONTRIBUTING.md",
            "SECURITY.md",
            ".gitignore",
        ] {
            assert!(is_evidence_neutral_path(path), "{path} must be neutral");
        }
    }

    #[test]
    fn product_evaluator_and_acceptance_inputs_are_not_neutral() {
        for path in [
            "crates/kio-index/src/registry.rs",
            "crates/kio-cli/src/main.rs",
            "crates/kio-cli/build.rs",
            "crates/kio-cli/Cargo.toml",
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "crates/kio-eval/src/acceptance.rs",
            "crates/kio-eval/tests/cli.rs",
            "crates/kio-eval/acceptance-fixtures/v1/office-public/a.docx",
            ".github/workflows/v1-acceptance.yml",
            ".github/workflows/v1-acceptance-verify.yml",
            ".github/workflows/draft-release.yml",
            "eval/README.md",
            "LICENSE.md",
        ] {
            assert!(
                !is_evidence_neutral_path(path),
                "{path} must not be neutral"
            );
        }
    }

    #[test]
    fn neutral_prefixes_do_not_match_lookalike_or_unnormalized_paths() {
        for path in [
            "",
            "docs",
            "docs-evil/x.md",
            "crates/kio-cli/tests-evil/x.rs",
            "crates/kio-clix/tests/x.rs",
            "crates/kio-eval-ci/src/x.rs",
            "./docs/x.md",
            "/docs/x.md",
            "docs/../Cargo.lock",
            "docs//x.md",
            "docs/x.md/",
            "docs\\x.md",
            "README.md/x",
        ] {
            assert!(
                !is_evidence_neutral_path(path),
                "{path:?} must not be neutral"
            );
        }
    }

    #[test]
    fn name_status_output_yields_every_changed_path() {
        let output = b"M\0docs/a.md\0A\0tasks/b.md\0D\0crates/kio-ci/src/x.rs\0T\0README.md\0";
        assert_eq!(
            parse_name_status_z(output).unwrap(),
            [
                "docs/a.md",
                "tasks/b.md",
                "crates/kio-ci/src/x.rs",
                "README.md"
            ]
        );
        assert_eq!(parse_name_status_z(b"").unwrap(), Vec::<String>::new());
    }

    #[test]
    fn unexpected_name_status_records_fail_closed() {
        for output in [
            &b"R100\0docs/a.md\0docs/b.md\0"[..],
            b"C100\0docs/a.md\0docs/b.md\0",
            b"U\0docs/a.md\0",
            b"M\0docs/a.md",
            b"M\0",
            b"M\0\0",
            b"M\0docs/\xff.md\0",
        ] {
            assert!(
                parse_name_status_z(output).is_err(),
                "{:?} must be rejected",
                String::from_utf8_lossy(output)
            );
        }
    }

    struct Repository {
        _root: tempfile::TempDir,
        path: PathBuf,
    }

    impl Repository {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("repo");
            fs::create_dir(&path).unwrap();
            let repository = Self { _root: root, path };
            repository.git(&["init", "-q", "-b", "main"]);
            repository
        }

        fn git(&self, args: &[&str]) -> String {
            let output = Command::new("git")
                .current_dir(&self.path)
                .args([
                    "-c",
                    "user.name=kio-test",
                    "-c",
                    "user.email=kio@example.invalid",
                ])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        }

        fn commit(&self, files: &[(&str, &str)]) -> String {
            for (path, contents) in files {
                let path = self.path.join(path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, contents).unwrap();
            }
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "-m", "change"]);
            self.git(&["rev-parse", "HEAD"])
        }
    }

    #[test]
    fn receipts_are_reused_across_neutral_descendant_commits() {
        let repository = Repository::new();
        let evidence = repository.commit(&[
            ("crates/kio-core/src/lib.rs", "pub fn product() {}\n"),
            ("docs/a.md", "old\n"),
        ]);
        let verify = repository.commit(&[
            ("docs/a.md", "new\n"),
            ("crates/kio-cli/tests/flaky.rs", "#[test]\nfn fixed() {}\n"),
        ]);
        let report = verify_evidence_reuse(&repository.path, &evidence, &verify).unwrap();
        assert_eq!(report.evidence_sha, evidence);
        assert_eq!(report.verify_sha, verify);
        assert_eq!(
            report.changed_paths,
            ["crates/kio-cli/tests/flaky.rs", "docs/a.md"]
        );

        let same = verify_evidence_reuse(&repository.path, &verify, &verify).unwrap();
        assert!(same.changed_paths.is_empty());
    }

    #[test]
    fn product_changes_and_reverted_paths_are_judged_by_the_net_diff() {
        let repository = Repository::new();
        let evidence = repository.commit(&[("crates/kio-core/src/lib.rs", "pub fn a() {}\n")]);
        let product = repository.commit(&[("crates/kio-core/src/lib.rs", "pub fn b() {}\n")]);
        let error = verify_evidence_reuse(&repository.path, &evidence, &product).unwrap_err();
        assert!(
            error.to_string().contains("crates/kio-core/src/lib.rs"),
            "{error}"
        );

        let reverted = repository.commit(&[("crates/kio-core/src/lib.rs", "pub fn a() {}\n")]);
        let report = verify_evidence_reuse(&repository.path, &evidence, &reverted).unwrap();
        assert!(report.changed_paths.is_empty());
    }

    #[test]
    fn renames_out_of_a_product_path_are_not_reusable() {
        let repository = Repository::new();
        let evidence = repository.commit(&[("crates/kio-app/src/old.rs", "pub fn a() {}\n")]);
        fs::create_dir(repository.path.join("docs")).unwrap();
        repository.git(&["mv", "crates/kio-app/src/old.rs", "docs/old.rs"]);
        repository.git(&["commit", "-q", "-m", "move"]);
        let moved = repository.git(&["rev-parse", "HEAD"]);
        let error = verify_evidence_reuse(&repository.path, &evidence, &moved).unwrap_err();
        assert!(
            error.to_string().contains("crates/kio-app/src/old.rs"),
            "{error}"
        );
    }

    #[test]
    fn replacement_refs_cannot_hide_a_product_change() {
        let repository = Repository::new();
        let evidence = repository.commit(&[("crates/kio-core/src/lib.rs", "pub fn a() {}\n")]);
        let neutral = repository.commit(&[("docs/a.md", "note\n")]);
        repository.git(&["checkout", "-q", "--detach", &evidence]);
        let product = repository.commit(&[("crates/kio-core/src/lib.rs", "pub fn b() {}\n")]);
        repository.git(&["replace", &product, &neutral]);
        let error = verify_evidence_reuse(&repository.path, &evidence, &product).unwrap_err();
        assert!(
            error.to_string().contains("crates/kio-core/src/lib.rs"),
            "{error}"
        );
    }

    #[test]
    fn non_descendant_unknown_and_malformed_commits_are_rejected() {
        let repository = Repository::new();
        let base = repository.commit(&[("docs/a.md", "base\n")]);
        let evidence = repository.commit(&[("docs/a.md", "evidence\n")]);
        repository.git(&["checkout", "-q", "-b", "side", &base]);
        let side = repository.commit(&[("docs/a.md", "side\n")]);
        assert!(verify_evidence_reuse(&repository.path, &evidence, &side).is_err());
        assert!(verify_evidence_reuse(&repository.path, &evidence, &base).is_err());
        assert!(verify_evidence_reuse(&repository.path, &evidence, &"0".repeat(40)).is_err());
        for malformed in [
            "",
            "HEAD",
            "main",
            &evidence[..39],
            &evidence.to_uppercase(),
        ] {
            assert!(verify_evidence_reuse(&repository.path, malformed, &side).is_err());
            assert!(verify_evidence_reuse(&repository.path, &evidence, malformed).is_err());
        }
    }
}
