fn main() {
    // The sign-up service's address and key, from provision.env (written by
    // provision/deploy.ps1, not committed). Without it the app builds fine but
    // has no public link.
    println!("cargo:rerun-if-changed=provision.env");
    if let Ok(text) = std::fs::read_to_string("provision.env") {
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                if key == "ACCESS_PORTAL_PROVISION_URL" || key == "ACCESS_PORTAL_PROVISION_KEY" {
                    println!("cargo:rustc-env={key}={}", value.trim());
                }
            }
        }
    }
    tauri_build::build()
}
