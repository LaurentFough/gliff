use std::{env, path::PathBuf, process::Command};

//= build.rs v1.1.0 — the native Swift helper (macOS) and GLIFF_BUILD, the
//= build ID shown in Gliff ▸ About Gliff: `<commit>[-dirty] <arch>`, or
//= GLIFF_BUILD_ID when an installer sets it, or "unknown" without git.
fn build_id() -> String {
    if let Ok(id) = env::var("GLIFF_BUILD_ID") {
        if !id.trim().is_empty() {
            return id.trim().to_string();
        }
    }
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    match git(&["rev-parse", "--short=10", "HEAD"]) {
        Some(commit) if !commit.is_empty() => {
            //= Tracked changes only; untracked files do not mark a build dirty.
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                .is_some_and(|s| !s.is_empty());
            format!("{commit}{} {arch}", if dirty { "-dirty" } else { "" })
        }
        _ => format!("unknown {arch}"),
    }
}

fn main() {
    println!("cargo:rerun-if-changed=native/Gliff.swift");
    //= A new commit gives a new build ID: watch HEAD and the branch it
    //= names (not the index, which `git status` touches; so "-dirty" can lag
    //= until the next commit or Swift change, and a clean rebuild refreshes it).
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/packed-refs");
    if let Ok(head) = std::fs::read_to_string("../../.git/HEAD") {
        if let Some(branch) = head.trim().strip_prefix("ref: ") {
            println!("cargo:rerun-if-changed=../../.git/{branch}");
        }
    }
    println!("cargo:rerun-if-env-changed=GLIFF_BUILD_ID");
    println!("cargo:rustc-env=GLIFF_BUILD={}", build_id());
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => panic!("unsupported macOS architecture: {other}"),
    };
    let dest = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("gliff-native");
    let status = Command::new("xcrun")
        .args(["swiftc", "-O", "-swift-version", "5", "-target"])
        .arg(format!("{arch}-apple-macosx13.0"))
        .args(["native/Gliff.swift", "-o"])
        .arg(&dest)
        .status()
        .expect("macOS builds need Xcode Command Line Tools (xcode-select --install)");
    assert!(
        status.success(),
        "could not compile the macOS native bridge"
    );
}
