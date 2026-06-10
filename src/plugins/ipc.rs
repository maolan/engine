//! Shared IPC helpers for out-of-process plugin processors.

use crate::audio::io::AudioIO;
use crate::mutex::UnsafeMutex;
use maolan_plugin_protocol::events::EventPair;
use maolan_plugin_protocol::protocol::*;
use maolan_plugin_protocol::shm::ShmMapping;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(0);

/// Generate a globally unique instance ID for plugin SHM naming.
pub fn unique_instance_id(format: &str) -> String {
    let n = NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}-{}", format, std::process::id(), n)
}

/// Arguments for spawning a plugin host subprocess.
pub struct HostSpawnArgs<'a> {
    pub host_binary: &'a Path,
    pub format: &'a str,
    pub plugin_spec: &'a str,
    pub instance_id: &'a str,
    pub extra_args: &'a [&'a str],
}

/// Spawn the unified `maolan-plugin-host` binary and set up SHM + event pipes.
pub fn spawn_host(args: HostSpawnArgs) -> Result<(Child, ShmMapping, EventPair, String), String> {
    let pid = std::process::id();
    let shm_name = format!("/maolan-{pid}-{}", args.instance_id);

    let mapping = ShmMapping::create(&shm_name, SHM_SIZE)
        .map_err(|e| format!("failed to create shared memory: {e}"))?;
    unsafe {
        init_shm_layout(mapping.as_ptr(), mapping.size());
    }

    let mut events = EventPair::new().map_err(|e| format!("failed to create event pipes: {e}"))?;

    let mut cmd = Command::new(args.host_binary);
    cmd.arg(args.format)
        .arg(args.plugin_spec)
        .arg(&shm_name)
        .arg(args.instance_id)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    #[cfg(unix)]
    {
        cmd.arg(events.host_read_fd().to_string())
            .arg(events.host_write_fd().to_string());
    }

    for arg in args.extra_args {
        cmd.arg(arg);
    }
    #[cfg(windows)]
    {
        cmd.arg(events.daw_to_host_name())
            .arg(events.host_to_daw_name());
    }

    let parent_args: Vec<String> = std::env::args().collect();
    if let Some(pos) = parent_args.iter().position(|a| a == "--log-level")
        && pos + 1 < parent_args.len()
    {
        cmd.arg("--log-level").arg(&parent_args[pos + 1]);
    }

    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {} host: {e}", args.format))?;

    events.close_daw_unused();

    Ok((child, mapping, events, shm_name))
}

/// Poll the SHM ready flag until it becomes non-zero or `timeout` elapses.
pub fn wait_for_ready(header: &ShmHeader, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if header.ready.load(Ordering::Acquire) != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// Copy input buffers to output buffers when the plugin host is bypassed or crashed.
pub fn bypass_copy_inputs_to_outputs(inputs: &[Arc<AudioIO>], outputs: &[Arc<AudioIO>]) {
    for (input, output) in inputs.iter().zip(outputs.iter()) {
        let src = input.buffer.lock();
        let dst = output.buffer.lock();
        dst.fill(0.0);
        for (d, s) in dst.iter_mut().zip(src.iter()) {
            *d = *s;
        }
        *output.finished.lock() = true;
    }
    for output in outputs.iter().skip(inputs.len()) {
        let dst = output.buffer.lock();
        dst.fill(0.0);
        *output.finished.lock() = true;
    }
}

/// Shared shutdown logic for the `Drop` impl of all OOP processors.
pub fn drop_host(
    mapping: &Option<ShmMapping>,
    events: &Option<EventPair>,
    child: &UnsafeMutex<Option<Child>>,
    shm_name: &str,
) {
    if let Some(mapping) = mapping
        && let Some(events) = events
    {
        let header = unsafe { header_mut(mapping.as_ptr()) };
        header.shutdown_request.store(1, Ordering::Release);
        let _ = events.signal_host();
    }
    let mut child_opt = child.lock().take();
    if let Some(mut child) = child_opt.take() {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            if child.try_wait().map(|s| s.is_some()).unwrap_or(true) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if child.try_wait().map(|s| s.is_none()).unwrap_or(false) {
            let _ = child.kill();
        }
    }
    let _ = ShmMapping::unlink(shm_name);
}

/// Locate the `maolan-plugin-host` binary at runtime.
///
/// Search order:
/// 1. Same directory as the current executable.
/// 2. Workspace `target/debug` or `target/release` (development).
/// 3. `PATH` environment variable.
pub fn find_plugin_host_binary() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from));

    // 1. Same directory as current executable.
    if let Some(ref dir) = exe_dir {
        let candidate = dir.join("maolan-plugin-host");
        if candidate.exists() {
            tracing::info!(path = %candidate.display(), "Using plugin-host from exe directory");
            return Some(candidate);
        }
    }

    // 2. Development workspace paths.
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let engine_root = Path::new(&manifest);
        for profile in ["debug", "release"] {
            // Primary workspace target directory (build from daw/)
            let candidate = engine_root
                .parent()
                .unwrap_or(Path::new(""))
                .join("daw")
                .join("target")
                .join(profile)
                .join("maolan-plugin-host");
            if candidate.exists() {
                tracing::info!(path = %candidate.display(), "Using plugin-host from daw workspace target");
                return Some(candidate);
            }

            // Crate-specific target directory (build from daw/plugin-host/)
            let candidate = engine_root
                .parent()
                .unwrap_or(Path::new(""))
                .join("daw")
                .join("plugin-host")
                .join("target")
                .join(profile)
                .join("maolan-plugin-host");
            if candidate.exists() {
                tracing::info!(path = %candidate.display(), "Using plugin-host from plugin-host crate target");
                return Some(candidate);
            }
        }
    }

    // 3. PATH.
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let candidate = Path::new(dir).join("maolan-plugin-host");
            if candidate.exists() {
                tracing::info!(path = %candidate.display(), "Using plugin-host from PATH");
                return Some(candidate);
            }
        }
    }

    tracing::error!("maolan-plugin-host binary not found");
    None
}

/// Copy input AudioIO buffers to shared memory (bus 0).
///
/// # Safety
/// `ptr` must be a valid pointer to the start of the plugin-host SHM region.
pub unsafe fn copy_inputs_to_shm(inputs: &[Arc<AudioIO>], ptr: *mut u8, frames: usize) {
    for (ch, input) in inputs.iter().enumerate() {
        let src = input.buffer.lock();
        let dst = unsafe { audio_channel_ptr(ptr, ch, 0) };
        let len = frames.min(src.len());
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst, len);
        }
    }
}

/// Copy output shared memory (bus 1) back to AudioIO buffers.
///
/// # Safety
/// `ptr` must be a valid pointer to the start of the plugin-host SHM region.
pub unsafe fn copy_outputs_from_shm(outputs: &[Arc<AudioIO>], ptr: *mut u8, frames: usize) {
    for (ch, output) in outputs.iter().enumerate() {
        let dst = output.buffer.lock();
        let src = unsafe { audio_channel_ptr(ptr, ch, 1) };
        let len = frames.min(dst.len());
        unsafe {
            std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), len);
        }
        *output.finished.lock() = true;
    }
}

/// Set the standard SHM header fields for a processing block.
///
/// # Safety
/// `ptr` must be a valid pointer to the start of the plugin-host SHM region.
pub unsafe fn configure_shm_header(ptr: *mut u8, frames: usize, num_in: usize, num_out: usize) {
    unsafe {
        let h = header_mut(ptr);
        h.block_size.store(frames as u32, Ordering::Release);
        h.num_input_channels.store(num_in as u32, Ordering::Release);
        h.num_output_channels
            .store(num_out as u32, Ordering::Release);
    }
}

/// Generate `UnsafeMutex<Processor>` forwarding methods that are identical
/// across all out-of-process plugin formats (CLAP, VST3, LV2).
#[macro_export]
macro_rules! impl_ipc_processor_wrapper {
    ($processor:ty) => {
        impl $crate::mutex::UnsafeMutex<$processor> {
            pub fn setup_audio_ports(&self) {
                self.lock().setup_audio_ports();
            }

            pub fn audio_inputs(&self) -> &[std::sync::Arc<$crate::audio::io::AudioIO>] {
                self.lock().audio_inputs()
            }

            pub fn audio_outputs(&self) -> &[std::sync::Arc<$crate::audio::io::AudioIO>] {
                self.lock().audio_outputs()
            }

            pub fn main_audio_input_count(&self) -> usize {
                self.lock().main_audio_input_count()
            }

            pub fn main_audio_output_count(&self) -> usize {
                self.lock().main_audio_output_count()
            }

            pub fn midi_input_count(&self) -> usize {
                self.lock().midi_input_count()
            }

            pub fn midi_output_count(&self) -> usize {
                self.lock().midi_output_count()
            }

            pub fn set_bypassed(&self, bypassed: bool) {
                self.lock().set_bypassed(bypassed);
            }

            pub fn name(&self) -> String {
                self.lock().name().to_string()
            }

            pub fn run_host_callbacks_main_thread(&self) {
                self.lock().run_host_callbacks_main_thread();
            }

            pub fn reconfigure_ports_if_needed(&self) -> Result<bool, String> {
                self.lock().reconfigure_ports_if_needed()
            }
        }
    };
}
