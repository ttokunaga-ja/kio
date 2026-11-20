use super::*;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;

#[test]
fn fake_worker_entry() {
    let Some(mode) = std::env::var_os("KIO_EG2_FAKE_FIXTURE") else {
        return;
    };
    let mut input = std::io::stdin().lock();
    let mut output: Box<dyn Write> = if mode == "diagnostics" {
        Box::new(platform::private_stdout().unwrap())
    } else {
        Box::new(std::io::stdout().lock())
    };
    let mut start = [0];
    input.read_exact(&mut start).unwrap();
    assert_eq!(start, [1]);
    platform::configure_worker(EngineSlot::Query).unwrap();
    let _gate = if mode == "index-slot" || mode == "query-slot" {
        let slot = if mode == "index-slot" {
            EngineSlot::Index
        } else {
            EngineSlot::Query
        };
        match acquire_slot(slot, Duration::from_millis(80)) {
            Ok(gate) => Some(gate),
            Err(error) => {
                output.write_all(&[error.wire()]).unwrap();
                output.flush().unwrap();
                std::process::exit(1);
            }
        }
    } else {
        None
    };
    output.write_all(b"\x01\x02").unwrap(); // Ready
    output.flush().unwrap();
    if mode == "blocked-write" {
        std::thread::sleep(Duration::from_secs(60));
        std::process::exit(0);
    }
    let mut count = 0;
    while let Ok(Some(frame)) = read_frame(&mut input, REQUEST_ID_BYTES + MAX_REQUEST_BYTES) {
        let (id, request) = split_message(&frame).unwrap();
        let mut reply = |payload: &[u8]| {
            let mut response = id.to_vec();
            response.extend_from_slice(payload);
            write_frame(&mut output, &response).unwrap();
        };
        match mode.to_str().unwrap() {
            "abort" => std::process::abort(),
            "eof" => std::process::exit(0),
            "malformed" => output.write_all(&u32::MAX.to_le_bytes()).unwrap(),
            "descendant" => {
                #[cfg(unix)]
                let mut descendant = Command::new("/bin/sleep").arg("4").spawn().unwrap();
                #[cfg(windows)]
                let mut descendant = Command::new("powershell.exe")
                    .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 4"])
                    .spawn()
                    .unwrap();
                #[cfg(target_os = "linux")]
                if let Some(path) = std::env::var_os("KIO_EG2_DESCENDANT_PID") {
                    std::fs::write(path, descendant.id().to_string()).unwrap();
                }
                std::thread::sleep(Duration::from_secs(5));
                descendant.wait().unwrap();
            }
            "timeout" => std::thread::sleep(Duration::from_secs(60)),
            "hard-cap" => reply(&[memory_denial().wire()]),
            "memory" => reply(&[WorkerError::MemoryLimit.wire()]),
            "nan" => {
                let mut response = vec![0];
                response.extend([f32::NAN.to_le_bytes(); DIMENSIONS].into_iter().flatten());
                reply(&response);
            }
            "reuse" | "overflow" | "duplicate" | "diagnostics" | "window-query"
            | "window-document" | "slow-windows" => {
                if mode == "diagnostics" {
                    writeln!(std::io::stdout(), "native stdout diagnostic").unwrap();
                    writeln!(std::io::stderr(), "native stderr diagnostic").unwrap();
                }
                count += 1;
                if mode == "window-query" || mode == "window-document" {
                    let windows = ["猫犬鳥馬", "猫犬", "鳥馬"];
                    let prefix = if mode == "window-query" {
                        "task: search result | query: "
                    } else {
                        "title: none | text: Pet Notes\n\n"
                    };
                    assert_eq!(
                        request,
                        format!("\0{prefix}{}", windows[count - 1]).as_bytes()
                    );
                }
                if matches!(
                    mode.to_str(),
                    Some("overflow" | "window-query" | "window-document" | "slow-windows")
                ) && count == 1
                {
                    reply(&[WorkerError::InputOverflow.wire()]);
                    continue;
                }
                if mode == "slow-windows" {
                    std::thread::sleep(Duration::from_millis(225));
                }
                let mut response = vec![0];
                response.extend(
                    [(count as f32).to_le_bytes(); DIMENSIONS]
                        .into_iter()
                        .flatten(),
                );
                reply(&response);
                if mode == "duplicate" && count == 1 {
                    reply(&response);
                }
            }
            _ => unreachable!(),
        }
        output.flush().unwrap();
    }
    std::process::exit(0);
}

fn fake_command(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "eg2::tests::fake_worker_entry",
        "--nocapture",
        "--quiet",
    ]);
    command.env("KIO_EG2_FAKE_FIXTURE", mode);
    command
}

fn fake(mode: &str) -> Worker {
    // The test harness emits its preamble before entering the fake worker.
    Worker::spawn(fake_command(mode), Duration::from_secs(10), &|| false, true).unwrap()
}

#[test]
fn reuses_worker_and_returns_only_complete_vectors() {
    let mut worker = fake("reuse");
    assert_eq!(
        worker
            .embed(b"\x00hello", Instant::now() + TEST_TIMEOUT, &|| false)
            .unwrap()[0],
        1.0
    );
    assert_eq!(
        worker
            .embed(b"\x00again", Instant::now() + TEST_TIMEOUT, &|| false)
            .unwrap()[0],
        2.0
    );
}

#[test]
fn worker_failures_never_return_a_vector() {
    for (mode, expected) in [
        ("abort", WorkerError::Crash),
        ("eof", WorkerError::Eof),
        ("malformed", WorkerError::Protocol),
        ("nan", WorkerError::Protocol),
        ("timeout", WorkerError::Timeout),
        ("memory", WorkerError::MemoryLimit),
    ] {
        let mut worker = fake(mode);
        assert_eq!(
            worker.embed(b"\x00hello", Instant::now() + TEST_TIMEOUT, &|| false),
            Err(expected),
            "{mode}"
        );
        assert!(worker.child.is_none(), "{mode} child must have been reaped");
    }
}

#[test]
fn cancel_reaps_worker_instead_of_waiting_for_native_code() {
    let mut worker = fake("timeout");
    let start = Instant::now();
    assert_eq!(
        worker.embed(b"\x00hello", start + Duration::from_secs(60), &|| start
            .elapsed()
            >= Duration::from_millis(30)),
        Err(WorkerError::Cancelled)
    );
    assert!(worker.child.is_none());
}

#[test]
fn request_and_frame_sizes_are_checked_before_allocation() {
    assert_eq!(
        read_frame(&mut &u32::MAX.to_le_bytes()[..], MAX_REQUEST_BYTES),
        Err(WorkerError::Protocol)
    );
    assert!(request(WireInput::Text(&"x".repeat(MAX_REQUEST_BYTES))).is_err());
    let mut worker = fake("reuse");
    assert_eq!(
        worker.embed(
            &vec![0; MAX_REQUEST_BYTES + 1],
            Instant::now() + TEST_TIMEOUT,
            &|| false
        ),
        Err(WorkerError::InputLimit)
    );
    assert_eq!(
        worker
            .embed(b"\x00hello", Instant::now() + TEST_TIMEOUT, &|| false)
            .unwrap()[0],
        1.0
    );
}

#[test]
fn initialization_checks_the_pinned_location_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bin/kio");
    let entry = kio_core::litert::Bundle::for_target(super::runtime::TARGET).unwrap();
    let library = dir
        .path()
        .join(entry.directory())
        .join(entry.library.name());
    std::fs::create_dir_all(library.parent().unwrap()).unwrap();
    assert_eq!(
        runtime::verify(&path).unwrap_err(),
        WorkerError::RuntimeMissing
    );
    std::fs::write(library, b"wrong library").unwrap();
    assert_eq!(
        runtime::verify(&path).unwrap_err(),
        WorkerError::RuntimeHashMismatch
    );
}

const TEST_TIMEOUT: Duration = Duration::from_millis(350);

#[test]
fn overflow_requires_the_exact_2048_token_diagnostic() {
    const OVERFLOW: &str =
        "Input sequence length (2049) exceeds maximum supported signature length (2048).";
    for (code, message, expected) in [
        (3, OVERFLOW, WorkerError::InputOverflow),
        (13, OVERFLOW, WorkerError::Inference),
        (8, OVERFLOW, WorkerError::MemoryLimit),
        (
            3,
            "Input sequence length (2048) exceeds maximum supported signature length (2048).",
            WorkerError::Inference,
        ),
        (
            3,
            "Input sequence length (2049) exceeds maximum supported signature length (512).",
            WorkerError::Inference,
        ),
        (
            3,
            "Other failure: Input sequence length (2049) exceeds maximum supported signature length (2048).",
            WorkerError::Inference,
        ),
    ] {
        assert_eq!(native::classify_error(code, message.as_bytes()), expected);
    }
}

#[test]
#[ignore = "requires the imported official EG2 model and a built Kio executable"]
fn native_worker_embeds_on_cpu() {
    let executable = std::env::var_os("KIO_EG2_TEST_KIO")
        .expect("set KIO_EG2_TEST_KIO to the built Kio executable");
    let mut command = Command::new(executable);
    command.args([WORKER_ARGUMENT, EngineSlot::Query.name()]);
    let mut worker = Worker::spawn(command, STARTUP_TIMEOUT, &|| false, false).unwrap();

    // Prove the real tokenizer rejects the whole query before verifying that
    // the member entry point recovers by sending smaller windows.
    let query = " x".repeat(2_800);
    let full_query = format!("task: search result | query: {query}");
    let payload = request(WireInput::Text(&full_query)).unwrap();
    assert_eq!(
        worker.embed(&payload, Instant::now() + INFERENCE_TIMEOUT, &|| false),
        Err(WorkerError::InputOverflow)
    );

    let mut session = EmbeddingSession {
        slot: EngineSlot::Query,
        worker: Some(worker),
    };
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        64,
        64,
        image::Rgb([10, 50, 90]),
    ))
    .write_to(&mut png, image::ImageFormat::Png)
    .unwrap();

    let short_query = EmbeddingInput::Query("hello");
    let first = session.embed(short_query, &|| false).unwrap();
    assert_eq!(
        session.embed(short_query, &|| false).unwrap().vector,
        first.vector
    );
    for input in [
        EmbeddingInput::Query(&query),
        EmbeddingInput::Document {
            body: "A bird above a lake.",
            filename_context: Some("nature walk"),
        },
        EmbeddingInput::Image(png.get_ref()),
    ] {
        let embedded = session.embed(input, &|| false).unwrap();
        assert!(valid_vector(&embedded.vector));
        assert_eq!(embedded.producer.profile_hash, profile_hash());
        let norm: f32 = embedded.vector.iter().map(|value| value * value).sum();
        assert!((norm - 1.0).abs() < 0.01);
    }
}

#[test]
fn cancellation_interrupts_a_blocked_request_write() {
    let mut worker = fake("blocked-write");
    let request = vec![1; MAX_REQUEST_BYTES];
    let start = Instant::now();
    assert_eq!(
        worker.embed(&request, start + Duration::from_secs(10), &|| start
            .elapsed()
            >= Duration::from_millis(80)),
        Err(WorkerError::Cancelled)
    );
    assert!(worker.child.is_none());
}

#[test]
fn overflow_leaves_the_same_worker_available_for_another_request() {
    let mut worker = fake("overflow");
    assert_eq!(
        worker.embed(b"\x00long", Instant::now() + TEST_TIMEOUT, &|| false),
        Err(WorkerError::InputOverflow)
    );
    assert_eq!(
        worker
            .embed(b"\x00short", Instant::now() + TEST_TIMEOUT, &|| false)
            .unwrap()[0],
        2.0
    );
}

#[cfg(target_os = "linux")]
#[test]
fn dropping_a_reused_worker_reaps_it() {
    let mut worker = fake("reuse");
    let pid = worker.child.as_ref().unwrap().id();
    worker
        .embed(b"\x00hello", Instant::now() + TEST_TIMEOUT, &|| false)
        .unwrap();
    drop(worker);
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
}

#[test]
fn duplicate_response_cannot_be_reused_for_another_member() {
    let mut worker = fake("duplicate");
    assert_eq!(
        worker
            .embed(b"\x00first", Instant::now() + TEST_TIMEOUT, &|| false)
            .unwrap()[0],
        1.0
    );
    assert_eq!(
        worker.embed(b"\x00second", Instant::now() + TEST_TIMEOUT, &|| false),
        Err(WorkerError::Protocol)
    );
    assert!(worker.child.is_none());
}

#[test]
fn teardown_is_bounded_when_a_descendant_holds_the_pipe() {
    let pid_file = tempfile::NamedTempFile::new().unwrap();
    let mut command = fake_command("descendant");
    command.env("KIO_EG2_DESCENDANT_PID", pid_file.path());
    let mut worker = Worker::spawn(command, STARTUP_TIMEOUT, &|| false, true).unwrap();
    let start = Instant::now();
    assert_eq!(
        worker.embed(b"\x00wait", Instant::now() + TEST_TIMEOUT, &|| false),
        Err(WorkerError::Timeout)
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    #[cfg(target_os = "linux")]
    {
        let pid = std::fs::read_to_string(pid_file.path()).unwrap();
        assert!(
            !pid.is_empty(),
            "descendant must have spawned before timeout"
        );
        let status = Path::new("/proc").join(pid).join("stat");
        let deadline = Instant::now() + Duration::from_millis(500);
        while let Ok(stat) = std::fs::read_to_string(&status) {
            if stat
                .rsplit_once(") ")
                .is_some_and(|(_, tail)| tail.starts_with('Z'))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "worker descendant is still running"
            );
            std::thread::sleep(POLL);
        }
    }
}

#[test]
fn device_slots_are_shared_across_processes_with_different_data_roots() {
    let data_a = crate::test_support::PrivateTempDir::new();
    let data_b = crate::test_support::PrivateTempDir::new();
    let launch = |mode, data: &Path| {
        let mut command = fake_command(mode);
        command.env("XDG_DATA_HOME", data);
        Worker::spawn(command, Duration::from_secs(10), &|| false, true)
    };
    let index = launch("index-slot", data_a.path()).unwrap();
    let start = Instant::now();
    assert!(matches!(
        launch("index-slot", data_b.path()),
        Err(WorkerError::ResourceBusy)
    ));
    assert!(start.elapsed() >= Duration::from_millis(80));
    let query = launch("query-slot", data_b.path()).unwrap();
    assert!(matches!(
        launch("query-slot", data_a.path()),
        Err(WorkerError::ResourceBusy)
    ));
    drop((index, query));
    assert!(launch("index-slot", data_b.path()).is_ok());
}

#[cfg(unix)]
fn memory_denial() -> WorkerError {
    // PROT_NONE checks the OS address-space limit without committing pages
    // even if a regression permits the mapping.
    let bytes = MEMORY_BYTES as usize + 4096;
    let address = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            bytes,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if address == libc::MAP_FAILED
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOMEM)
    {
        return WorkerError::MemoryLimit;
    }
    if address != libc::MAP_FAILED {
        unsafe { libc::munmap(address, bytes) };
    }
    WorkerError::Inference
}

#[cfg(windows)]
fn memory_denial() -> WorkerError {
    use windows_sys::Win32::Foundation::{
        ERROR_COMMITMENT_LIMIT, ERROR_NOT_ENOUGH_MEMORY, GetLastError,
    };
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    let address = unsafe {
        VirtualAlloc(
            std::ptr::null(),
            192 * 1024 * 1024,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_READWRITE,
        )
    };
    if address.is_null()
        && matches!(
            unsafe { GetLastError() },
            ERROR_COMMITMENT_LIMIT | ERROR_NOT_ENOUGH_MEMORY
        )
    {
        return WorkerError::MemoryLimit;
    }
    if !address.is_null() {
        unsafe { VirtualFree(address, 0, MEM_RELEASE) };
    }
    WorkerError::Inference
}

#[test]
fn worker_os_rejects_an_allocation_over_the_limit() {
    #[cfg(unix)]
    let bytes = MEMORY_BYTES;
    #[cfg(windows)]
    let bytes = 128 * 1024 * 1024;
    let mut worker = Worker::spawn_limited(
        fake_command("hard-cap"),
        Duration::from_secs(10),
        &|| false,
        true,
        bytes,
    )
    .unwrap();
    assert_eq!(
        worker.embed(b"\x00limit", Instant::now() + TEST_TIMEOUT, &|| false),
        Err(WorkerError::MemoryLimit)
    );
    assert!(worker.child.is_none());
}

#[test]
fn worker_diagnostics_do_not_enter_the_response_pipe() {
    let mut worker = fake("diagnostics");
    assert_eq!(
        worker
            .embed(b"\x00hello", Instant::now() + TEST_TIMEOUT, &|| false)
            .unwrap()[0],
        1.0
    );
}

#[test]
fn session_splits_overflow_and_returns_one_normalized_vector() {
    for (mode, input) in [
        ("window-query", EmbeddingInput::Query("猫犬鳥馬")),
        (
            "window-document",
            EmbeddingInput::Document {
                body: "猫犬鳥馬",
                filename_context: Some("Pet Notes"),
            },
        ),
    ] {
        let mut session = EmbeddingSession {
            slot: EngineSlot::Query,
            worker: Some(fake(mode)),
        };
        let embedded = session.embed(input, &|| false).unwrap();
        assert_eq!(embedded.producer.threads, 2);
        assert_eq!(embedded.producer.profile_hash, profile_hash());
        assert_eq!(
            embedded.vector,
            vec![(DIMENSIONS as f64).sqrt().recip() as f32; DIMENSIONS]
        );
    }
}

#[test]
fn session_normalizes_even_one_successful_text_window() {
    let mut session = EmbeddingSession {
        slot: EngineSlot::Query,
        worker: Some(fake("reuse")),
    };
    let embedded = session
        .embed(EmbeddingInput::Query("猫"), &|| false)
        .unwrap();
    assert_eq!(
        embedded.vector,
        vec![(DIMENSIONS as f64).sqrt().recip() as f32; DIMENSIONS]
    );
}

#[test]
fn all_windows_share_one_deadline() {
    let mut worker = fake("slow-windows");
    let input = EmbeddingInput::Query("猫犬").prepare().unwrap();
    let start = Instant::now();
    assert_eq!(
        embed_with_worker(&mut worker, input, start + TEST_TIMEOUT, &|| false),
        Err(WorkerError::Timeout)
    );
    assert!(worker.child.is_none());
}

#[test]
fn member_limits_apply_to_the_body_and_prepared_image() {
    let body = "🐦".repeat(6000);
    assert!(EmbeddingInput::Query(&body).prepare().is_ok());
    assert!(
        EmbeddingInput::Document {
            body: &body,
            filename_context: Some("filename"),
        }
        .prepare()
        .is_ok()
    );
    let over_limit = format!("{body}猫");
    for input in [
        EmbeddingInput::Query(""),
        EmbeddingInput::Query(&over_limit),
        EmbeddingInput::Document {
            body: "",
            filename_context: Some("filename"),
        },
        EmbeddingInput::Document {
            body: &over_limit,
            filename_context: Some("filename"),
        },
        EmbeddingInput::Image(b""),
    ] {
        assert!(matches!(input.prepare(), Err(WorkerError::InputLimit)));
    }
    assert!(matches!(
        EmbeddingInput::Image(b"GIF89a").prepare(),
        Err(WorkerError::InvalidInput)
    ));
}

#[test]
fn profile_binds_exact_prompts_and_semantics_across_platforms() {
    use kio_adapter::identity::tool_profile_hash;
    let mut profile = profile_value();
    assert_eq!(profile["dimensions"], 768);
    assert_eq!(profile["distance"], "cosine");
    assert_eq!(profile["modality"], "multimodal");
    assert_eq!(
        profile["embedding_semantics"]["query_prompt"],
        "task: search result | query: "
    );
    assert_eq!(
        profile["embedding_semantics"]["document_prompt"],
        "title: none | text: "
    );
    assert_eq!(
        profile["embedding_semantics"]["runtime"]["artifact_set_sha256"],
        kio_core::litert::ARCHIVE_SHA256
    );
    assert_eq!(tool_profile_hash(&profile).unwrap(), profile_hash());
    profile["embedding_semantics"]["query_prompt"] = "task: search result | query:".into();
    assert_ne!(tool_profile_hash(&profile).unwrap(), profile_hash());
}
