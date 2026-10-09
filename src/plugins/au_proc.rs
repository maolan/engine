//! AudioUnit (AUv2) processor: drives the out-of-process plugin host over the
//! shared-memory protocol, mirroring [`crate::vst3_proc::Vst3Processor`].
//! The plugin spec is `au:<type>:<subtype>:<manufacturer>` with literal
//! fourccs, e.g. `au:aufx:dely:appl`.

use crate::audio::io::AudioIO;
use crate::midi::io::{MIDIIO, MidiEvent};
use crate::plugins::ipc;
use crate::plugins::types::{AuParamInfo, AuPluginState};
use arc_swap::ArcSwapOption;
use maolan_plugin_protocol::events::EventPair;
use maolan_plugin_protocol::protocol::*;
use maolan_plugin_protocol::ringbuf::RingBuffer;
use maolan_plugin_protocol::shm::ShmMapping;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

const SHM_LATENCY_SAMPLES_OFFSET: usize = 84;

unsafe fn latency_samples_atomic(ptr: *mut u8) -> &'static AtomicU32 {
    unsafe { &*(ptr.add(SHM_LATENCY_SAMPLES_OFFSET) as *const AtomicU32) }
}

unsafe fn response_counter(ptr: *mut u8) -> &'static AtomicU32 {
    unsafe { &header_ref(ptr).response_counter }
}

/// Magic + record layout for the `REQUEST_AU_PARAMETERS` scratch payload.
/// Must stay in sync with `au::write_au_params_to_scratch` in
/// maolan-plugin-host.
const AU_PARAMS_MAGIC: u32 = 0x4155_5052; // "AUPR"
const AU_PARAMS_OFFSET: usize = 3072;
const AU_PARAMS_MAX_SIZE: usize = SCRATCH_SIZE - AU_PARAMS_OFFSET;
/// Fixed part of one serialized param record: index, scope, element, paramID,
/// min, max, default, flags (8 x u32) plus name_len (u32); name bytes follow.
const AU_PARAM_RECORD_FIXED: usize = 36;

/// Read the parameter table written by the host's `REQUEST_AU_PARAMETERS`
/// handler. Duplicated from plugin-host (same as the CLAP scratch readers)
/// so the engine does not depend on the host crate.
///
/// # Safety
/// `ptr` must point to a valid SHM allocation.
unsafe fn read_au_params_from_scratch(ptr: *mut u8) -> Option<Vec<AuParamInfo>> {
    unsafe {
        let mut src = scratch_ptr(ptr).add(AU_PARAMS_OFFSET);
        let mut remaining = AU_PARAMS_MAX_SIZE;
        if remaining < 8 {
            return None;
        }
        if std::ptr::read_unaligned(src as *mut u32) != AU_PARAMS_MAGIC {
            return None;
        }
        src = src.add(4);
        remaining -= 4;
        let count = std::ptr::read_unaligned(src as *mut u32) as usize;
        src = src.add(4);
        remaining -= 4;
        let mut params = Vec::with_capacity(count);
        for _ in 0..count {
            if remaining < AU_PARAM_RECORD_FIXED {
                return None;
            }
            let index = std::ptr::read_unaligned(src as *mut u32);
            let scope = std::ptr::read_unaligned(src.add(4) as *mut u32);
            let element = std::ptr::read_unaligned(src.add(8) as *const u32);
            let param_id = std::ptr::read_unaligned(src.add(12) as *const u32);
            let min = f32::from_bits(std::ptr::read_unaligned(src.add(16) as *const u32)) as f64;
            let max = f32::from_bits(std::ptr::read_unaligned(src.add(20) as *const u32)) as f64;
            let default =
                f32::from_bits(std::ptr::read_unaligned(src.add(24) as *const u32)) as f64;
            let flags = u64::from(std::ptr::read_unaligned(src.add(28) as *const u32));
            src = src.add(32);
            remaining -= 32;
            if remaining < 4 {
                return None;
            }
            let name_len = std::ptr::read_unaligned(src as *mut u32) as usize;
            src = src.add(4);
            remaining -= 4;
            if name_len > remaining {
                return None;
            }
            let bytes = std::slice::from_raw_parts(src, name_len);
            let name = String::from_utf8(bytes.to_vec()).ok()?;
            src = src.add(name_len);
            remaining -= name_len;
            params.push(AuParamInfo {
                index,
                scope,
                element,
                param_id,
                name,
                min,
                max,
                default,
                flags,
            });
        }
        Some(params)
    }
}

pub struct AuProcessor {
    spec: String,
    plugin_id: String,
    name: String,
    audio_inputs: Vec<Arc<AudioIO>>,
    audio_outputs: Vec<Arc<AudioIO>>,
    main_audio_inputs: usize,
    main_audio_outputs: usize,
    midi_input_ports: Vec<Arc<MIDIIO>>,
    midi_output_ports: Vec<Arc<MIDIIO>>,
    param_infos: Vec<AuParamInfo>,
    /// Current value of every known parameter, keyed by the dense table
    /// index and stored as `f64` bits. Pre-populated from `param_infos` at
    /// construction and only ever touched through atomic loads/stores.
    param_values: HashMap<u32, AtomicU64>,
    bypassed: Arc<AtomicBool>,

    /// Host child process handle. Control-side only: the audio thread never
    /// touches it (crash detection is done by the single
    /// `watchdog::ProcessWatchdog` from the PID), and `Drop` is the only
    /// accessor, handing it to `ipc::drop_host`.
    child: Option<Child>,
    /// Host stderr pipe; control-side only (`take_stderr`). RCU-published so
    /// no blocking primitive is involved.
    stderr: ArcSwapOption<ChildStderr>,
    mapping: Option<ShmMapping>,
    events: Option<EventPair>,
    shm_name: String,

    last_latency_samples: AtomicUsize,
    latency_changed: AtomicBool,
}

pub type SharedAuProcessor = Arc<AuProcessor>;

impl AuProcessor {
    #[cfg(test)]
    pub(crate) fn new_for_test(
        input_count: usize,
        output_count: usize,
        buffer_size: usize,
    ) -> Self {
        Self {
            spec: "au:aufx:pass:appl".to_string(),
            plugin_id: "au:aufx:pass:appl".to_string(),
            name: "Test AU".to_string(),
            audio_inputs: (0..input_count)
                .map(|_| Arc::new(AudioIO::new(buffer_size)))
                .collect(),
            audio_outputs: (0..output_count)
                .map(|_| Arc::new(AudioIO::new(buffer_size)))
                .collect(),
            main_audio_inputs: input_count,
            main_audio_outputs: output_count,
            midi_input_ports: Vec::new(),
            midi_output_ports: Vec::new(),
            param_infos: Vec::new(),
            param_values: HashMap::new(),
            bypassed: Arc::new(AtomicBool::new(false)),
            child: None,
            stderr: ArcSwapOption::from(None),
            mapping: None,
            events: None,
            shm_name: String::new(),
            last_latency_samples: AtomicUsize::new(0),
            latency_changed: AtomicBool::new(false),
        }
    }

    pub fn new(
        sample_rate: f64,
        buffer_size: usize,
        plugin_spec: &str,
        plugin_id: &str,
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

        let instance_id = ipc::unique_instance_id("au");
        let num_inputs = input_count.max(1);
        let num_outputs = output_count.max(1);
        let (mut child, mapping, events, shm_name, stderr) = ipc::spawn_host(ipc::HostSpawnArgs {
            host_binary: &host_binary,
            format: "au",
            plugin_spec,
            instance_id: &instance_id,
            extra_args: &[
                &sample_rate.to_string(),
                &buffer_size.to_string(),
                &num_inputs.to_string(),
                &num_outputs.to_string(),
            ],
        })?;

        let header = unsafe { header_ref(mapping.as_ptr()) };
        if !ipc::wait_for_ready(header, &mut child, Duration::from_secs(10)) {
            let _ = child.kill();
            return Err("AU host did not signal ready".to_string());
        }

        let name = unsafe {
            maolan_plugin_protocol::protocol::read_plugin_name_from_scratch(mapping.as_ptr())
                .unwrap_or_else(|| {
                    Path::new(plugin_spec)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("AudioUnit")
                        .to_string()
                })
        };

        let param_infos: Vec<AuParamInfo> = Self::fetch_parameter_infos(&mapping, &events)
            .unwrap_or_else(|e| {
                tracing::warn!("AU parameter enumeration failed for '{plugin_spec}': {e}");
                Vec::new()
            });
        let param_values = param_infos
            .iter()
            .map(|info| (info.index, AtomicU64::new(info.default.to_bits())))
            .collect();

        let header = unsafe { header_ref(mapping.as_ptr()) };
        let midi_in_count = header.midi_in_port_count.load(Ordering::Acquire) as usize;
        let midi_out_count = header.midi_out_port_count.load(Ordering::Acquire) as usize;
        let midi_input_ports: Vec<_> = (0..midi_in_count)
            .map(|_| Arc::new(MIDIIO::new()))
            .collect();
        let midi_output_ports: Vec<_> = (0..midi_out_count)
            .map(|_| Arc::new(MIDIIO::new()))
            .collect();

        let bypassed = Arc::new(AtomicBool::new(false));
        // Register with the process-wide crash watchdog: if the host process
        // exits, the shared bypass flag is flipped and the RT path bypasses.
        crate::plugins::watchdog::ProcessWatchdog::global()
            .watch(child.id(), Arc::clone(&bypassed));

        Ok(Self {
            spec: plugin_spec.to_string(),
            plugin_id: plugin_id.to_string(),
            name,
            audio_inputs,
            audio_outputs,
            main_audio_inputs: input_count.max(1),
            main_audio_outputs: output_count.max(1),
            midi_input_ports,
            midi_output_ports,
            param_infos,
            param_values,
            bypassed,
            child: Some(child),
            stderr: ArcSwapOption::from_pointee(stderr),
            mapping: Some(mapping),
            events: Some(events),
            shm_name,
            last_latency_samples: AtomicUsize::new(0),
            latency_changed: AtomicBool::new(false),
        })
    }

    /// Ask the host to serialize the unit's parameter table into scratch
    /// (request 13, "AUPR" magic at offset 3072) and read it back.
    fn fetch_parameter_infos(
        mapping: &ShmMapping,
        events: &EventPair,
    ) -> Result<Vec<AuParamInfo>, String> {
        let ptr = mapping.as_ptr();
        let header = unsafe { header_mut(ptr) };
        header.request_status.store(0, Ordering::Release);
        header
            .request_type
            .store(REQUEST_AU_PARAMETERS, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Failed to signal host for AU parameters: {e}"));
        }
        if let Err(e) = events.wait_host(Duration::from_secs(5)) {
            header.request_type.store(0, Ordering::Release);
            return Err(format!(
                "Host did not respond to AU parameters request: {e}"
            ));
        }
        let status = header.request_status.load(Ordering::Acquire);
        header.request_type.store(0, Ordering::Release);
        if status != 1 {
            return Err("AU parameter enumeration failed in host".to_string());
        }
        unsafe { read_au_params_from_scratch(ptr) }
            .ok_or_else(|| "Failed to read AU parameters from scratch".to_string())
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
        let previous = self.bypassed.swap(bypassed, Ordering::Relaxed);
        if previous != bypassed {
            self.latency_changed.store(true, Ordering::Release);
        }
    }

    pub fn is_bypassed(&self) -> bool {
        self.bypassed.load(Ordering::Relaxed)
    }

    pub fn latency_samples(&self) -> usize {
        if self.bypassed.load(Ordering::Relaxed) {
            let previous = self.last_latency_samples.swap(0, Ordering::AcqRel);
            if previous != 0 {
                self.latency_changed.store(true, Ordering::Release);
            }
            return 0;
        }
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

    pub fn parameter_infos(&self) -> Vec<AuParamInfo> {
        self.param_infos.clone()
    }

    pub fn parameter_values(&self) -> HashMap<u32, f64> {
        self.param_values
            .iter()
            .map(|(&id, value)| (id, f64::from_bits(value.load(Ordering::Relaxed))))
            .collect()
    }

    /// `param_index` is the dense index into the unit's enumerated parameter
    /// table, exactly as published in `parameter_infos`.
    pub fn set_parameter(&self, param_index: u32, value: f64) -> Result<(), String> {
        self.set_parameter_at(param_index, value, 0)
    }

    pub fn set_parameter_at(
        &self,
        param_index: u32,
        value: f64,
        _frame: u32,
    ) -> Result<(), String> {
        if let Some(slot) = self.param_values.get(&param_index) {
            slot.store(value.to_bits(), Ordering::Relaxed);
        } else {
            tracing::warn!("AU set_parameter_at: unknown parameter index {param_index}");
        }

        if let Some(ref mapping) = self.mapping {
            let ring = unsafe {
                let buf = param_ring_ptr(mapping.as_ptr());
                let (w, r) = param_indices(mapping.as_ptr());
                RingBuffer::new(buf, w, r, RING_CAPACITY)
            };
            let ev = ParameterEvent {
                param_index,
                value: value as f32,
                sample_offset: 0,
                event_kind: maolan_plugin_protocol::PARAM_EVENT_VALUE,
            };
            if !ring.push(ev) {}
        }
        Ok(())
    }

    pub fn snapshot_state(&self) -> Result<AuPluginState, String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("AU processor not initialized".to_string()),
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

        let scratch = unsafe { scratch_ptr(ptr) };
        let bytes = deserialize_au_state(scratch, size)?;
        header.request_type.store(0, Ordering::Release);
        Ok(AuPluginState { bytes })
    }

    pub fn restore_state(&self, state: &AuPluginState) -> Result<(), String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("AU processor not initialized".to_string()),
        };
        let ptr = mapping.as_ptr();
        let header = unsafe { header_mut(ptr) };

        let scratch = unsafe { scratch_ptr(ptr) };
        let size = serialize_au_state(scratch, &state.bytes)?;
        header.scratch_size.store(size as u32, Ordering::Release);

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

        let timeout = ipc::plugin_wait_timeout();
        let _plugin_wait = crate::cycle_trace::plugin_wait();
        match ipc::wait_block_response(unsafe { response_counter(ptr) }, events, timeout) {
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

    pub fn spec(&self) -> &str {
        &self.spec
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn take_stderr(&self) -> Option<ChildStderr> {
        // Control-side only: the EC is the sole accessor, so after `swap`
        // the `Arc` is unique and `try_unwrap` cannot fail in practice.
        self.stderr.swap(None).and_then(|s| Arc::try_unwrap(s).ok())
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

    /// AU editors run in a floating `NSWindow` owned by the host; the DAW
    /// side only toggles visibility (requests 3/4).
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
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(mapping), Some(events)) => (mapping, events),
            _ => return Err("No active host to show GUI".to_string()),
        };

        let header = unsafe { header_mut(mapping.as_ptr()) };
        header.request_status.store(0, Ordering::Release);
        header.request_type.store(3, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Failed to signal host for AU GUI show: {e}"));
        }

        if let Err(e) = events.wait_host(Duration::from_secs(5)) {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Host did not respond to AU GUI show: {e}"));
        }

        let status = header.request_status.load(Ordering::Acquire);
        header.request_type.store(0, Ordering::Release);
        if status != 1 {
            return Err("AU GUI show failed in host".to_string());
        }
        Ok(())
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
}

impl Drop for AuProcessor {
    fn drop(&mut self) {
        if let Some(ref child) = self.child {
            crate::plugins::watchdog::ProcessWatchdog::global().unwatch(child.id());
        }
        let mapping = self.mapping.take();
        let events = self.events.take();
        let child = self.child.take();
        let shm_name = std::mem::take(&mut self.shm_name);
        ipc::drop_host(mapping, events, child, shm_name);
    }
}

/// Layout: u32 length followed by the state bytes.
fn serialize_au_state(scratch: *mut u8, bytes: &[u8]) -> Result<usize, String> {
    let max_len = maolan_plugin_protocol::protocol::SCRATCH_SIZE;
    let mut offset = 0usize;
    if offset + 4 > max_len {
        return Err("scratch overflow".to_string());
    }
    unsafe {
        std::ptr::write_unaligned(scratch as *mut u32, bytes.len() as u32);
    }
    offset += 4;
    if offset + bytes.len() > max_len {
        return Err("scratch overflow".to_string());
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), scratch.add(offset), bytes.len());
    }
    offset += bytes.len();
    Ok(offset)
}

fn deserialize_au_state(scratch: *const u8, size: usize) -> Result<Vec<u8>, String> {
    if size < 4 {
        return Err("scratch too small for AU state".to_string());
    }
    let len = unsafe { std::ptr::read_unaligned(scratch as *const u32) } as usize;
    if 4 + len > size {
        return Err("scratch underflow".to_string());
    }
    let mut bytes = vec![0u8; len];
    unsafe {
        std::ptr::copy_nonoverlapping(scratch.add(4), bytes.as_mut_ptr(), len);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn au_state_serialization_roundtrip() {
        let bytes = vec![1u8, 2, 3, 4, 5, 6, 7];
        let mut scratch = vec![0u8; SCRATCH_SIZE];
        let size = serialize_au_state(scratch.as_mut_ptr(), &bytes).expect("serialize");
        assert!(size > 0);
        assert!(size < SCRATCH_SIZE);

        let decoded = deserialize_au_state(scratch.as_ptr(), size).expect("deserialize");
        assert_eq!(decoded, bytes);
    }

    #[test]
    fn au_params_scratch_roundtrip() {
        let params = vec![
            AuParamInfo {
                index: 0,
                scope: 0,
                element: 0,
                param_id: 1,
                name: "Gain".to_string(),
                min: 0.0,
                max: 1.0,
                default: 0.5,
                flags: 1,
            },
            AuParamInfo {
                index: 1,
                scope: 0,
                element: 0,
                param_id: 2,
                name: "Delay Time".to_string(),
                min: 0.0,
                max: 2000.0,
                default: 250.0,
                flags: 0,
            },
        ];
        let mut scratch = vec![0u8; SCRATCH_SIZE];
        unsafe {
            write_test_au_params(scratch.as_mut_ptr(), &params).expect("write");
            let decoded = read_au_params_from_scratch(scratch.as_mut_ptr()).expect("read");
            assert_eq!(decoded, params);
        }
    }

    /// Test-only writer mirroring the plugin-host's
    /// `write_au_params_to_scratch` so the reader can be exercised in-tree.
    ///
    /// # Safety
    /// `ptr` must point to a `SCRATCH_SIZE` allocation.
    unsafe fn write_test_au_params(ptr: *mut u8, params: &[AuParamInfo]) -> Result<(), String> {
        unsafe {
            let mut dest = scratch_ptr(ptr).add(AU_PARAMS_OFFSET);
            std::ptr::write_unaligned(dest as *mut u32, AU_PARAMS_MAGIC);
            dest = dest.add(4);
            std::ptr::write_unaligned(dest as *mut u32, params.len() as u32);
            dest = dest.add(4);
            for p in params {
                std::ptr::write_unaligned(dest as *mut u32, p.index);
                std::ptr::write_unaligned(dest.add(4) as *mut u32, p.scope);
                std::ptr::write_unaligned(dest.add(8) as *mut u32, p.element);
                std::ptr::write_unaligned(dest.add(12) as *mut u32, p.param_id);
                std::ptr::write_unaligned(dest.add(16) as *mut u32, (p.min as f32).to_bits());
                std::ptr::write_unaligned(dest.add(20) as *mut u32, (p.max as f32).to_bits());
                std::ptr::write_unaligned(dest.add(24) as *mut u32, (p.default as f32).to_bits());
                std::ptr::write_unaligned(dest.add(28) as *mut u32, p.flags as u32);
                dest = dest.add(32);
                let bytes = p.name.as_bytes();
                std::ptr::write_unaligned(dest as *mut u32, bytes.len() as u32);
                dest = dest.add(4);
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), dest, bytes.len());
                dest = dest.add(bytes.len());
            }
            Ok(())
        }
    }
}
