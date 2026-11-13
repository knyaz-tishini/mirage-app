// The window is the process that spawns the subscriber, which spawns
// mirage2tun. CreateProcess does not show a second consent dialog for that
// child, so the window itself has to already be elevated. RequireAdministrator
// makes Windows prompt before any window exists. A helper that elevates and
// then restarts this exe drops the token on the way down.
//
// The exe icon is the same set mylittlechart embeds: one .ico (16, 32, 48,
// 64, 128, 256) via winresource, on the gui bin only. winresource is not
// given a manifest, so the two resources do not collide.
fn main() {
    #[cfg(target_os = "windows")]
    {
        use embed_manifest::{embed_manifest, manifest::ExecutionLevel, new_manifest};
        if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
            // A picture window (`--visual`) has no tunnel, so it must not ask
            // for administrator. An elevated session-0 spawn cannot bind a
            // DXGI surface. Customer builds stay RequireAdministrator.
            println!("cargo:rerun-if-env-changed=MIRAGE_NO_ELEVATION");
            let level = if std::env::var_os("MIRAGE_NO_ELEVATION").is_some() {
                ExecutionLevel::AsInvoker
            } else {
                ExecutionLevel::RequireAdministrator
            };
            let manifest = new_manifest("m1r4g3").requested_execution_level(level);
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
