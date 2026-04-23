fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out_path = std::path::PathBuf::from(&out_dir);
    // OUT_DIR: target/release/build/mirage2light-app-xxx/out → up 3 = release/
    if let Some(profile_dir) = out_path.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
        // Copy testbed config
        let config_src = std::path::PathBuf::from("../mirage2monitor/mirage-testbed-persistent-0.json");
        let config_dst = profile_dir.join("mirage-testbed-persistent-0.json");
        if config_src.exists() {
            std::fs::copy(&config_src, &config_dst).expect("failed to copy config");
            println!("cargo:warning=Copied config to {}", config_dst.display());
        }

        // Copy mirage-client.exe from mirage2web build output
        let client_src = std::path::PathBuf::from("../mirage2web/target/release/mirage-client.exe");
        let client_dst = profile_dir.join("mirage-client.exe");
        if client_src.exists() {
            std::fs::copy(&client_src, &client_dst).ok();
            println!("cargo:warning=Copied mirage-client.exe to {}", client_dst.display());
        }

        // Copy wintun.dll
        let wintun_src = std::path::PathBuf::from("../mirage2web/crates/mirage-client-desktop/vendor/wintun.dll");
        let wintun_dst = profile_dir.join("wintun.dll");
        if wintun_src.exists() && !wintun_dst.exists() {
            std::fs::copy(&wintun_src, &wintun_dst).ok();
            println!("cargo:warning=Copied wintun.dll to {}", wintun_dst.display());
        }
    }
    println!("cargo:rerun-if-changed=../mirage2monitor/mirage-testbed-persistent-0.json");
    println!("cargo:rerun-if-changed=../mirage2web/target/release/mirage-client.exe");
}
