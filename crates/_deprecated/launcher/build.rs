//! Embeds the launcher icon into the Windows .exe so it shows up in
//! taskbar / Alt-Tab / window caption / file explorer / systray.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/manifest.xml");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(windows)]
    {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR is set by cargo");
        let assets = std::path::Path::new(&manifest_dir).join("assets");
        let ico_path = assets.join("icon.ico");
        let manifest_path = assets.join("manifest.xml");
        if !ico_path.exists() {
            panic!("icon.ico not found at {}", ico_path.display());
        }
        if !manifest_path.exists() {
            panic!("manifest.xml not found at {}", manifest_path.display());
        }

        let manifest_xml = std::fs::read_to_string(&manifest_path)
            .expect("manifest.xml is readable");

        let mut res = winresource::WindowsResource::new();
        res.set_icon(ico_path.to_str().expect("path is utf8"));
        // Embed the manifest verbatim — this is what flips the .exe into
        // requireAdministrator mode, so wintun and child processes get the
        // privileges they need.
        res.set_manifest(&manifest_xml);
        if let Err(e) = res.compile() {
            panic!("failed to embed icon/manifest resource: {e}");
        }
    }
}
