use crate::audio::io::AudioIO;
use crate::midi::io::{MIDIIO, MidiEvent};
use crate::plugins::ipc;
use crate::plugins::types::ParameterInfo;
use crate::plugins::types::Vst3PluginState;
use arc_swap::ArcSwapOption;
use maolan_plugin_protocol::events::EventPair;
use maolan_plugin_protocol::protocol::*;
use maolan_plugin_protocol::ringbuf::RingBuffer;
use maolan_plugin_protocol::shm::ShmMapping;
use std::cell::UnsafeCell;
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

pub struct Vst3Processor {
    path: String,
    plugin_id: String,
    name: String,
    audio_inputs: Vec<Arc<AudioIO>>,
    audio_outputs: Vec<Arc<AudioIO>>,
    main_audio_inputs: usize,
    main_audio_outputs: usize,
    midi_input_ports: Vec<Arc<MIDIIO>>,
    midi_output_ports: Vec<Arc<MIDIIO>>,
    param_infos: Vec<ParameterInfo>,
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
unsafe impl Sync for Vst3Processor {}

pub type SharedVst3Processor = Arc<Vst3Processor>;

impl Vst3Processor {
    pub fn new(
        sample_rate: f64,
        buffer_size: usize,
        plugin_path: &str,
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

        let instance_id = ipc::unique_instance_id("vst3");
        let num_inputs = input_count.max(1);
        let num_outputs = output_count.max(1);
        let (mut child, mapping, events, shm_name, stderr) = ipc::spawn_host(ipc::HostSpawnArgs {
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
            maolan_plugin_protocol::protocol::read_plugin_name_from_scratch(mapping.as_ptr())
                .unwrap_or_else(|| {
                    Path::new(plugin_path)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("VST3")
                        .to_string()
                })
        };

        let param_infos: Vec<ParameterInfo> = Vec::new();
        let param_values = param_infos
            .iter()
            .map(|info| (info.id, AtomicU64::new(info.default_value.to_bits())))
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

        Ok(Self {
            path: plugin_path.to_string(),
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

    pub fn parameter_infos(&self) -> Vec<ParameterInfo> {
        self.param_infos.clone()
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
            tracing::warn!("VST3 set_parameter_at: unknown parameter id {param_id}");
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

    pub fn snapshot_state(&self) -> Result<Vst3PluginState, String> {
        let (mapping, events) = match (&self.mapping, &self.events) {
            (Some(m), Some(e)) => (m, e),
            _ => return Err("VST3 processor not initialized".to_string()),
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

        let scratch = unsafe { scratch_ptr(ptr) };
        let size = serialize_vst3_state(scratch, state)?;
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
        let mapping = self.mapping.take();
        let events = self.events.take();
        let child = self.child.get_mut().take();
        let shm_name = std::mem::take(&mut self.shm_name);
        ipc::drop_host(mapping, events, child, shm_name);
    }
}

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

    #[cfg_attr(
        all(miri, target_os = "freebsd"),
        ignore = "plugin host discovery/runtime uses OS facilities not supported by Miri on FreeBSD"
    )]
    #[test]
    fn vst3_processor_crash_bypass() {
        let host_bin = find_host_binary();

        let processor = Vst3Processor::new(48000.0, 256, "__crash__", "__crash__", 1, 1, host_bin)
            .expect("should create VST3 processor for crash test");

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
        assert!(
            out_buf.iter().all(|&s| s == 1.0),
            "after crash, output should be bypass copy of input"
        );
    }
}
