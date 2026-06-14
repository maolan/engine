pub mod clap_proc;
pub mod ipc;
#[cfg(all(unix, not(target_os = "macos")))]
pub mod lv2_proc;
pub mod types;
pub mod vst3_proc;

pub use types::*;

use serde::de::DeserializeOwned;

pub fn scan_plugins<T: DeserializeOwned>(format: &str) -> Result<Vec<T>, String> {
    let host_bin = ipc::find_plugin_host_binary().ok_or("maolan-plugin-host binary not found")?;

    let output = std::process::Command::new(&host_bin)
        .arg("--scan")
        .arg("--format")
        .arg(format)
        .arg("--path")
        .arg("--system")
        .output()
        .map_err(|e| format!("failed to spawn plugin-host scanner: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "plugin-host scanner exited with code {:?}: {stderr}",
            output.status.code()
        ));
    }

    let json = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&json).map_err(|e| format!("failed to parse scan JSON: {e}"))
}
