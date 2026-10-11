// SOCKS and HTTP are loopback and do not need elevation; this process does
// not spawn a tunnel driver. On non-Windows targets this is a no-op.
fn main() {
    #[cfg(target_os = "windows")]
    {
        use embed_manifest::{embed_manifest, manifest::ExecutionLevel, new_manifest};
        if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
            let manifest = new_manifest("MirageClientGui")
                .requested_execution_level(ExecutionLevel::AsInvoker);
            embed_manifest(manifest).expect("embed manifest");
        }
    }
}
