//! Out-of-process LV2 processor using `maolan-plugin-host` IPC.

use crate::audio::io::AudioIO;
use crate::midi::io::MidiEvent;
use crate::mutex::UnsafeMutex;
use crate::plugins::ipc;
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

/// Shared state for an out-of-process LV2 plugin instance.
pub struct Lv2Processor {
    uri: String,
    name: String,
    audio_inputs: Vec<Arc<AudioIO>>,
    audio_outputs: Vec<Arc<AudioIO>>,
    main_audio_inputs: usize,
    main_audio_outputs: usize,
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

pub type SharedLv2Processor = Arc<UnsafeMutex<Lv2Processor>>;

impl Lv2Processor {
    pub fn new(
        sample_rate: f64,
        buffer_size: usize,
        plugin_uri: &str,
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

        let instance_id = format!("lv2-{}", std::process::id());
        let sample_rate_str = sample_rate.to_string();
        let buffer_size_str = buffer_size.to_string();
        let num_inputs_str = input_count.max(1).to_string();
        let num_outputs_str = output_count.max(1).to_string();
        let (mut child, mapping, events, shm_name) = ipc::spawn_host(ipc::HostSpawnArgs {
            host_binary: &host_binary,
            format: "lv2",
            plugin_spec: plugin_uri,
            instance_id: &instance_id,
            extra_args: &[
                &sample_rate_str,
                &buffer_size_str,
                &num_inputs_str,
                &num_outputs_str,
            ],
        })?;

        let header = unsafe { header_ref(mapping.as_ptr()) };
        if !ipc::wait_for_ready(header, Duration::from_secs(10)) {
            let _ = child.kill();
            return Err("LV2 host did not signal ready".to_string());
        }

        let name = plugin_uri
            .rsplit_once('/')
            .map(|(_, name)| name)
            .unwrap_or(plugin_uri)
            .to_string();

        Ok(Self {
            uri: plugin_uri.to_string(),
            name,
            audio_inputs,
            audio_outputs,
            main_audio_inputs: input_count.max(1),
            main_audio_outputs: output_count.max(1),
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
                tracing::warn!("LV2 param ring full, dropping parameter event");
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

    pub fn snapshot_state(&self) -> Result<Vec<u8>, String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("LV2 processor not initialized".to_string()),
        };
        let ptr = mapping.as_ptr();
        let header = unsafe { header_mut(ptr) };

        header.request_type.store(1, Ordering::Release);
        header.request_status.store(0, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Failed to signal host for state save: {}", e));
        }

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
        let mut bytes = vec![0u8; size];
        unsafe {
            std::ptr::copy_nonoverlapping(scratch, bytes.as_mut_ptr(), size);
        }
        header.request_type.store(0, Ordering::Release);
        Ok(bytes)
    }

    pub fn restore_state(&self, state: &[u8]) -> Result<(), String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("LV2 processor not initialized".to_string()),
        };
        let ptr = mapping.as_ptr();
        let header = unsafe { header_mut(ptr) };

        let scratch = unsafe { scratch_ptr(ptr) };
        let size = state.len().min(SCRATCH_SIZE);
        unsafe {
            std::ptr::copy_nonoverlapping(state.as_ptr(), scratch, size);
        }
        header.scratch_size.store(size as u32, Ordering::Release);

        header.request_type.store(2, Ordering::Release);
        header.request_status.store(0, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Failed to signal host for state restore: {}", e));
        }

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
                            "LV2 plugin host crashed for '{}' ({})",
                            self.name,
                            self.uri
                        );
                        self.crash_count.fetch_add(1, Ordering::Relaxed);
                        ipc::bypass_copy_inputs_to_outputs(&self.audio_inputs, &self.audio_outputs);
                        return Vec::new();
                    }
                    Ok(Some(status)) => {
                        eprintln!("[LV2 debug] host exited with success: {:?}", status);
                    }
                    Ok(None) => {
                        eprintln!("[LV2 debug] host still alive");
                    }
                    Err(e) => {
                        eprintln!("[LV2 debug] try_wait error: {}", e);
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
            tracing::error!("Failed to signal LV2 host: {e}");
            ipc::bypass_copy_inputs_to_outputs(&self.audio_inputs, &self.audio_outputs);
            return Vec::new();
        }
        eprintln!("[LV2 debug] signal_host succeeded");

        // Wait for host to complete (with timeout).
        let timeout = Duration::from_millis(100);
        match events.wait_host(timeout) {
            Ok(()) => {
                eprintln!("[LV2 debug] wait_host succeeded");
            }
            Err(e) => {
                eprintln!(
                    "[LV2 debug] host did not respond for '{}' ({}): {}",
                    self.name, self.uri, e
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
                "Slow LV2 process '{}' ({}) took {:.3} ms for {} frames",
                self.name,
                self.uri,
                elapsed.as_secs_f64() * 1000.0,
                frames
            );
        }

        *self.last_process_time.lock() = Instant::now();
        Vec::new()
    }

    pub fn uri(&self) -> &str {
        &self.uri
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

impl Drop for Lv2Processor {
    fn drop(&mut self) {
        ipc::drop_host(&self.mapping, &self.events, &self.child, &self.shm_name);
    }
}

crate::impl_ipc_processor_wrapper!(Lv2Processor);

impl UnsafeMutex<Lv2Processor> {
    pub fn process_with_midi(&self, frames: usize, midi_events: &[MidiEvent]) -> Vec<MidiEvent> {
        self.lock().process_with_midi(frames, midi_events)
    }

    pub fn snapshot_state(&self) -> Result<Vec<u8>, String> {
        self.lock().snapshot_state()
    }

    pub fn restore_state(&self, state: &[u8]) -> Result<(), String> {
        self.lock().restore_state(state)
    }

    pub fn drain_echoed_parameters(&self) -> Vec<ParameterEvent> {
        self.lock().drain_echoed_parameters()
    }

    pub fn drain_midi_outputs(&self) -> Vec<crate::midi::io::MidiEvent> {
        self.lock().drain_midi_outputs()
    }
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
    fn lv2_processor_crash_bypass() {
        let host_bin = find_host_binary();

        let processor = Lv2Processor::new(48000.0, 256, "__crash__", 1, 1, host_bin)
            .expect("should create LV2 processor for crash test");

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
        let first_few: Vec<f32> = out_buf.iter().take(10).copied().collect();
        assert!(
            out_buf.iter().all(|&s| s == 1.0),
            "after crash, output should be bypass copy of input, got: {:?}",
            first_few
        );
    }
}
