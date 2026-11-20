use super::*;
use std::io::{Read, Write};
use std::process::Command;

#[test]
fn fake_worker_entry() {
    let Some(mode) = std::env::var_os("KIO_EG2_FAKE_FIXTURE") else {
        return;
    };
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let mut start = [0];
    input.read_exact(&mut start).unwrap();
    assert_eq!(start, [1]);
    platform::configure_worker(EngineSlot::Query).unwrap();
    output.write_all(b"\x01\x02").unwrap(); // Ready
    output.flush().unwrap();
    if mode == "blocked-write" {
        std::thread::sleep(Duration::from_secs(60));
        std::process::exit(0);
    }
    let mut count = 0;
    while let Ok(Some(_)) = read_frame(&mut input, MAX_REQUEST_BYTES) {
        match mode.to_str().unwrap() {
            "abort" => std::process::abort(),
            "eof" => std::process::exit(0),
            "malformed" => output.write_all(&u32::MAX.to_le_bytes()).unwrap(),
            "timeout" => std::thread::sleep(Duration::from_secs(60)),
            #[cfg(target_os = "linux")]
            "allocate" => {
                let memory = vec![0xfe; 64 * 1024 * 1024];
                std::hint::black_box(&memory);
                std::thread::sleep(Duration::from_secs(60));
            }
            "memory" => write_frame(&mut output, &[WorkerError::MemoryLimit.wire()]).unwrap(),
            "nan" => {
                let mut response = vec![0];
                response.extend([f32::NAN.to_le_bytes(); DIMENSIONS].into_iter().flatten());
                write_frame(&mut output, &response).unwrap();
            }
            "reuse" | "overflow" => {
                count += 1;
                if mode == "overflow" && count == 1 {
                    write_frame(&mut output, &[WorkerError::InputOverflow.wire()]).unwrap();
                    continue;
                }
                let mut response = vec![0];
                response.extend(
                    [(count as f32).to_le_bytes(); DIMENSIONS]
                        .into_iter()
                        .flatten(),
                );
                write_frame(&mut output, &response).unwrap();
            }
            _ => unreachable!(),
        }
        output.flush().unwrap();
    }
    std::process::exit(0);
}

fn fake(mode: &str) -> Worker {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "eg2::tests::fake_worker_entry",
        "--nocapture",
        "--quiet",
    ]);
    command.env("KIO_EG2_FAKE_FIXTURE", mode);
    // The test harness emits its preamble before entering the fake worker.
    Worker::spawn(command, Duration::from_secs(10), &|| false, true).unwrap()
}

#[test]
fn reuses_worker_and_returns_only_complete_vectors() {
    let mut worker = fake("reuse");
    assert_eq!(
        worker.embed(b"\x00hello", TEST_TIMEOUT, &|| false).unwrap()[0],
        1.0
    );
    assert_eq!(
        worker.embed(b"\x00again", TEST_TIMEOUT, &|| false).unwrap()[0],
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
            worker.embed(b"\x00hello", TEST_TIMEOUT, &|| false),
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
        worker.embed(b"\x00hello", Duration::from_secs(60), &|| start.elapsed()
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
    assert!(request(EmbeddingInput::Text(&"x".repeat(MAX_REQUEST_BYTES))).is_err());
    let mut worker = fake("reuse");
    assert_eq!(
        worker.embed(&vec![0; MAX_REQUEST_BYTES + 1], TEST_TIMEOUT, &|| false),
        Err(WorkerError::InputLimit)
    );
    assert_eq!(
        worker.embed(b"\x00hello", TEST_TIMEOUT, &|| false).unwrap()[0],
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
fn indexing_and_query_have_independent_exclusive_slots() {
    let temporary = crate::test_support::PrivateTempDir::new();
    let directory = kio_core::store_dir::StoreDirectory::open(temporary.path()).unwrap();
    let index = acquire_slot(&directory, EngineSlot::Index, Duration::ZERO).unwrap();
    let query = acquire_slot(&directory, EngineSlot::Query, Duration::ZERO).unwrap();
    let wait = Duration::from_millis(25);
    let start = Instant::now();
    assert_eq!(
        acquire_slot(&directory, EngineSlot::Index, wait).unwrap_err(),
        WorkerError::ResourceBusy
    );
    assert!(start.elapsed() >= wait);
    assert_eq!(
        acquire_slot(&directory, EngineSlot::Query, Duration::ZERO).unwrap_err(),
        WorkerError::ResourceBusy
    );
    drop((index, query));
    assert!(acquire_slot(&directory, EngineSlot::Index, Duration::ZERO).is_ok());
    assert_eq!(EngineSlot::Index.threads(), 2);
    assert!((1..=4).contains(&EngineSlot::Query.threads()));
}

#[test]
fn overflow_requires_the_exact_2048_token_diagnostic() {
    for (message, overflow) in [
        (
            "Input sequence length (2049) exceeds maximum supported signature length (2048).",
            true,
        ),
        (
            "Input sequence length (2048) exceeds maximum supported signature length (2048).",
            false,
        ),
        (
            "Input sequence length (2049) exceeds maximum supported signature length (512).",
            false,
        ),
        (
            "Other failure: Input sequence length (2049) exceeds maximum supported signature length (2048).",
            false,
        ),
    ] {
        assert_eq!(native::is_input_overflow(message.as_bytes()), overflow);
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
    let mut vector = None;
    for _ in 0..2 {
        let payload = request(EmbeddingInput::Text("task: search result | query: hello")).unwrap();
        let current = worker
            .embed(&payload, INFERENCE_TIMEOUT, &|| false)
            .unwrap();
        assert!(valid_vector(&current));
        let norm: f32 = current.iter().map(|value| value * value).sum();
        assert!((norm - 1.0).abs() < 0.01);
        if let Some(previous) = &vector {
            assert_eq!(previous, &current);
        }
        vector = Some(current);
    }
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        64,
        64,
        image::Rgb([10, 50, 90]),
    ))
    .write_to(&mut png, image::ImageFormat::Png)
    .unwrap();
    let payload = request(EmbeddingInput::Image(png.get_ref())).unwrap();
    assert!(valid_vector(
        &worker
            .embed(&payload, INFERENCE_TIMEOUT, &|| false)
            .unwrap()
    ));
}

#[test]
fn cancellation_interrupts_a_blocked_request_write() {
    let mut worker = fake("blocked-write");
    let request = vec![1; MAX_REQUEST_BYTES];
    let start = Instant::now();
    assert_eq!(
        worker.embed(&request, Duration::from_secs(10), &|| start.elapsed()
            >= Duration::from_millis(80)),
        Err(WorkerError::Cancelled)
    );
    assert!(worker.child.is_none());
}

#[test]
fn overflow_leaves_the_same_worker_available_for_another_request() {
    let mut worker = fake("overflow");
    assert_eq!(
        worker.embed(b"\x00long", TEST_TIMEOUT, &|| false),
        Err(WorkerError::InputOverflow)
    );
    assert_eq!(
        worker.embed(b"\x00short", TEST_TIMEOUT, &|| false).unwrap()[0],
        2.0
    );
}

#[cfg(target_os = "linux")]
#[test]
fn resident_memory_limit_kills_and_reaps_a_live_worker() {
    let mut worker = fake("allocate");
    worker.limits.bytes = 48 * 1024 * 1024;
    let pid = worker.child.as_ref().unwrap().id();
    assert_eq!(
        worker.embed(b"\x00allocate", Duration::from_secs(5), &|| false),
        Err(WorkerError::MemoryLimit)
    );
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
}

#[cfg(target_os = "linux")]
#[test]
fn dropping_a_reused_worker_reaps_it() {
    let mut worker = fake("reuse");
    let pid = worker.child.as_ref().unwrap().id();
    worker.embed(b"\x00hello", TEST_TIMEOUT, &|| false).unwrap();
    drop(worker);
    assert!(!Path::new(&format!("/proc/{pid}")).exists());
}
