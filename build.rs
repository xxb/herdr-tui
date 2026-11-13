#[path = "src/version.rs"]
mod version;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn emit_rerun_if_exists(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn run_git(manifest_dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(manifest_dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn register_rerun_triggers(manifest_dir: &Path) {
    println!("cargo:rerun-if-env-changed=HCT_GIT_DESCRIBE");
    println!("cargo:rerun-if-env-changed=HCT_GIT_SHA");

    for rel in [
        "build.rs",
        "Cargo.toml",
        "Cargo.lock",
        "src",
        "flake.nix",
        "README.md",
        "CHANGELOG.md",
        "AGENTS.md",
        "LICENSE",
        ".gitignore",
        "release-please-config.json",
        ".release-please-manifest.json",
        ".github",
    ] {
        emit_rerun_if_exists(&manifest_dir.join(rel));
    }

    let Some(git_dir_raw) = run_git(manifest_dir, &["rev-parse", "--git-dir"]) else {
        return;
    };
    let git_dir = manifest_dir.join(git_dir_raw);
    let common_dir = run_git(manifest_dir, &["rev-parse", "--git-common-dir"])
        .map(|raw| manifest_dir.join(raw))
        .unwrap_or_else(|| git_dir.clone());

    let head_path = git_dir.join("HEAD");
    emit_rerun_if_exists(&head_path);
    emit_rerun_if_exists(&git_dir.join("index"));
    emit_rerun_if_exists(&common_dir.join("packed-refs"));
    emit_rerun_if_exists(&common_dir.join("refs").join("heads"));
    emit_rerun_if_exists(&common_dir.join("refs").join("tags"));

    if let Ok(head_contents) = fs::read_to_string(&head_path) {
        if let Some(rel_ref) = head_contents.trim().strip_prefix("ref: ") {
            let ref_path = common_dir.join(rel_ref.trim());
            emit_rerun_if_exists(&ref_path);
        }
    }
}

fn non_empty_env(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn main() {
    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    register_rerun_triggers(&manifest_dir);

    let pkg_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let env_describe = non_empty_env("HCT_GIT_DESCRIBE");
    let env_sha = non_empty_env("HCT_GIT_SHA");

    let (git_describe, git_sha) = if env_describe.is_some() || env_sha.is_some() {
        (env_describe, env_sha)
    } else {
        let describe = run_git(
            &manifest_dir,
            &[
                "describe", "--tags", "--always", "--dirty", "--match", "v[0-9]*",
            ],
        );
        let sha = run_git(&manifest_dir, &["rev-parse", "--short=7", "HEAD"]);
        (describe, sha)
    };

    let build_version =
        version::format_build_version(&pkg_version, git_describe.as_deref(), git_sha.as_deref());
    println!("cargo:rustc-env=HCT_BUILD_VERSION={build_version}");
}
