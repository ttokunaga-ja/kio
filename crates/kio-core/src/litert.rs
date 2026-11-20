//! Pins shared by the build, release verifier and worker. Paths are relative
//! to the distribution root, which contains sibling `bin` and `lib` trees.

pub const VERSION: &str = "0.19.1";
pub const ARCHIVE_URL: &str = "https://github.com/google-ai-edge/LiteRT-LM/releases/download/v0.19.1/litert_lm_c_api-1.0.0.zip";
pub const ARCHIVE_SHA256: &str = "1d430d80eaf536f696a4922270ba55eb3d112189ac198c3066d487a45e3b4d4c";
pub const ARCHIVE_BYTES: u64 = 123_469_788;
pub const LICENSE: &str = "LICENSE";
pub const THIRD_PARTY_LICENSES: &str = "third-party-licenses.zip";

#[derive(Clone, Copy)]
pub struct Library {
    pub member: &'static str,
    pub sha256: &'static str,
}

impl Library {
    pub fn name(self) -> &'static str {
        self.member.rsplit('/').next().expect("pinned library name")
    }
}

pub struct Bundle {
    pub target: &'static str,
    pub library: Library,
    pub import_library: Option<Library>,
}

impl Bundle {
    pub fn for_target(target: &str) -> Option<Self> {
        let (target, member, sha256) = match target {
            "x86_64-unknown-linux-gnu" => (
                "x86_64-unknown-linux-gnu",
                "lib/linux_x86_64/liblitert-lm.so",
                "1fa0e6a9b194124615a9445aca86f91857445e3a767d200a3d2670ea630d5365",
            ),
            "aarch64-apple-darwin" => (
                "aarch64-apple-darwin",
                "lib/macos_arm64/liblitert-lm.dylib",
                "9d0a396985b976b20a42dbbbce63756de1c1b374930ca76a8ff423209ce0c4e8",
            ),
            "x86_64-pc-windows-msvc" => (
                "x86_64-pc-windows-msvc",
                "lib/windows_x86_64/bin/litert-lm.dll",
                "442e411af57bf6cf36ebf11aba03504a02c6f6fbe405010150f87a877ad12a11",
            ),
            _ => return None,
        };
        Some(Self {
            target,
            library: Library { member, sha256 },
            import_library: (target == "x86_64-pc-windows-msvc").then_some(Library {
                member: "lib/windows_x86_64/lib/litert-lm.lib",
                sha256: "c5bd603ad06ac6eb4b6faa16d0b82690c8e437358e1a113f43b9719c6c85d095",
            }),
        })
    }

    pub fn directory(&self) -> String {
        format!("lib/litert-lm/{VERSION}/{}", self.target)
    }

    pub fn libraries(&self) -> impl Iterator<Item = Library> {
        std::iter::once(self.library).chain(self.import_library)
    }

    pub fn names(&self) -> impl Iterator<Item = &'static str> {
        self.libraries()
            .map(Library::name)
            .chain([LICENSE, THIRD_PARTY_LICENSES])
    }
}

/// SHA-256 of a bounded stream, used for the pinned archive and native image.
pub fn sha256(reader: &mut impl std::io::Read) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};

    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => hash.update(&buffer[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
