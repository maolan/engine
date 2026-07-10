pub mod clap_proc;
pub mod ipc;
#[cfg(all(unix, not(target_os = "macos")))]
pub mod lv2_proc;
pub mod types;
pub mod vst3_proc;

pub use types::*;

use serde::de::DeserializeOwned;

#[derive(serde::Deserialize)]
struct ScanDiagnostic {
    message: String,
    plugin_uri: Option<String>,
    plugin_name: Option<String>,
    bundle_uri: Option<String>,
}

#[derive(serde::Deserialize)]
struct ScanOutput<T> {
    data: T,
    errors: Vec<ScanDiagnostic>,
    warnings: Vec<ScanDiagnostic>,
}

use crate::message::PluginKind;

pub fn resolve_plugin_identifier(kind: PluginKind, identifier: &str) -> Result<String, String> {
    if identifier.is_empty() {
        return Err("plugin identifier is empty".to_string());
    }
    if identifier.contains('/')
        || identifier.contains('\\')
        || identifier.contains("::")
        || identifier.contains('#')
        || identifier.contains("://")
        || identifier.starts_with("file:")
        || std::path::Path::new(identifier).exists()
    {
        return Ok(identifier.to_string());
    }

    match kind {
        PluginKind::Clap => {
            let plugins = scan_plugins::<ClapPluginInfo>("clap")
                .map_err(|e| format!("failed to scan CLAP plugins: {e}"))?;
            plugins
                .into_iter()
                .find(|p| !p.id.is_empty() && p.id == identifier)
                .map(|p| p.path)
                .ok_or_else(|| format!("CLAP plugin ID not found: {identifier}"))
        }
        PluginKind::Vst3 => {
            let plugins = scan_plugins::<Vst3PluginInfo>("vst3")
                .map_err(|e| format!("failed to scan VST3 plugins: {e}"))?;
            plugins
                .into_iter()
                .find(|p| !p.id.is_empty() && p.id == identifier)
                .map(|p| p.path)
                .ok_or_else(|| format!("VST3 plugin ID not found: {identifier}"))
        }
        PluginKind::Lv2 => {
            let plugins = scan_plugins::<Lv2PluginInfo>("lv2")
                .map_err(|e| format!("failed to scan LV2 plugins: {e}"))?;
            plugins
                .into_iter()
                .find(|p| p.uri == identifier)
                .map(|p| p.uri)
                .ok_or_else(|| format!("LV2 plugin URI not found: {identifier}"))
        }
    }
}

pub fn scan_plugins<T: DeserializeOwned>(format: &str) -> Result<Vec<T>, String> {
    let host_bin = ipc::find_plugin_host_binary().ok_or("maolan-plugin-host binary not found")?;

    let mut cmd = std::process::Command::new(&host_bin);
    cmd.arg("--scan")
        .arg("--format")
        .arg(format)
        .arg("--path")
        .arg("--system");
    ipc::append_parent_log_level(&mut cmd);

    let output = cmd
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
    let parsed: ScanOutput<Vec<T>> =
        serde_json::from_str(&json).map_err(|e| format!("failed to parse scan JSON: {e}"))?;

    for error in &parsed.errors {
        tracing::error!(
            message = %error.message,
            plugin_uri = ?error.plugin_uri,
            plugin_name = ?error.plugin_name,
            bundle_uri = ?error.bundle_uri,
            "plugin scan error"
        );
    }
    for warning in &parsed.warnings {
        tracing::warn!(
            message = %warning.message,
            plugin_uri = ?warning.plugin_uri,
            plugin_name = ?warning.plugin_name,
            bundle_uri = ?warning.bundle_uri,
            "plugin scan warning"
        );
    }

    Ok(parsed.data)
}

#[cfg(test)]
mod tests {
    use super::ScanOutput;

    #[test]
    fn scan_output_parses_wrapper() {
        let json = r#"{
            "errors": [
                {
                    "message": "error: failed to open manifest.ttl",
                    "bundle_uri": "file:///tmp/broken.lv2/"
                }
            ],
            "warnings": [
                {
                    "message": "warning: duplicate version",
                    "plugin_uri": "http://example.com/plugin"
                }
            ],
            "data": [{"name": "Test", "path": "/tmp/test.clap", "capabilities": null}]
        }"#;
        let output: ScanOutput<Vec<serde_json::Value>> = serde_json::from_str(json).unwrap();
        assert_eq!(output.errors.len(), 1);
        assert_eq!(
            output.errors[0].message,
            "error: failed to open manifest.ttl"
        );
        assert_eq!(
            output.errors[0].bundle_uri,
            Some("file:///tmp/broken.lv2/".to_string())
        );
        assert_eq!(output.warnings.len(), 1);
        assert_eq!(
            output.warnings[0].plugin_uri,
            Some("http://example.com/plugin".to_string())
        );
        assert_eq!(output.data.len(), 1);
        assert_eq!(output.data[0]["name"], "Test");
    }
}
