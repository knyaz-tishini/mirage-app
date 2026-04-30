//! Embeds the launcher icon into the Windows .exe so it shows up in
//! taskbar / Alt-Tab / window caption / file explorer / systray.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    {
        let manifest = std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR is set by cargo");
        let ico_path = std::path::Path::new(&manifest).join("assets").join("icon.ico");
        if !ico_path.exists() {
            panic!("icon.ico not found at {}", ico_path.display());
        }

        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico_path.to_str().expect("path is utf8"));
        if let Err(e) = res.compile() {
            panic!("failed to embed icon resource: {e}");
        }
    }
}
