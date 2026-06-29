use crate::audio::io::AudioIO;
use crate::midi::io::{MidiEvent, MIDIIO};
use crate::mutex::UnsafeMutex;
use crate::plugins::ipc;
use maolan_plugin_protocol::events::EventPair;
use maolan_plugin_protocol::protocol::*;
use maolan_plugin_protocol::ringbuf::RingBuffer;
use maolan_plugin_protocol::shm::ShmMapping;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, ChildStderr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, atomic::AtomicU32};
use std::time::{Duration, Instant};

pub struct Lv2Processor {
    uri: String,
    name: String,
    audio_inputs: Vec<Arc<AudioIO>>,
    audio_outputs: Vec<Arc<AudioIO>>,
    main_audio_inputs: usize,
    main_audio_outputs: usize,
    midi_input_ports: Vec<Arc<UnsafeMutex<Box<MIDIIO>>>>,
    midi_output_ports: Vec<Arc<UnsafeMutex<Box<MIDIIO>>>>,
    param_values: UnsafeMutex<HashMap<u32, f64>>,
    bypassed: Arc<AtomicBool>,

    child: UnsafeMutex<Option<Child>>,
    stderr: UnsafeMutex<Option<ChildStderr>>,
    mapping: Option<ShmMapping>,
    events: Option<EventPair>,
    shm_name: String,

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

        let instance_id = ipc::unique_instance_id("lv2");
        let sample_rate_str = sample_rate.to_string();
        let buffer_size_str = buffer_size.to_string();
        let num_inputs_str = input_count.max(1).to_string();
        let num_outputs_str = output_count.max(1).to_string();
        let (mut child, mapping, events, shm_name, stderr) = ipc::spawn_host(ipc::HostSpawnArgs {
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

        let name = unsafe {
            maolan_plugin_protocol::protocol::read_plugin_name_from_scratch(mapping.as_ptr())
                .unwrap_or_else(|| {
                    plugin_uri
                        .rsplit_once('/')
                        .map(|(_, name)| name)
                        .unwrap_or(plugin_uri)
                        .to_string()
                })
        };

        let header = unsafe { header_ref(mapping.as_ptr()) };
        let midi_in_count = header.midi_in_port_count.load(Ordering::Acquire) as usize;
        let midi_out_count = header.midi_out_port_count.load(Ordering::Acquire) as usize;
        let midi_input_ports: Vec<_> = (0..midi_in_count)
            .map(|_| Arc::new(UnsafeMutex::new(Box::new(MIDIIO::new()))))
            .collect();
        let midi_output_ports: Vec<_> = (0..midi_out_count)
            .map(|_| Arc::new(UnsafeMutex::new(Box::new(MIDIIO::new()))))
            .collect();

        Ok(Self {
            uri: plugin_uri.to_string(),
            name,
            audio_inputs,
            audio_outputs,
            main_audio_inputs: input_count.max(1),
            main_audio_outputs: output_count.max(1),
            midi_input_ports,
            midi_output_ports,
            param_values: UnsafeMutex::new(HashMap::new()),
            bypassed: Arc::new(AtomicBool::new(false)),
            child: UnsafeMutex::new(Some(child)),
            stderr: UnsafeMutex::new(stderr),
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

    pub fn setup_midi_ports(&self) {
        for port in &self.midi_input_ports {
            port.lock().setup();
        }
        for port in &self.midi_output_ports {
            port.lock().setup();
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
        self.midi_input_ports.len()
    }

    pub fn midi_output_count(&self) -> usize {
        self.midi_output_ports.len()
    }

    pub fn midi_input_ports(&self) -> &[Arc<UnsafeMutex<Box<MIDIIO>>>] {
        &self.midi_input_ports
    }

    pub fn midi_output_ports(&self) -> &[Arc<UnsafeMutex<Box<MIDIIO>>>] {
        &self.midi_output_ports
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
            if !ring.push(ev) {}
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

    pub fn set_resource_directory(&self, dir: &std::path::Path) -> Result<(), String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("LV2 processor not initialized".to_string()),
        };
        let ptr = mapping.as_ptr();
        let header = unsafe { header_mut(ptr) };
        let path_str = dir.to_string_lossy().to_string();
        unsafe {
            write_resource_directory_to_scratch(ptr, &path_str)
                .map_err(|e| format!("Failed to write resource directory: {e}"))?;
        }
        std::sync::atomic::fence(Ordering::SeqCst);

        header.request_type.store(5, Ordering::Release);
        header.request_status.store(0, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Failed to signal host for resource directory: {e}"));
        }

        if let Err(e) = events.wait_host(Duration::from_secs(5)) {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Host did not respond to resource directory: {e}"));
        }

        let status = header.request_status.load(Ordering::Acquire);
        header.request_type.store(0, Ordering::Release);
        if status != 1 {
            return Err("Resource directory update failed in host".to_string());
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

        {
            let child = self.child.lock();
            if let Some(ref mut c) = child.as_mut() {
                match c.try_wait() {
                    Ok(Some(status)) if !status.success() => {
                        self.crash_count.fetch_add(1, Ordering::Relaxed);
                        ipc::bypass_copy_inputs_to_outputs(&self.audio_inputs, &self.audio_outputs);
                        return Vec::new();
                    }
                    Ok(Some(_status)) => {}
                    Ok(None) => {}
                    Err(_) => {}
                }
            }
        }

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
        let midi_in_count = self.midi_input_ports.len();
        let midi_out_count = self.midi_output_ports.len();
        unsafe {
            ipc::configure_shm_header(ptr, frames, num_in, num_out, midi_in_count, midi_out_count);

            let t = transport_mut(ptr);
            t.playhead_sample = 0;
            t.tempo = 120.0;
            t.numerator = 4;
            t.denominator = 4;
            t.flags = 1;

            ipc::copy_inputs_to_shm(&self.audio_inputs, ptr, frames);

            for (port_idx, port) in self.midi_input_ports.iter().enumerate() {
                let buf = midi_in_ring_ptr(ptr, port_idx);
                let (w, r) = midi_in_indices(ptr, port_idx);
                let ring = RingBuffer::new(buf, w, r, RING_CAPACITY);
                let lock = port.lock();
                for ev in &lock.buffer {
                    let data = {
                        let mut d = [0u8; 3];
                        for (i, b) in ev.data.iter().enumerate().take(3) {
                            d[i] = *b;
                        }
                        d
                    };
                    let _ = ring.push(maolan_plugin_protocol::MidiEvent {
                        sample_offset: ev.frame,
                        data,
                        channel: ev.data.first().copied().unwrap_or(0) & 0x0F,
                        flags: 0,
                        _pad: 0,
                    });
                }
                lock.mark_finished();
            }
        }

        if events.signal_host().is_err() {
            ipc::bypass_copy_inputs_to_outputs(&self.audio_inputs, &self.audio_outputs);
            return Vec::new();
        }

        let timeout = Duration::from_millis(100);
        match events.wait_host(timeout) {
            Ok(()) => {}
            Err(_) => {
                ipc::bypass_copy_inputs_to_outputs(&self.audio_inputs, &self.audio_outputs);
                return Vec::new();
            }
        }

        unsafe {
            ipc::copy_outputs_from_shm(&self.audio_outputs, ptr, frames);

            let mut output_events = Vec::new();
            for (port_idx, port) in self.midi_output_ports.iter().enumerate() {
                let buf = midi_out_ring_ptr(ptr, port_idx);
                let (w, r) = midi_out_indices(ptr, port_idx);
                let ring = RingBuffer::new(buf, w, r, RING_CAPACITY);
                let lock = port.lock();
                lock.buffer.clear();
                while let Some(ev) = ring.pop() {
                    let event = MidiEvent {
                        frame: ev.sample_offset,
                        data: ev.data.to_vec(),
                    };
                    lock.buffer.push(event.clone());
                    output_events.push(event);
                }
                lock.mark_finished();
            }
            *self.last_process_time.lock() = Instant::now();
            return output_events;
        }
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn take_stderr(&self) -> Option<ChildStderr> {
        self.stderr.lock().take()
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
                let buf = midi_out_ring_ptr(mapping.as_ptr(), 0);
                let (w, r) = midi_out_indices(mapping.as_ptr(), 0);
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

        {
            let buf = processor.audio_inputs()[0].buffer.lock();
            buf.fill(1.0);
            *processor.audio_inputs()[0].finished.lock() = true;
        }

        processor.process_with_audio_io(256);

        let out_buf = processor.audio_outputs()[0].buffer.lock();
        let first_few: Vec<f32> = out_buf.iter().take(10).copied().collect();
        assert!(
            out_buf.iter().all(|&s| s == 1.0),
            "after crash, output should be bypass copy of input, got: {:?}",
            first_few
        );
    }
}
