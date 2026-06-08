//! Out-of-process CLAP processor using `maolan-plugin-host` IPC.

use crate::audio::io::AudioIO;
use crate::midi::io::MidiEvent;
use crate::mutex::UnsafeMutex;
use crate::plugins::ipc;
use crate::plugins::types::{
    ClapMidiOutputEvent, ClapParamUpdate, ClapParameterInfo, ClapTransportInfo,
};
use maolan_plugin_protocol::events::EventPair;
use maolan_plugin_protocol::protocol::*;
use maolan_plugin_protocol::ringbuf::RingBuffer;
use maolan_plugin_protocol::shm::ShmMapping;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Child;
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
    midi_inputs: usize,
    midi_outputs: usize,
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

        // Spawn the host immediately so we can query params.
        let instance_id = ipc::unique_instance_id("clap");
        let plugin_spec = if plugin_id.is_empty() {
            plugin_path.to_string()
        } else {
            format!("{plugin_path}::{plugin_id}")
        };
        let (mut child, mapping, events, shm_name) = ipc::spawn_host(ipc::HostSpawnArgs {
            host_binary: &host_binary,
            format: "clap",
            plugin_spec: &plugin_spec,
            instance_id: &instance_id,
            extra_args: &[],
        })?;

        let header = unsafe { header_ref(mapping.as_ptr()) };
        if !ipc::wait_for_ready(header, Duration::from_secs(10)) {
            let _ = child.kill();
            return Err("host did not signal ready".to_string());
        }

        let name = unsafe {
            let mut name = None;
            for _ in 0..50 {
                name = maolan_plugin_protocol::protocol::read_plugin_name_from_scratch(
                    mapping.as_ptr(),
                );
                if name.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            name.unwrap_or_else(|| plugin_id.to_string())
        };

        // Read port counts written by the host (with fallback to constructor params).
        let (actual_audio_in, actual_audio_out, actual_midi_in, actual_midi_out) = unsafe {
            let mut counts = None;
            for _ in 0..50 {
                counts = maolan_plugin_protocol::protocol::read_port_counts_from_scratch(
                    mapping.as_ptr(),
                );
                if counts.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let result = counts.unwrap_or((input_count as u32, output_count as u32, 0, 0));
            tracing::info!(
                plugin = %plugin_spec,
                audio_in = result.0,
                audio_out = result.1,
                midi_in = result.2,
                midi_out = result.3,
                from_host = counts.is_some(),
                "CLAP processor port counts"
            );
            result
        };

        let audio_inputs = (0..actual_audio_in as usize)
            .map(|_| Arc::new(AudioIO::new(buffer_size)))
            .collect::<Vec<_>>();
        let audio_outputs = (0..actual_audio_out as usize)
            .map(|_| Arc::new(AudioIO::new(buffer_size)))
            .collect::<Vec<_>>();

        // Query parameter count from host via a simple param ring echo.
        // For now, we use a minimal stub param list.
        let param_infos = Vec::new();

        Ok(Self {
            path: plugin_spec.to_string(),
            plugin_id: plugin_id.to_string(),
            name,
            audio_inputs,
            audio_outputs,
            main_audio_inputs: actual_audio_in as usize,
            main_audio_outputs: actual_audio_out as usize,
            midi_inputs: actual_midi_in as usize,
            midi_outputs: actual_midi_out as usize,
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
        self.midi_inputs
    }

    pub fn midi_output_count(&self) -> usize {
        self.midi_outputs
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

    pub fn snapshot_state(&self) -> Result<crate::plugins::types::ClapPluginState, String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("CLAP processor not initialized".to_string()),
        };
        let ptr = mapping.as_ptr();
        let header = unsafe { header_mut(ptr) };

        header.request_type.store(1, Ordering::Release);
        header.request_status.store(0, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Failed to signal host for state save: {e}"));
        }

        if let Err(e) = events.wait_host(Duration::from_secs(5)) {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Host did not respond to state save: {e}"));
        }

        let status = header.request_status.load(Ordering::Acquire);
        let size = header.scratch_size.load(Ordering::Acquire) as usize;
        if status != 1 {
            header.request_type.store(0, Ordering::Release);
            return Err("State save failed in host".to_string());
        }
        if size > SCRATCH_SIZE {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Host returned invalid CLAP state size: {size}"));
        }

        let scratch = unsafe { scratch_ptr(ptr) };
        let mut bytes = vec![0u8; size];
        unsafe {
            std::ptr::copy_nonoverlapping(scratch, bytes.as_mut_ptr(), size);
        }
        header.request_type.store(0, Ordering::Release);
        Ok(crate::plugins::types::ClapPluginState { bytes })
    }

    pub fn restore_state(
        &self,
        state: &crate::plugins::types::ClapPluginState,
    ) -> Result<(), String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("CLAP processor not initialized".to_string()),
        };
        if state.bytes.len() > SCRATCH_SIZE {
            return Err(format!(
                "CLAP state is too large for scratch buffer: {} bytes",
                state.bytes.len()
            ));
        }

        let ptr = mapping.as_ptr();
        let header = unsafe { header_mut(ptr) };
        let scratch = unsafe { scratch_ptr(ptr) };
        unsafe {
            std::ptr::copy_nonoverlapping(state.bytes.as_ptr(), scratch, state.bytes.len());
        }
        header
            .scratch_size
            .store(state.bytes.len() as u32, Ordering::Release);

        header.request_type.store(2, Ordering::Release);
        header.request_status.store(0, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Failed to signal host for state restore: {e}"));
        }

        if let Err(e) = events.wait_host(Duration::from_secs(5)) {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Host did not respond to state restore: {e}"));
        }

        let status = header.request_status.load(Ordering::Acquire);
        header.request_type.store(0, Ordering::Release);
        if status != 1 {
            return Err("State restore failed in host".to_string());
        }
        Ok(())
    }

    pub fn process_with_audio_io(&self, frames: usize) {
        let _ = self.process_with_midi(frames, &[], ClapTransportInfo::default());
    }

    pub fn process_with_midi(
        &self,
        frames: usize,
        midi_in: &[MidiEvent],
        transport: ClapTransportInfo,
    ) -> Vec<ClapMidiOutputEvent> {
        if self.bypassed.load(Ordering::Relaxed) {
            ipc::bypass_copy_inputs_to_outputs(&self.audio_inputs, &self.audio_outputs);
            return Vec::new();
        }

        {
            let child = self.child.lock();
            if let Some(ref mut c) = child.as_mut() {
                match c.try_wait() {
                    Ok(Some(status)) if !status.success() => {
                        tracing::error!("plugin host crashed for '{}' ({})", self.name, self.path);
                        self.crash_count.fetch_add(1, Ordering::Relaxed);
                        ipc::bypass_copy_inputs_to_outputs(&self.audio_inputs, &self.audio_outputs);
                        return Vec::new();
                    }
                    _ => {}
                }
            }
        }

        let started = Instant::now();

        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => {
                ipc::bypass_copy_inputs_to_outputs(&self.audio_inputs, &self.audio_outputs);
                return Vec::new();
            }
        };

        let ptr = mapping.as_ptr();
        unsafe {
            ipc::configure_shm_header(
                ptr,
                frames,
                self.audio_inputs.len(),
                self.audio_outputs.len(),
            );
            ipc::copy_inputs_to_shm(&self.audio_inputs, ptr, frames);

            // Write transport state.
            let t = transport_mut(ptr);
            t.playhead_sample = transport.transport_sample as u64;
            t.tempo = transport.bpm;
            t.numerator = transport.tsig_num as u32;
            t.denominator = transport.tsig_denom as u32;
            t.flags = if transport.playing { 1 } else { 0 };

            // Write MIDI input events to the shared-memory ring buffer.
            let midi_buf = midi_ring_ptr(ptr);
            let (midi_w, midi_r) = midi_indices(ptr);
            let midi_ring = RingBuffer::new(midi_buf, midi_w, midi_r, RING_CAPACITY);
            if !midi_in.is_empty() {
                eprintln!(
                    "[CLAP-PROC] {} forwarding {} MIDI events to host",
                    self.name,
                    midi_in.len()
                );
            }
            for ev in midi_in {
                let midi_event = maolan_plugin_protocol::protocol::MidiEvent {
                    sample_offset: ev.frame,
                    data: [
                        ev.data.first().copied().unwrap_or(0),
                        ev.data.get(1).copied().unwrap_or(0),
                        ev.data.get(2).copied().unwrap_or(0),
                    ],
                    channel: ev.data.first().map(|b| b & 0x0F).unwrap_or(0),
                    flags: 0,
                    _pad: 0,
                };
                if !midi_ring.push(midi_event) {
                    tracing::warn!(
                        "MIDI input ring full for '{}' ({}), dropping event",
                        self.name,
                        self.path
                    );
                    break;
                }
            }
        }

        if let Err(e) = events.signal_host() {
            tracing::error!("Failed to signal host: {e}");
            ipc::bypass_copy_inputs_to_outputs(&self.audio_inputs, &self.audio_outputs);
            return Vec::new();
        }

        let timeout = Duration::from_millis(100);
        if let Err(e) = events.wait_host(timeout) {
            tracing::error!(
                "host did not respond for '{}' ({}): {e}",
                self.name,
                self.path
            );
            ipc::bypass_copy_inputs_to_outputs(&self.audio_inputs, &self.audio_outputs);
            return Vec::new();
        }

        unsafe {
            ipc::copy_outputs_from_shm(&self.audio_outputs, ptr, frames);
        }

        // Read MIDI output events from the plugin host.
        let mut midi_out = Vec::new();
        unsafe {
            let midi_out_buf = midi_out_ring_ptr(ptr);
            let (midi_out_w, midi_out_r) = midi_out_indices(ptr);
            let midi_out_ring =
                RingBuffer::new(midi_out_buf, midi_out_w, midi_out_r, RING_CAPACITY);
            while let Some(ev) = midi_out_ring.pop() {
                midi_out.push(ClapMidiOutputEvent {
                    port: 0,
                    event: crate::midi::io::MidiEvent::new(ev.sample_offset, ev.data.to_vec()),
                });
            }
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
        midi_out
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
    pub fn ui_take_state_update(&self) -> Option<crate::plugins::types::ClapPluginState> {
        None
    }

    pub fn gui_info(&self) -> Result<crate::plugins::types::ClapGuiInfo, String> {
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
            header.set_parent_window(window);
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

    pub fn drain_midi_outputs(&self) -> Vec<crate::midi::io::MidiEvent> {
        let mut result = Vec::new();
        if let Some(ref mapping) = self.mapping {
            let ring = unsafe {
                let buf = midi_out_ring_ptr(mapping.as_ptr());
                let (w, r) = midi_out_indices(mapping.as_ptr());
                RingBuffer::new(buf, w, r, RING_CAPACITY)
            };
            while let Some(ev) = ring.pop() {
                result.push(crate::midi::io::MidiEvent {
                    frame: ev.sample_offset,
                    data: ev.data.to_vec(),
                });
            }
        }
        result
    }
}

impl Drop for ClapProcessor {
    fn drop(&mut self) {
        ipc::drop_host(&self.mapping, &self.events, &self.child, &self.shm_name);
    }
}

crate::impl_ipc_processor_wrapper!(ClapProcessor);

impl UnsafeMutex<ClapProcessor> {
    pub fn process_with_midi(
        &self,
        frames: usize,
        midi_events: &[MidiEvent],
        transport: ClapTransportInfo,
    ) -> Vec<ClapMidiOutputEvent> {
        self.lock()
            .process_with_midi(frames, midi_events, transport)
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

    pub fn snapshot_state(&self) -> Result<crate::plugins::types::ClapPluginState, String> {
        self.lock().snapshot_state()
    }

    pub fn restore_state(
        &self,
        state: &crate::plugins::types::ClapPluginState,
    ) -> Result<(), String> {
        self.lock().restore_state(state)
    }

    pub fn path(&self) -> String {
        self.lock().path().to_string()
    }

    pub fn plugin_id(&self) -> String {
        self.lock().plugin_id().to_string()
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

    pub fn ui_take_state_update(&self) -> Option<crate::plugins::types::ClapPluginState> {
        self.lock().ui_take_state_update()
    }

    pub fn gui_info(&self) -> Result<crate::plugins::types::ClapGuiInfo, String> {
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
fn split_plugin_spec(spec: &str) -> (&str, &str) {
    // CLAP scanner uses "path::id"; host protocol uses "path#id".
    if let Some(pos) = spec.rfind("::") {
        (&spec[..pos], &spec[pos + 2..])
    } else if let Some(pos) = spec.rfind('#') {
        (&spec[..pos], &spec[pos + 1..])
    } else {
        (spec, "")
    }
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

        // Process directly through the plugin processor to verify IPC works.
        // Track-level routing requires explicit audio connections; this test
        // verifies that a plugin loaded on a track can process audio correctly.
        let processor = track.clap_plugins[0].processor.lock();
        processor.setup_audio_ports();

        for (i, input) in processor.audio_inputs().iter().enumerate() {
            let buf = input.buffer.lock();
            for (j, sample) in buf.iter_mut().enumerate() {
                *sample = (i * 1000 + j) as f32;
            }
            *input.finished.lock() = true;
        }

        processor.process_with_audio_io(256);

        for (ch, output) in processor.audio_outputs().iter().enumerate() {
            let buf = output.buffer.lock();
            assert!(
                buf.iter().any(|&s| s != 0.0),
                "plugin output ch={ch} should contain non-zero samples after CLAP processing"
            );
        }
    }
}
