use std::{env, fs, path::PathBuf};

const RC_ENVIRONMENT: &[&str] = &[
    "KIO_RC_BUILD",
    "KIO_RC_VERSION",
    "KIO_RC_COMMIT_SHA",
    "KIO_RC_GIT_TREE",
    "KIO_RC_CARGO_LOCK_SHA256",
    "KIO_RC_TOOLCHAIN_SHA256",
    "KIO_RC_RUSTC_VERSION",
    "KIO_RC_TARGET",
    "KIO_RC_FEATURES",
    "KIO_RC_PROFILE",
    "KIO_RC_REPRO_RECIPE",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for name in RC_ENVIRONMENT {
        println!("cargo:rerun-if-env-changed={name}");
    }

    let package_version = required_cargo_value("CARGO_PKG_VERSION");
    let target = required_cargo_value("TARGET");
    let profile = required_cargo_value("PROFILE");

    let binding = match env::var("KIO_RC_BUILD") {
        Ok(value) if value == "1" => candidate_binding(&package_version, &target, &profile),
        Ok(value) => fail(format!(
            "KIO_RC_BUILD must be exactly `1` when set, got {value:?}"
        )),
        Err(env::VarError::NotPresent) => development_binding(&package_version, &target, &profile),
        Err(err) => fail(format!("could not read KIO_RC_BUILD: {err}")),
    };

    write_binding_source(&binding);
}

fn write_binding_source(binding: &str) {
    let out_dir = PathBuf::from(required_cargo_value("OUT_DIR"));
    let retained = format!("KIO_RC_BINDING_V2\n{binding}\nKIO_RC_BINDING_END_V2");
    let source = format!("static RELEASE_BINDING: &str = {retained:?};\n");
    fs::write(out_dir.join("release_binding_generated.rs"), source)
        .unwrap_or_else(|error| fail(format!("could not write release binding source: {error}")));
}

fn development_binding(version: &str, target: &str, profile: &str) -> String {
    require_safe("CARGO_PKG_VERSION", version);
    require_safe("TARGET", target);
    require_safe("PROFILE", profile);
    fixed_binding(&[
        ("schema", "2"),
        ("bound", "0"),
        ("version", version),
        ("target", target),
        ("profile", profile),
    ])
}

fn candidate_binding(package_version: &str, actual_target: &str, actual_profile: &str) -> String {
    let version = required_rc_value("KIO_RC_VERSION");
    if package_version != version {
        fail(format!(
            "KIO_RC_VERSION must equal Cargo package version {package_version}, got {version}"
        ));
    }
    if actual_profile != "release" {
        fail(format!(
            "candidate build requires Cargo PROFILE=release, got {actual_profile}"
        ));
    }

    let commit = required_rc_value("KIO_RC_COMMIT_SHA");
    let git_tree = required_rc_value("KIO_RC_GIT_TREE");
    let cargo_lock = required_rc_value("KIO_RC_CARGO_LOCK_SHA256");
    let toolchain = required_rc_value("KIO_RC_TOOLCHAIN_SHA256");
    let rustc_version = required_rc_value("KIO_RC_RUSTC_VERSION");
    let target = required_rc_value("KIO_RC_TARGET");
    let features = required_rc_value("KIO_RC_FEATURES");
    let profile = required_rc_value("KIO_RC_PROFILE");
    let repro_recipe = required_rc_value("KIO_RC_REPRO_RECIPE");

    for (name, value) in [
        ("KIO_RC_COMMIT_SHA", commit.as_str()),
        ("KIO_RC_GIT_TREE", git_tree.as_str()),
    ] {
        if !is_lowercase_hex(value, 40) {
            fail(format!(
                "{name} must be 40 lowercase hexadecimal characters"
            ));
        }
    }
    for (name, value) in [
        ("KIO_RC_CARGO_LOCK_SHA256", cargo_lock.as_str()),
        ("KIO_RC_TOOLCHAIN_SHA256", toolchain.as_str()),
    ] {
        if !is_lowercase_hex(value, 64) {
            fail(format!(
                "{name} must be 64 lowercase hexadecimal characters"
            ));
        }
    }
    if !rustc_version.starts_with("rustc 1.98.0 ") {
        fail(format!(
            "KIO_RC_RUSTC_VERSION must start with `rustc 1.98.0`, got {rustc_version:?}"
        ));
    }
    if target != actual_target {
        fail(format!(
            "KIO_RC_TARGET must equal Cargo TARGET ({actual_target}), got {target}"
        ));
    }
    if profile != actual_profile {
        fail(format!(
            "KIO_RC_PROFILE must equal Cargo PROFILE ({actual_profile}), got {profile}"
        ));
    }
    if features != "all-features" {
        fail(format!(
            "KIO_RC_FEATURES must be exactly `all-features`, got {features:?}"
        ));
    }
    if repro_recipe != repro_recipe_for_target(&target) {
        fail(format!(
            "KIO_RC_REPRO_RECIPE does not match target {target}, got {repro_recipe:?}"
        ));
    }

    fixed_binding(&[
        ("schema", "2"),
        ("bound", "1"),
        ("version", &version),
        ("commit", &commit),
        ("git_tree", &git_tree),
        ("cargo_lock_sha256", &cargo_lock),
        ("rust_toolchain_sha256", &toolchain),
        ("rustc_version", &rustc_version),
        ("target", &target),
        ("features", &features),
        ("profile", &profile),
        ("repro_recipe", &repro_recipe),
    ])
}

fn repro_recipe_for_target(target: &str) -> &'static str {
    match target {
        "x86_64-unknown-linux-gnu" => "linux-rustc-default-v1",
        "aarch64-apple-darwin" => "macos-rust-lld-no-uuid-macos11-sdk26.5-25F70-v2",
        "x86_64-pc-windows-msvc" => "windows-msvc-brepro-v1",
        _ => fail(format!("unsupported RC target {target}")),
    }
}

fn fixed_binding(entries: &[(&str, &str)]) -> String {
    entries
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn required_cargo_value(name: &str) -> String {
    match env::var(name) {
        Ok(value) => value,
        Err(err) => fail(format!("Cargo did not provide required {name}: {err}")),
    }
}

fn required_rc_value(name: &str) -> String {
    let value = match env::var(name) {
        Ok(value) => value,
        Err(err) => fail(format!("candidate build requires {name}: {err}")),
    };
    require_safe(name, &value);
    value
}

fn require_safe(name: &str, value: &str) {
    if value.is_empty() || value.contains(['\n', '\r', '\0', '=']) {
        fail(format!(
            "{name} must be non-empty and contain no newline, NUL, or `=`"
        ));
    }
}

fn is_lowercase_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn fail(message: String) -> ! {
    panic!("invalid Kio release binding: {message}");
}
