// Embed the application icon into the Windows executable so Explorer shows a
// file icon. No-op for other targets. The .ico is produced by
// `cargo run --release --example gen_icon` and committed under assets/.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    // Check the *target* OS (works whether building on Windows or cross-compiling).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        if let Err(e) = res.compile() {
            // Non-fatal: build still succeeds, just without an embedded icon.
            println!("cargo:warning=failed to embed Windows icon: {e}");
        }
    }
}
