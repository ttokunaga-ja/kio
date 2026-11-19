//! Exercise the eval consumer with stdout from Cargo's actual CLI binary.
mod support;

use std::{fs, path::Path, process::Command};

fn run(root: &Path, device: &Path, args: &[&str]) -> Vec<u8> {
    invoke(root, device, args, false).stdout
}

fn invoke(root: &Path, device: &Path, args: &[&str], scan_heap: bool) -> std::process::Output {
    let output = Command::new(env!("CARGO_BIN_EXE_kio"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "SystemRoot",
            std::env::var_os("SystemRoot").unwrap_or_default(),
        )
        .envs(support::home_env(device))
        .env("HOME", device.join("home"))
        .env("USERPROFILE", device.join("home"))
        .env("APPDATA", device.join("config"))
        .env("LOCALAPPDATA", device.join("data"))
        .env("XDG_CONFIG_HOME", device.join("config"))
        .env("XDG_DATA_HOME", device.join("data"))
        .env("XDG_CACHE_HOME", device.join("cache"))
        .env("XDG_STATE_HOME", device.join("state"))
        .env("XDG_RUNTIME_DIR", device.join("runtime"))
        .env("TMPDIR", device.join("tmp"))
        .env("TMP", device.join("tmp"))
        .env("TEMP", device.join("tmp"))
        .env("KIO_TEST_SCAN_HEAP", if scan_heap { "1" } else { "0" })
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "args={args:?}; {output:?}");
    output
}

#[test]
fn real_offline_index_stdout_obeys_eval_canonical_contract() {
    for initialize_ledger in [false, true] {
        let temporary = support::canonical_tempdir();
        let root = temporary.path().join("scope");
        let device = temporary.path().join("device");
        fs::create_dir(&root).unwrap();
        for name in ["home", "config", "data", "cache", "state", "runtime", "tmp"] {
            fs::create_dir_all(device.join(name)).unwrap();
        }
        fs::write(
            root.join("document.md"),
            "# Contract fixture\n\nOffline text evidence.\n",
        )
        .unwrap();
        run(&root, &device, &["--json", "init", "."]);
        if initialize_ledger {
            run(&root, &device, &["--json", "ledger", "init"]);
        }
        let bytes = run(&root, &device, &["--json", "index", "--offline", "--yes"]);
        kio_eval::scale_prepare::validate_offline_repair_index_output(&bytes, 1, 0).unwrap_or_else(
            |error| {
                panic!(
                    "ledger={initialize_ledger}; {error}; stdout={}",
                    String::from_utf8_lossy(&bytes)
                )
            },
        );
    }
}

#[cfg(debug_assertions)]
#[test]
fn working_scan_heap_is_bounded_through_cli_publication_and_output() {
    use kio_core::disk_records::SORT_BUFFER_RECORDS;
    use serde_json::Value;

    let mut measurements = Vec::new();
    let mut search_order = None;
    for count in [SORT_BUFFER_RECORDS + 1, SORT_BUFFER_RECORDS * 4 + 1] {
        let temporary = support::canonical_tempdir();
        let root = temporary.path().join("scope");
        let device = temporary.path().join("device");
        fs::create_dir(&root).unwrap();
        for name in ["home", "config", "data", "cache", "state", "runtime", "tmp"] {
            fs::create_dir_all(device.join(name)).unwrap();
        }
        let names = (0..count)
            .map(|n| format!("{n:05}-{}.rs", "source".repeat(15)))
            .collect::<Vec<_>>();
        for (index, path) in names.iter().enumerate().rev() {
            fs::write(
                root.join(path),
                format!("syntheticrustneedle {}\n", index % 8),
            )
            .unwrap();
        }
        fs::write(
            root.join("a.md"),
            "workingfixtureword workingfixtureword workingfixtureword\n",
        )
        .unwrap();
        fs::write(
            root.join("z.md"),
            "workingfixtureword along with additional ordinary words\n",
        )
        .unwrap();
        run(&root, &device, &["--json", "init", "."]);
        let measure = |args: &[&str], publication| {
            let output = invoke(&root, &device, args, true);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let line = stderr
                .lines()
                .find_map(|line| line.strip_prefix("kio_scan_heap "))
                .unwrap_or_else(|| {
                    panic!("missing CLI scan heap measurement for {args:?}: {stderr}")
                });
            let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
            assert_eq!(fields[2], if publication { "published" } else { "read" });
            let working: usize = fields[0].parse().unwrap();
            let output_peak: usize = fields[1].parse().unwrap();
            assert!(working > 0 && output_peak > 0);
            (
                serde_json::from_slice::<Value>(&output.stdout).unwrap(),
                (working, output_peak),
            )
        };

        let (preview, preview_peak) =
            measure(&["--json", "index", "--preview", "--offline"], false);
        assert_eq!(preview["estimated_file_count"], count + 2);
        let paths = preview["candidates"].as_array().unwrap();
        assert!(
            paths
                .iter()
                .map(|row| row["input_path"].as_str().unwrap())
                .eq(names.iter().map(String::as_str).chain(["a.md", "z.md"]))
        );

        let (indexed, index_peak) = measure(&["--json", "index", "--offline"], true);
        assert_eq!(indexed["commit"]["stats"]["files_added"], count + 2);
        // Force the journal reader to merge multiple runs. The last row for a
        // path must win even when the earlier reason is in another run.
        let journal = root.join(".kio/unsupported-inputs.jsonl");
        let frames = fs::read_to_string(&journal).unwrap();
        let mut reordered = Vec::new();
        for line in frames.lines() {
            let mut old: Value = serde_json::from_str(line).unwrap();
            old["reason"] = "resolved".into();
            serde_json::to_writer(&mut reordered, &old).unwrap();
            reordered.push(b'\n');
        }
        for line in frames.lines().rev() {
            reordered.extend_from_slice(line.as_bytes());
            reordered.push(b'\n');
        }
        fs::write(&journal, reordered).unwrap();

        let (status, status_peak) = measure(&["--json", "status"], false);
        let files = status["files"].as_array().unwrap();
        let dispositions = status["unsupported_inputs"].as_array().unwrap();
        assert_eq!((files.len(), dispositions.len()), (count + 2, count));
        assert_eq!(files[count]["relative_path"], "a.md");
        assert_eq!(files[count + 1]["relative_path"], "z.md");
        for ((name, file), disposition) in names.iter().zip(files).zip(dispositions) {
            assert_eq!(file["relative_path"], name.as_str());
            assert_eq!(file["status"], "unchanged");
            assert_eq!(disposition["path"], name.as_str());
            assert_eq!(disposition["raw_hash"], file["raw_hash"]);
            assert_eq!(disposition["reason"], "source_code_is_not_supported");
        }
        let search: Value = serde_json::from_slice(&run(
            &root,
            &device,
            &["--json", "search", "syntheticrustneedle", "--mode", "text"],
        ))
        .unwrap();
        assert!(search["results"].as_array().unwrap().is_empty());
        let search: Value = serde_json::from_slice(&run(
            &root,
            &device,
            &["--json", "search", "workingfixtureword", "--mode", "text"],
        ))
        .unwrap();
        let order = search["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                row["evidence_pointer"]["path_at_commit"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(order.len(), 2);
        assert!(order.iter().any(|path| path == "a.md") && order.iter().any(|path| path == "z.md"));
        if let Some(previous) = search_order.replace(order.clone()) {
            assert_eq!(order, previous);
        }
        measurements.push([preview_peak, index_peak, status_peak]);
    }
    for (index, label) in ["preview", "index", "status"].into_iter().enumerate() {
        let (small, large) = (measurements[0][index], measurements[1][index]);
        eprintln!(
            "{label}: scan {} -> {}, output {} -> {} bytes",
            small.0, large.0, small.1, large.1
        );
        assert!(
            large.0 <= small.0 + 192 * 1024,
            "{label}: working-scan heap grew from {} to {}",
            small.0,
            large.0
        );
        assert!(
            large.1 <= small.1 + 64 * 1024,
            "{label}: output heap grew from {} to {}",
            small.1,
            large.1
        );
    }
}
