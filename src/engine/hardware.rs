use super::*;
#[cfg(target_os = "linux")]
use crate::hw::alsa::{HwDriver, HwOptions, MidiHub};
#[cfg(target_os = "macos")]
use crate::hw::coreaudio::{HwDriver, HwOptions, MidiHub};
#[cfg(unix)]
use crate::hw::jack::JackRuntime;
#[cfg(target_os = "windows")]
use crate::hw::options::HwOptions;
#[cfg(target_os = "freebsd")]
use crate::hw::oss as hw;
#[cfg(target_os = "freebsd")]
use crate::hw::oss::{HwDriver, HwOptions};
#[cfg(target_os = "openbsd")]
use crate::hw::sndio::{HwDriver, HwOptions, MidiHub};
#[cfg(target_os = "windows")]
use crate::hw::wasapi::{self, HwDriver, MidiHub};
#[cfg(target_os = "linux")]
use crate::workers::alsa_worker::HwWorker;
#[cfg(target_os = "macos")]
use crate::workers::coreaudio_worker::HwWorker;
#[cfg(target_os = "freebsd")]
use crate::workers::oss_worker::HwWorker;
#[cfg(target_os = "openbsd")]
use crate::workers::sndio_worker::HwWorker;
#[cfg(target_os = "windows")]
use crate::workers::wasapi_worker::HwWorker;
use crate::{
    audio::io::AudioIO,
    engine::HwDriverInfo,
    hw::{
        config,
        traits::{HwDevice, HwWorkerDriver},
    },
    message::{Action, Message},
};
#[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "openbsd"))]
use std::fs::read_dir;
use std::sync::Arc;
use tokio::sync::mpsc::channel;
use tracing::error;

impl Engine {
    pub(crate) fn finalize_midi_hw_devices(mut devices: Vec<String>) -> Vec<String> {
        devices.sort();
        devices.dedup();
        devices
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd"))]
    pub(crate) fn discover_midi_hw_devices_from_dir(path: &str, prefixes: &[&str]) -> Vec<String> {
        let devices = read_dir(path)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter_map(|path| {
                        let name = path.file_name()?.to_str()?;
                        prefixes
                            .iter()
                            .any(|prefix| name.starts_with(prefix))
                            .then(|| path.to_string_lossy().into_owned())
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self::finalize_midi_hw_devices(devices)
    }

    pub(crate) fn discover_midi_hw_devices() -> Vec<String> {
        #[cfg(target_os = "freebsd")]
        let devices = Self::discover_midi_hw_devices_from_dir("/dev", &["umidi", "midi"]);
        #[cfg(target_os = "linux")]
        let devices = Self::discover_midi_hw_devices_from_dir("/dev/snd", &["midiC"]);
        #[cfg(target_os = "openbsd")]
        let devices = Self::discover_midi_hw_devices_from_dir("/dev", &["midi"]);
        #[cfg(target_os = "windows")]
        let devices = {
            let mut devices = wasapi::list_midi_input_devices();
            devices.extend(wasapi::list_midi_output_devices());
            Self::finalize_midi_hw_devices(devices)
        };
        #[cfg(target_os = "macos")]
        let devices = {
            let mut devices = Vec::new();
            for source in coremidi::Sources {
                if let Some(name) = source.display_name() {
                    devices.push(name);
                }
            }
            for dest in coremidi::Destinations {
                if let Some(name) = dest.display_name() {
                    devices.push(name);
                }
            }
            Self::finalize_midi_hw_devices(devices)
        };
        devices
    }

    pub(crate) fn open_hw_driver(
        device: &str,
        _input_device: Option<&str>,
        sample_rate_hz: i32,
        bits: i32,
        hw_opts: HwOptions,
    ) -> Result<HwDriver, String> {
        #[cfg(any(target_os = "windows", target_os = "freebsd", target_os = "linux"))]
        {
            HwDriver::new_with_options(device, _input_device, sample_rate_hz, bits, hw_opts)
                .map_err(|e| e.to_string())
        }
        #[cfg(target_os = "openbsd")]
        {
            HwDriver::new_with_options(device, sample_rate_hz, bits, hw_opts)
                .map_err(|e| e.to_string())
        }
    }

    pub(crate) fn hw_profile_backend_label(_device: &str) -> &'static str {
        #[cfg(target_os = "windows")]
        let label = "WASAPI";
        #[cfg(target_os = "linux")]
        let label = "ALSA";
        #[cfg(target_os = "freebsd")]
        let label = "OSS";
        #[cfg(target_os = "openbsd")]
        let label = "sndio";
        #[cfg(target_os = "macos")]
        let label = "CoreAudio";
        label
    }

    #[cfg(target_os = "freebsd")]
    pub(crate) fn maybe_start_freebsd_sync_group(&self) {
        if let Some(oss) = &self.hw_driver {
            let in_fd = oss.input_fd();
            let out_fd = oss.output_fd();
            let mut group = 0;
            let in_group = hw::add_to_sync_group(in_fd, group, true);
            if in_group > 0 {
                group = in_group;
            }
            let out_group = hw::add_to_sync_group(out_fd, group, false);
            if out_group > 0 {
                group = out_group;
            }
            let sync_started = if group > 0 {
                hw::start_sync_group(in_fd, group).is_ok()
            } else {
                false
            };
            if !sync_started {
                let _ = oss.start_input_trigger();
                let _ = oss.start_output_trigger();
            }
        }
    }

    #[cfg(not(target_os = "freebsd"))]
    pub(crate) fn maybe_start_freebsd_sync_group(&self) {}

    pub(crate) async fn open_discovered_midi_hw_devices(&mut self) {
        for device in Self::discover_midi_hw_devices() {
            if let Some(worker) = &self.hw_worker {
                // The worker performs the actual open and responds with the
                // real result via Message::Response, which is forwarded to
                // clients; notifying an optimistic Ok here would produce a
                // spurious success followed by a contradictory Err when the
                // open fails in the worker.
                let _ = worker
                    .tx
                    .send(Message::HWOpenMidiInputDevice(device.clone()))
                    .await;
                let _ = worker
                    .tx
                    .send(Message::HWOpenMidiOutputDevice(device.clone()))
                    .await;
            } else {
                let (opened_in, opened_out) = if let Some(midi_hub) = self.midi_hub.as_mut() {
                    (
                        midi_hub.open_input(&device).is_ok(),
                        midi_hub.open_output(&device).is_ok(),
                    )
                } else {
                    (false, false)
                };
                if opened_in {
                    self.notify_clients(Ok(Action::OpenMidiInputDevice(device.clone())))
                        .await;
                }
                if opened_out {
                    self.notify_clients(Ok(Action::OpenMidiOutputDevice(device.clone())))
                        .await;
                }
            }
        }
    }

    #[cfg(unix)]
    pub(crate) async fn maybe_open_jack_runtime(
        &mut self,
        request: AudioOpenRequest<'_>,
    ) -> Option<()> {
        if !request.device.eq_ignore_ascii_case("jack") {
            return None;
        }
        match JackRuntime::new(
            "maolan",
            crate::hw::jack::Config::default(),
            self.tx.clone(),
            self.plan_slot.clone(),
        ) {
            Ok(runtime) => {
                let input_channels = runtime.input_channels();
                let output_channels = runtime.output_channels();
                let midi_inputs = runtime.midi_input_devices();
                let midi_outputs = runtime.midi_output_devices();
                let rate = runtime.sample_rate;
                if let Some(worker) = self.hw_worker.take() {
                    if let Some(hw) = self.hw_driver.as_mut() {
                        hw.request_stop();
                    }
                    let _ = worker.tx.send(Message::Request(Action::Quit)).await;
                    let _ = worker.handle.await;
                }
                self.hw_driver = None;
                self.hw_driver_info = None;
                self.hw_input_ports.clear();
                self.hw_output_ports.clear();
                if self.midi_hub.is_none() {
                    self.midi_hub = Some(MidiHub::default());
                }
                self.jack_runtime = Some(runtime);
                self.publish_hw_ports();
                self.publish_hw_infos(input_channels, output_channels, rate)
                    .await;
                for device in midi_inputs {
                    self.notify_clients(Ok(Action::OpenMidiInputDevice(device)))
                        .await;
                }
                for device in midi_outputs {
                    self.notify_clients(Ok(Action::OpenMidiOutputDevice(device)))
                        .await;
                }
                self.notify_clients(Ok(Action::OpenAudioDevice {
                    device: request.device.to_string(),
                    input_device: request.input_device.map(ToOwned::to_owned),
                    sample_rate_hz: request.sample_rate_hz,
                    bits: request.bits,
                    exclusive: request.exclusive,
                    period_frames: request.period_frames,
                    nperiods: request.nperiods,
                    sync_mode: request.sync_mode,
                    actual_period_frames: request.period_frames,
                    input_channels,
                    output_channels,
                    bytes_per_frame: 0,
                }))
                .await;
                self.awaiting_hwfinished = true;
            }
            Err(e) => {
                error!("Failed to open JACK runtime: {e}");
                self.notify_clients(Err(e)).await;
            }
        }
        Some(())
    }

    pub(crate) fn hw_driver_input_audio_port(&self, from_port: usize) -> Option<Arc<AudioIO>> {
        self.hw_input_ports.get(from_port).cloned()
    }

    pub(crate) fn hw_driver_output_audio_port(&self, to_port: usize) -> Option<Arc<AudioIO>> {
        self.hw_output_ports.get(to_port).cloned()
    }

    #[cfg(unix)]
    pub(crate) fn jack_input_audio_port(&self, from_port: usize) -> Option<Arc<AudioIO>> {
        self.jack_runtime
            .as_ref()
            .and_then(|j| j.input_audio_port(from_port))
    }

    #[cfg(not(unix))]
    pub(crate) fn jack_input_audio_port(&self, _from_port: usize) -> Option<Arc<AudioIO>> {
        None
    }

    #[cfg(unix)]
    pub(crate) fn jack_output_audio_port(&self, to_port: usize) -> Option<Arc<AudioIO>> {
        self.jack_runtime
            .as_ref()
            .and_then(|j| j.output_audio_port(to_port))
    }

    #[cfg(not(unix))]
    pub(crate) fn jack_output_audio_port(&self, _to_port: usize) -> Option<Arc<AudioIO>> {
        None
    }

    #[cfg(unix)]
    pub(crate) fn jack_transport_sync_decision(
        current_playing: bool,
        current_sample: usize,
        jack_playing: bool,
        normalized_frame: usize,
        cycle_samples: usize,
    ) -> JackTransportSyncDecision {
        let play_sync = match (current_playing, jack_playing) {
            (false, true) => Some(JackTransportPlaySync::Start),
            (true, false) => Some(JackTransportPlaySync::Stop),
            _ => None,
        };
        let position_drift = normalized_frame.abs_diff(current_sample);
        let position_changed = normalized_frame != current_sample;
        let should_sync_position = position_changed
            && (!jack_playing || play_sync.is_some() || position_drift > cycle_samples.max(1));

        JackTransportSyncDecision {
            play_sync,
            position_sync: should_sync_position.then_some(normalized_frame),
        }
    }

    #[cfg(unix)]
    pub(crate) async fn sync_from_jack_transport(&mut self) {
        let Some(jack) = self.jack_runtime.as_ref() else {
            return;
        };
        let Ok((jack_state, jack_frame)) = jack.transport_state_and_frame() else {
            return;
        };

        let jack_playing = matches!(
            jack_state,
            jack::TransportState::Rolling | jack::TransportState::Starting
        );
        let normalized_frame = self.normalize_transport_sample(jack_frame);
        let decision = Self::jack_transport_sync_decision(
            self.playing,
            self.transport_sample,
            jack_playing,
            normalized_frame,
            self.current_cycle_samples(),
        );

        if let Some(play_sync) = decision.play_sync {
            self.playing = matches!(play_sync, JackTransportPlaySync::Start);
            self.transport_running = self.playing;
            if matches!(play_sync, JackTransportPlaySync::Start) {
                self.transport_restart_pending = false;
                self.transport_panic_flush_pending = false;
                self.notify_clients(Ok(Action::Play)).await;
            } else {
                self.transport_panic_flush_pending = false;
                self.transport_restart_pending = false;
                let panic_events = self.note_off_events_for_all_active_tracks();
                self.pending_hw_midi_out_events_by_device
                    .extend(panic_events);
                self.flush_recordings().await;
                self.notify_clients(Ok(Action::Stop)).await;
            }
        }

        if let Some(sample) = decision.position_sync {
            self.transport_sample = sample;
            self.notify_clients(Ok(Action::TransportPosition(self.transport_sample)))
                .await;
        }
    }

    pub(crate) fn hw_device_info<D: HwDevice>(d: &D) -> HwDeviceInfo {
        (
            d.input_channels(),
            d.output_channels(),
            d.sample_rate() as usize,
            d.latency_ranges(),
        )
    }

    pub(crate) async fn publish_hw_infos(
        &mut self,
        input_channels: usize,
        output_channels: usize,
        rate: usize,
    ) {
        self.notify_clients(Ok(Action::HWInfo {
            channels: input_channels,
            rate,
            input: true,
        }))
        .await;
        self.notify_clients(Ok(Action::HWInfo {
            channels: output_channels,
            rate,
            input: false,
        }))
        .await;
    }

    #[cfg(unix)]
    pub(crate) fn jack_runtime_is_some(&self) -> bool {
        self.jack_runtime.is_some()
    }

    #[cfg(not(unix))]
    pub(crate) fn jack_runtime_is_some(&self) -> bool {
        false
    }

    pub(crate) fn can_schedule_hw_cycle(&self) -> bool {
        self.playing && (self.hw_worker.is_some() || self.jack_runtime_is_some())
    }

    pub(crate) async fn ensure_hw_worker_running(&mut self) {
        if self.hw_worker.is_some() || self.hw_driver.is_none() {
            return;
        }
        let (tx, rx) = channel::<Message>(32);
        let hw = self.hw_driver.take().unwrap();
        let midi_hub = self.midi_hub.take().unwrap_or_default();
        let tx_engine = self.tx.clone();
        let handler = tokio::spawn(async move {
            let worker = HwWorker::new(hw, midi_hub, rx, tx_engine);
            worker.work().await;
        });
        self.hw_worker = Some(WorkerData::new(tx, handler));
    }

    pub(crate) fn build_hw_options(
        exclusive: bool,
        period_frames: usize,
        nperiods: usize,
        sync_mode: bool,
    ) -> HwOptions {
        HwOptions {
            exclusive,
            period_frames: period_frames.max(1).next_power_of_two(),
            nperiods: nperiods.max(1),
            sync_mode,
            ..Default::default()
        }
    }

    pub(crate) async fn open_non_jack_audio_device(
        &mut self,
        device: &str,
        input_device: Option<&str>,
        sample_rate_hz: i32,
        bits: i32,
        hw_opts: HwOptions,
    ) -> Result<(), String> {
        let hw_profile_enabled = config::env_flag(config::HW_PROFILE_ENV);
        let mut d = Self::open_hw_driver(device, input_device, sample_rate_hz, bits, hw_opts)?;
        d.set_plan_slot(self.plan_slot.clone());
        let (in_channels, out_channels, rate, (in_lat, out_lat)) = Self::hw_device_info(&d);
        if hw_profile_enabled {
            let label = Self::hw_profile_backend_label(device);
            error!(
                "{} config: exclusive={}, period={}, nperiods={}, ignore_hwbuf={}, sync_mode={}, in_latency_extra={}, out_latency_extra={}, input_range={:?}, output_range={:?}",
                label,
                hw_opts.exclusive,
                hw_opts.period_frames,
                hw_opts.nperiods,
                hw_opts.ignore_hwbuf,
                hw_opts.sync_mode,
                hw_opts.input_latency_frames,
                hw_opts.output_latency_frames,
                in_lat,
                out_lat
            );
        }
        self.hw_input_latency_frames = in_lat.0;
        self.hw_output_latency_frames = out_lat.0;
        self.hw_input_ports = (0..in_channels)
            .filter_map(|idx| d.input_port(idx))
            .collect();
        self.hw_output_ports = (0..out_channels)
            .filter_map(|idx| d.output_port(idx))
            .collect();
        self.hw_driver_info = Some(HwDriverInfo {
            cycle_samples: d.cycle_samples(),
            sample_rate: d.sample_rate(),
            input_channels: in_channels,
            output_channels: out_channels,
            sample_bits: d.sample_bits(),
            frame_size_bytes: d.frame_size_bytes(),
        });
        #[cfg(unix)]
        {
            self.jack_runtime = None;
        }
        self.hw_driver = Some(d);
        self.publish_hw_ports();
        self.publish_hw_infos(in_channels, out_channels, rate).await;
        Ok(())
    }

    /// Push the current hardware port list into the plan builder and request
    /// a new render plan. Until the new plan is published, the old one keeps
    /// executing.
    pub(crate) fn publish_hw_ports(&mut self) {
        let ports = crate::plan_builder::HwPorts {
            ins: self.all_hw_input_audio_ports(),
            outs: self.all_hw_output_audio_ports(),
            buffer_size: self.current_cycle_samples(),
        };
        self.hw_ports.store(Arc::new(ports));
        self.plan_builder.mark_dirty();
    }

    pub(crate) async fn finalize_open_audio_device(&mut self) {
        self.maybe_start_freebsd_sync_group();
        if self.metronome_enabled {
            self.ensure_metronome_track().await;
        }
        if self.hw_worker.is_none() && (self.hw_driver.is_some() || self.hw_driver_info.is_some()) {
            self.ensure_hw_worker_running().await;
            self.request_hw_cycle().await;
        }
        self.open_discovered_midi_hw_devices().await;
    }

    pub(crate) fn hw_input_audio_port(&self, from_port: usize) -> Option<Arc<AudioIO>> {
        self.hw_driver_input_audio_port(from_port)
            .or_else(|| self.jack_input_audio_port(from_port))
    }

    pub(crate) fn hw_output_audio_port(&self, to_port: usize) -> Option<Arc<AudioIO>> {
        self.hw_driver_output_audio_port(to_port)
            .or_else(|| self.jack_output_audio_port(to_port))
    }

    pub(crate) fn all_hw_output_audio_ports(&self) -> Vec<Arc<AudioIO>> {
        if !self.hw_output_ports.is_empty() {
            return self.hw_output_ports.clone();
        }
        #[cfg(unix)]
        if let Some(jack) = &self.jack_runtime {
            return jack.audio_outs();
        }
        Vec::new()
    }

    pub(crate) fn all_hw_input_audio_ports(&self) -> Vec<Arc<AudioIO>> {
        if !self.hw_input_ports.is_empty() {
            return self.hw_input_ports.clone();
        }
        #[cfg(unix)]
        if let Some(jack) = &self.jack_runtime {
            return jack.audio_ins();
        }
        Vec::new()
    }

    pub(crate) async fn handle_open_audio_device(
        &mut self,
        action: Action,
    ) -> (bool, Option<Action>) {
        let Action::OpenAudioDevice {
            device,
            input_device,
            sample_rate_hz,
            bits,
            exclusive,
            period_frames,
            nperiods,
            sync_mode,
            ..
        } = action
        else {
            return (false, None);
        };
        #[cfg(unix)]
        {
            let request = AudioOpenRequest {
                device: &device,
                input_device: input_device.as_deref(),
                sample_rate_hz,
                bits,
                exclusive,
                period_frames,
                nperiods,
                sync_mode,
            };
            if self.maybe_open_jack_runtime(request).await.is_some() {
                return (true, None);
            }
        }
        let hw_opts = Self::build_hw_options(exclusive, period_frames, nperiods, sync_mode);
        let open_result = self
            .open_non_jack_audio_device(
                &device,
                input_device.as_deref(),
                sample_rate_hz,
                bits,
                hw_opts,
            )
            .await;
        match open_result {
            Ok(()) => {}
            Err(e) => {
                error!("Failed to open audio device: {e}");
                self.notify_clients(Err(e)).await;
                return (true, None);
            }
        }
        self.finalize_open_audio_device().await;
        if let Some(info) = self.hw_driver_info {
            let effective_action = Action::OpenAudioDevice {
                device: device.clone(),
                input_device: input_device.clone(),
                sample_rate_hz: info.sample_rate,
                bits: info.sample_bits,
                exclusive,
                period_frames,
                nperiods,
                sync_mode,
                actual_period_frames: info.cycle_samples,
                input_channels: info.input_channels,
                output_channels: info.output_channels,
                bytes_per_frame: info.frame_size_bytes,
            };
            return (false, Some(effective_action));
        }
        (false, None)
    }

    pub(crate) async fn handle_jack_add_audio_input_port(&mut self, a: Action) -> bool {
        #[cfg(unix)]
        {
            let Some(jack) = self.jack_runtime.as_mut() else {
                self.notify_clients(Err(
                    "JACK runtime is not active; open the JACK backend first".to_string(),
                ))
                .await;
                return false;
            };
            let result = jack.add_audio_input_port().map(|_| {
                (
                    jack.input_channels(),
                    jack.output_channels(),
                    jack.sample_rate,
                )
            });
            let (input_channels, output_channels, rate) = match result {
                Ok(info) => info,
                Err(e) => {
                    self.notify_clients(Err(e)).await;
                    return true;
                }
            };
            self.publish_hw_ports();
            self.publish_hw_infos(input_channels, output_channels, rate)
                .await;
            self.notify_clients(Ok(a.clone())).await;
        }
        #[cfg(not(unix))]
        {
            self.notify_clients(Err(
                "JACK backend is not available on this platform build".to_string()
            ))
            .await;
        }
        false
    }

    pub(crate) async fn handle_jack_remove_audio_input_port(
        &mut self,
        _removed_port: usize,
        a: Action,
    ) -> bool {
        #[cfg(unix)]
        {
            let removed_port = _removed_port;
            if self.jack_runtime.is_none() {
                self.notify_clients(Err(
                    "JACK runtime is not active; open the JACK backend first".to_string(),
                ))
                .await;
                return false;
            }
            let Some(removed_io) = self
                .jack_runtime
                .as_ref()
                .and_then(|jack| jack.input_audio_port(removed_port))
            else {
                self.notify_clients(Err(
                    "JACK audio input port index is out of range".to_string()
                ))
                .await;
                return true;
            };
            let reindex_notifications =
                self.reindex_notifications_for_removed_hw_input(removed_port);
            for disconnect in
                self.disconnect_actions_for_removed_hw_input(removed_port, &removed_io)
            {
                if let Err(e) = self.disconnect_audio_route_and_notify(disconnect).await {
                    self.notify_clients(Err(e)).await;
                    return true;
                }
            }
            let result = self
                .jack_runtime
                .as_mut()
                .expect("checked jack runtime")
                .remove_audio_input_port(removed_port);
            if let Err(e) = result {
                self.notify_clients(Err(e)).await;
                return true;
            }
            let (input_channels, output_channels, rate) = {
                let jack = self.jack_runtime.as_ref().expect("checked jack runtime");
                (
                    jack.input_channels(),
                    jack.output_channels(),
                    jack.sample_rate,
                )
            };
            self.publish_hw_ports();
            for action in reindex_notifications {
                self.notify_clients(Ok(action)).await;
            }
            self.publish_hw_infos(input_channels, output_channels, rate)
                .await;
            self.notify_clients(Ok(a.clone())).await;
        }
        #[cfg(not(unix))]
        {
            self.notify_clients(Err(
                "JACK backend is not available on this platform build".to_string()
            ))
            .await;
        }
        false
    }

    pub(crate) async fn handle_jack_add_audio_output_port(&mut self, a: Action) -> bool {
        #[cfg(unix)]
        {
            let Some(jack) = self.jack_runtime.as_mut() else {
                self.notify_clients(Err(
                    "JACK runtime is not active; open the JACK backend first".to_string(),
                ))
                .await;
                return false;
            };
            let result = jack.add_audio_output_port().map(|_| {
                (
                    jack.input_channels(),
                    jack.output_channels(),
                    jack.sample_rate,
                )
            });
            let (input_channels, output_channels, rate) = match result {
                Ok(info) => info,
                Err(e) => {
                    self.notify_clients(Err(e)).await;
                    return true;
                }
            };
            self.publish_hw_ports();
            self.publish_hw_infos(input_channels, output_channels, rate)
                .await;
            self.notify_clients(Ok(a.clone())).await;
        }
        #[cfg(not(unix))]
        {
            self.notify_clients(Err(
                "JACK backend is not available on this platform build".to_string()
            ))
            .await;
        }
        false
    }

    pub(crate) async fn handle_jack_remove_audio_output_port(
        &mut self,
        _removed_port: usize,
        a: Action,
    ) -> bool {
        #[cfg(unix)]
        {
            let removed_port = _removed_port;
            if self.jack_runtime.is_none() {
                self.notify_clients(Err(
                    "JACK runtime is not active; open the JACK backend first".to_string(),
                ))
                .await;
                return false;
            }
            let Some(removed_io) = self
                .jack_runtime
                .as_ref()
                .and_then(|jack| jack.output_audio_port(removed_port))
            else {
                self.notify_clients(Err(
                    "JACK audio output port index is out of range".to_string()
                ))
                .await;
                return true;
            };
            let reindex_notifications =
                self.reindex_notifications_for_removed_hw_output(removed_port);
            for disconnect in
                self.disconnect_actions_for_removed_hw_output(removed_port, &removed_io)
            {
                if let Err(e) = self.disconnect_audio_route_and_notify(disconnect).await {
                    self.notify_clients(Err(e)).await;
                    return true;
                }
            }
            let result = self
                .jack_runtime
                .as_mut()
                .expect("checked jack runtime")
                .remove_audio_output_port(removed_port);
            if let Err(e) = result {
                self.notify_clients(Err(e)).await;
                return true;
            }
            let (input_channels, output_channels, rate) = {
                let jack = self.jack_runtime.as_ref().expect("checked jack runtime");
                (
                    jack.input_channels(),
                    jack.output_channels(),
                    jack.sample_rate,
                )
            };
            self.publish_hw_ports();
            for action in reindex_notifications {
                self.notify_clients(Ok(action)).await;
            }
            self.publish_hw_infos(input_channels, output_channels, rate)
                .await;
            self.notify_clients(Ok(a.clone())).await;
        }
        #[cfg(not(unix))]
        {
            self.notify_clients(Err(
                "JACK backend is not available on this platform build".to_string()
            ))
            .await;
        }
        false
    }
}
