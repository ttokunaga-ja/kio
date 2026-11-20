//! The pinned device model is installed only through the explicit model command.

use std::fs::File;
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};

use kio_core::control::ControlHandle;
use kio_core::error::{Resource, ResourceLimit};
use kio_core::store_dir::{ATOMIC_WORKSPACE_DIR, Publication, StoreDirectory, hash_regular_file};
use kio_core::{ExitCode, KioError, Result};

use crate::commands::ModelCommand;
use crate::context::AppContext;
use crate::response::model::{ModelReadiness, ModelResponse};
use crate::response::{CommandResponse, Completion, ResponseBody};

const MODEL: &str = "embeddinggemma-2";
const REVISION: &str = "e301f74d5551b0c2641bd5cb4652a76239d5c5f8";
const ARTIFACT: &str = "embeddinggemma-2-text-vision-440m.litertlm";
const SOURCE: &str =
    "https://huggingface.co/litert-community/embeddinggemma-2-text-vision-440m-litert-lm";

struct Pin {
    bytes: u64,
    sha256: &'static str,
}

impl Pin {
    fn current(control: &ControlHandle) -> Self {
        #[cfg(debug_assertions)]
        if !control.debug().adapters.model_source.is_unset() {
            return Self {
                bytes: 10,
                sha256: "e6a8036e9cb9f8c9f52ffd985c4d6f0498d94e6919f4c67b6423da0176e549fa",
            };
        }
        let _ = control;
        Self {
            bytes: 387_710_976,
            sha256: "92dcbea108899e5d6e30d919b0744f90d9967e80c67a4ab5503ac16d54f62eb0",
        }
    }

    fn directory(&self, base: &Path) -> PathBuf {
        base.join("kio/models")
            .join(MODEL)
            .join(REVISION)
            .join(self.sha256)
    }

    fn response(
        &self,
        path: PathBuf,
        readiness: ModelReadiness,
        reason: &'static str,
    ) -> CommandResponse {
        CommandResponse {
            body: ResponseBody::Model(ModelResponse {
                model: MODEL,
                selected_profile: None,
                profile_hash: None,
                model_revision: REVISION,
                model_hash: self.sha256,
                runtime_version: None,
                readiness,
                reason,
                path,
                reembedding_count: None,
                producers: Vec::new(),
                mixed_producers: false,
            }),
            completion: Completion::Complete,
        }
    }

    /// Returns a mismatch reason for a regular file whose bytes fail the pin.
    fn mismatch(&self, file: &mut File, path: &Path) -> Result<Option<&'static str>> {
        if file
            .metadata()
            .map_err(|error| file_error(path, error))?
            .len()
            != self.bytes
        {
            return Ok(Some("model size differs from the pinned size"));
        }
        let (digest, _) = hash_regular_file(file, self.bytes)?;
        Ok((digest != self.sha256).then_some("model SHA-256 differs from the pinned hash"))
    }
}

pub(crate) fn run(
    ctx: &AppContext,
    command: ModelCommand,
    control: &ControlHandle,
) -> Result<CommandResponse> {
    let base = crate::settings::data_home(control);
    let pin = Pin::current(control);
    let directory = pin.directory(&base);
    let path = directory.join(ARTIFACT);
    match command {
        ModelCommand::Status => {
            let installed = crate::private_state::open_readonly(&directory, &base, control)?;
            let Some(installed) = installed else {
                return Ok(missing(&pin, path));
            };
            let leaf = Path::new(ARTIFACT);
            if !installed.contains_entry(leaf)? {
                return Ok(missing(&pin, path));
            }
            let mut file = verified_model_file(&installed)?;
            if let Some(reason) = pin.mismatch(&mut file, &path)? {
                return Ok(pin.response(path, ModelReadiness::HashMismatch, reason));
            }
        }
        ModelCommand::Install { from, yes, offline } => {
            if offline && from.is_none() {
                return Err(KioError::invalid_usage(
                    "model download is unavailable in offline mode; use --from <file>",
                ));
            }
            let source_url = format!("{SOURCE}/resolve/{REVISION}/{ARTIFACT}");
            confirm(ctx, from.as_deref(), yes, &source_url, &pin, &path)?;
            if let Some(from) = from {
                let source = ctx.cwd().join(from);
                let source = source
                    .canonicalize()
                    .map_err(|error| file_error(&source, error))?;
                let (mut file, bytes) = open_import(&source)?;
                if bytes != pin.bytes {
                    return Err(integrity_error("model size differs from the pinned size"));
                }
                let installed =
                    crate::private_state::ensure_private_directory(&directory, &base, control)?;
                // An import may point to the installed file itself through a
                // user-supplied alias. Keep its existing binding intact.
                if path.canonicalize().is_ok_and(|target| source == target) {
                    let mut existing = verified_model_file(&installed)?;
                    if let Some(reason) = pin.mismatch(&mut existing, &path)? {
                        return Err(integrity_error(reason));
                    }
                } else {
                    if directory
                        .join(ATOMIC_WORKSPACE_DIR)
                        .canonicalize()
                        .is_ok_and(|workspace| source.starts_with(workspace))
                    {
                        return Err(KioError::invalid_usage(
                            "model import source must be outside the installation staging directory",
                        ));
                    }
                    publish(&installed, &pin, &mut file)?;
                }
            } else {
                let mut body = kio_adapter::model_download::open(&source_url, pin.bytes, control)
                    .map_err(|error| {
                    KioError::new(
                        "KIO-E-MODEL-DOWNLOAD-001",
                        error.to_string(),
                        serde_json::json!({}),
                        ExitCode::Failure,
                    )
                })?;
                let installed =
                    crate::private_state::ensure_private_directory(&directory, &base, control)?;
                publish(&installed, &pin, &mut body)?;
            }
        }
    }
    Ok(pin.response(
        path,
        ModelReadiness::Ready,
        "model size and SHA-256 verified",
    ))
}

fn confirm(
    ctx: &AppContext,
    from: Option<&Path>,
    yes: bool,
    source: &str,
    pin: &Pin,
    path: &Path,
) -> Result<()> {
    let action = match from {
        Some(file) => format!("Import from {file:?}"),
        None => "Download from the official source".to_owned(),
    };
    ctx.interaction.write_diagnostic(&format!(
        "{action}\nOfficial source: {source}\nRevision: {REVISION}\nSize: {} bytes\nSHA-256: {}\nInstall to: {path:?}\nLicense: Apache-2.0 (https://www.apache.org/licenses/LICENSE-2.0)\nGoogle states that deployments must adhere to the Gemma Prohibited Use Policy (https://ai.google.dev/gemma/prohibited_use_policy)\nGoogle model card: https://ai.google.dev/gemma/docs/embeddinggemma/model_card_2\n",
        pin.bytes, pin.sha256,
    ))?;
    ctx.interaction.flush_diagnostics()?;
    if yes {
        return Ok(());
    }
    if !ctx.interaction.is_interactive() {
        return Err(KioError::invalid_usage(
            "model installation requires confirmation or --yes",
        ));
    }
    if ctx
        .interaction
        .confirm("Install this model on this device? [y/N]")?
    {
        Ok(())
    } else {
        Err(KioError::invalid_usage("model installation declined"))
    }
}

fn missing(pin: &Pin, path: PathBuf) -> CommandResponse {
    pin.response(
        path,
        ModelReadiness::Missing,
        "model is not installed; run kio model install embeddinggemma-2",
    )
}

fn verified_model_file(directory: &StoreDirectory) -> Result<File> {
    let file = directory.open_regular_read(Path::new(ARTIFACT), u64::MAX)?;
    kio_core::private_fs::verify_private_regular_file_handle(&file)?;
    Ok(file)
}

fn open_import(path: &Path) -> Result<(File, u64)> {
    let regular_only = || KioError::invalid_usage("model import source must be a regular file");
    if !path
        .metadata()
        .map_err(|error| file_error(path, error))?
        .is_file()
    {
        return Err(regular_only());
    }
    let mut options = File::options();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|error| file_error(path, error))?;
    let metadata = file.metadata().map_err(|error| file_error(path, error))?;
    if !metadata.is_file() {
        return Err(regular_only());
    }
    Ok((file, metadata.len()))
}

fn publish(directory: &StoreDirectory, pin: &Pin, source: &mut impl Read) -> Result<()> {
    let leaf = Path::new(ARTIFACT);
    // The shared atomic workspace serializes installers, discards abandoned
    // staging on retry, and renames only after the callback and sync succeed.
    directory.write_atomic_stream(leaf, Publication::Upsert, |staged| {
        let path = directory.path().join(leaf);
        // Bound staging even when the source has no trustworthy length.
        io::copy(&mut source.take(pin.bytes + 1), staged)
            .map_err(|error| file_error(&path, error))?;
        staged.rewind().map_err(|error| file_error(&path, error))?;
        if let Some(reason) = pin.mismatch(staged, &path)? {
            return Err(integrity_error(reason));
        }
        Ok(())
    })
}

fn file_error(path: &Path, error: io::Error) -> KioError {
    if error.kind() == io::ErrorKind::StorageFull {
        return ResourceLimit {
            resource: Resource::Storage,
            stage: "model_install".into(),
            limit: None,
            observed: None,
            resource_tier: None,
            page_progress: None,
        }
        .into_error(error.to_string());
    }
    KioError::io(error.to_string(), path.display().to_string())
}

fn integrity_error(reason: &str) -> KioError {
    KioError::new(
        "KIO-E-MODEL-INTEGRITY-001",
        reason,
        serde_json::json!({}),
        ExitCode::PermanentFailure,
    )
}
