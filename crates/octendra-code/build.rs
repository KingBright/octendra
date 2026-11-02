//! Build script configuring macOS Swift Concurrency runtime library search path.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
        if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64") {
            let output =
                std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo output"));
            let sdk = std::process::Command::new("xcrun")
                .arg("--show-sdk-path")
                .output()
                .expect("Xcode SDK");
            assert!(sdk.status.success(), "Xcode SDK unavailable");
            let sdk = String::from_utf8(sdk.stdout).expect("SDK path");
            let sdk = sdk.trim();
            let status = std::process::Command::new("xcrun")
                .args([
                    "swiftc",
                    "-parse-as-library",
                    "-swift-version",
                    "5",
                    "-O",
                    "-emit-library",
                    "-static",
                    "-module-name",
                    "OctendraVM",
                    "-target",
                    "arm64-apple-macos13.0",
                    "-sdk",
                    sdk,
                    "src/macos_vm/host.swift",
                    "src/macos_vm/control.swift",
                    "-o",
                ])
                .arg(output.join("libOctendraVM.a"))
                .status()
                .expect("Swift compiler");
            assert!(status.success(), "native VM module compilation failed");
            println!("cargo:rustc-link-search=native={}", output.display());
            println!("cargo:rustc-link-search=native={sdk}/usr/lib/swift");
            println!("cargo:rustc-link-lib=static=OctendraVM");
            println!("cargo:rustc-link-lib=framework=Virtualization");
            println!("cargo:rustc-link-lib=framework=AppKit");
            println!("cargo:rustc-link-lib=framework=ScreenCaptureKit");
            println!("cargo:rerun-if-changed=src/macos_vm/host.swift");
            println!("cargo:rerun-if-changed=src/macos_vm/control.swift");
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
