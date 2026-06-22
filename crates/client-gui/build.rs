// Embed a Windows application manifest that requests admin elevation at
// launch. On non-Windows targets this is a no-op.
//
// User-level start (double-click) triggers a UAC prompt; once consented the
// process runs elevated. The GUI spawns the L0 orchestrator (→ sentinel →
// cli2 → tun) as a child, and on Windows the child inherits the parent token,
// so elevating the GUI is what gives mirage2tun the admin rights Wintun
// requires. Without this the client connects + registers in the mesh but the
// data-plane (tun) never loads — no internet.
fn main() {
    #[cfg(target_os = "windows")]
    {
        use embed_manifest::{embed_manifest, manifest::ExecutionLevel, new_manifest};
        if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
            let manifest = new_manifest("MirageClientGui")
                .requested_execution_level(ExecutionLevel::RequireAdministrator);
            embed_manifest(manifest).expect("embed manifest");
        }
    }
}
