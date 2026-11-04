use std::{env, path::PathBuf, process::Command};

//= build.rs v1.3.0 — the native Swift helper (macOS) and GLIFF_BUILD, the
//= build ID shown in Gliff ▸ About Gliff: `<commit>[-dirty]+<arch>`, or
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
    //= The same form as bin/lib/macos-install.sh (gliff_build_id), so an
    //= installer's ID and this fallback look alike: COMMIT[-dirty]+ARCH.
    let arch = match env::var("CARGO_CFG_TARGET_ARCH")
        .unwrap_or_default()
        .as_str()
    {
        "aarch64" => "arm64".to_string(),
        other => other.to_string(),
    };
    match git(&["rev-parse", "--short=12", "HEAD"]) {
        Some(commit) if !commit.is_empty() => {
            //= Tracked changes to the code only (CODE): untracked files and
            //= docs do not change the binary, so they do not mark it dirty.
            //= `:/` names a path from the repository root (this runs in the crate).
            let specs: Vec<String> = CODE
                .iter()
                .map(|p| format!(":/{}", p.trim_start_matches("../../")))
                .collect();
            let mut args = vec!["status", "--porcelain", "--untracked-files=no", "--"];
            args.extend(specs.iter().map(String::as_str));
            let dirty = git(&args).is_some_and(|s| !s.is_empty());
            format!("{commit}{}+{arch}", if dirty { "-dirty" } else { "" })
        }
        _ => format!("unknown+{arch}"),
    }
}

/// What goes into the binary, relative to this crate: a change here
/// re-runs this script, so "-dirty" follows the code at once (it used to lag
/// until the next commit). Not the git index, which `git status` touches.
//= The same list as bin/lib/build-id.sh (installers and packages).
const CODE: &[&str] = &[
    "../../crates",
    "../../Cargo.toml",
    "../../Cargo.lock",
    "../../bin",
    "../../pkgbuild",
];

/// FNV-1a over the Swift source and the target, to skip an unchanged
/// helper: the script now re-runs on any code change, and swiftc is slow.
fn fingerprint(parts: &[&[u8]]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.iter().chain(&[0xff]) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

fn main() {
    println!("cargo:rerun-if-changed=native/Gliff.swift");
    for path in CODE {
        println!("cargo:rerun-if-changed={path}");
    }
    //= A new commit gives a new build ID: watch HEAD and the branch it
    //= names.
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
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    let dest = out.join("gliff-native");
    let stamp = out.join("gliff-native.fingerprint");
    let source = std::fs::read("native/Gliff.swift").expect("native/Gliff.swift");
    let target = format!("{arch}-apple-macosx13.0");
    let print = fingerprint(&[&source, target.as_bytes()]);
    if dest.is_file() && std::fs::read_to_string(&stamp).is_ok_and(|s| s == print) {
        //= The helper is current: only the build ID was refreshed.
        return;
    }
    let status = Command::new("xcrun")
        .args(["swiftc", "-O", "-swift-version", "5", "-target"])
        .arg(&target)
        .args(["native/Gliff.swift", "-o"])
        .arg(&dest)
        .status()
        .expect("macOS builds need Xcode Command Line Tools (xcode-select --install)");
    assert!(
        status.success(),
        "could not compile the macOS native bridge"
    );
    let _ = std::fs::write(&stamp, print);
}
