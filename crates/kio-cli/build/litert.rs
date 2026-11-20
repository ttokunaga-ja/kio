#[allow(dead_code)] // The shared bundle also exposes release-verifier metadata.
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
    let file = if let Some(path) = std::env::var_os("KIO_LITERT_C_API_ZIP") {
        let path = PathBuf::from(path);
        if !path.is_absolute() || !path.is_file() {
            return Err("KIO_LITERT_C_API_ZIP must name an existing absolute ZIP path".into());
        }
        println!("cargo:rerun-if-changed={}", path.display());
        open_pinned_archive(&path)?
    } else {
        open_cached_archive(root, &out)?
    };
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

    Ok(())
}

fn open_pinned_archive(path: &Path) -> Result<File, Box<dyn Error>> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() != pins::ARCHIVE_BYTES {
        return Err("LiteRT-LM ZIP size differs from the pin".into());
    }
    if pins::sha256(&mut (&mut file).take(pins::ARCHIVE_BYTES + 1))? != pins::ARCHIVE_SHA256 {
        return Err("LiteRT-LM ZIP SHA-256 differs from the pin".into());
    }
    file.rewind()?;
    Ok(file)
}

fn open_cached_archive(root: &Path, out: &Path) -> Result<File, Box<dyn Error>> {
    if std::env::var("CARGO_NET_OFFLINE").is_ok_and(|value| value == "true" || value == "1") {
        return Err(
            "offline build requires KIO_LITERT_C_API_ZIP to point to the pinned ZIP".into(),
        );
    }
    // The cache and staged assets are outputs. Watching either would dirty
    // the next Cargo invocation after staging. An explicit ZIP is an input.
    let cache = root.join(".kio-litert-cache");
    let archive = cache.join(format!("{}.zip", pins::ARCHIVE_SHA256));
    if let Ok(verified) = open_pinned_archive(&archive) {
        return Ok(verified);
    }
    fs::create_dir_all(cache)?;
    let partial = out.join("litert-download.zip");
    download(&partial)?;
    // Cache only verified bytes, then open the published name using the same
    // pin. Close the temporary input before renaming it on Windows.
    drop(open_pinned_archive(&partial)?);
    fs::rename(partial, &archive)?;
    open_pinned_archive(&archive)
}

fn download(path: &Path) -> Result<(), Box<dyn Error>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(90)))
        .https_only(true)
        .build()
        .into();
    let response = agent.get(pins::ARCHIVE_URL).call()?;
    let mut file = File::create(path)?;
    io::copy(
        &mut response
            .into_body()
            .into_reader()
            .take(pins::ARCHIVE_BYTES + 1),
        &mut file,
    )?;
    file.flush()?;
    Ok(())
}
