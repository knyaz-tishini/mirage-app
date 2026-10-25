// SOCKS and HTTP are loopback and do not need elevation; this process does
// not spawn a tunnel driver. On non-Windows targets this is a no-op.
//
// The exe icon is the same set mylittlechart embeds: one .ico (16, 32, 48,
// 64, 128, 256) via winresource, on the gui bin only. The manifest stays
// AsInvoker through embed-manifest; winresource is not given a manifest, so
// the two resources do not collide.
fn main() {
    #[cfg(target_os = "windows")]
    {
        use embed_manifest::{embed_manifest, manifest::ExecutionLevel, new_manifest};
        if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
            let manifest = new_manifest("m1r4g3")
                .requested_execution_level(ExecutionLevel::AsInvoker);
            embed_manifest(manifest).expect("embed manifest");

            let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
            let mut res = winresource::WindowsResource::new();
            res.set_icon("assets/icon.ico");
            res.set("ProductName", "m1r4g3");
            res.set("FileDescription", "m1r4g3");
            res.set("OriginalFilename", "mirage-client-gui.exe");
            res.set("ProductVersion", &version);
            res.set("FileVersion", &version);
            res.compile_for("mirage-client-gui").expect("winresource");
            println!("cargo:rerun-if-changed=assets/icon.ico");
        }
    }
}
