use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=native/Gliff.swift");
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
