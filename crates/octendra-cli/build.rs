//! Build script configuring macOS Swift Concurrency runtime library search path.

fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
