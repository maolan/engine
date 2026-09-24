use maolan_plugin_protocol::events::EventPair;
use maolan_plugin_protocol::protocol::*;
use maolan_plugin_protocol::shm::ShmMapping;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Poll interval used while waiting for the plugin host to signal readiness.
const HOST_READY_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// How long the RT thread spin-polls the shared-memory block-response
/// counter before falling back to the blocking event wait. The host answers
/// a block on another core within microseconds in the healthy case; 2 ms
/// covers scheduler jitter while staying far below the 100 ms hung-host
/// budget (roughly one small audio-block period).
const BLOCK_RESPONSE_SPIN_BUDGET: Duration = Duration::from_millis(2);

/// Maximum single event wait once the spin budget is exhausted. Slicing the
/// remaining timeout lets us notice a counter bump that lands mid-wait (the
/// host may run "eventless" and never write the completion byte).
const BLOCK_RESPONSE_WAIT_SLICE: Duration = Duration::from_millis(5);

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
        // We observe block completion via the shm response counter, so the
        // host can skip writing the per-block completion event byte (which
        // would accumulate unread in the pipe). Hosts predating the counter
        // treat this as a no-op write into padding and keep sending events,
        // which `wait_block_response` still consumes via the fallback.
        header_mut(mapping.as_ptr()).set_block_response_eventless(true);
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
        for (d, s) in output.iter_mut().zip(input.iter()) {
            *d = *s;
        }
    }
    for output in outputs.iter_mut().skip(inputs.len()) {
        output.fill(0.0);
    }
}

/// Wait for the plugin host to finish the current audio block.
///
/// Fast path: spin-poll the shm block-response counter (the host bumps it
/// right before reporting each block) — no syscalls at all. If the counter
/// does not move within [`BLOCK_RESPONSE_SPIN_BUDGET`] — e.g. a pre-0.0.19
/// host binary that never bumps it, or a genuinely stuck host — fall back to
/// the classic blocking event wait with the remaining timeout, sliced so a
/// late counter bump is still noticed. The total worst-case latency is the
/// same `timeout` the pure event wait had.
pub fn wait_block_response(
    counter: &AtomicU32,
    events: &EventPair,
    timeout: Duration,
) -> std::io::Result<()> {
    let start = Instant::now();
    let expected = counter.load(Ordering::Acquire);
    loop {
        if counter.load(Ordering::Acquire) != expected {
            return Ok(());
        }
        if start.elapsed() >= BLOCK_RESPONSE_SPIN_BUDGET {
            break;
        }
        std::hint::spin_loop();
    }
    loop {
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "block response timeout",
            ));
        }
        let slice = timeout
            .saturating_sub(elapsed)
            .min(BLOCK_RESPONSE_WAIT_SLICE);
        match events.wait_host(slice) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                // Re-check the counter: the host may have finished while we
                // were parked (eventless hosts never write the byte).
                if counter.load(Ordering::Acquire) != expected {
                    return Ok(());
                }
            }
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
    fn wait_block_response_fast_path_notices_counter() {
        let events = EventPair::new().expect("event pair");
        let counter = AtomicU32::new(7);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(10));
                counter.fetch_add(1, Ordering::Release);
            });
            let start = Instant::now();
            wait_block_response(&counter, &events, Duration::from_secs(5))
                .expect("counter bump should satisfy the wait");
            // Fast path must not need the event byte nor anywhere near the
            // 5 s timeout.
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "counter fast path took too long"
            );
        });
    }

    #[test]
    fn wait_block_response_falls_back_to_event() {
        let events = EventPair::new().expect("event pair");
        let counter = AtomicU32::new(0);
        // Simulate a pre-counter host: only the event byte ever arrives.
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                let _ = events.signal_daw();
            });
            wait_block_response(&counter, &events, Duration::from_secs(5))
                .expect("event byte should satisfy the wait");
        });
    }

    #[test]
    fn wait_block_response_times_out_when_host_silent() {
        let events = EventPair::new().expect("event pair");
        let counter = AtomicU32::new(0);
        let start = Instant::now();
        let err = wait_block_response(&counter, &events, Duration::from_millis(100)).unwrap_err();
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
    fn wait_block_response_notices_counter_during_fallback() {
        let events = EventPair::new().expect("event pair");
        let counter = AtomicU32::new(0);
        // Eventless host: counter moves only after the spin budget is spent
        // and no byte is ever written.
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(30));
                counter.fetch_add(1, Ordering::Release);
            });
            let start = Instant::now();
            wait_block_response(&counter, &events, Duration::from_secs(5))
                .expect("mid-wait counter bump should satisfy the wait");
            assert!(start.elapsed() < Duration::from_secs(1));
        });
    }
}
