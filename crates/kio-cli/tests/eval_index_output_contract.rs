//! Exercise the eval consumer with stdout from Cargo's actual CLI binary.
mod support;

use std::{fs, path::Path, process::Command};

fn run(root: &Path, device: &Path, args: &[&str]) -> Vec<u8> {
    invoke(root, device, args, false).stdout
}

fn invoke(root: &Path, device: &Path, args: &[&str], scan_heap: bool) -> std::process::Output {
    let output = command(root, device, args, scan_heap).output().unwrap();
    assert!(output.status.success(), "args={args:?}; {output:?}");
    output
}

fn command(root: &Path, device: &Path, args: &[&str], scan_heap: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kio"));
    command
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
        .args(args);
    command
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

#[cfg(debug_assertions)]
#[test]
fn mf3_deferrals_are_bounded_through_publication_and_status_output() {
    use kio_core::disk_records::SORT_BUFFER_RECORDS;
    use serde_json::Value;
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };

    let mut measurements = Vec::new();
    for count in [SORT_BUFFER_RECORDS + 1, SORT_BUFFER_RECORDS * 16 + 1] {
        let fixture = support::canonical_tempdir();
        let root = fixture.path().join("scope");
        let device = fixture.path().join("device");
        fs::create_dir(&root).unwrap();
        for name in ["home", "config", "data", "cache", "state", "runtime", "tmp"] {
            fs::create_dir_all(device.join(name)).unwrap();
        }
        fs::write(root.join("published.md"), "ordinary searchable text\n").unwrap();
        run(&root, &device, &["--json", "init", "."]);
        let incoming = fixture.path().join("incoming");
        fs::create_dir(&incoming).unwrap();
        let name = |index| format!("{index:05}-{}.rs", "late".repeat(30));
        for index in 0..count {
            fs::write(incoming.join(name(index)), "late source fixture\n").unwrap();
        }
        let ready = fixture.path().join("ready");
        let mut child = command(&root, &device, &["--json", "index", "--offline"], true)
            .env("KIO_TEST_DURABILITY_POINT", "before_index_snapshot")
            .env("KIO_TEST_DURABILITY_READY", &ready)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let marker = format!("point=before_index_snapshot\npid={}\n", child.id());
        let deadline = Instant::now() + Duration::from_secs(30);
        while fs::read_to_string(&ready).ok().as_deref() != Some(&marker) {
            if child.try_wait().unwrap().is_some() || Instant::now() >= deadline {
                let _ = child.kill();
                panic!(
                    "index never reached publication: {:?}",
                    child.wait_with_output().unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        for index in (0..count).rev() {
            let leaf = name(index);
            fs::rename(incoming.join(&leaf), root.join(leaf)).unwrap();
        }
        fs::write(ready.with_extension("release"), b"release").unwrap();

        let indexed = child.wait_with_output().unwrap();
        assert!(indexed.status.success(), "{indexed:?}");
        let status = invoke(&root, &device, &["--json", "status"], true);
        let human = invoke(&root, &device, &["status"], true);
        let mut peaks = Vec::new();
        for output in [&indexed, &status, &human] {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let fields = stderr
                .lines()
                .find_map(|line| line.strip_prefix("kio_scan_heap "))
                .unwrap_or_else(|| panic!("missing scan heap: {stderr}"))
                .split_ascii_whitespace()
                .collect::<Vec<_>>();
            peaks.push((
                fields[0].parse::<usize>().unwrap(),
                fields[1].parse::<usize>().unwrap(),
            ));
        }
        measurements.push(peaks);

        let indexed: Value = serde_json::from_slice(&indexed.stdout).unwrap();
        let status: Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(indexed["commit"]["stats"]["files_added"], 1);
        assert_eq!(status["files"].as_array().unwrap().len(), 1);
        assert_eq!(indexed["deferred_inputs"], status["deferred_inputs"]);
        let deferred = status["deferred_inputs"].as_array().unwrap();
        assert_eq!(deferred.len(), count);
        for (index, row) in deferred.iter().enumerate() {
            assert_eq!(row["path"], name(index));
            assert_eq!(row["reason"], "added_after_scan");
        }
        let human = String::from_utf8(human.stdout).unwrap();
        assert_eq!(
            human
                .lines()
                .filter(|line| line.starts_with("deferred "))
                .count(),
            count
        );
        assert!(human.contains(&format!("deferred {} (added_after_scan)", name(count - 1))));
    }
    for (label, (small, large)) in ["index", "status JSON", "status text"]
        .into_iter()
        .zip(measurements[0].iter().zip(&measurements[1]))
    {
        eprintln!(
            "{label} deferrals: scan {} -> {}, output {} -> {}",
            small.0, large.0, small.1, large.1
        );
        assert!(
            large.0 <= small.0 + 192 * 1024,
            "{label}: working heap grew from {small:?} to {large:?}"
        );
        assert!(
            large.1 <= small.1 + 64 * 1024,
            "{label}: output heap grew from {small:?} to {large:?}"
        );
    }
}

#[cfg(all(debug_assertions, unix))]
#[test]
fn mf4_search_spills_unsupported_observations_without_record_stages() {
    let fixture = support::canonical_tempdir();
    let root = fixture.path().join("scope");
    let device = fixture.path().join("device");
    fs::create_dir(&root).unwrap();
    for name in ["home", "config", "data", "cache", "state", "runtime", "tmp"] {
        fs::create_dir_all(device.join(name)).unwrap();
    }
    fs::write(root.join("note.md"), "needle searchable text").unwrap();
    fs::write(root.join("unsupported.rs"), "source fixture").unwrap();
    run(&root, &device, &["--json", "init", "."]);
    run(&root, &device, &["--json", "index", "--offline"]);

    // Initialize the device's cursor key before observing staged writes.
    run(
        &root,
        &device,
        &["--json", "search", "needle", "--mode", "text"],
    );

    // Force the search's disposition projection to sort across spilled runs
    // without indexing hundreds of originals. The temp directory alias can
    // hold anonymous files but cannot be opened as a retained stage directory.
    let journal = root.join(".kio/unsupported-inputs.jsonl");
    let mut pair = fs::read(&journal).unwrap();
    let mut stale: serde_json::Value = serde_json::from_slice(&pair).unwrap();
    stale["path"] = "deleted.rs".into();
    serde_json::to_writer(&mut pair, &stale).unwrap();
    pair.push(b'\n');
    fs::write(
        &journal,
        pair.repeat(kio_core::disk_records::SORT_BUFFER_RECORDS / 2 + 1),
    )
    .unwrap();
    let temp_alias = fixture.path().join("temp-alias");
    std::os::unix::fs::symlink(device.join("tmp"), &temp_alias).unwrap();
    let ready = fixture.path().join("write-staged");
    fs::write(ready.with_extension("release"), b"release").unwrap();
    let output = command(
        &root,
        &device,
        &["--json", "search", "needle", "--mode", "text"],
        false,
    )
    .env("TMPDIR", &temp_alias)
    .env("KIO_TEST_DURABILITY_POINT", "atomic_write_staged")
    .env("KIO_TEST_DURABILITY_READY", &ready)
    .output()
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["results"][0]["evidence_pointer"]["path_at_commit"],
        "note.md"
    );
    assert_eq!(
        report["index_status"]["unsupported_input_errors"],
        serde_json::json!([]),
        "{report}"
    );
    assert_eq!(
        report["index_status"]["unsupported_inputs"][0]["reason"],
        "source_code_is_not_supported"
    );
    assert!(!ready.exists(), "search created a recoverable record stage");
}
