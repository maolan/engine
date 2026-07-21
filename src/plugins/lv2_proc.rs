use crate::audio::io::AudioIO;
use crate::midi::io::{MIDIIO, MidiEvent};
use crate::plugins::ipc;
use arc_swap::ArcSwapOption;
use maolan_plugin_protocol::events::EventPair;
use maolan_plugin_protocol::protocol::*;
use maolan_plugin_protocol::ringbuf::RingBuffer;
use maolan_plugin_protocol::shm::ShmMapping;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, ChildStderr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const SHM_LATENCY_SAMPLES_OFFSET: usize = 84;

unsafe fn latency_samples_atomic(ptr: *mut u8) -> &'static AtomicU32 {
    unsafe { &*(ptr.add(SHM_LATENCY_SAMPLES_OFFSET) as *const AtomicU32) }
}

fn wait_for_host_request_complete(
    header: &ShmHeader,
    events: &EventPair,
    timeout: Duration,
) -> Result<(), String> {
    let start = Instant::now();
    loop {
        if header.request_status.load(Ordering::Acquire) != 0 {
            return Ok(());
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return Err("Host did not respond to request".to_string());
        }
        match events.wait_host(timeout - elapsed) {
            Ok(()) => {
                std::sync::atomic::fence(Ordering::Acquire);
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(e) => return Err(format!("Host did not respond to request: {e}")),
        }
    }
}

pub struct Lv2Processor {
    uri: String,
    name: String,
    audio_inputs: Vec<Arc<AudioIO>>,
    audio_outputs: Vec<Arc<AudioIO>>,
    main_audio_inputs: usize,
    main_audio_outputs: usize,
    midi_input_ports: Vec<Arc<MIDIIO>>,
    midi_output_ports: Vec<Arc<MIDIIO>>,
    /// Current value of every known parameter, keyed by parameter id and
    /// stored as `f64` bits. Only ever touched through atomic loads/stores.
    param_values: HashMap<u32, AtomicU64>,
    bypassed: Arc<AtomicBool>,

    /// Host child process handle. Interior-mutable so `process_with_audio_buffers`
    /// (which takes `&self`) can poll it with `try_wait`.
    ///
    /// Invariant: at any moment there is at most one accessor — either the
    /// audio thread running this plugin's own plan task node (exactly one per
    /// cycle), or control code running after the last `Arc` reference to this
    /// processor is gone (`Drop`).
    child: UnsafeCell<Option<Child>>,
    /// Host stderr pipe; control-side only (`take_stderr`). RCU-published so
    /// no blocking primitive is involved.
    stderr: ArcSwapOption<ChildStderr>,
    mapping: Option<ShmMapping>,
    events: Option<EventPair>,
    shm_name: String,

    crash_count: AtomicU32,
    last_latency_samples: AtomicUsize,
    latency_changed: AtomicBool,
}

// Safety: see the invariants on `child` and `stderr` above. Every other field
// is either immutable after construction or synchronized on its own
// (atomics / RCU).
unsafe impl Sync for Lv2Processor {}

pub type SharedLv2Processor = Arc<Lv2Processor>;

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
            .map(|_| Arc::new(MIDIIO::new()))
            .collect();
        let midi_output_ports: Vec<_> = (0..midi_out_count)
            .map(|_| Arc::new(MIDIIO::new()))
            .collect();

        let param_values: HashMap<u32, AtomicU64> = HashMap::new();

        Ok(Self {
            uri: plugin_uri.to_string(),
            name,
            audio_inputs,
            audio_outputs,
            main_audio_inputs: input_count.max(1),
            main_audio_outputs: output_count.max(1),
            midi_input_ports,
            midi_output_ports,
            param_values,
            bypassed: Arc::new(AtomicBool::new(false)),
            child: UnsafeCell::new(Some(child)),
            stderr: ArcSwapOption::from_pointee(stderr),
            mapping: Some(mapping),
            events: Some(events),
            shm_name,
            crash_count: AtomicU32::new(0),
            last_latency_samples: AtomicUsize::new(0),
            latency_changed: AtomicBool::new(false),
        })
    }

    /// Access the host child process handle.
    ///
    /// # Safety
    /// The caller must be the sole accessor of `child` at this time: either
    /// the audio thread running this plugin's own plan task node (exactly one
    /// per cycle), or control code running after the last `Arc` reference to
    /// this processor is gone.
    unsafe fn with_child<R>(&self, f: impl FnOnce(&mut Option<Child>) -> R) -> R {
        f(unsafe { &mut *self.child.get() })
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
            // Safety: plan single-writer invariant — this task is the sole
            // writer of its own ports this cycle; sources it reads were
            // produced by earlier plan nodes (LOCKLESS.md Phase 3).
            unsafe { port.setup() };
        }
        for port in &self.midi_output_ports {
            // Safety: as above — sole writer of this port this cycle.
            unsafe { port.setup() };
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

    pub fn midi_input_ports(&self) -> &[Arc<MIDIIO>] {
        &self.midi_input_ports
    }

    pub fn midi_output_ports(&self) -> &[Arc<MIDIIO>] {
        &self.midi_output_ports
    }

    pub fn set_bypassed(&self, bypassed: bool) {
        self.bypassed.store(bypassed, Ordering::Relaxed);
    }

    pub fn is_bypassed(&self) -> bool {
        self.bypassed.load(Ordering::Relaxed)
    }

    pub fn latency_samples(&self) -> usize {
        let latency = self
            .mapping
            .as_ref()
            .map(|mapping| unsafe {
                latency_samples_atomic(mapping.as_ptr()).load(Ordering::Acquire) as usize
            })
            .unwrap_or(0);
        let previous = self.last_latency_samples.swap(latency, Ordering::AcqRel);
        if previous != latency {
            self.latency_changed.store(true, Ordering::Release);
        }
        latency
    }

    pub fn take_latency_changed(&self) -> bool {
        self.latency_changed.swap(false, Ordering::AcqRel)
    }

    pub fn parameter_values(&self) -> HashMap<u32, f64> {
        self.param_values
            .iter()
            .map(|(&id, value)| (id, f64::from_bits(value.load(Ordering::Relaxed))))
            .collect()
    }

    pub fn set_parameter(&self, param_id: u32, value: f64) -> Result<(), String> {
        self.set_parameter_at(param_id, value, 0)
    }

    pub fn set_parameter_at(&self, param_id: u32, value: f64, _frame: u32) -> Result<(), String> {
        if let Some(slot) = self.param_values.get(&param_id) {
            slot.store(value.to_bits(), Ordering::Relaxed);
        } else {
            tracing::warn!("LV2 set_parameter_at: unknown parameter id {param_id}");
        }

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

        tracing::info!("LV2 snapshot_state: sending request to host");
        header.request_type.store(1, Ordering::Release);
        header.request_status.store(0, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Failed to signal host for state save: {}", e));
        }

        if let Err(e) = wait_for_host_request_complete(header, events, Duration::from_secs(5)) {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Host did not respond to state save: {}", e));
        }

        let status = header.request_status.load(Ordering::Acquire);
        let size = header.scratch_size.load(Ordering::Acquire) as usize;
        tracing::info!(status, size, "LV2 snapshot_state: host responded");
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

        if let Err(e) = wait_for_host_request_complete(header, events, Duration::from_secs(5)) {
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

    fn deserialize_lv2_control_ports(
        scratch: *const u8,
        size: usize,
    ) -> Result<Vec<crate::message::Lv2ControlPortInfo>, String> {
        if size < 4 {
            return Err("scratch too small for LV2 control ports".to_string());
        }
        let mut offset = 0usize;

        let count = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
        offset += 4;

        let mut ports = Vec::with_capacity(count);
        for _ in 0..count {
            if offset + 4 > size {
                return Err("scratch underflow".to_string());
            }
            let index = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) };
            offset += 4;

            if offset + 4 > size {
                return Err("scratch underflow".to_string());
            }
            let name_len =
                unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
            offset += 4;
            if offset + name_len > size {
                return Err("scratch underflow".to_string());
            }
            let mut name_bytes = vec![0u8; name_len];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    scratch.add(offset),
                    name_bytes.as_mut_ptr(),
                    name_len,
                );
            }
            offset += name_len;
            let name = String::from_utf8(name_bytes).map_err(|e| e.to_string())?;

            if offset + 12 > size {
                return Err("scratch underflow".to_string());
            }
            let min = f32::from_bits(unsafe {
                std::ptr::read_unaligned(scratch.add(offset) as *const u32)
            });
            let max = f32::from_bits(unsafe {
                std::ptr::read_unaligned(scratch.add(offset + 4) as *const u32)
            });
            let value = f32::from_bits(unsafe {
                std::ptr::read_unaligned(scratch.add(offset + 8) as *const u32)
            });
            offset += 12;

            ports.push(crate::message::Lv2ControlPortInfo {
                index,
                name,
                min,
                max,
                value,
            });
        }

        Ok(ports)
    }

    fn deserialize_lv2_note_names(
        scratch: *const u8,
        size: usize,
    ) -> Result<HashMap<u8, String>, String> {
        if size < 4 {
            return Err("scratch too small for LV2 note names".to_string());
        }
        let mut offset = 0usize;
        let count = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
        offset += 4;

        let mut note_names = HashMap::with_capacity(count);
        for _ in 0..count {
            if offset + 4 > size {
                return Err("scratch underflow".to_string());
            }
            let note = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as u8;
            offset += 4;

            if offset + 4 > size {
                return Err("scratch underflow".to_string());
            }
            let name_len =
                unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
            offset += 4;
            if offset + name_len > size {
                return Err("scratch underflow".to_string());
            }
            let mut name_bytes = vec![0u8; name_len];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    scratch.add(offset),
                    name_bytes.as_mut_ptr(),
                    name_len,
                );
            }
            offset += name_len;
            let name = String::from_utf8(name_bytes).map_err(|e| e.to_string())?;
            note_names.insert(note, name);
        }

        Ok(note_names)
    }

    pub fn control_ports(&self) -> Result<Vec<crate::message::Lv2ControlPortInfo>, String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("LV2 processor not initialized".to_string()),
        };
        let ptr = mapping.as_ptr();
        let header = unsafe { header_mut(ptr) };

        tracing::info!("LV2 control_ports: sending request to host");
        header.request_type.store(
            maolan_plugin_protocol::protocol::REQUEST_LV2_CONTROL_PORTS,
            Ordering::Release,
        );
        header.request_status.store(0, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Failed to signal host for LV2 control ports: {e}"));
        }

        if let Err(e) = wait_for_host_request_complete(header, events, Duration::from_secs(5)) {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Host did not respond to LV2 control ports: {e}"));
        }

        let status = header.request_status.load(Ordering::Acquire);
        let size = header.scratch_size.load(Ordering::Acquire) as usize;
        tracing::info!(status, size, "LV2 control_ports: host responded");
        if status != 1 {
            header.request_type.store(0, Ordering::Release);
            return Err("LV2 control port enumeration failed in host".to_string());
        }

        let scratch = unsafe { scratch_ptr(ptr) };
        let result = Self::deserialize_lv2_control_ports(scratch, size);
        match &result {
            Ok(ports) => tracing::info!(count = ports.len(), "LV2 control_ports: deserialized"),
            Err(e) => tracing::error!("LV2 control_ports: deserialize failed: {e}"),
        }
        header.request_type.store(0, Ordering::Release);
        result
    }

    pub fn note_names(&self) -> Result<HashMap<u8, String>, String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("LV2 processor not initialized".to_string()),
        };
        let ptr = mapping.as_ptr();
        let header = unsafe { header_mut(ptr) };

        header.request_type.store(
            maolan_plugin_protocol::protocol::REQUEST_LV2_MIDNAM,
            Ordering::Release,
        );
        header.request_status.store(0, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Failed to signal host for LV2 midnam: {e}"));
        }

        if let Err(e) = wait_for_host_request_complete(header, events, Duration::from_secs(5)) {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Host did not respond to LV2 midnam: {e}"));
        }

        let status = header.request_status.load(Ordering::Acquire);
        let size = header.scratch_size.load(Ordering::Acquire) as usize;
        if status != 1 {
            header.request_type.store(0, Ordering::Release);
            return Err("LV2 midnam enumeration failed in host".to_string());
        }

        let scratch = unsafe { scratch_ptr(ptr) };
        let result = Self::deserialize_lv2_note_names(scratch, size);
        header.request_type.store(0, Ordering::Release);
        result
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

        if let Err(e) = wait_for_host_request_complete(header, events, Duration::from_secs(5)) {
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

    pub fn process_with_audio_buffers(
        &self,
        frames: usize,
        audio_inputs: &[&[f32]],
        audio_outputs: &mut [&mut [f32]],
    ) -> Vec<MidiEvent> {
        if self.bypassed.load(Ordering::Relaxed) {
            ipc::bypass_copy_input_slices_to_outputs(audio_inputs, audio_outputs);
            return Vec::new();
        }

        // Safety: the sole RT accessor of `child` is this processor's own
        // plan task node, which runs exactly once per cycle.
        let crashed = unsafe {
            self.with_child(|child| {
                if let Some(c) = child.as_mut()
                    && let Ok(Some(status)) = c.try_wait()
                    && !status.success()
                {
                    self.crash_count.fetch_add(1, Ordering::Relaxed);
                    return true;
                }
                false
            })
        };
        if crashed {
            ipc::bypass_copy_input_slices_to_outputs(audio_inputs, audio_outputs);
            return Vec::new();
        }

        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => {
                ipc::bypass_copy_input_slices_to_outputs(audio_inputs, audio_outputs);
                return Vec::new();
            }
        };

        let ptr = mapping.as_ptr();
        let num_in = audio_inputs.len();
        let num_out = audio_outputs.len();
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

            ipc::copy_input_slices_to_shm(audio_inputs, ptr, frames);

            for (port_idx, port) in self.midi_input_ports.iter().enumerate() {
                let buf = midi_in_ring_ptr(ptr, port_idx);
                let (w, r) = midi_in_indices(ptr, port_idx);
                let ring = RingBuffer::new(buf, w, r, RING_CAPACITY);
                // Safety: plan single-writer invariant — this task is the sole
                // writer of its own ports this cycle; this read is of the
                // port's own buffer, which no other node touches now
                // (LOCKLESS.md Phase 3).
                let port_buffer = port.buffer();
                for ev in port_buffer {
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
                port.mark_finished();
            }
        }

        if events.signal_host().is_err() {
            ipc::bypass_copy_input_slices_to_outputs(audio_inputs, audio_outputs);
            return Vec::new();
        }

        let timeout = Duration::from_millis(100);
        match events.wait_host(timeout) {
            Ok(()) => {}
            Err(_) => {
                ipc::bypass_copy_input_slices_to_outputs(audio_inputs, audio_outputs);
                return Vec::new();
            }
        }

        unsafe {
            ipc::copy_outputs_from_shm_to_slices(audio_outputs, ptr, frames);

            let mut output_events = Vec::new();
            for (port_idx, port) in self.midi_output_ports.iter().enumerate() {
                let buf = midi_out_ring_ptr(ptr, port_idx);
                let (w, r) = midi_out_indices(ptr, port_idx);
                let ring = RingBuffer::new(buf, w, r, RING_CAPACITY);
                // Safety: plan single-writer invariant — this task is the sole
                // writer of its own ports this cycle (LOCKLESS.md Phase 3).
                let mut port_buffer = port.buffer_mut();
                port_buffer.clear();
                while let Some(ev) = ring.pop() {
                    let event = MidiEvent {
                        frame: ev.sample_offset,
                        data: ev.data.to_vec(),
                    };
                    port_buffer.push(event.clone());
                    output_events.push(event);
                }
                port.mark_finished();
            }
            output_events
        }
    }

    pub fn uri(&self) -> &str {
        &self.uri
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn take_stderr(&self) -> Option<ChildStderr> {
        // Control-side only: the EC is the sole accessor, so after `swap`
        // the `Arc` is unique and `try_unwrap` cannot fail in practice.
        self.stderr.swap(None).and_then(|s| Arc::try_unwrap(s).ok())
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
            header.set_gui_parent_api(maolan_plugin_protocol::protocol::GuiParentApi::X11);
            return Ok(());
        }
        Err("No active host to set parent window".to_string())
    }

    pub fn gui_set_parent_wayland(&self, window: usize) -> Result<(), String> {
        if let Some(ref mapping) = self.mapping {
            let header = unsafe { header_mut(mapping.as_ptr()) };
            header.set_parent_window(window);
            header.set_gui_parent_api(maolan_plugin_protocol::protocol::GuiParentApi::Wayland);
            return Ok(());
        }
        Err("No active host to set parent window".to_string())
    }

    pub fn gui_set_floating_mode(&self, floating: bool) -> Result<(), String> {
        if let Some(ref mapping) = self.mapping {
            let header = unsafe { header_mut(mapping.as_ptr()) };
            header.set_gui_mode(if floating {
                GuiMode::Floating
            } else {
                GuiMode::Embedded
            });
            if floating {
                header.set_parent_window(0);
                header.set_gui_parent_api(maolan_plugin_protocol::protocol::GuiParentApi::None);
            }
            return Ok(());
        }
        Err("No active host to set GUI mode".to_string())
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
        let mapping = self.mapping.take();
        let events = self.events.take();
        let child = self.child.get_mut().take();
        let shm_name = std::mem::take(&mut self.shm_name);
        ipc::drop_host(mapping, events, child, shm_name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_host_binary() -> PathBuf {
        ipc::find_plugin_host_binary().expect("maolan-plugin-host binary should be built for tests")
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "plugin host discovery/runtime uses OS facilities not supported by Miri on FreeBSD"
    )]
    #[test]
    fn find_host_binary_locates_binary() {
        let host_bin = find_host_binary();
        assert!(
            host_bin.exists(),
            "plugin-host binary should exist at {}",
            host_bin.display()
        );
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "plugin host discovery/runtime uses OS facilities not supported by Miri on FreeBSD"
    )]
    #[test]
    fn lv2_processor_crash_bypass() {
        let host_bin = find_host_binary();

        let processor = Lv2Processor::new(48000.0, 256, "__crash__", 1, 1, host_bin)
            .expect("should create LV2 processor for crash test");

        processor.setup_audio_ports();

        let input_buffers = [vec![1.0; 256]];
        let mut output_buffers = [vec![0.0; 256]];
        let inputs = input_buffers.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let mut outputs = output_buffers
            .iter_mut()
            .map(Vec::as_mut_slice)
            .collect::<Vec<_>>();
        processor.process_with_audio_buffers(256, &inputs, &mut outputs);

        let out_buf = &output_buffers[0];
        let first_few: Vec<f32> = out_buf.iter().take(10).copied().collect();
        assert!(
            out_buf.iter().all(|&s| s == 1.0),
            "after crash, output should be bypass copy of input, got: {:?}",
            first_few
        );
    }
}
