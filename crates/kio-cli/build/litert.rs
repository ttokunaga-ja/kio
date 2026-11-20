#[path = "../../kio-core/src/litert.rs"]
mod pins;

use std::error::Error;
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use zip::{ZipArchive, ZipWriter, write::SimpleFileOptions};

pub(crate) fn stage(target: &str) -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=../kio-core/src/litert.rs");
    println!("cargo:rerun-if-changed=build/litert.rs");
    println!("cargo:rerun-if-env-changed=KIO_LITERT_C_API_ZIP");
    println!("cargo:rerun-if-env-changed=CARGO_NET_OFFLINE");
    let Some(bundle) = pins::Bundle::for_target(target) else {
        return Ok(());
    };
    let out = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("missing OUT_DIR")?);
    // Cargo's default layout is <root>/<profile>/build/<unit>/out, with
    // the kio executable in <profile>. Release packaging relocates it to bin/.
    let root = out
        .ancestors()
        .nth(4)
        .ok_or("unexpected Cargo output directory")?;
    let staged = root.join(bundle.directory());
    let archive = match std::env::var_os("KIO_LITERT_C_API_ZIP") {
        Some(path) => {
            let path = PathBuf::from(path);
            if !path.is_absolute() || !path.is_file() {
                return Err("KIO_LITERT_C_API_ZIP must name an existing absolute ZIP path".into());
            }
            println!("cargo:rerun-if-changed={}", path.display());
            path
        }
        None => download(&out)?,
    };
    let mut file = File::open(&archive)?;
    if file.metadata()?.len() != pins::ARCHIVE_BYTES {
        return Err("LiteRT-LM ZIP size differs from the pin".into());
    }
    if pins::sha256(&mut (&mut file).take(pins::ARCHIVE_BYTES + 1))? != pins::ARCHIVE_SHA256 {
        return Err("LiteRT-LM ZIP SHA-256 differs from the pin".into());
    }
    file.rewind()?;
    let mut zip = ZipArchive::new(file)?;
    fs::create_dir_all(&staged)?;

    for library in bundle.libraries() {
        let temporary = out.join(library.name());
        let mut file = File::create(&temporary)?;
        io::copy(&mut zip.by_name(library.member)?, &mut file)?;
        file.flush()?;
        drop(file);
        if pins::sha256(&mut File::open(&temporary)?)? != library.sha256 {
            return Err(format!(
                "LiteRT-LM member {} SHA-256 differs from the pin",
                library.member
            )
            .into());
        }
        fs::rename(temporary, staged.join(library.name()))?;
    }
    let license = out.join(pins::LICENSE);
    io::copy(
        &mut zip.by_name(pins::LICENSE)?,
        &mut File::create(&license)?,
    )?;
    fs::rename(license, staged.join(pins::LICENSE))?;

    let notice = out.join(pins::THIRD_PARTY_LICENSES);
    let mut licenses = ZipWriter::new(File::create(&notice)?);
    let mut names: Vec<_> = zip
        .file_names()
        .filter(|name| name.starts_with("licenses/") && !name.ends_with('/'))
        .map(str::to_owned)
        .collect();
    names.sort_unstable();
    for name in names {
        licenses.start_file(&name, SimpleFileOptions::default())?;
        io::copy(&mut zip.by_name(&name)?, &mut licenses)?;
    }
    licenses.finish()?.flush()?;
    fs::rename(notice, staged.join(pins::THIRD_PARTY_LICENSES))?;

    for name in bundle.names() {
        println!("cargo:rerun-if-changed={}", staged.join(name).display());
    }
    Ok(())
}

fn download(out: &Path) -> Result<PathBuf, Box<dyn Error>> {
    if std::env::var("CARGO_NET_OFFLINE").is_ok_and(|value| value == "true" || value == "1") {
        return Err(
            "offline build requires KIO_LITERT_C_API_ZIP to point to the pinned ZIP".into(),
        );
    }
    // OUT_DIR belongs to this Cargo build unit. In particular, concurrent
    // release builds have independent download and extraction work areas.
    let path = out.join("litert_lm_c_api-1.0.0.zip");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(300)))
        .https_only(true)
        .build()
        .into();
    let response = agent.get(pins::ARCHIVE_URL).call()?;
    let mut file = File::create(&path)?;
    io::copy(
        &mut response
            .into_body()
            .into_reader()
            .take(pins::ARCHIVE_BYTES + 1),
        &mut file,
    )?;
    file.flush()?;
    Ok(path)
}
