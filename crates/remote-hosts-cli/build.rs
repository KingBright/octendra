fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // ScreenCaptureKit Swift support uses @rpath for its concurrency runtime.
        // Apply this only to this package, keeping shared dependency cache flags stable.
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
    }
}
