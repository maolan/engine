//! Out-of-process CLAP processor using `maolan-plugin-host` IPC.

use crate::audio::io::AudioIO;
use crate::clap::{ClapMidiOutputEvent, ClapParamUpdate, ClapParameterInfo, ClapTransportInfo};
use crate::midi::io::MidiEvent;
use crate::mutex::UnsafeMutex;
use maolan_plugin_protocol::events::EventPair;
use maolan_plugin_protocol::protocol::*;
use maolan_plugin_protocol::ringbuf::RingBuffer;
use maolan_plugin_protocol::shm::ShmMapping;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, atomic::AtomicU32};
use std::time::{Duration, Instant};

/// Shared state for an out-of-process CLAP plugin instance.
pub struct ClapProcessor {
    path: String,
    plugin_id: String,
    name: String,
    audio_inputs: Vec<Arc<AudioIO>>,
    audio_outputs: Vec<Arc<AudioIO>>,
    main_audio_inputs: usize,
    main_audio_outputs: usize,
    param_infos: Vec<ClapParameterInfo>,
    param_values: UnsafeMutex<HashMap<u32, f64>>,
    bypassed: Arc<AtomicBool>,
    // IPC state
    child: UnsafeMutex<Option<Child>>,
    mapping: Option<ShmMapping>,
    events: Option<EventPair>,
    shm_name: String,
    // Crash recovery
    crash_count: AtomicU32,
    last_process_time: UnsafeMutex<Instant>,
}

pub type SharedClapProcessor = Arc<UnsafeMutex<ClapProcessor>>;

impl ClapProcessor {
    pub fn new(
        _sample_rate: f64,
        buffer_size: usize,
        plugin_spec: &str,
        input_count: usize,
        output_count: usize,
        host_binary: PathBuf,
    ) -> Result<Self, String> {
        let (plugin_path, plugin_id) = split_plugin_spec(plugin_spec);

        let audio_inputs = (0..input_count.max(1))
            .map(|_| Arc::new(AudioIO::new(buffer_size)))
            .collect::<Vec<_>>();
        let audio_outputs = (0..output_count.max(1))
            .map(|_| Arc::new(AudioIO::new(buffer_size)))
            .collect::<Vec<_>>();

        // Spawn the host immediately so we can query params.
        let instance_id = format!("clap-{}", std::process::id());
        let (mut child, mapping, events, shm_name) =
            spawn_host(&host_binary, plugin_path, plugin_id, &instance_id)?;

        let header = unsafe { header_ref(mapping.as_ptr()) };
        if !wait_for_ready(header, Duration::from_secs(10)) {
            let _ = child.kill();
            return Err("host did not signal ready".to_string());
        }

        // Query parameter count from host via a simple param ring echo.
        // For now, we use a minimal stub param list.
        let param_infos = Vec::new();

        Ok(Self {
            path: plugin_spec.to_string(),
            plugin_id: plugin_id.to_string(),
            name: plugin_id.to_string(),
            audio_inputs,
            audio_outputs,
            main_audio_inputs: input_count.max(1),
            main_audio_outputs: output_count.max(1),
            param_infos,
            param_values: UnsafeMutex::new(HashMap::new()),
            bypassed: Arc::new(AtomicBool::new(false)),
            child: UnsafeMutex::new(Some(child)),
            mapping: Some(mapping),
            events: Some(events),
            shm_name,
            crash_count: AtomicU32::new(0),
            last_process_time: UnsafeMutex::new(Instant::now()),
        })
    }

    pub fn setup_audio_ports(&self) {
        for port in &self.audio_inputs {
            port.setup();
        }
        for port in &self.audio_outputs {
            port.setup();
        }
    }

    pub fn audio_inputs(&self) -> &[Arc<AudioIO>] {
        &self.audio_inputs
    }

    pub fn audio_outputs(&self) -> &[Arc<AudioIO>] {
        &self.audio_outputs
    }

    pub fn main_audio_input_count(&self) -> usize {
        self.main_audio_inputs
    }

    pub fn main_audio_output_count(&self) -> usize {
        self.main_audio_outputs
    }

    pub fn midi_input_count(&self) -> usize {
        0 // Stub: MIDI not yet wired over IPC
    }

    pub fn midi_output_count(&self) -> usize {
        0
    }

    pub fn set_bypassed(&self, bypassed: bool) {
        self.bypassed.store(bypassed, Ordering::Relaxed);
    }

    pub fn is_bypassed(&self) -> bool {
        self.bypassed.load(Ordering::Relaxed)
    }

    pub fn parameter_infos(&self) -> Vec<ClapParameterInfo> {
        self.param_infos.clone()
    }

    pub fn parameter_values(&self) -> HashMap<u32, f64> {
        self.param_values.lock().clone()
    }

    pub fn set_parameter(&self, param_id: u32, value: f64) -> Result<(), String> {
        self.set_parameter_at(param_id, value, 0)
    }

    pub fn set_parameter_at(&self, param_id: u32, value: f64, _frame: u32) -> Result<(), String> {
        self.param_values.lock().insert(param_id, value);
        // Write to param ring buffer if host is alive.
        if let Some(ref mapping) = self.mapping {
            let ring = unsafe {
                let buf = param_ring_ptr(mapping.as_ptr());
                let (w, r) = param_indices(mapping.as_ptr());
                RingBuffer::new(buf, w, r, RING_CAPACITY)
            };
            let ev = ParameterEvent {
                param_index: param_id,
                value: value as f32,
                sample_offset: 0,
                event_kind: maolan_plugin_protocol::PARAM_EVENT_VALUE,
            };
            if !ring.push(ev) {
                tracing::warn!("param ring full, dropping parameter event");
            }
        }
        Ok(())
    }

    pub fn begin_parameter_edit(&self, _param_id: u32) -> Result<(), String> {
        Ok(())
    }

    pub fn end_parameter_edit(&self, _param_id: u32) -> Result<(), String> {
        Ok(())
    }

    pub fn is_parameter_edit_active(&self, _param_id: u32) -> bool {
        false
    }

    pub fn snapshot_state(&self) -> Result<crate::clap::ClapPluginState, String> {
        Err("state snapshot not yet implemented".to_string())
    }

    pub fn restore_state(&self, _state: &crate::clap::ClapPluginState) -> Result<(), String> {
        Err("state restore not yet implemented".to_string())
    }

    pub fn process_with_audio_io(&self, frames: usize) {
        let _ = self.process_with_midi(frames, &[], ClapTransportInfo::default());
    }

    pub fn process_with_midi(
        &self,
        frames: usize,
        _midi_in: &[MidiEvent],
        _transport: ClapTransportInfo,
    ) -> Vec<ClapMidiOutputEvent> {
        if self.bypassed.load(Ordering::Relaxed) {
            self.bypass_copy_inputs_to_outputs();
            return Vec::new();
        }

        // Check if host process has crashed.
        {
            let child = self.child.lock();
            if let Some(ref mut c) = child.as_mut() {
                match c.try_wait() {
                    Ok(Some(status)) if !status.success() => {
                        tracing::error!("plugin host crashed for '{}' ({})", self.name, self.path);
                        self.crash_count.fetch_add(1, Ordering::Relaxed);
                        self.bypass_copy_inputs_to_outputs();
                        return Vec::new();
                    }
                    _ => {}
                }
            }
        }

        let started = Instant::now();

        // We need mapping and events to proceed.
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => {
                self.bypass_copy_inputs_to_outputs();
                return Vec::new();
            }
        };

        let ptr = mapping.as_ptr();
        // Configure block size and channels for this call.
        let num_in = self.audio_inputs.len();
        let num_out = self.audio_outputs.len();
        unsafe {
            let h = header_mut(ptr);
            h.block_size.store(frames as u32, Ordering::Release);
            h.num_input_channels.store(num_in as u32, Ordering::Release);
            h.num_output_channels
                .store(num_out as u32, Ordering::Release);
        }

        // Copy input AudioIO buffers to shared memory (bus 0).
        for (ch, input) in self.audio_inputs.iter().enumerate() {
            let src = input.buffer.lock();
            let dst = unsafe { audio_channel_ptr(ptr, ch, 0) };
            let len = frames.min(src.len());
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), dst, len);
            }
        }

        // Signal host to process.
        if let Err(e) = events.signal_host() {
            tracing::error!("Failed to signal host: {e}");
            self.bypass_copy_inputs_to_outputs();
            return Vec::new();
        }

        // Wait for host to complete (with timeout).
        let timeout = Duration::from_millis(100);
        if let Err(e) = events.wait_host(timeout) {
            tracing::error!(
                "host did not respond for '{}' ({}): {e}",
                self.name,
                self.path
            );
            self.bypass_copy_inputs_to_outputs();
            return Vec::new();
        }

        // Copy output shared memory (bus 1) back to AudioIO buffers.
        for (ch, output) in self.audio_outputs.iter().enumerate() {
            let dst = output.buffer.lock();
            let src = unsafe { audio_channel_ptr(ptr, ch, 1) };
            let len = frames.min(dst.len());
            unsafe {
                std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), len);
            }
            *output.finished.lock() = true;
        }

        let elapsed = started.elapsed();
        if elapsed > Duration::from_millis(20) {
            tracing::warn!(
                "Slow process '{}' ({}) took {:.3} ms for {} frames",
                self.name,
                self.path,
                elapsed.as_secs_f64() * 1000.0,
                frames
            );
        }

        *self.last_process_time.lock() = Instant::now();
        Vec::new() // MIDI output stub
    }

    fn bypass_copy_inputs_to_outputs(&self) {
        for (input, output) in self.audio_inputs.iter().zip(self.audio_outputs.iter()) {
            let src = input.buffer.lock();
            let dst = output.buffer.lock();
            dst.fill(0.0);
            for (d, s) in dst.iter_mut().zip(src.iter()) {
                *d = *s;
            }
            *output.finished.lock() = true;
        }
        for output in self.audio_outputs.iter().skip(self.audio_inputs.len()) {
            output.buffer.lock().fill(0.0);
            *output.finished.lock() = true;
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn begin_parameter_edit_at(&self, _param_id: u32, _frame: u32) -> Result<(), String> {
        Ok(())
    }

    pub fn end_parameter_edit_at(&self, _param_id: u32, _frame: u32) -> Result<(), String> {
        Ok(())
    }

    pub fn run_host_callbacks_main_thread(&self) {}

    pub fn reconfigure_ports_if_needed(&self) -> Result<bool, String> {
        Ok(false)
    }

    pub fn ui_begin_session(&self) {}
    pub fn ui_end_session(&self) {}
    pub fn ui_should_close(&self) -> bool {
        false
    }
    pub fn ui_take_due_timers(&self) -> Vec<u32> {
        Vec::new()
    }
    pub fn ui_take_param_updates(&self) -> Vec<ClapParamUpdate> {
        Vec::new()
    }
    pub fn ui_take_state_update(&self) -> Option<crate::clap::ClapPluginState> {
        None
    }

    pub fn gui_info(&self) -> Result<crate::clap::ClapGuiInfo, String> {
        Err("GUI not yet supported for CLAP plugins".to_string())
    }

    pub fn gui_create(&self, _api: &str, _is_floating: bool) -> Result<(), String> {
        Err("GUI not yet supported for CLAP plugins".to_string())
    }

    pub fn gui_get_size(&self) -> Result<(u32, u32), String> {
        Err("GUI not yet supported for CLAP plugins".to_string())
    }

    pub fn gui_set_parent_x11(&self, window: usize) -> Result<(), String> {
        if let Some(ref mapping) = self.mapping {
            let header = unsafe { header_mut(mapping.as_ptr()) };
            header.parent_window.store(window as u32, Ordering::Release);
            return Ok(());
        }
        Err("No active host to set parent window".to_string())
    }

    pub fn gui_show(&self) -> Result<(), String> {
        if let Some(ref mapping) = self.mapping
            && let Some(ref events) = self.events
        {
            let header = unsafe { header_mut(mapping.as_ptr()) };
            header.request_type.store(3, Ordering::Release);
            let _ = events.signal_host();
            return Ok(());
        }
        Err("No active host to show GUI".to_string())
    }

    pub fn gui_hide(&self) {
        if let Some(ref mapping) = self.mapping
            && let Some(ref events) = self.events
        {
            let header = unsafe { header_mut(mapping.as_ptr()) };
            header.request_type.store(4, Ordering::Release);
            let _ = events.signal_host();
        }
    }

    pub fn gui_destroy(&self) {}

    pub fn gui_on_main_thread(&self) {}

    pub fn gui_on_timer(&self, _timer_id: u32) {}

    pub fn note_names(&self) -> std::collections::HashMap<u8, String> {
        std::collections::HashMap::new()
    }

    pub fn drain_echoed_parameters(&self) -> Vec<ParameterEvent> {
        let mut result = Vec::new();
        if let Some(ref mapping) = self.mapping {
            let ring = unsafe {
                let buf = echo_ring_ptr(mapping.as_ptr());
                let (w, r) = echo_indices(mapping.as_ptr());
                RingBuffer::new(buf, w, r, RING_CAPACITY)
            };
            while let Some(ev) = ring.pop() {
                result.push(ev);
            }
        }
        result
    }
}

impl Drop for ClapProcessor {
    fn drop(&mut self) {
        if let Some(ref mapping) = self.mapping
            && let Some(ref events) = self.events
        {
            let header = unsafe { header_mut(mapping.as_ptr()) };
            header.shutdown_request.store(1, Ordering::Release);
            let _ = events.signal_host();
        }
        let mut child_opt = self.child.lock().take();
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
        let _ = ShmMapping::unlink(&self.shm_name);
    }
}

impl UnsafeMutex<ClapProcessor> {
    pub fn setup_audio_ports(&self) {
        self.lock().setup_audio_ports();
    }

    pub fn process_with_midi(
        &self,
        frames: usize,
        midi_events: &[MidiEvent],
        transport: ClapTransportInfo,
    ) -> Vec<ClapMidiOutputEvent> {
        self.lock()
            .process_with_midi(frames, midi_events, transport)
    }

    pub fn set_bypassed(&self, bypassed: bool) {
        self.lock().set_bypassed(bypassed);
    }

    pub fn is_bypassed(&self) -> bool {
        self.lock().is_bypassed()
    }

    pub fn parameter_infos(&self) -> Vec<ClapParameterInfo> {
        self.lock().parameter_infos()
    }

    pub fn set_parameter(&self, param_id: u32, value: f64) -> Result<(), String> {
        self.lock().set_parameter(param_id, value)
    }

    pub fn set_parameter_at(&self, param_id: u32, value: f64, frame: u32) -> Result<(), String> {
        self.lock().set_parameter_at(param_id, value, frame)
    }

    pub fn begin_parameter_edit_at(&self, param_id: u32, frame: u32) -> Result<(), String> {
        self.lock().begin_parameter_edit_at(param_id, frame)
    }

    pub fn end_parameter_edit_at(&self, param_id: u32, frame: u32) -> Result<(), String> {
        self.lock().end_parameter_edit_at(param_id, frame)
    }

    pub fn snapshot_state(&self) -> Result<crate::clap::ClapPluginState, String> {
        self.lock().snapshot_state()
    }

    pub fn restore_state(&self, state: &crate::clap::ClapPluginState) -> Result<(), String> {
        self.lock().restore_state(state)
    }

    pub fn audio_inputs(&self) -> &[Arc<AudioIO>] {
        self.lock().audio_inputs()
    }

    pub fn audio_outputs(&self) -> &[Arc<AudioIO>] {
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

    pub fn path(&self) -> String {
        self.lock().path().to_string()
    }

    pub fn plugin_id(&self) -> String {
        self.lock().plugin_id().to_string()
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

    pub fn ui_begin_session(&self) {
        self.lock().ui_begin_session();
    }

    pub fn ui_end_session(&self) {
        self.lock().ui_end_session();
    }

    pub fn ui_should_close(&self) -> bool {
        self.lock().ui_should_close()
    }

    pub fn ui_take_due_timers(&self) -> Vec<u32> {
        self.lock().ui_take_due_timers()
    }

    pub fn ui_take_param_updates(&self) -> Vec<ClapParamUpdate> {
        self.lock().ui_take_param_updates()
    }

    pub fn ui_take_state_update(&self) -> Option<crate::clap::ClapPluginState> {
        self.lock().ui_take_state_update()
    }

    pub fn gui_info(&self) -> Result<crate::clap::ClapGuiInfo, String> {
        self.lock().gui_info()
    }

    pub fn gui_create(&self, api: &str, is_floating: bool) -> Result<(), String> {
        self.lock().gui_create(api, is_floating)
    }

    pub fn gui_get_size(&self) -> Result<(u32, u32), String> {
        self.lock().gui_get_size()
    }

    pub fn gui_set_parent_x11(&self, window: usize) -> Result<(), String> {
        self.lock().gui_set_parent_x11(window)
    }

    pub fn gui_show(&self) -> Result<(), String> {
        self.lock().gui_show()
    }

    pub fn gui_hide(&self) {
        self.lock().gui_hide();
    }

    pub fn gui_destroy(&self) {
        self.lock().gui_destroy();
    }

    pub fn gui_on_main_thread(&self) {
        self.lock().gui_on_main_thread();
    }

    pub fn gui_on_timer(&self, timer_id: u32) {
        self.lock().gui_on_timer(timer_id);
    }

    pub fn note_names(&self) -> std::collections::HashMap<u8, String> {
        self.lock().note_names()
    }
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
            return Some(candidate);
        }
    }

    // 2. Development workspace paths.
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        let engine_root = Path::new(&manifest);
        for profile in ["debug", "release"] {
            let candidate = engine_root
                .parent()
                .unwrap_or(Path::new(""))
                .join("daw")
                .join("target")
                .join(profile)
                .join("maolan-plugin-host");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    // 3. PATH.
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            let candidate = Path::new(dir).join("maolan-plugin-host");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    None
}

fn split_plugin_spec(spec: &str) -> (&str, &str) {
    if let Some(pos) = spec.rfind('#') {
        (&spec[..pos], &spec[pos + 1..])
    } else {
        (spec, "")
    }
}

fn spawn_host(
    host_binary: &PathBuf,
    plugin_path: &str,
    plugin_id: &str,
    instance_id: &str,
) -> Result<(Child, ShmMapping, EventPair, String), String> {
    let pid = std::process::id();
    let shm_name = format!("/maolan-{pid}-{instance_id}");

    let mapping = ShmMapping::create(&shm_name, SHM_SIZE)?;
    unsafe {
        init_shm_layout(mapping.as_ptr(), mapping.size());
    }

    let mut events = EventPair::new().map_err(|e| format!("failed to create pipes: {e}"))?;

    let plugin_spec = if plugin_id.is_empty() {
        plugin_path.to_string()
    } else {
        format!("{plugin_path}#{plugin_id}")
    };

    let mut cmd = Command::new(host_binary);
    cmd.arg("clap")
        .arg(&plugin_spec)
        .arg(&shm_name)
        .arg(instance_id)
        .arg(events.host_read_fd().to_string())
        .arg(events.host_write_fd().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn host: {e}"))?;

    events.close_daw_unused();

    Ok((child, mapping, events, shm_name))
}

fn wait_for_ready(header: &ShmHeader, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if header.ready.load(Ordering::Acquire) != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_host_binary() -> PathBuf {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let workspace_root = std::path::Path::new(&manifest)
            .parent()
            .unwrap()
            .join("daw");
        workspace_root
            .join("target")
            .join("debug")
            .join("maolan-plugin-host")
    }

    #[test]
    fn clap_processor_processes_audio() {
        let host_bin = find_host_binary();
        if !host_bin.exists() {
            eprintln!(
                "Skipping test: host binary not found at {}",
                host_bin.display()
            );
            return;
        }

        let plugin_path = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .parent()
            .unwrap()
            .join("daw")
            .join("plugin-host")
            .join("tests")
            .join("test_passthrough.clap");

        if !plugin_path.exists() {
            eprintln!(
                "Skipping test: plugin not found at {}",
                plugin_path.display()
            );
            return;
        }

        let processor = ClapProcessor::new(
            48000.0,
            256,
            &format!("{}#com.maolan.test.passthrough", plugin_path.display()),
            2,
            2,
            host_bin,
        )
        .expect("should create processor");

        processor.setup_audio_ports();

        // Fill input buffers with a ramp.
        for (i, input) in processor.audio_inputs().iter().enumerate() {
            let buf = input.buffer.lock();
            for (j, sample) in buf.iter_mut().enumerate() {
                *sample = (i * 1000 + j) as f32;
            }
            *input.finished.lock() = true;
        }

        // Process one block.
        processor.process_with_audio_io(256);

        // Verify output buffers were written (non-zero).
        for output in processor.audio_outputs().iter() {
            let buf = output.buffer.lock();
            assert!(
                buf.iter().any(|&s| s != 0.0),
                "output buffer should contain non-zero samples"
            );
        }

        // Processor is dropped here, which should gracefully shut down the host.
    }

    #[test]
    fn clap_processor_crash_bypass() {
        let host_bin = find_host_binary();
        if !host_bin.exists() {
            eprintln!("Skipping crash test: host binary not found");
            return;
        }

        // Use the crash test mode.
        let processor = ClapProcessor::new(48000.0, 256, "__crash__", 1, 1, host_bin)
            .expect("should create processor for crash test");

        processor.setup_audio_ports();

        // Fill input buffer.
        {
            let buf = processor.audio_inputs()[0].buffer.lock();
            buf.fill(1.0);
            *processor.audio_inputs()[0].finished.lock() = true;
        }

        // First process should trigger the crash; subsequent calls should bypass.
        processor.process_with_audio_io(256);

        // After crash, output should be a copy of input (bypass).
        let out_buf = processor.audio_outputs()[0].buffer.lock();
        assert!(
            out_buf.iter().all(|&s| s == 1.0),
            "after crash, output should be bypass copy of input"
        );
    }

    #[test]
    fn clap_track_integration() {
        use crate::track::Track;

        let host_bin = find_host_binary();
        if !host_bin.exists() {
            eprintln!("Skipping track integration test: host binary not found");
            return;
        }

        let plugin_path = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .parent()
            .unwrap()
            .join("daw")
            .join("plugin-host")
            .join("tests")
            .join("test_passthrough.clap");

        if !plugin_path.exists() {
            eprintln!(
                "Skipping track integration test: plugin not found at {}",
                plugin_path.display()
            );
            return;
        }

        let mut track = Track::new("test-track".to_string(), 2, 2, 0, 0, 256, 48000.0);

        track
            .load_clap_plugin(
                &format!("{}::com.maolan.test.passthrough", plugin_path.display()),
                None,
            )
            .expect("should load CLAP plugin on track");

        assert_eq!(track.clap_plugins.len(), 1);

        // Fill plugin inputs with a ramp and mark them ready.
        for input in track.clap_plugins[0].processor.audio_inputs() {
            let buf = input.buffer.lock();
            for (j, sample) in buf.iter_mut().enumerate() {
                *sample = j as f32;
            }
            *input.finished.lock() = true;
        }

        // Process one block.
        track.process();

        // Verify the plugin's output buffers contain the passthrough signal.
        for (ch, output) in track.clap_plugins[0]
            .processor
            .audio_outputs()
            .iter()
            .enumerate()
        {
            let buf = output.buffer.lock();
            assert!(
                buf.iter().any(|&s| s != 0.0),
                "plugin output ch={ch} should contain non-zero samples after CLAP processing"
            );
        }
    }
}
