fn main() {
    #[cfg(target_os = "windows")]
    {
        // Embed manifest requesting administrator privileges (eliminates UAC prompts for Wintun)
        println!("cargo:rerun-if-changed=manifest.xml");
        let mut res = winres::WindowsResource::new();
        res.set_manifest_file("manifest.xml");
        res.compile().expect("failed to compile Windows resources");
    }
}
