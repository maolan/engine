use crate::audio::io::AudioIO;
use maolan_plugin_protocol::events::EventPair;
use maolan_plugin_protocol::protocol::*;
use maolan_plugin_protocol::shm::ShmMapping;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(0);

pub fn unique_instance_id(format: &str) -> String {
    let n = NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}-{}", format, std::process::id(), n)
}

pub struct HostSpawnArgs<'a> {
    pub host_binary: &'a Path,
    pub format: &'a str,
    pub plugin_spec: &'a str,
    pub instance_id: &'a str,
    pub extra_args: &'a [&'a str],
}

pub fn spawn_host(
    args: HostSpawnArgs,
) -> Result<(Child, ShmMapping, EventPair, String, Option<ChildStderr>), String> {
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
        .stderr(Stdio::piped());

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

    append_parent_log_level(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn {} host: {e}", args.format))?;
    let stderr = child.stderr.take();

    events.close_daw_unused();

    Ok((child, mapping, events, shm_name, stderr))
}

pub fn append_parent_log_level(cmd: &mut Command) {
    let parent_args: Vec<String> = std::env::args().collect();
    if let Some(pos) = parent_args.iter().position(|a| a == "--log-level")
        && pos + 1 < parent_args.len()
    {
        cmd.arg("--log-level").arg(&parent_args[pos + 1]);
    }
}

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

pub fn bypass_copy_inputs_to_outputs(inputs: &[Arc<AudioIO>], outputs: &[Arc<AudioIO>]) {
    for (input, output) in inputs.iter().zip(outputs.iter()) {
        let src = input.buffer.lock();
        let dst = output.buffer.lock();
        dst.fill(0.0);
        for (d, s) in dst.iter_mut().zip(src.iter()) {
            *d = *s;
        }
        output.finished.store(true, Ordering::Release);
    }
    for output in outputs.iter().skip(inputs.len()) {
        let dst = output.buffer.lock();
        dst.fill(0.0);
        output.finished.store(true, Ordering::Release);
    }
}

pub fn drop_host(
    mapping: Option<ShmMapping>,
    events: Option<EventPair>,
    child: Option<Child>,
    shm_name: String,
) {
    if let Some(ref mapping) = mapping
        && let Some(ref events) = events
    {
        let header = unsafe { header_mut(mapping.as_ptr()) };
        header.shutdown_request.store(1, Ordering::Release);
        let _ = events.signal_host();
    }

    std::thread::spawn(move || {
        tracing::info!(%shm_name, "drop_host: waiting for plugin host process to exit");
        if let Some(mut child) = child {
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(5) {
                if child.try_wait().map(|s| s.is_some()).unwrap_or(true) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if child.try_wait().map(|s| s.is_none()).unwrap_or(false) {
                tracing::warn!(%shm_name, "drop_host: plugin host did not exit in time, killing");
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        drop(mapping);
        drop(events);
        let _ = ShmMapping::unlink(&shm_name);
        tracing::info!(%shm_name, "drop_host: cleanup complete");
    });
}

pub fn find_plugin_host_binary() -> Option<PathBuf> {
    let host_name = if cfg!(windows) {
        "maolan-plugin-host.exe"
    } else {
        "maolan-plugin-host"
    };

    if let Ok(override_path) = std::env::var("MAOLAN_PLUGIN_HOST") {
        let candidate = PathBuf::from(override_path);
        if candidate.exists() {
            tracing::info!(path = %candidate.display(), "Using plugin-host from MAOLAN_PLUGIN_HOST");
            return Some(candidate);
        }
        tracing::warn!(path = %candidate.display(), "MAOLAN_PLUGIN_HOST points to a missing file");
    }

    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from));

    if let Some(ref dir) = exe_dir {
        let candidate = dir.join(host_name);
        if candidate.exists() {
            tracing::info!(path = %candidate.display(), "Using plugin-host from exe directory");
            return Some(candidate);
        }
    }

    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let engine_root = Path::new(&manifest);
        for profile in ["debug", "release"] {
            let candidate = engine_root
                .parent()
                .unwrap_or(Path::new(""))
                .join("daw")
                .join("target")
                .join(profile)
                .join(host_name);
            if candidate.exists() {
                tracing::info!(path = %candidate.display(), "Using plugin-host from daw workspace target");
                return Some(candidate);
            }

            let candidate = engine_root
                .parent()
                .unwrap_or(Path::new(""))
                .join("daw")
                .join("plugin-host")
                .join("target")
                .join(profile)
                .join(host_name);
            if candidate.exists() {
                tracing::info!(path = %candidate.display(), "Using plugin-host from plugin-host crate target");
                return Some(candidate);
            }
        }
    }

    if let Ok(path_var) = std::env::var("PATH") {
        #[cfg(windows)]
        let path_sep = ';';
        #[cfg(not(windows))]
        let path_sep = ':';
        for dir in path_var.split(path_sep) {
            let candidate = Path::new(dir).join(host_name);
            if candidate.exists() {
                tracing::info!(path = %candidate.display(), "Using plugin-host from PATH");
                return Some(candidate);
            }
        }
    }

    tracing::error!("maolan-plugin-host binary not found");
    None
}

/// # Safety
///
/// `ptr` must point to a valid, initialized shared-memory layout with enough
/// space for the configured number of input channels and `frames` samples.
/// `frames` must not exceed the block size reserved in that layout.
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

/// # Safety
///
/// `ptr` must point to a valid, initialized shared-memory layout with enough
/// space for the configured number of output channels and `frames` samples.
/// Each output buffer must be writable and at least `frames` elements long.
pub unsafe fn copy_outputs_from_shm(outputs: &[Arc<AudioIO>], ptr: *mut u8, frames: usize) {
    for (ch, output) in outputs.iter().enumerate() {
        let dst = output.buffer.lock();
        let src = unsafe { audio_channel_ptr(ptr, ch, 1) };
        let len = frames.min(dst.len());
        unsafe {
            std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), len);
        }
        output.finished.store(true, Ordering::Release);
    }
}

/// # Safety
///
/// `ptr` must point to a valid, initialized shared-memory layout whose header
/// can safely be written to.
pub unsafe fn configure_shm_header(
    ptr: *mut u8,
    frames: usize,
    num_in: usize,
    num_out: usize,
    midi_in: usize,
    midi_out: usize,
) {
    unsafe {
        let h = header_mut(ptr);
        h.block_size.store(frames as u32, Ordering::Release);
        h.num_input_channels.store(num_in as u32, Ordering::Release);
        h.num_output_channels
            .store(num_out as u32, Ordering::Release);
        h.midi_in_port_count
            .store(midi_in as u32, Ordering::Release);
        h.midi_out_port_count
            .store(midi_out as u32, Ordering::Release);
    }
}

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
