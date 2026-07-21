use crate::audio::io::AudioIO;
use crate::midi::io::{MIDIIO, MidiEvent};
use crate::plugins::ipc;
use crate::plugins::types::{
    ClapMidiOutputEvent, ClapParamUpdate, ClapParameterInfo, ClapTransportInfo,
};
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
        // The host sets request_status before signalling and clears request_type
        // after signalling. Check either condition so we don't miss the response
        // if request_type is still non-zero when the wake-up arrives.
        if header.request_type.load(Ordering::Acquire) == 0
            || header.request_status.load(Ordering::Acquire) != 0
        {
            return Ok(());
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return Err("Host did not respond to request".to_string());
        }
        if let Err(e) = events.wait_host(timeout - elapsed) {
            return Err(format!("Host did not respond to request: {e}"));
        }
    }
}

pub struct ClapProcessor {
    path: String,
    plugin_id: String,
    name: String,
    audio_inputs: Vec<Arc<AudioIO>>,
    audio_outputs: Vec<Arc<AudioIO>>,
    main_audio_inputs: usize,
    main_audio_outputs: usize,
    midi_input_count: usize,
    midi_output_count: usize,
    midi_input_ports: Vec<Arc<MIDIIO>>,
    midi_output_ports: Vec<Arc<MIDIIO>>,
    param_infos: Vec<ClapParameterInfo>,
    /// Current value of every known parameter, keyed by parameter id and
    /// stored as `f64` bits. Pre-populated from `param_infos` at construction
    /// and only ever touched through atomic loads/stores.
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
unsafe impl Sync for ClapProcessor {}

pub type SharedClapProcessor = Arc<ClapProcessor>;

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

        let instance_id = ipc::unique_instance_id("clap");
        let plugin_spec = if plugin_id.is_empty() {
            plugin_path.to_string()
        } else {
            format!("{plugin_path}::{plugin_id}")
        };
        let (mut child, mapping, events, shm_name, stderr) = ipc::spawn_host(ipc::HostSpawnArgs {
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
            maolan_plugin_protocol::protocol::read_plugin_name_from_scratch(mapping.as_ptr())
                .unwrap_or_else(|| plugin_id.to_string())
        };

        let (actual_audio_in, actual_audio_out, actual_midi_in, actual_midi_out) = unsafe {
            let counts =
                maolan_plugin_protocol::protocol::read_port_counts_from_scratch(mapping.as_ptr());

            counts.unwrap_or((input_count as u32, output_count as u32, 0, 0))
        };

        let audio_inputs = (0..actual_audio_in as usize)
            .map(|_| Arc::new(AudioIO::new(buffer_size)))
            .collect::<Vec<_>>();
        let audio_outputs = (0..actual_audio_out as usize)
            .map(|_| Arc::new(AudioIO::new(buffer_size)))
            .collect::<Vec<_>>();
        let midi_input_ports = (0..actual_midi_in as usize)
            .map(|_| Arc::new(MIDIIO::new()))
            .collect::<Vec<_>>();
        let midi_output_ports = (0..actual_midi_out as usize)
            .map(|_| Arc::new(MIDIIO::new()))
            .collect::<Vec<_>>();

        let param_infos = Self::fetch_parameter_infos(&mapping, &events).unwrap_or_else(|e| {
            tracing::warn!("Failed to fetch CLAP parameter infos: {e}");
            Vec::new()
        });
        let param_values = param_infos
            .iter()
            .map(|info| (info.id, AtomicU64::new(info.default_value.to_bits())))
            .collect();

        Ok(Self {
            path: plugin_spec.to_string(),
            plugin_id: plugin_id.to_string(),
            name,
            audio_inputs,
            audio_outputs,
            main_audio_inputs: actual_audio_in as usize,
            main_audio_outputs: actual_audio_out as usize,
            midi_input_count: actual_midi_in as usize,
            midi_output_count: actual_midi_out as usize,
            midi_input_ports,
            midi_output_ports,
            param_infos,
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
        self.midi_input_count
    }

    pub fn midi_output_count(&self) -> usize {
        self.midi_output_count
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

    pub fn parameter_infos(&self) -> Vec<ClapParameterInfo> {
        self.param_infos.clone()
    }

    fn fetch_parameter_infos(
        mapping: &ShmMapping,
        events: &EventPair,
    ) -> Result<Vec<ClapParameterInfo>, String> {
        let ptr = mapping.as_ptr();
        let header = unsafe { header_mut(ptr) };

        tracing::info!("CLAP fetch_parameter_infos: sending request to host");
        header
            .request_type
            .store(REQUEST_CLAP_PARAMETERS, Ordering::Release);
        header.request_status.store(0, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Failed to signal host for CLAP parameters: {e}"));
        }

        if let Err(e) = wait_for_host_request_complete(header, events, Duration::from_secs(5)) {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Host did not respond to CLAP parameters: {e}"));
        }

        let status = header.request_status.load(Ordering::Acquire);
        let size = header.scratch_size.load(Ordering::Acquire) as usize;
        tracing::info!(status, size, "CLAP fetch_parameter_infos: host responded");
        if status != 1 {
            header.request_type.store(0, Ordering::Release);
            return Err("CLAP parameter enumeration failed in host".to_string());
        }

        let scratch = unsafe { scratch_ptr(ptr) };
        let result = Self::deserialize_clap_parameters(scratch, size);
        match &result {
            Ok(params) => tracing::info!(
                count = params.len(),
                "CLAP fetch_parameter_infos: deserialized"
            ),
            Err(e) => tracing::error!("CLAP fetch_parameter_infos: deserialize failed: {e}"),
        }
        header.request_type.store(0, Ordering::Release);
        result
    }

    fn deserialize_clap_parameters(
        scratch: *const u8,
        size: usize,
    ) -> Result<Vec<ClapParameterInfo>, String> {
        if size < 4 {
            return Err("scratch too small for CLAP parameters".to_string());
        }
        let mut offset = 0usize;

        let count = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
        offset += 4;

        let mut params = Vec::with_capacity(count);
        for _ in 0..count {
            if offset + 4 > size {
                return Err("scratch underflow".to_string());
            }
            let id = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) };
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

            if offset + 4 > size {
                return Err("scratch underflow".to_string());
            }
            let module_len =
                unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
            offset += 4;
            if offset + module_len > size {
                return Err("scratch underflow".to_string());
            }
            let mut module_bytes = vec![0u8; module_len];
            unsafe {
                std::ptr::copy_nonoverlapping(
                    scratch.add(offset),
                    module_bytes.as_mut_ptr(),
                    module_len,
                );
            }
            offset += module_len;
            let module = String::from_utf8(module_bytes).map_err(|e| e.to_string())?;

            if offset + 24 > size {
                return Err("scratch underflow".to_string());
            }
            let min_value = f64::from_bits(unsafe {
                std::ptr::read_unaligned(scratch.add(offset) as *const u64)
            });
            let max_value = f64::from_bits(unsafe {
                std::ptr::read_unaligned(scratch.add(offset + 8) as *const u64)
            });
            let default_value = f64::from_bits(unsafe {
                std::ptr::read_unaligned(scratch.add(offset + 16) as *const u64)
            });
            offset += 24;

            params.push(ClapParameterInfo {
                id,
                name,
                module,
                min_value,
                max_value,
                default_value,
            });
        }

        Ok(params)
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
            tracing::warn!("CLAP set_parameter_at: unknown parameter id {param_id}");
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

    pub fn take_state_dirty(&self) -> bool {
        let header = match self.mapping.as_ref() {
            Some(m) => unsafe { header_mut(m.as_ptr()) },
            None => return false,
        };
        header.state_dirty.swap(0, Ordering::Acquire) != 0
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

        if let Err(e) = wait_for_host_request_complete(header, events, Duration::from_secs(5)) {
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

        if let Err(e) = wait_for_host_request_complete(header, events, Duration::from_secs(5)) {
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

    pub fn set_resource_directory(&self, dir: &std::path::Path) -> Result<(), String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("CLAP processor not initialized".to_string()),
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

    pub fn file_references(
        &self,
    ) -> Result<Vec<maolan_plugin_protocol::protocol::FileReference>, String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("CLAP processor not initialized".to_string()),
        };
        let ptr = mapping.as_ptr();
        let header = unsafe { header_mut(ptr) };

        header.request_type.store(6, Ordering::Release);
        header.request_status.store(0, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Failed to signal host for file references: {e}"));
        }

        if let Err(e) = wait_for_host_request_complete(header, events, Duration::from_secs(5)) {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Host did not respond to file references: {e}"));
        }

        let status = header.request_status.load(Ordering::Acquire);
        if status != 1 {
            header.request_type.store(0, Ordering::Release);
            return Err("File references enumeration failed in host".to_string());
        }

        let paths = unsafe { read_file_references_from_scratch(ptr) }
            .ok_or("Failed to read file references from scratch")?;
        header.request_type.store(0, Ordering::Release);
        Ok(paths)
    }

    pub fn update_file_reference(&self, index: u32, path: &str) -> Result<(), String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("CLAP processor not initialized".to_string()),
        };
        let ptr = mapping.as_ptr();
        let header = unsafe { header_mut(ptr) };
        unsafe {
            write_file_reference_update_to_scratch(ptr, index, path)
                .map_err(|e| format!("Failed to write file-reference update: {e}"))?;
        }

        header.request_type.store(7, Ordering::Release);
        header.request_status.store(0, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!(
                "Failed to signal host for file-reference update: {e}"
            ));
        }

        if let Err(e) = wait_for_host_request_complete(header, events, Duration::from_secs(5)) {
            header.request_type.store(0, Ordering::Release);
            return Err(format!(
                "Host did not respond to file-reference update: {e}"
            ));
        }

        let status = header.request_status.load(Ordering::Acquire);
        header.request_type.store(0, Ordering::Release);
        if status != 1 {
            return Err("File-reference update failed in host".to_string());
        }
        Ok(())
    }

    pub fn process_with_audio_buffers(
        &self,
        frames: usize,
        midi_in: &[MidiEvent],
        transport: ClapTransportInfo,
        audio_inputs: &[&[f32]],
        audio_outputs: &mut [&mut [f32]],
    ) -> Vec<ClapMidiOutputEvent> {
        if self.bypassed.load(Ordering::Relaxed) {
            ipc::bypass_copy_input_slices_to_outputs(audio_inputs, audio_outputs);
            return Vec::new();
        }

        self.setup_midi_ports();

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
        unsafe {
            ipc::configure_shm_header(
                ptr,
                frames,
                audio_inputs.len(),
                audio_outputs.len(),
                self.midi_input_ports.len(),
                self.midi_output_ports.len(),
            );
            ipc::copy_input_slices_to_shm(audio_inputs, ptr, frames);

            let t = transport_mut(ptr);
            t.playhead_sample = transport.transport_sample as u64;
            t.tempo = transport.bpm;
            t.numerator = transport.tsig_num as u32;
            t.denominator = transport.tsig_denom as u32;
            t.flags = if transport.playing { 1 } else { 0 };

            // Transitional: copy caller-supplied MIDI events into port 0 so
            // existing engine scheduling keeps working until plugin MIDI
            // connections are fully migrated to MIDIIO.
            if let Some(port0) = self.midi_input_ports.first() {
                // Safety: plan single-writer invariant — this task is the sole
                // writer of its own ports this cycle; sources it reads were
                // produced by earlier plan nodes (LOCKLESS.md Phase 3).
                let mut buffer = port0.buffer_mut();
                buffer.extend_from_slice(midi_in);
                port0.mark_finished();
            }

            for (port_idx, port) in self.midi_input_ports.iter().enumerate() {
                let midi_buf = midi_in_ring_ptr(ptr, port_idx);
                let (midi_w, midi_r) = midi_in_indices(ptr, port_idx);
                let midi_ring = RingBuffer::new(midi_buf, midi_w, midi_r, RING_CAPACITY);
                // Safety: as above — sole writer this cycle; this read is of
                // the port's own buffer, which no other node touches now.
                let port_buffer = port.buffer();
                for ev in port_buffer {
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
                        break;
                    }
                }
            }
        }

        if events.signal_host().is_err() {
            ipc::bypass_copy_input_slices_to_outputs(audio_inputs, audio_outputs);
            return Vec::new();
        }

        let timeout = Duration::from_millis(100);
        if events.wait_host(timeout).is_err() {
            ipc::bypass_copy_input_slices_to_outputs(audio_inputs, audio_outputs);
            return Vec::new();
        }

        // Safety: same single-accessor invariant as the pre-process check.
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

        unsafe {
            ipc::copy_outputs_from_shm_to_slices(audio_outputs, ptr, frames);
        }

        let mut midi_out = Vec::new();
        unsafe {
            for (port_idx, port) in self.midi_output_ports.iter().enumerate() {
                // Safety: plan single-writer invariant — this task is the sole
                // writer of its own ports this cycle (LOCKLESS.md Phase 3).
                let mut port_buffer = port.buffer_mut();
                port_buffer.clear();
                let midi_out_buf = midi_out_ring_ptr(ptr, port_idx);
                let (midi_out_w, midi_out_r) = midi_out_indices(ptr, port_idx);
                let midi_out_ring =
                    RingBuffer::new(midi_out_buf, midi_out_w, midi_out_r, RING_CAPACITY);
                while let Some(ev) = midi_out_ring.pop() {
                    let event = crate::midi::io::MidiEvent::new(ev.sample_offset, ev.data.to_vec());
                    port_buffer.push(event.clone());
                    midi_out.push(ClapMidiOutputEvent {
                        port: port_idx,
                        event,
                    });
                }
                port.mark_finished();
            }
        }

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

    pub fn gui_destroy(&self) {}

    pub fn gui_on_main_thread(&self) {}

    pub fn gui_on_timer(&self, _timer_id: u32) {}

    fn deserialize_clap_note_names(
        scratch: *const u8,
        size: usize,
    ) -> Result<HashMap<u8, String>, String> {
        if size < 4 {
            return Err("scratch too small for CLAP note names".to_string());
        }
        let mut offset = 0usize;
        let count = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) } as usize;
        offset += 4;

        let mut note_names = HashMap::with_capacity(count);
        for _ in 0..count {
            if offset + 4 > size {
                return Err("scratch underflow".to_string());
            }
            let note = unsafe { std::ptr::read_unaligned(scratch.add(offset) as *const u32) };
            offset += 4;
            if note > 127 {
                return Err(format!("CLAP note name key out of range: {note}"));
            }

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
            note_names.insert(note as u8, name);
        }

        Ok(note_names)
    }

    pub fn note_names(&self) -> Result<HashMap<u8, String>, String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("CLAP processor not initialized".to_string()),
        };
        let ptr = mapping.as_ptr();
        let header = unsafe { header_mut(ptr) };

        header
            .request_type
            .store(REQUEST_CLAP_NOTE_NAMES, Ordering::Release);
        header.request_status.store(0, Ordering::Release);
        if let Err(e) = events.signal_host() {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Failed to signal host for CLAP note names: {e}"));
        }

        if let Err(e) = wait_for_host_request_complete(header, events, Duration::from_secs(5)) {
            header.request_type.store(0, Ordering::Release);
            return Err(format!("Host did not respond to CLAP note names: {e}"));
        }

        let status = header.request_status.load(Ordering::Acquire);
        let size = header.scratch_size.load(Ordering::Acquire) as usize;
        if status != 1 {
            header.request_type.store(0, Ordering::Release);
            return Err("CLAP note name enumeration failed in host".to_string());
        }

        let scratch = unsafe { scratch_ptr(ptr) };
        let result = Self::deserialize_clap_note_names(scratch, size);
        header.request_type.store(0, Ordering::Release);
        result
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

impl Drop for ClapProcessor {
    fn drop(&mut self) {
        let mapping = self.mapping.take();
        let events = self.events.take();
        let child = self.child.get_mut().take();
        let shm_name = std::mem::take(&mut self.shm_name);
        ipc::drop_host(mapping, events, child, shm_name);
    }
}

fn split_plugin_spec(spec: &str) -> (&str, &str) {
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

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "plugin host discovery/runtime uses OS facilities not supported by Miri on FreeBSD"
    )]
    #[test]
    fn clap_processor_processes_audio() {
        let host_bin = find_host_binary();
        if !host_bin.exists() {
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

        let input_buffers = (0..processor.audio_inputs().len())
            .map(|i| (0..256).map(|j| (i * 1000 + j) as f32).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        let mut output_buffers = vec![vec![0.0; 256]; processor.audio_outputs().len()];
        let inputs = input_buffers.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let mut outputs = output_buffers
            .iter_mut()
            .map(Vec::as_mut_slice)
            .collect::<Vec<_>>();
        processor.process_with_audio_buffers(
            256,
            &[],
            ClapTransportInfo::default(),
            &inputs,
            &mut outputs,
        );

        for output in output_buffers.iter() {
            assert!(
                output.iter().any(|&s| s != 0.0),
                "output buffer should contain non-zero samples"
            );
        }
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "plugin host discovery/runtime uses OS facilities not supported by Miri on FreeBSD"
    )]
    #[test]
    fn clap_processor_crash_bypass() {
        let host_bin = find_host_binary();
        if !host_bin.exists() {
            return;
        }

        let processor = ClapProcessor::new(48000.0, 256, "__crash__", 1, 1, host_bin)
            .expect("should create processor for crash test");

        processor.setup_audio_ports();

        let input_buffers = [vec![1.0; 256]];
        let mut output_buffers = [vec![0.0; 256]];
        let inputs = input_buffers.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let mut outputs = output_buffers
            .iter_mut()
            .map(Vec::as_mut_slice)
            .collect::<Vec<_>>();

        // Give the aborted host a moment to be reaped so the crash is visible.
        std::thread::sleep(std::time::Duration::from_millis(50));

        processor.process_with_audio_buffers(
            256,
            &[],
            ClapTransportInfo::default(),
            &inputs,
            &mut outputs,
        );

        assert!(
            output_buffers[0].iter().all(|&s| s == 1.0),
            "after crash, output should be bypass copy of input"
        );
    }

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "plugin host discovery/runtime uses OS facilities not supported by Miri on FreeBSD"
    )]
    #[test]
    fn clap_track_integration() {
        use crate::track::Track;

        let host_bin = find_host_binary();
        if !host_bin.exists() {
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

        let processor = track.clap_plugins[0].processor.clone();
        processor.setup_audio_ports();

        let input_buffers = (0..processor.audio_inputs().len())
            .map(|i| (0..256).map(|j| (i * 1000 + j) as f32).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        let mut output_buffers = vec![vec![0.0; 256]; processor.audio_outputs().len()];
        let inputs = input_buffers.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let mut outputs = output_buffers
            .iter_mut()
            .map(Vec::as_mut_slice)
            .collect::<Vec<_>>();
        processor.process_with_audio_buffers(
            256,
            &[],
            ClapTransportInfo::default(),
            &inputs,
            &mut outputs,
        );

        for (ch, output) in output_buffers.iter().enumerate() {
            assert!(
                output.iter().any(|&s| s != 0.0),
                "plugin output ch={ch} should contain non-zero samples after CLAP processing"
            );
        }
    }
}
