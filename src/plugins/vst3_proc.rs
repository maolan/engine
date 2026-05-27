//! Out-of-process VST3 processor using `maolan-plugin-host` IPC.

use crate::audio::io::AudioIO;
use crate::midi::io::MidiEvent;
use crate::mutex::UnsafeMutex;
use crate::plugins::ipc;
use crate::plugins::types::ParameterInfo;
use crate::plugins::types::Vst3PluginState;
use maolan_plugin_protocol::events::EventPair;
use maolan_plugin_protocol::protocol::*;
use maolan_plugin_protocol::ringbuf::RingBuffer;
use maolan_plugin_protocol::shm::ShmMapping;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, atomic::AtomicU32};
use std::time::{Duration, Instant};

/// Shared state for an out-of-process VST3 plugin instance.
pub struct Vst3Processor {
    path: String,
    name: String,
    audio_inputs: Vec<Arc<AudioIO>>,
    audio_outputs: Vec<Arc<AudioIO>>,
    main_audio_inputs: usize,
    main_audio_outputs: usize,
    param_infos: Vec<ParameterInfo>,
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

pub type SharedVst3Processor = Arc<UnsafeMutex<Vst3Processor>>;

impl Vst3Processor {
    pub fn new(
        sample_rate: f64,
        buffer_size: usize,
        plugin_path: &str,
        input_count: usize,
        output_count: usize,
        host_binary: PathBuf,
    ) -> Result<Self, String> {
        let audio_inputs = (0..input_count.max(1))
            .map(|_| Arc::new(AudioIO::new(buffer_size)))
            .collect::<Vec<_>>();
        let audio_outputs = (0..output_count.max(1))
            .map(|_| Arc::new(AudioIO::new(buffer_size)))
            .collect::<Vec<_>>();

        let instance_id = format!("vst3-{}", std::process::id());
        let num_inputs = input_count.max(1);
        let num_outputs = output_count.max(1);
        let (mut child, mapping, events, shm_name) = ipc::spawn_host(ipc::HostSpawnArgs {
            host_binary: &host_binary,
            format: "vst3",
            plugin_spec: plugin_path,
            instance_id: &instance_id,
            extra_args: &[
                &sample_rate.to_string(),
                &buffer_size.to_string(),
                &num_inputs.to_string(),
                &num_outputs.to_string(),
            ],
        })?;

        let header = unsafe { header_ref(mapping.as_ptr()) };
        if !ipc::wait_for_ready(header, Duration::from_secs(10)) {
            let _ = child.kill();
            return Err("VST3 host did not signal ready".to_string());
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
            name.unwrap_or_else(|| {
                Path::new(plugin_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("VST3")
                    .to_string()
            })
        };

        let param_infos = Vec::new();

        Ok(Self {
            path: plugin_path.to_string(),
            name,
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
        0
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

    pub fn parameter_infos(&self) -> Vec<ParameterInfo> {
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
                tracing::warn!("VST3 param ring full, dropping parameter event");
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

    pub fn snapshot_state(&self) -> Result<Vst3PluginState, String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("VST3 processor not initialized".to_string()),
        };
        let ptr = mapping.as_ptr();
        let header = unsafe { header_mut(ptr) };

        // Signal host to save state.
        header.request_type.store(1, Ordering::Release);
        header.request_status.store(0, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Failed to signal host for state save: {}", e));
        }

        // Wait for host to complete (up to 5 seconds).
        if let Err(e) = events.wait_host(Duration::from_secs(5)) {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Host did not respond to state save: {}", e));
        }

        let status = header.request_status.load(Ordering::Acquire);
        let size = header.scratch_size.load(Ordering::Acquire) as usize;
        if status != 1 {
            header.request_type.store(0, Ordering::Release);
            return Err("State save failed in host".to_string());
        }

        let scratch = unsafe { scratch_ptr(ptr) };
        let state = deserialize_vst3_state(scratch, size)?;
        header.request_type.store(0, Ordering::Release);
        Ok(state)
    }

    pub fn restore_state(&self, state: &Vst3PluginState) -> Result<(), String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("VST3 processor not initialized".to_string()),
        };
        let ptr = mapping.as_ptr();
        let header = unsafe { header_mut(ptr) };

        // Serialize state into scratch area.
        let scratch = unsafe { scratch_ptr(ptr) };
        let size = serialize_vst3_state(scratch, state)?;
        header.scratch_size.store(size as u32, Ordering::Release);

        // Signal host to restore state.
        header.request_type.store(2, Ordering::Release);
        header.request_status.store(0, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Failed to signal host for state restore: {}", e));
        }

        // Wait for host to complete (up to 5 seconds).
        if let Err(e) = events.wait_host(Duration::from_secs(5)) {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Host did not respond to state restore: {}", e));
        }

        let status = header.request_status.load(Ordering::Acquire);
        header.request_type.store(0, Ordering::Release);
        if status != 1 {
            return Err("State restore failed in host".to_string());
        }
        Ok(())
    }

    pub fn process_with_audio_io(&self, frames: usize) {
        let _ = self.process_with_midi(frames, &[]);
    }

    pub fn process_with_midi(&self, frames: usize, _midi_in: &[MidiEvent]) -> Vec<MidiEvent> {
        if self.bypassed.load(Ordering::Relaxed) {
            ipc::bypass_copy_inputs_to_outputs(&self.audio_inputs, &self.audio_outputs);
            return Vec::new();
        }

        // Check if host process has crashed.
        {
            let child = self.child.lock();
            if let Some(ref mut c) = child.as_mut() {
                match c.try_wait() {
                    Ok(Some(status)) if !status.success() => {
                        tracing::error!(
                            "VST3 plugin host crashed for '{}' ({})",
                            self.name,
                            self.path
                        );
                        self.crash_count.fetch_add(1, Ordering::Relaxed);
                        ipc::bypass_copy_inputs_to_outputs(&self.audio_inputs, &self.audio_outputs);
                        return Vec::new();
                    }
                    Ok(None) => {
                        eprintln!("[VST3 debug] host still alive");
                    }
                    Ok(Some(status)) => {
                        eprintln!("[VST3 debug] host exited with success: {:?}", status);
                    }
                    Err(e) => {
                        eprintln!("[VST3 debug] try_wait error: {}", e);
                    }
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
        let num_in = self.audio_inputs.len();
        let num_out = self.audio_outputs.len();
        unsafe {
            ipc::configure_shm_header(ptr, frames, num_in, num_out);
            // Write default transport state (can be overridden by track later).
            let t = transport_mut(ptr);
            t.playhead_sample = 0;
            t.tempo = 120.0;
            t.numerator = 4;
            t.denominator = 4;
            t.flags = 1; // playing

            // Copy input AudioIO buffers to shared memory (bus 0).
            ipc::copy_inputs_to_shm(&self.audio_inputs, ptr, frames);
        }

        // Signal host to process.
        if let Err(e) = events.signal_host() {
            tracing::error!("Failed to signal VST3 host: {e}");
            ipc::bypass_copy_inputs_to_outputs(&self.audio_inputs, &self.audio_outputs);
            return Vec::new();
        }
        eprintln!("[VST3 debug] signal_host succeeded");

        // Wait for host to complete (with timeout).
        let timeout = Duration::from_millis(100);
        match events.wait_host(timeout) {
            Ok(()) => {
                eprintln!("[VST3 debug] wait_host succeeded");
            }
            Err(e) => {
                eprintln!(
                    "[VST3 debug] host did not respond for '{}' ({}): {}",
                    self.name, self.path, e
                );
                ipc::bypass_copy_inputs_to_outputs(&self.audio_inputs, &self.audio_outputs);
                return Vec::new();
            }
        }

        // Copy output shared memory (bus 1) back to AudioIO buffers.
        unsafe {
            ipc::copy_outputs_from_shm(&self.audio_outputs, ptr, frames);
        }

        let elapsed = started.elapsed();
        if elapsed > Duration::from_millis(20) {
            tracing::warn!(
                "Slow VST3 process '{}' ({}) took {:.3} ms for {} frames",
                self.name,
                self.path,
                elapsed.as_secs_f64() * 1000.0,
                frames
            );
        }

        *self.last_process_time.lock() = Instant::now();
        Vec::new()
    }

    pub fn path(&self) -> &str {
        &self.path
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
    pub fn ui_take_param_updates(&self) -> Vec<(u32, f64)> {
        Vec::new()
    }
    pub fn ui_take_state_update(&self) -> Option<Vst3PluginState> {
        None
    }

    pub fn gui_info(&self) -> Result<crate::plugins::types::Vst3GuiInfo, String> {
        Err("GUI not yet supported for VST3 plugins".to_string())
    }

    pub fn gui_create(&self, _platform_type: &str) -> Result<(), String> {
        Err("GUI not yet supported for VST3 plugins".to_string())
    }

    pub fn gui_get_size(&self) -> Result<(i32, i32), String> {
        Err("GUI not yet supported for VST3 plugins".to_string())
    }

    pub fn gui_set_parent(&self, _window: usize, _platform_type: &str) -> Result<(), String> {
        Err("GUI not yet supported for VST3 plugins".to_string())
    }

    pub fn gui_on_size(&self, _width: i32, _height: i32) -> Result<(), String> {
        Err("GUI not yet supported for VST3 plugins".to_string())
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

    pub fn gui_check_resize(&self) -> Option<(i32, i32)> {
        None
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

impl Drop for Vst3Processor {
    fn drop(&mut self) {
        ipc::drop_host(&self.mapping, &self.events, &self.child, &self.shm_name);
    }
}

impl UnsafeMutex<Vst3Processor> {
    pub fn setup_audio_ports(&self) {
        self.lock().setup_audio_ports();
    }

    pub fn process_with_midi(&self, frames: usize, midi_events: &[MidiEvent]) -> Vec<MidiEvent> {
        self.lock().process_with_midi(frames, midi_events)
    }

    pub fn set_bypassed(&self, bypassed: bool) {
        self.lock().set_bypassed(bypassed);
    }

    pub fn is_bypassed(&self) -> bool {
        self.lock().is_bypassed()
    }

    pub fn parameter_infos(&self) -> Vec<ParameterInfo> {
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

    pub fn snapshot_state(&self) -> Result<Vst3PluginState, String> {
        self.lock().snapshot_state()
    }

    pub fn restore_state(&self, state: &Vst3PluginState) -> Result<(), String> {
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

    pub fn ui_take_param_updates(&self) -> Vec<(u32, f64)> {
        self.lock().ui_take_param_updates()
    }

    pub fn ui_take_state_update(&self) -> Option<Vst3PluginState> {
        self.lock().ui_take_state_update()
    }

    pub fn gui_info(&self) -> Result<crate::plugins::types::Vst3GuiInfo, String> {
        self.lock().gui_info()
    }

    pub fn gui_create(&self, platform_type: &str) -> Result<(), String> {
        self.lock().gui_create(platform_type)
    }

    pub fn gui_get_size(&self) -> Result<(i32, i32), String> {
        self.lock().gui_get_size()
    }

    pub fn gui_set_parent(&self, window: usize, platform_type: &str) -> Result<(), String> {
        self.lock().gui_set_parent(window, platform_type)
    }

    pub fn gui_on_size(&self, width: i32, height: i32) -> Result<(), String> {
        self.lock().gui_on_size(width, height)
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

    pub fn gui_check_resize(&self) -> Option<(i32, i32)> {
        self.lock().gui_check_resize()
    }
}

/// Serialize VST3 state into scratch area. Returns bytes written or error.
fn serialize_vst3_state(scratch: *mut u8, state: &Vst3PluginState) -> Result<usize, String> {
    let max_len = maolan_plugin_protocol::protocol::SCRATCH_SIZE;
    let mut offset = 0usize;

    let plugin_id_bytes = state.plugin_id.as_bytes();
    if offset + 4 > max_len {
        return Err("scratch overflow".to_string());
    }
    unsafe {
        std::ptr::write_unaligned(
            scratch.add(offset) as *mut u32,
            plugin_id_bytes.len() as u32,
        );
    }
    offset += 4;
    if offset + plugin_id_bytes.len() > max_len {
        return Err("scratch overflow".to_string());
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            plugin_id_bytes.as_ptr(),
            scratch.add(offset),
            plugin_id_bytes.len(),
        );
    }
    offset += plugin_id_bytes.len();

    if offset + 4 > max_len {
        return Err("scratch overflow".to_string());
    }
    unsafe {
        std::ptr::write_unaligned(
            scratch.add(offset) as *mut u32,
            state.component_state.len() as u32,
        );
    }
    offset += 4;
    if offset + state.component_state.len() > max_len {
        return Err("scratch overflow".to_string());
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            state.component_state.as_ptr(),
            scratch.add(offset),
            state.component_state.len(),
        );
    }
    offset += state.component_state.len();

    if offset + 4 > max_len {
        return Err("scratch overflow".to_string());
    }
    unsafe {
        std::ptr::write_unaligned(
            scratch.add(offset) as *mut u32,
            state.controller_state.len() as u32,
        );
    }
    offset += 4;
    if offset + state.controller_state.len() > max_len {
        return Err("scratch overflow".to_string());
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            state.controller_state.as_ptr(),
            scratch.add(offset),
            state.controller_state.len(),
        );
    }
    offset += state.controller_state.len();

    Ok(offset)
}

/// Deserialize VST3 state from scratch area.
fn deserialize_vst3_state(scratch: *const u8, size: usize) -> Result<Vst3PluginState, String> {
    if size < 12 {
        return Err("scratch too small for VST3 state".to_string());
    }
    let mut offset = 0usize;

    let plugin_id_len =
        unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
    offset += 4;
    if offset + plugin_id_len > size {
        return Err("scratch underflow".to_string());
    }
    let mut plugin_id_bytes = vec![0u8; plugin_id_len];
    unsafe {
        std::ptr::copy_nonoverlapping(
            scratch.add(offset),
            plugin_id_bytes.as_mut_ptr(),
            plugin_id_len,
        );
    }
    offset += plugin_id_len;
    let plugin_id = String::from_utf8(plugin_id_bytes).map_err(|e| e.to_string())?;

    let component_state_len =
        unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
    offset += 4;
    if offset + component_state_len > size {
        return Err("scratch underflow".to_string());
    }
    let mut component_state = vec![0u8; component_state_len];
    unsafe {
        std::ptr::copy_nonoverlapping(
            scratch.add(offset),
            component_state.as_mut_ptr(),
            component_state_len,
        );
    }
    offset += component_state_len;

    let controller_state_len =
        unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
    offset += 4;
    if offset + controller_state_len > size {
        return Err("scratch underflow".to_string());
    }
    let mut controller_state = vec![0u8; controller_state_len];
    unsafe {
        std::ptr::copy_nonoverlapping(
            scratch.add(offset),
            controller_state.as_mut_ptr(),
            controller_state_len,
        );
    }

    Ok(Vst3PluginState {
        plugin_id,
        component_state,
        controller_state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_host_binary() -> PathBuf {
        ipc::find_plugin_host_binary().expect("maolan-plugin-host binary should be built for tests")
    }

    #[test]
    fn find_host_binary_locates_binary() {
        let host_bin = find_host_binary();
        assert!(
            host_bin.exists(),
            "plugin-host binary should exist at {}",
            host_bin.display()
        );
    }

    #[test]
    fn vst3_state_serialization_roundtrip() {
        let state = Vst3PluginState {
            plugin_id: "test.plugin.vst3".to_string(),
            component_state: vec![1, 2, 3, 4, 5],
            controller_state: vec![10, 20, 30],
        };
        let mut scratch = vec![0u8; SCRATCH_SIZE];
        let size =
            serialize_vst3_state(scratch.as_mut_ptr(), &state).expect("serialize should succeed");
        assert!(size > 0);
        assert!(size < SCRATCH_SIZE);

        let decoded =
            deserialize_vst3_state(scratch.as_ptr(), size).expect("deserialize should succeed");
        assert_eq!(decoded.plugin_id, state.plugin_id);
        assert_eq!(decoded.component_state, state.component_state);
        assert_eq!(decoded.controller_state, state.controller_state);
    }

    #[test]
    fn vst3_processor_crash_bypass() {
        let host_bin = find_host_binary();

        let processor = Vst3Processor::new(48000.0, 256, "__crash__", 1, 1, host_bin)
            .expect("should create VST3 processor for crash test");

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
}
