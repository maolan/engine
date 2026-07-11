use crate::{audio::io::AudioIO, midi::io::MidiEvent, mutex::UnsafeMutex};
use jack::{
    AudioIn, AudioOut, Client, ClientOptions, Control, MidiIn, MidiOut, NotificationHandler, Port,
    ProcessHandler, ProcessScope, RawMidi, TransportPosition, TransportState,
};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::Sender;

/// Capacity of the SPSC MIDI rings between the JACK callback and the engine
/// thread, in events. One ring-full covers ~85 note-ons per 12 ms cycle.
const MIDI_RING_CAPACITY: usize = 1024;

#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub audio_inputs: usize,
    pub audio_outputs: usize,
    pub midi_inputs: usize,
    pub midi_outputs: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            audio_inputs: 2,
            audio_outputs: 2,
            midi_inputs: 1,
            midi_outputs: 1,
        }
    }
}

#[derive(Debug, Default)]
struct Notifications;

impl NotificationHandler for Notifications {}

struct Process {
    audio_in_ports: Arc<UnsafeMutex<Vec<Port<AudioIn>>>>,
    audio_out_ports: Arc<UnsafeMutex<Vec<Port<AudioOut>>>>,
    midi_in_ports: Vec<Port<MidiIn>>,
    midi_out_ports: Vec<Port<MidiOut>>,
    plan_slot: Arc<crate::render_plan::PlanSlot>,
    /// RT side of the MIDI-in ring: this callback is the sole producer, the
    /// engine thread drains at the cycle boundary.
    midi_in_producer: rtrb::Producer<MidiEvent>,
    /// RT side of the MIDI-out ring: this callback is the sole consumer, the
    /// engine thread produces after each completed cycle.
    midi_out_consumer: rtrb::Consumer<MidiEvent>,
    /// In-events dropped because the engine did not drain fast enough.
    midi_in_dropped: Arc<AtomicU64>,
    output_gain_linear: Arc<AtomicU32>,
    output_balance: Arc<AtomicU32>,
    tx_engine: Sender<crate::message::Message>,
}

impl Process {
    fn copy_audio_inputs(&mut self, ps: &ProcessScope) {
        let audio_in_ports = self.audio_in_ports.lock();
        let plan = self.plan_slot.load();
        for (idx, port) in audio_in_ports.iter().enumerate() {
            let Some(&(_channel, buf)) = plan.hw_in_map.get(idx) else {
                continue;
            };
            let src = port.as_slice(ps);
            // Safety: the driver is the sole producer of HwInput arena
            // buffers; the engine only dispatches the cycle after receiving
            // the HWFinished message sent at the end of this callback.
            let dst = unsafe { &mut *plan.buffer_ptr(buf) };
            let n = src.len().min(dst.len());
            dst[..n].copy_from_slice(&src[..n]);
            if n < dst.len() {
                dst[n..].fill(0.0);
            }
        }
    }

    fn copy_audio_outputs(&mut self, ps: &ProcessScope) {
        let gain = f32::from_bits(self.output_gain_linear.load(Ordering::Relaxed));
        let balance = f32::from_bits(self.output_balance.load(Ordering::Relaxed)).clamp(-1.0, 1.0);
        let audio_out_ports = self.audio_out_ports.lock();
        let plan = self.plan_slot.load();
        let stereo = audio_out_ports.len() == 2;
        let left_gain = if stereo {
            (1.0 - balance).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let right_gain = if stereo {
            (1.0 + balance).clamp(0.0, 1.0)
        } else {
            1.0
        };

        for (idx, port) in audio_out_ports.iter_mut().enumerate() {
            let dst = port.as_mut_slice(ps);
            let Some(&(buf, _channel)) = plan.hw_out_map.get(idx) else {
                dst.fill(0.0);
                continue;
            };
            // Safety: the engine sends HWFinished only after the cycle
            // completed, so every producer of these buffers has finished and
            // no worker touches the arena during this callback.
            let src = unsafe { plan.buffer(buf) };
            let n = src.len().min(dst.len());
            let balance_gain = if stereo {
                if idx == 0 { left_gain } else { right_gain }
            } else {
                1.0
            };
            crate::simd::copy_scaled_inplace(&mut dst[..n], &src[..n], gain * balance_gain);
            if n < dst.len() {
                dst[n..].fill(0.0);
            }
        }
    }

    fn collect_midi_input(&mut self, ps: &ProcessScope) {
        for port in &self.midi_in_ports {
            for raw in port.iter(ps) {
                let event = MidiEvent::new(raw.time, raw.bytes.to_vec());
                if self.midi_in_producer.push(event).is_err() {
                    self.midi_in_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    fn emit_midi_output(&mut self, ps: &ProcessScope) {
        if self.midi_out_ports.is_empty() {
            while self.midi_out_consumer.pop().is_ok() {}
            return;
        }
        let mut events = Vec::new();
        while let Ok(event) = self.midi_out_consumer.pop() {
            events.push(event);
        }
        if events.is_empty() {
            return;
        }
        for out_port in &mut self.midi_out_ports {
            let mut writer = out_port.writer(ps);
            for event in &events {
                let raw = RawMidi {
                    time: event.frame,
                    bytes: &event.data,
                };
                let _ = writer.write(&raw);
            }
        }
    }
}

impl ProcessHandler for Process {
    fn process(&mut self, _client: &Client, ps: &ProcessScope) -> Control {
        crate::enable_flush_denormals_to_zero();
        self.copy_audio_inputs(ps);
        self.collect_midi_input(ps);
        self.copy_audio_outputs(ps);
        self.emit_midi_output(ps);
        let _ = self.tx_engine.try_send(crate::message::Message::HWFinished);
        Control::Continue
    }
}

pub struct JackRuntime {
    client: Option<jack::AsyncClient<Notifications, Process>>,
    audio_in_ports: Arc<UnsafeMutex<Vec<Port<AudioIn>>>>,
    audio_out_ports: Arc<UnsafeMutex<Vec<Port<AudioOut>>>>,
    audio_ins: Arc<UnsafeMutex<Vec<Arc<AudioIO>>>>,
    audio_outs: Arc<UnsafeMutex<Vec<Arc<AudioIO>>>>,
    /// Engine-thread ends of the MIDI rings. Mutexes are control-side only;
    /// the JACK callback holds the bare ring ends.
    midi_in_consumer: Mutex<rtrb::Consumer<MidiEvent>>,
    midi_out_producer: Mutex<rtrb::Producer<MidiEvent>>,
    midi_in_dropped: Arc<AtomicU64>,
    output_gain_linear: Arc<AtomicU32>,
    output_balance: Arc<AtomicU32>,
    midi_input_count: usize,
    midi_output_count: usize,
    pub sample_rate: usize,
    pub buffer_size: usize,
}

impl JackRuntime {
    pub fn new(
        client_name: &str,
        config: Config,
        tx_engine: Sender<crate::message::Message>,
        plan_slot: Arc<crate::render_plan::PlanSlot>,
    ) -> Result<Self, String> {
        let (client, _status) = Client::new(client_name, ClientOptions::NO_START_SERVER)
            .map_err(|e| format!("Failed to create JACK client '{client_name}': {e}"))?;
        let sample_rate = client.sample_rate() as usize;
        let buffer_size = client.buffer_size() as usize;

        let audio_ins: Vec<Arc<AudioIO>> = (0..config.audio_inputs)
            .map(|_| Arc::new(AudioIO::new(buffer_size)))
            .collect();
        let audio_outs: Vec<Arc<AudioIO>> = (0..config.audio_outputs)
            .map(|_| Arc::new(AudioIO::new(buffer_size)))
            .collect();
        let audio_in_bridges = Arc::new(UnsafeMutex::new(audio_ins));
        let audio_out_bridges = Arc::new(UnsafeMutex::new(audio_outs));

        let mut audio_in_ports = Vec::with_capacity(config.audio_inputs);
        for i in 0..config.audio_inputs {
            let p = client
                .register_port(&format!("in_{}", i + 1), AudioIn::default())
                .map_err(|e| format!("Failed to register JACK audio input port {}: {e}", i + 1))?;
            audio_in_ports.push(p);
        }

        let mut audio_out_ports = Vec::with_capacity(config.audio_outputs);
        for i in 0..config.audio_outputs {
            let p = client
                .register_port(&format!("out_{}", i + 1), AudioOut::default())
                .map_err(|e| format!("Failed to register JACK audio output port {}: {e}", i + 1))?;
            audio_out_ports.push(p);
        }
        let audio_in_ports = Arc::new(UnsafeMutex::new(audio_in_ports));
        let audio_out_ports = Arc::new(UnsafeMutex::new(audio_out_ports));

        let mut midi_in_ports = Vec::with_capacity(config.midi_inputs);
        for i in 0..config.midi_inputs {
            let p = client
                .register_port(&format!("midi_in_{}", i + 1), MidiIn::default())
                .map_err(|e| format!("Failed to register JACK MIDI input port {}: {e}", i + 1))?;
            midi_in_ports.push(p);
        }

        let mut midi_out_ports = Vec::with_capacity(config.midi_outputs);
        for i in 0..config.midi_outputs {
            let p = client
                .register_port(&format!("midi_out_{}", i + 1), MidiOut::default())
                .map_err(|e| format!("Failed to register JACK MIDI output port {}: {e}", i + 1))?;
            midi_out_ports.push(p);
        }

        let (midi_in_producer, midi_in_consumer) = rtrb::RingBuffer::new(MIDI_RING_CAPACITY);
        let (midi_out_producer, midi_out_consumer) = rtrb::RingBuffer::new(MIDI_RING_CAPACITY);
        let midi_in_dropped = Arc::new(AtomicU64::new(0));
        let output_gain_linear = Arc::new(AtomicU32::new(1.0_f32.to_bits()));
        let output_balance = Arc::new(AtomicU32::new(0.0_f32.to_bits()));

        let process = Process {
            audio_in_ports: audio_in_ports.clone(),
            audio_out_ports: audio_out_ports.clone(),
            midi_in_ports,
            midi_out_ports,
            plan_slot,
            midi_in_producer,
            midi_out_consumer,
            midi_in_dropped: midi_in_dropped.clone(),
            output_gain_linear: output_gain_linear.clone(),
            output_balance: output_balance.clone(),
            tx_engine,
        };

        let client = client
            .activate_async(Notifications, process)
            .map_err(|e| format!("Failed to activate JACK client: {e}"))?;

        Ok(Self {
            client: Some(client),
            audio_in_ports,
            audio_out_ports,
            audio_ins: audio_in_bridges,
            audio_outs: audio_out_bridges,
            midi_in_consumer: Mutex::new(midi_in_consumer),
            midi_out_producer: Mutex::new(midi_out_producer),
            midi_in_dropped,
            output_gain_linear,
            output_balance,
            midi_input_count: config.midi_inputs,
            midi_output_count: config.midi_outputs,
            sample_rate,
            buffer_size,
        })
    }

    pub fn read_events_into(&self, out: &mut Vec<MidiEvent>) {
        out.clear();
        let mut consumer = self.midi_in_consumer.lock().expect("midi ring poisoned");
        while let Ok(event) = consumer.pop() {
            out.push(event);
        }
    }

    pub fn write_events(&self, events: &[MidiEvent]) {
        let mut producer = self.midi_out_producer.lock().expect("midi ring poisoned");
        for event in events {
            if producer.push(event.clone()).is_err() {
                self.midi_in_dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// MIDI events dropped at a full ring since the last call (both
    /// directions share the counter).
    pub fn take_midi_events_dropped(&self) -> u64 {
        self.midi_in_dropped.swap(0, Ordering::Relaxed)
    }

    pub fn set_output_gain_linear(&self, gain: f32) {
        self.output_gain_linear
            .store(gain.max(0.0).to_bits(), Ordering::Relaxed);
    }

    pub fn set_output_balance(&self, balance: f32) {
        self.output_balance
            .store(balance.clamp(-1.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn input_channels(&self) -> usize {
        self.audio_ins.lock().len()
    }

    pub fn output_channels(&self) -> usize {
        self.audio_outs.lock().len()
    }

    pub fn audio_ins(&self) -> Vec<Arc<AudioIO>> {
        self.audio_ins.lock().clone()
    }

    pub fn audio_outs(&self) -> Vec<Arc<AudioIO>> {
        self.audio_outs.lock().clone()
    }

    pub fn input_audio_port(&self, idx: usize) -> Option<Arc<AudioIO>> {
        self.audio_ins.lock().get(idx).cloned()
    }

    pub fn output_audio_port(&self, idx: usize) -> Option<Arc<AudioIO>> {
        self.audio_outs.lock().get(idx).cloned()
    }

    pub fn transport_start(&self) -> Result<(), String> {
        let client = self
            .client
            .as_ref()
            .ok_or("JACK client is not active".to_string())?;
        client
            .as_client()
            .transport()
            .start()
            .map_err(|e| format!("Failed to start JACK transport: {e}"))
    }

    pub fn transport_stop(&self) -> Result<(), String> {
        let client = self
            .client
            .as_ref()
            .ok_or("JACK client is not active".to_string())?;
        client
            .as_client()
            .transport()
            .stop()
            .map_err(|e| format!("Failed to stop JACK transport: {e}"))
    }

    pub fn transport_locate(&self, frame: usize) -> Result<(), String> {
        let client = self
            .client
            .as_ref()
            .ok_or("JACK client is not active".to_string())?;
        let mut position = TransportPosition::default();
        position.set_frame(frame as u32);
        client
            .as_client()
            .transport()
            .reposition(&position)
            .map_err(|e| format!("Failed to reposition JACK transport: {e}"))
    }

    pub fn transport_state_and_frame(&self) -> Result<(TransportState, usize), String> {
        let client = self
            .client
            .as_ref()
            .ok_or("JACK client is not active".to_string())?;
        let state = client
            .as_client()
            .transport()
            .query()
            .map_err(|e| format!("Failed to query JACK transport: {e}"))?;
        Ok((state.state, state.pos.frame() as usize))
    }

    pub fn add_audio_input_port(&mut self) -> Result<usize, String> {
        let client = self
            .client
            .as_ref()
            .ok_or("JACK client is not active".to_string())?;
        let next_index = self.audio_in_ports.lock().len();
        let port = client
            .as_client()
            .register_port(&format!("in_{}", next_index + 1), AudioIn::default())
            .map_err(|e| {
                format!(
                    "Failed to register JACK audio input port {}: {e}",
                    next_index + 1
                )
            })?;
        self.audio_in_ports.lock().push(port);
        self.audio_ins
            .lock()
            .push(Arc::new(AudioIO::new(self.buffer_size)));
        Ok(next_index + 1)
    }

    pub fn add_audio_output_port(&mut self) -> Result<usize, String> {
        let client = self
            .client
            .as_ref()
            .ok_or("JACK client is not active".to_string())?;
        let next_index = self.audio_out_ports.lock().len();
        let port = client
            .as_client()
            .register_port(&format!("out_{}", next_index + 1), AudioOut::default())
            .map_err(|e| {
                format!(
                    "Failed to register JACK audio output port {}: {e}",
                    next_index + 1
                )
            })?;
        self.audio_out_ports.lock().push(port);
        self.audio_outs
            .lock()
            .push(Arc::new(AudioIO::new(self.buffer_size)));
        Ok(next_index + 1)
    }

    pub fn remove_audio_input_port(&mut self, idx: usize) -> Result<Arc<AudioIO>, String> {
        let client = self
            .client
            .as_ref()
            .ok_or("JACK client is not active".to_string())?;
        if idx >= self.audio_in_ports.lock().len() || idx >= self.audio_ins.lock().len() {
            return Err("JACK audio input port index is out of range".to_string());
        }
        let port = self.audio_in_ports.lock().remove(idx);
        let bridge = self.audio_ins.lock().remove(idx);
        client
            .as_client()
            .unregister_port(port)
            .map_err(|e| format!("Failed to unregister JACK audio input port: {e}"))?;
        Ok(bridge)
    }

    pub fn remove_audio_output_port(&mut self, idx: usize) -> Result<Arc<AudioIO>, String> {
        let client = self
            .client
            .as_ref()
            .ok_or("JACK client is not active".to_string())?;
        if idx >= self.audio_out_ports.lock().len() || idx >= self.audio_outs.lock().len() {
            return Err("JACK audio output port index is out of range".to_string());
        }
        let port = self.audio_out_ports.lock().remove(idx);
        let bridge = self.audio_outs.lock().remove(idx);
        client
            .as_client()
            .unregister_port(port)
            .map_err(|e| format!("Failed to unregister JACK audio output port: {e}"))?;
        Ok(bridge)
    }

    pub fn midi_input_devices(&self) -> Vec<String> {
        (0..self.midi_input_count)
            .map(|idx| format!("jack:midi_in_{}", idx + 1))
            .collect()
    }

    pub fn midi_output_devices(&self) -> Vec<String> {
        (0..self.midi_output_count)
            .map(|idx| format!("jack:midi_out_{}", idx + 1))
            .collect()
    }
}

impl Drop for JackRuntime {
    fn drop(&mut self) {
        if let Some(client) = self.client.take() {
            let _ = client.deactivate();
        }
    }
}
