//! Baseline-bound memory admission and recovery for the fixed local GPU route.
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use kio_core::{
    private_fs::{read_private_file_at, verify_private_directory},
    store_dir::{Publication, StoreDirectory},
};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

const SCHEMA: &str = "kio.local-gpu.memory/v1";
const FILE: &str = "gpu-memory.json";
const RESERVE: u64 = 128;
#[cfg(any(unix, test))]
const MAX_SAMPLE: usize = 256;

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}
#[derive(Subcommand, Debug)]
pub enum Command {
    Capture(State),
    CheckStart(Start),
    CheckRecovered(Recovered),
}
#[derive(ClapArgs, Debug)]
pub struct State {
    #[arg(long)]
    pub state_dir: PathBuf,
}
#[derive(ClapArgs, Debug)]
pub struct Start {
    #[command(flatten)]
    pub state: State,
    #[arg(long, value_enum)]
    pub phase: Phase,
}
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Phase {
    Ocr,
    Embedding,
}
#[derive(ClapArgs, Debug)]
pub struct Recovered {
    #[command(flatten)]
    pub state: State,
    /// Only cleanup after both exact owned projects have been proved absent.
    #[arg(long)]
    pub allow_missing: bool,
}
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Sample {
    uuid: String,
    total_mib: u64,
    used_mib: u64,
    free_mib: u64,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    schema: String,
    state_dir: PathBuf,
    sample: Sample,
}

pub fn run(args: Args) -> Result<(), String> {
    match args.command {
        Command::Capture(args) => capture(&args.state_dir, sample_stdin()?),
        Command::CheckStart(args) => {
            let baseline = load(&args.state.state_dir, false)?.ok_or("missing baseline")?;
            check_start(&baseline.sample, &sample_stdin()?, args.phase)
        }
        Command::CheckRecovered(args) => {
            // Drain and validate the bounded sample even when an interrupted init
            // has no baseline, so the pipeline cannot race a successful early exit.
            let current = sample_stdin()?;
            let Some(baseline) = load(&args.state.state_dir, args.allow_missing)? else {
                return Ok(());
            };
            check_recovered(&baseline.sample, &current)
        }
    }
}
fn state(path: &Path) -> Result<StoreDirectory, String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err("memory state directory must be absolute and normalized".into());
    }
    verify_private_directory(path).map_err(|_| "memory state directory is unsafe".into())
}
fn capture(path: &Path, sample: Sample) -> Result<(), String> {
    let directory = state(path)?;
    let baseline = Baseline {
        schema: SCHEMA.into(),
        state_dir: path.into(),
        sample,
    };
    let bytes = serde_jcs::to_vec(&baseline).map_err(|_| "cannot encode GPU baseline")?;
    directory
        .write_atomic(Path::new(FILE), &bytes, Publication::CreateOnly)
        .map_err(|_| "refusing to overwrite GPU baseline")?;
    directory
        .sync()
        .map_err(|_| "cannot sync GPU baseline".into())
}
fn load(path: &Path, allow_missing: bool) -> Result<Option<Baseline>, String> {
    let directory = state(path)?;
    if allow_missing
        && !directory
            .contains_entry(Path::new(FILE))
            .map_err(|_| "cannot inspect GPU baseline")?
    {
        return Ok(None);
    }
    let bytes = read_private_file_at(&directory, FILE, 4096)
        .map_err(|_| "GPU baseline is missing or unsafe")?;
    let baseline: Baseline =
        serde_json::from_slice(&bytes).map_err(|_| "GPU baseline is malformed")?;
    if baseline.schema != SCHEMA
        || baseline.state_dir != path
        || !valid(&baseline.sample)
        || serde_jcs::to_vec(&baseline).map_err(|_| "cannot encode GPU baseline")? != bytes
    {
        return Err("GPU baseline is invalid or not canonical".into());
    }
    Ok(Some(baseline))
}
fn valid(s: &Sample) -> bool {
    let Some(uuid) = s.uuid.strip_prefix("GPU-") else {
        return false;
    };
    uuid.len() == 36
        && uuid.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
        && s.total_mib > 0
        && s.used_mib
            .checked_add(s.free_mib)
            .is_some_and(|n| n <= s.total_mib)
}
#[cfg(any(unix, test))]
fn parse(bytes: &[u8]) -> Result<Sample, String> {
    if bytes.len() > MAX_SAMPLE {
        return Err("GPU sample too large".into());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "GPU sample is not UTF-8")?;
    let row = text.strip_suffix('\n').unwrap_or(text);
    let row = row.strip_suffix('\r').unwrap_or(row);
    if row.contains(['\n', '\r']) {
        return Err("GPU sample must contain exactly one row".into());
    }
    let fields: Vec<_> = row.split(',').map(|s| s.trim_matches(' ')).collect();
    if fields.len() != 4 {
        return Err("GPU sample must contain four fields".into());
    }
    let number = |v: &str| -> Result<u64, String> {
        if v.is_empty() || !v.bytes().all(|c| c.is_ascii_digit()) {
            return Err("GPU memory must be an unsigned integer".into());
        }
        v.parse().map_err(|_| "GPU memory overflow".into())
    };
    let sample = Sample {
        uuid: fields[0].into(),
        total_mib: number(fields[1])?,
        used_mib: number(fields[2])?,
        free_mib: number(fields[3])?,
    };
    if !valid(&sample) {
        return Err("GPU sample identity or memory is invalid".into());
    }
    Ok(sample)
}
fn same_gpu(baseline: &Sample, current: &Sample) -> Result<(), String> {
    if baseline.uuid != current.uuid || baseline.total_mib != current.total_mib {
        return Err("GPU identity or capacity changed".into());
    }
    Ok(())
}
fn check_start(baseline: &Sample, current: &Sample, phase: Phase) -> Result<(), String> {
    same_gpu(baseline, current)?;
    let required = match phase {
        Phase::Ocr => 6141,
        Phase::Embedding => 6638.max(((u128::from(current.total_mib) * 4).div_ceil(5)) as u64),
    }
    .checked_add(RESERVE)
    .ok_or("GPU admission threshold overflow")?;
    if current.free_mib < required {
        return Err(format!(
            "insufficient GPU free memory: {} MiB; need {required} MiB",
            current.free_mib
        ));
    }
    Ok(())
}
fn check_recovered(baseline: &Sample, current: &Sample) -> Result<(), String> {
    same_gpu(baseline, current)?;
    if current.used_mib.saturating_sub(baseline.used_mib) > RESERVE {
        return Err("GPU memory has not recovered to baseline plus 128 MiB".into());
    }
    Ok(())
}
#[cfg(unix)]
fn sample_stdin() -> Result<Sample, String> {
    use std::{
        os::fd::AsRawFd,
        time::{Duration, Instant},
    };
    let fd = std::io::stdin().as_raw_fd();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = [0u8; MAX_SAMPLE + 1];
    let mut n = 0;
    loop {
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            return Err("GPU sample stdin timed out".into());
        }
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll points to one initialized descriptor; read writes into the bounded buffer.
        if unsafe { libc::poll(&mut poll, 1, remain.as_millis().max(1) as i32) } <= 0 {
            return Err("GPU sample stdin timed out".into());
        }
        let got = unsafe { libc::read(fd, bytes[n..].as_mut_ptr().cast(), bytes.len() - n) };
        if got < 0 {
            return Err("GPU sample unreadable".into());
        }
        if got == 0 {
            return parse(&bytes[..n]);
        }
        n += got as usize;
        if n > MAX_SAMPLE {
            return Err("GPU sample too large".into());
        }
    }
}
#[cfg(not(unix))]
fn sample_stdin() -> Result<Sample, String> {
    Err("GPU memory controller requires Unix".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    const UUID: &str = "GPU-01234567-89ab-cdef-0123-456789abcdef";
    fn sample(used: u64) -> Sample {
        parse(format!("{UUID}, 8188, {used}, {}\n", 8188 - used - 8).as_bytes()).unwrap()
    }
    #[test]
    fn idle_1222_and_measured_admission() {
        let baseline = sample(1222);
        check_start(&baseline, &baseline, Phase::Ocr).unwrap();
        check_start(&baseline, &baseline, Phase::Embedding).unwrap();
        check_recovered(&baseline, &sample(1350)).unwrap();
        assert!(check_recovered(&baseline, &sample(1351)).is_err());
        assert!(check_start(&baseline, &sample(2000), Phase::Ocr).is_err());
        assert!(check_start(&baseline, &sample(1500), Phase::Embedding).is_err());
        let mut changed = sample(1222);
        changed.uuid = UUID.replace("01234567", "11234567");
        assert!(check_recovered(&baseline, &changed).is_err());
        assert!(check_start(&baseline, &changed, Phase::Ocr).is_err());
        changed = sample(1222);
        changed.total_mib += 1;
        assert!(check_recovered(&baseline, &changed).is_err());
    }
    #[test]
    fn samples_are_bounded_single_gpu_and_strict() {
        for csv in [
            format!("{UUID},0,0,0"),
            format!("{UUID},10,8,3"),
            format!("{UUID},10,-1,9"),
            format!("{UUID},10,N/A,9"),
            format!("{UUID},10,1,9\n{UUID},10,1,9"),
            "GPU-bad,10,1,9".into(),
            format!("{UUID},18446744073709551616,0,0"),
        ] {
            assert!(parse(csv.as_bytes()).is_err(), "{csv}");
        }
        assert!(parse(&[b'0'; MAX_SAMPLE + 1]).is_err());
        assert!(parse(format!("{UUID},10,1,8\r\n").as_bytes()).is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn baseline_missing_invalid_duplicate_and_run_binding() {
        use std::{
            fs,
            os::unix::fs::{PermissionsExt, symlink},
        };
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(load(temp.path(), true).unwrap().is_none());
        assert!(load(temp.path(), false).is_err());
        capture(temp.path(), sample(1222)).unwrap();
        assert_eq!(
            load(temp.path(), false).unwrap().unwrap().sample,
            sample(1222)
        );
        assert!(capture(temp.path(), sample(1000)).is_err());
        let other = tempfile::tempdir().unwrap();
        fs::set_permissions(other.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::copy(temp.path().join(FILE), other.path().join(FILE)).unwrap();
        assert!(load(other.path(), true).is_err());
        fs::write(temp.path().join(FILE), b"{}").unwrap();
        assert!(load(temp.path(), true).is_err());
        fs::remove_file(temp.path().join(FILE)).unwrap();
        symlink("missing", temp.path().join(FILE)).unwrap();
        assert!(load(temp.path(), true).is_err());
    }
}
