//! The pinned device model is installed only through the explicit model command.

use std::fs::File;
use std::io::{self, Read, Seek};
use std::path::{Path, PathBuf};

use kio_core::control::ControlHandle;
use kio_core::error::{Resource, ResourceLimit};
use kio_core::store_dir::{Publication, StoreDirectory, hash_regular_file};
use kio_core::{ExitCode, KioError, Result};

use crate::commands::ModelCommand;
use crate::context::AppContext;
use crate::response::model::{ModelReadiness, ModelResponse};
use crate::response::{CommandResponse, Completion, ResponseBody};

use kio_core::eg2::{MODEL, MODEL_SHA256, REVISION};

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
            sha256: MODEL_SHA256,
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
        path: &Path,
        readiness: ModelReadiness,
        reason: &'static str,
        runtime_version: Option<String>,
    ) -> CommandResponse {
        CommandResponse {
            body: ResponseBody::Model(ModelResponse {
                model: MODEL,
                selected_profile: None,
                profile_hash: None,
                model_revision: REVISION,
                model_hash: self.sha256,
                runtime_version,
                readiness,
                reason,
                path: path.display().to_string(),
                reembedding_count: None,
                producers: Vec::new(),
                mixed_producers: false,
            }),
            completion: Completion::Complete,
        }
    }

    fn open_installed(
        &self,
        base: &Path,
        control: &ControlHandle,
    ) -> Result<Option<(File, Option<ModelMismatch>)>> {
        let directory = self.directory(base);
        let Some(installed) = crate::private_state::open_readonly(&directory, base, control)?
        else {
            return Ok(None);
        };
        if !installed.contains_entry(Path::new(ARTIFACT))? {
            return Ok(None);
        }
        let mut file = verified_model_file(&installed)?;
        let mismatch = self.mismatch(&mut file, &directory.join(ARTIFACT))?;
        Ok(Some((file, mismatch)))
    }

    /// Returns a mismatch reason for a regular file whose bytes fail the pin.
    fn mismatch(&self, file: &mut File, path: &Path) -> Result<Option<ModelMismatch>> {
        if file
            .metadata()
            .map_err(|error| file_error(path, error))?
            .len()
            != self.bytes
        {
            return Ok(Some(ModelMismatch::Size));
        }
        let (digest, _) = hash_regular_file(file, self.bytes)?;
        Ok((digest != self.sha256).then_some(ModelMismatch::Hash))
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
            use crate::eg2::WorkerError;
            let Some((_, mismatch)) = pin.open_installed(&base, control)? else {
                return Ok(missing(&pin, &path));
            };
            if let Some(mismatch) = mismatch {
                return Ok(pin.response(
                    &path,
                    ModelReadiness::HashMismatch,
                    mismatch.message(),
                    None,
                ));
            }
            let (readiness, reason, version) = match crate::eg2::verify_runtime() {
                Ok(()) => (
                    ModelReadiness::Ready,
                    "model and runtime SHA-256 verified",
                    Some(kio_core::litert::VERSION.to_owned()),
                ),
                Err(WorkerError::RuntimeMissing) => {
                    (ModelReadiness::Missing, "runtime_missing", None)
                }
                Err(WorkerError::RuntimeHashMismatch) => {
                    (ModelReadiness::HashMismatch, "runtime_hash_mismatch", None)
                }
                Err(WorkerError::RuntimeUnsupported) => {
                    (ModelReadiness::Missing, "runtime_unsupported_target", None)
                }
                Err(_) => (ModelReadiness::Missing, "runtime_unavailable", None),
            };
            return Ok(pin.response(&path, readiness, reason, version));
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
                    return Err(ModelMismatch::Size.error());
                }
                let installed =
                    crate::private_state::ensure_private_directory(&directory, &base, control)?;
                import_file(&installed, &pin, &mut file)?;
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
        &path,
        ModelReadiness::Ready,
        "model size and SHA-256 verified",
        None,
    ))
}

/// Report an absent artifact without loading weights or opening a worker.
pub(crate) fn engine_model_absent(control: &ControlHandle) -> bool {
    let base = crate::settings::data_home(control);
    matches!(
        Pin::current(control)
            .directory(&base)
            .join(ARTIFACT)
            .try_exists(),
        Ok(false)
    )
}

pub(crate) fn open_engine_model(
    control: &ControlHandle,
) -> std::result::Result<(PathBuf, File), crate::eg2::WorkerError> {
    use crate::eg2::WorkerError;
    let base = crate::settings::data_home(control);
    if !base.is_absolute() {
        return Err(WorkerError::ModelUnavailable);
    }
    let pin = Pin::current(control);
    let (file, mismatch) = pin
        .open_installed(&base, control)
        .map_err(|_| WorkerError::ModelUnavailable)?
        .ok_or(WorkerError::ModelMissing)?;
    if mismatch.is_some() {
        return Err(WorkerError::ModelHashMismatch);
    }
    Ok((pin.directory(&base).join(ARTIFACT), file))
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
        return Err(consent_error(
            "model installation requires confirmation or --yes",
        ));
    }
    if ctx
        .interaction
        .confirm("Install this model on this device? [y/N]")?
    {
        Ok(())
    } else {
        Err(consent_error("model installation declined"))
    }
}

fn missing(pin: &Pin, path: &Path) -> CommandResponse {
    pin.response(
        path,
        ModelReadiness::Missing,
        "model is not installed; run kio model install embeddinggemma-2",
        None,
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

fn import_file(installed: &StoreDirectory, pin: &Pin, file: &mut File) -> Result<()> {
    let published = installed.write_atomic_stream_preserving_source(
        Path::new(ARTIFACT),
        Publication::Upsert,
        file,
        |staged| stage_model(installed, pin, &mut &*file, staged),
    )?;
    if !published {
        kio_core::private_fs::verify_private_regular_file_handle(file)?;
        verify_pin(pin, file, &installed.path().join(ARTIFACT))?;
    }
    Ok(())
}

fn publish(directory: &StoreDirectory, pin: &Pin, source: &mut impl Read) -> Result<()> {
    directory.write_atomic_stream(Path::new(ARTIFACT), Publication::Upsert, |staged| {
        stage_model(directory, pin, source, staged)
    })
}

fn stage_model(
    directory: &StoreDirectory,
    pin: &Pin,
    source: &mut impl Read,
    staged: &mut File,
) -> Result<()> {
    let path = directory.path().join(ARTIFACT);
    #[cfg(debug_assertions)]
    if directory.control().debug().cli.model_write_enospc {
        io::copy(&mut source.take(1), staged).map_err(|error| file_error(&path, error))?;
        return Err(file_error(&path, io::ErrorKind::StorageFull.into()));
    }
    // Bound staging even when the source has no trustworthy length.
    io::copy(&mut source.take(pin.bytes + 1), staged).map_err(|error| file_error(&path, error))?;
    staged.rewind().map_err(|error| file_error(&path, error))?;
    verify_pin(pin, staged, &path)
}

fn verify_pin(pin: &Pin, file: &mut File, path: &Path) -> Result<()> {
    match pin.mismatch(file, path)? {
        Some(mismatch) => Err(mismatch.error()),
        None => Ok(()),
    }
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

fn consent_error(message: &str) -> KioError {
    KioError::new(
        "KIO-E-MODEL-CONSENT-001",
        message,
        serde_json::json!({"reason": "model_consent_required"}),
        ExitCode::ConfirmationRejected,
    )
}

enum ModelMismatch {
    Size,
    Hash,
}

impl ModelMismatch {
    fn message(&self) -> &'static str {
        match self {
            Self::Size => "model size differs from the pinned size",
            Self::Hash => "model SHA-256 differs from the pinned hash",
        }
    }

    fn error(&self) -> KioError {
        let (code, reason) = match self {
            Self::Size => ("KIO-E-MODEL-INTEGRITY-001", "model_size_mismatch"),
            Self::Hash => ("KIO-E-MODEL-HASH-001", "model_hash_mismatch"),
        };
        KioError::new(
            code,
            self.message(),
            serde_json::json!({"reason": reason}),
            ExitCode::PermanentFailure,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_decline_returns_the_model_consent_error() {
        struct Decline;
        impl crate::context::Interaction for Decline {
            fn confirm(&self, prompt: &str) -> Result<bool> {
                assert!(prompt.contains("[y/N]"));
                Ok(false)
            }
            fn read_input(&self, _: usize) -> Result<Vec<u8>> {
                unreachable!("confirmation uses the device prompt")
            }
            fn is_interactive(&self) -> bool {
                true
            }
            fn diagnostic(&self, _: &str) {}
            fn write_diagnostic(&self, _: &str) -> Result<()> {
                Ok(())
            }
            fn flush_diagnostics(&self) -> Result<()> {
                Ok(())
            }
        }

        let ctx = AppContext {
            working_directory: PathBuf::from("."),
            interaction: std::sync::Arc::new(Decline),
        };
        let pin = Pin::current(&ControlHandle::default());
        let error = confirm(&ctx, None, false, SOURCE, &pin, Path::new(ARTIFACT)).unwrap_err();
        assert_eq!(error.exit_code().code(), 9);
        assert_eq!(error.error_code(), "KIO-E-MODEL-CONSENT-001");
        assert_eq!(error.context()["reason"], "model_consent_required");
    }

    #[cfg(unix)]
    #[test]
    fn import_preserves_the_retained_source_under_its_other_name() {
        use kio_core::store_dir::ATOMIC_WORKSPACE_DIR;
        use std::{io::Write as _, os::unix::fs::MetadataExt as _};

        for target_is_stage in [false, true] {
            let root = crate::test_support::PrivateTempDir::new();
            let installed = StoreDirectory::open(root.path()).unwrap();
            let source_path = root.path().join("other-name");
            let mut source = installed
                .create_private_file(Path::new("other-name"))
                .unwrap();
            source.write_all(b"test model").unwrap();
            source.rewind().unwrap();

            let artifact = root.path().join(ARTIFACT);
            let protected_path = if target_is_stage {
                installed
                    .create_directory(Path::new(ATOMIC_WORKSPACE_DIR))
                    .unwrap();
                root.path().join(ATOMIC_WORKSPACE_DIR).join("write")
            } else {
                artifact.clone()
            };
            // Use distinct path and handle names without requiring a privileged
            // bind mount. The already-open file is the caller's authority.
            let inode = source.metadata().unwrap().ino();
            std::fs::rename(&source_path, &protected_path).unwrap();
            let pin = Pin {
                bytes: 10,
                sha256: "e6a8036e9cb9f8c9f52ffd985c4d6f0498d94e6919f4c67b6423da0176e549fa",
            };
            let result = import_file(&installed, &pin, &mut source);
            if target_is_stage {
                assert!(
                    result.is_err(),
                    "recovery must reject the retained import source"
                );
                assert!(!artifact.exists());
            } else {
                result.unwrap();
            }
            assert_eq!(std::fs::metadata(&protected_path).unwrap().ino(), inode);
            assert_eq!(std::fs::read(&protected_path).unwrap(), b"test model");
        }
    }
}
