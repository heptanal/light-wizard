use std::{path::Path, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // Swift system libraries use @rpath install names. This is the stable
    // runtime location on macOS, including installations where those files
    // live in the dyld shared cache rather than visibly on disk.
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");

    // Swift-backed ScreenCaptureKit dependencies normally find their runtime
    // libraries through full Xcode. The standalone Apple Command Line Tools
    // place those libraries one directory higher, so discover the active
    // toolchain rather than requiring developers to change xcode-select.
    let Ok(output) = Command::new("xcrun").args(["--find", "swiftc"]).output() else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let swiftc = String::from_utf8_lossy(&output.stdout);
    let Some(toolchain_usr) = Path::new(swiftc.trim()).parent().and_then(Path::parent) else {
        return;
    };
    let swift_libs = toolchain_usr.join("lib/swift/macosx");
    if swift_libs.is_dir() {
        println!("cargo:rustc-link-search=native={}", swift_libs.display());
    }
}
