use maolan_plugin_protocol::events::EventPair;
use maolan_plugin_protocol::protocol::*;
use maolan_plugin_protocol::shm::ShmMapping;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Poll interval used while waiting for the plugin host to signal readiness.
const HOST_READY_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Fallback plugin-wait ceiling before any audio device period is known (and
/// after the device closes). Matches the historical fixed timeout.
const DEFAULT_PLUGIN_WAIT_BOUND: Duration = Duration::from_millis(100);

/// Current plugin-wait ceiling in nanoseconds, shared by every plugin proc.
/// Updated by the engine when the audio device opens/closes; read on the RT
/// cycle thread once per plugin block wait.
static PLUGIN_WAIT_BOUND_NS: AtomicU64 = AtomicU64::new(100_000_000);

/// Update the process-wide plugin-wait ceiling.
pub fn set_plugin_wait_bound(bound: Duration) {
    PLUGIN_WAIT_BOUND_NS.store(
        bound.as_nanos().min(u64::MAX as u128) as u64,
        Ordering::Release,
    );
}

/// Restore the default no-device plugin-wait ceiling (100 ms). Called when
/// the audio device closes or is replaced by one whose period is unknown
/// (e.g. JACK, which paces externally).
pub fn reset_plugin_wait_bound() {
    set_plugin_wait_bound(DEFAULT_PLUGIN_WAIT_BOUND);
}

/// The plugin-wait ceiling the plugin procs must pass to
/// [`wait_block_response`]: `min(100 ms, 2 × period)` when an audio device is
/// open, 100 ms otherwise.
pub fn plugin_wait_timeout() -> Duration {
    Duration::from_nanos(PLUGIN_WAIT_BOUND_NS.load(Ordering::Acquire))
}

/// Compute the wait bound for a device period: `min(100 ms, 2 × period)`.
pub fn plugin_wait_bound_for_period(cycle_samples: usize, sample_rate: usize) -> Duration {
    let period = Duration::from_secs_f64(cycle_samples as f64 / sample_rate.max(1) as f64);
    DEFAULT_PLUGIN_WAIT_BOUND.min(period.saturating_mul(2))
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(windows)]
pub fn hide_console_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
pub fn hide_console_window(_cmd: &mut Command) {}

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
        // Every block completion must wake the waiting audio worker. Consuming
        // one event per request also handles hosts that finish before we wait.
        header_mut(mapping.as_ptr()).set_block_response_eventless(false);
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
    hide_console_window(&mut cmd);

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

pub fn wait_for_ready(header: &ShmHeader, child: &mut Child, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if header.ready.load(Ordering::Acquire) != 0 {
            return true;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::warn!(
                    status = %status,
                    "plugin host exited without signalling ready"
                );
                return false;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "failed to poll plugin host status");
            }
        }
        std::thread::sleep(HOST_READY_POLL_INTERVAL);
    }
    false
}

pub fn bypass_copy_input_slices_to_outputs(inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
    for (input, output) in inputs.iter().zip(outputs.iter_mut()) {
        output.fill(0.0);
        let n = output.len().min(input.len());
        output[..n].copy_from_slice(&input[..n]);
    }
    for output in outputs.iter_mut().skip(inputs.len()) {
        output.fill(0.0);
    }
}

/// Wait for the plugin host to finish the current audio block.
///
/// Capture `expected` before signalling the host. Hosts send a completion event
/// after publishing each block; blocking on it lets the worker sleep during DSP.
/// Control requests share the event channel, so a wakeup only completes this
/// wait when the block counter has advanced. Even an early completion retains
/// its event, avoiding both missed responses and busy-polling.
pub fn wait_block_response(
    counter: &AtomicU32,
    expected: u32,
    events: &EventPair,
    timeout: Duration,
) -> std::io::Result<()> {
    let start = Instant::now();
    loop {
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "block response timeout",
            ));
        }
        match events.wait_host(remaining) {
            Ok(()) => {
                // Pair with the host's release increment before it signalled
                // completion, publishing its shared-memory audio output.
                if counter.load(Ordering::Acquire) != expected {
                    return Ok(());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
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
        crate::report_error(
            events.signal_host(),
            "Failed to signal plugin host shutdown",
        );
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
                crate::report_error(child.kill(), "Failed to kill unresponsive plugin host");
                crate::report_error(child.wait(), "Failed to reap plugin host process");
            }
        }
        drop(mapping);
        drop(events);
        crate::report_error(
            ShmMapping::unlink(&shm_name),
            "Failed to unlink plugin shared memory",
        );
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
        for workspace_dir in ["maolan", "daw"] {
            for profile in ["debug", "release"] {
                let candidate = engine_root
                    .parent()
                    .unwrap_or(Path::new(""))
                    .join(workspace_dir)
                    .join("target")
                    .join(profile)
                    .join(host_name);
                if candidate.exists() {
                    tracing::info!(path = %candidate.display(), "Using plugin-host from workspace target");
                    return Some(candidate);
                }

                let candidate = engine_root
                    .parent()
                    .unwrap_or(Path::new(""))
                    .join(workspace_dir)
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
pub unsafe fn copy_input_slices_to_shm(inputs: &[&[f32]], ptr: *mut u8, frames: usize) {
    for (ch, src) in inputs.iter().enumerate() {
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
pub unsafe fn copy_outputs_from_shm_to_slices(
    outputs: &mut [&mut [f32]],
    ptr: *mut u8,
    frames: usize,
) {
    for (ch, dst) in outputs.iter_mut().enumerate() {
        let src = unsafe { audio_channel_ptr(ptr, ch, 1) };
        let len = frames.min(dst.len());
        unsafe {
            std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), len);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_wait_bound_scales_with_period_until_capped() {
        // 512 frames @ 48 kHz: 2 × period ≈ 21.3 ms, well under the cap.
        let small = plugin_wait_bound_for_period(512, 48_000);
        assert!(
            small > Duration::from_millis(21) && small < Duration::from_millis(22),
            "512@48k bound should be ~21.3 ms, got {small:?}"
        );
        // 4096 frames @ 48 kHz: 2 × period ≈ 170.7 ms, capped at 100 ms.
        let large = plugin_wait_bound_for_period(4096, 48_000);
        assert_eq!(large, Duration::from_millis(100));
    }

    #[test]
    fn plugin_wait_bound_setter_updates_timeout() {
        set_plugin_wait_bound(Duration::from_millis(21));
        assert_eq!(plugin_wait_timeout(), Duration::from_millis(21));
        set_plugin_wait_bound(DEFAULT_PLUGIN_WAIT_BOUND);
        assert_eq!(plugin_wait_timeout(), Duration::from_millis(100));
    }

    #[test]
    fn wait_block_response_consumes_completion_sent_before_wait() {
        let events = EventPair::new().expect("event pair");
        let counter = AtomicU32::new(7);
        counter.fetch_add(1, Ordering::Release);
        events.signal_daw().expect("signal completion before wait");
        wait_block_response(&counter, 7, &events, Duration::from_millis(100))
            .expect("already completed block must not be missed");

        // The previous event must not satisfy the next block's wait.
        let err = wait_block_response(&counter, 8, &events, Duration::from_millis(10))
            .expect_err("completion must be consumed exactly once");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn wait_block_response_ignores_stale_control_event() {
        let events = EventPair::new().expect("event pair");
        let counter = AtomicU32::new(0);
        // A queued control response must not complete an audio block.
        events.signal_daw().expect("signal stale control response");
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                counter.fetch_add(1, Ordering::Release);
                events.signal_daw().expect("signal audio completion");
            });
            wait_block_response(&counter, 0, &events, Duration::from_secs(5))
                .expect("audio completion should satisfy the wait");
            assert_eq!(counter.load(Ordering::Acquire), 1);
        });
    }

    #[test]
    fn wait_block_response_times_out_when_host_silent() {
        let events = EventPair::new().expect("event pair");
        let counter = AtomicU32::new(0);
        let start = Instant::now();
        let err =
            wait_block_response(&counter, 0, &events, Duration::from_millis(100)).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(95),
            "gave up too early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "wait ran past its timeout: {elapsed:?}"
        );
    }

    #[test]
    fn wait_block_response_wakes_when_host_finishes() {
        let events = EventPair::new().expect("event pair");
        let counter = AtomicU32::new(0);
        // Completion after the worker has begun waiting must wake it.
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(30));
                counter.fetch_add(1, Ordering::Release);
                events.signal_daw().expect("signal block completion");
            });
            let start = Instant::now();
            wait_block_response(&counter, 0, &events, Duration::from_secs(5))
                .expect("host completion should wake the wait");
            assert!(start.elapsed() < Duration::from_secs(1));
        });
    }
}
