use super::*;
#[cfg(target_os = "macos")]
use crate::hw::coreaudio::{HwDriver, HwOptions, MidiHub};
#[cfg(target_os = "windows")]
use crate::hw::options::HwOptions;
#[cfg(target_os = "openbsd")]
use crate::hw::sndio::{HwDriver, HwOptions, MidiHub};
#[cfg(target_os = "windows")]
use crate::hw::wasapi::{self, HwDriver, MidiHub};
use crate::message::{Action, PluginKind};
#[cfg(target_os = "macos")]
use crate::workers::coreaudio_worker::HwWorker;
#[cfg(target_os = "openbsd")]
use crate::workers::sndio_worker::HwWorker;
#[cfg(target_os = "windows")]
use crate::workers::wasapi_worker::HwWorker;
use std::path::Path;

impl Engine {
    pub(crate) fn resolve_plugin_identifier(
        &self,
        kind: PluginKind,
        identifier: &str,
    ) -> Result<String, String> {
        if identifier.is_empty() {
            return Err("plugin identifier is empty".to_string());
        }
        if identifier.contains('/')
            || identifier.contains('\\')
            || identifier.contains("::")
            || identifier.contains('#')
            || identifier.contains("://")
            || identifier.starts_with("file:")
            || Path::new(identifier).exists()
        {
            return Ok(identifier.to_string());
        }

        match kind {
            PluginKind::Clap => {
                let plugins =
                    crate::plugins::scan_plugins::<crate::plugins::types::ClapPluginInfo>("clap")
                        .map_err(|e| format!("failed to scan CLAP plugins: {e}"))?;
                plugins
                    .into_iter()
                    .find(|p| !p.id.is_empty() && p.id == identifier)
                    .map(|p| p.path)
                    .ok_or_else(|| format!("CLAP plugin ID not found: {identifier}"))
            }
            PluginKind::Vst3 => {
                let plugins =
                    crate::plugins::scan_plugins::<crate::plugins::types::Vst3PluginInfo>("vst3")
                        .map_err(|e| format!("failed to scan VST3 plugins: {e}"))?;
                plugins
                    .into_iter()
                    .find(|p| !p.id.is_empty() && p.id == identifier)
                    .map(|p| p.path)
                    .ok_or_else(|| format!("VST3 plugin ID not found: {identifier}"))
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginKind::Lv2 => {
                let plugins =
                    crate::plugins::scan_plugins::<crate::plugins::types::Lv2PluginInfo>("lv2")
                        .map_err(|e| format!("failed to scan LV2 plugins: {e}"))?;
                plugins
                    .into_iter()
                    .find(|p| p.uri == identifier)
                    .map(|p| p.uri)
                    .ok_or_else(|| format!("LV2 plugin URI not found: {identifier}"))
            }
        }
    }

    pub(crate) const METRONOME_TRACK: &'static str = "metronome";
    pub(crate) const METRONOME_DEFAULT_LEVEL_DB: f32 = -10.0;
    pub(crate) const MIDI_CC_ALL_SOUND_OFF: u8 = 120;
    pub(crate) const MIDI_CC_SUSTAIN_PEDAL: u8 = 64;

    pub(crate) fn default_clip_plugin_graph_json(
        audio_ins: usize,
        audio_outs: usize,
    ) -> serde_json::Value {
        let connections = (0..audio_ins.min(audio_outs))
            .map(|port| {
                serde_json::json!({
                    "from_node": "TrackInput",
                    "from_port": port,
                    "to_node": "TrackOutput",
                    "to_port": port,
                    "kind": "Audio",
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "plugins": [],
            "connections": connections,
        })
    }

    pub(crate) fn set_clip_plugin_graph_json(
        &mut self,
        track_name: &str,
        clip_index: usize,
        plugin_graph_json: Option<serde_json::Value>,
    ) {
        if let Some(track) = self.state_snapshot.load_full().tracks.get(track_name) {
            let track = track.lock();
            track.audio.update_clip(clip_index, |clip| {
                clip.plugin_graph_json = plugin_graph_json;
            });
        }
    }

    pub(crate) async fn handle_track_load_clap_plugin(
        &mut self,
        track_name: &str,
        plugin_id: &str,
        instance_id: Option<usize>,
    ) -> bool {
        if self
            .reject_if_track_frozen(track_name, "CLAP plugin loading")
            .await
        {
            return true;
        }
        let resolved_plugin_path = match self.resolve_plugin_identifier(PluginKind::Clap, plugin_id)
        {
            Ok(path) => path,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let mut track = track.lock();
        if track.audio.processing() {
            self.notify_clients(Err(format!(
                "Track '{}' is currently processing audio; stop playback before loading CLAP plugins",
                track_name
            )))
            .await;
            return true;
        }
        if let Err(e) = track.load_clap_plugin(&resolved_plugin_path, instance_id) {
            self.notify_clients(Err(e)).await;
            return true;
        }
        self.notify_clients(Ok(Action::Log {
            source: "engine".to_string(),
            message: format!("CLAP plugin loaded on track '{track_name}': {resolved_plugin_path}"),
        }))
        .await;
        if let Some(instance) = track.clap_plugins.last()
            && let Some(stderr) = instance.processor.take_stderr()
        {
            let source = format!("clap:{resolved_plugin_path}");
            self.spawn_plugin_host_stderr_reader(stderr, source);
            self.notify_clients(Ok(Action::Log {
                source: "engine".to_string(),
                message: format!("Attached stderr reader for CLAP plugin on track '{track_name}'"),
            }))
            .await;
        }
        false
    }

    pub(crate) async fn handle_track_unload_clap_plugin(
        &mut self,
        track_name: &str,
        plugin_id: &str,
    ) -> bool {
        if self
            .reject_if_track_frozen(track_name, "CLAP plugin unloading")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let mut track = track.lock();
        if track.audio.processing() {
            self.notify_clients(Err(format!(
                "Track '{}' is currently processing audio; stop playback before unloading CLAP plugins",
                track_name
            )))
            .await;
            return true;
        }
        if let Err(e) = track.unload_clap_plugin(plugin_id) {
            self.notify_clients(Err(e)).await;
            return true;
        }
        false
    }

    pub(crate) async fn handle_track_unload_clap_plugin_instance(
        &mut self,
        track_name: &str,
        instance_id: usize,
    ) -> bool {
        if self
            .reject_if_track_frozen(track_name, "CLAP plugin unloading")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let mut track = track.lock();
        if track.audio.processing() {
            self.notify_clients(Err(format!(
                "Track '{}' is currently processing audio; stop playback before unloading CLAP plugins",
                track_name
            )))
            .await;
            return true;
        }
        if let Err(e) = track.unload_clap_plugin_instance(instance_id) {
            self.notify_clients(Err(e)).await;
            return true;
        }
        false
    }

    pub(crate) async fn handle_track_load_vst3_plugin(
        &mut self,
        track_name: &str,
        plugin_id: &str,
        instance_id: Option<usize>,
    ) -> bool {
        if self
            .reject_if_track_frozen(track_name, "VST3 plugin loading")
            .await
        {
            return true;
        }
        let resolved_plugin_path = match self.resolve_plugin_identifier(PluginKind::Vst3, plugin_id)
        {
            Ok(path) => path,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let mut track = track.lock();
        if track.audio.processing() {
            self.notify_clients(Err(format!(
                "Track '{}' is currently processing audio; stop playback before loading VST3 plugins",
                track_name
            )))
            .await;
            return true;
        }
        if let Err(e) = track.load_vst3_plugin(plugin_id, &resolved_plugin_path, instance_id) {
            self.notify_clients(Err(e)).await;
            return true;
        }
        if let Some(instance) = track.vst3_plugins.last()
            && let Some(stderr) = instance.processor.take_stderr()
        {
            let source = format!("vst3:{resolved_plugin_path}");
            self.spawn_plugin_host_stderr_reader(stderr, source);
        }
        false
    }

    pub(crate) async fn handle_track_unload_vst3_plugin(
        &mut self,
        track_name: &str,
        plugin_id: &str,
    ) -> bool {
        if self
            .reject_if_track_frozen(track_name, "VST3 plugin unloading")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let mut track = track.lock();
        if track.audio.processing() {
            self.notify_clients(Err(format!(
                "Track '{}' is currently processing audio; stop playback before unloading VST3 plugins",
                track_name
            )))
            .await;
            return true;
        }
        if let Err(e) = track.unload_vst3_plugin(plugin_id) {
            self.notify_clients(Err(e)).await;
            return true;
        }
        false
    }

    pub(crate) async fn handle_track_unload_vst3_plugin_instance(
        &mut self,
        track_name: &str,
        instance_id: usize,
    ) -> bool {
        if self
            .reject_if_track_frozen(track_name, "VST3 plugin unloading")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let mut track = track.lock();
        if track.audio.processing() {
            self.notify_clients(Err(format!(
                "Track '{}' is currently processing audio; stop playback before unloading VST3 plugins",
                track_name
            )))
            .await;
            return true;
        }
        if let Err(e) = track.unload_vst3_plugin_instance(instance_id) {
            self.notify_clients(Err(e)).await;
            return true;
        }
        false
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) async fn handle_track_load_lv2_plugin(
        &mut self,
        track_name: &str,
        plugin_uri: &str,
        instance_id: Option<usize>,
    ) -> bool {
        if self
            .reject_if_track_frozen(track_name, "LV2 plugin loading")
            .await
        {
            return true;
        }
        let resolved_plugin_uri = match self.resolve_plugin_identifier(PluginKind::Lv2, plugin_uri)
        {
            Ok(uri) => uri,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let mut track = track.lock();
        if track.audio.processing() {
            self.notify_clients(Err(format!(
                "Track '{}' is currently processing audio; stop playback before loading LV2 plugins",
                track_name
            )))
            .await;
            return true;
        }
        if let Err(e) = track.load_lv2_plugin(&resolved_plugin_uri, instance_id) {
            self.notify_clients(Err(e)).await;
            return true;
        }
        if let Some(instance) = track.lv2_plugins.last()
            && let Some(stderr) = instance.processor.take_stderr()
        {
            let source = format!("lv2:{resolved_plugin_uri}");
            self.spawn_plugin_host_stderr_reader(stderr, source);
        }
        false
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) async fn handle_track_unload_lv2_plugin(
        &mut self,
        track_name: &str,
        plugin_uri: &str,
    ) -> bool {
        if self
            .reject_if_track_frozen(track_name, "LV2 plugin unloading")
            .await
        {
            return true;
        }
        let resolved_plugin_uri = match self.resolve_plugin_identifier(PluginKind::Lv2, plugin_uri)
        {
            Ok(uri) => uri,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let mut track = track.lock();
        if track.audio.processing() {
            self.notify_clients(Err(format!(
                "Track '{}' is currently processing audio; stop playback before unloading LV2 plugins",
                track_name
            )))
            .await;
            return true;
        }
        if let Err(e) = track.unload_lv2_plugin(&resolved_plugin_uri) {
            self.notify_clients(Err(e)).await;
            return true;
        }
        false
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) async fn handle_track_unload_lv2_plugin_instance(
        &mut self,
        track_name: &str,
        instance_id: usize,
    ) -> bool {
        tracing::info!(%track_name, instance_id, "Engine handling TrackUnloadLv2PluginInstance");
        if self
            .reject_if_track_frozen(track_name, "LV2 plugin unloading")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let mut track = track.lock();
        if track.audio.processing() {
            self.notify_clients(Err(format!(
                "Track '{}' is currently processing audio; stop playback before unloading LV2 plugins",
                track_name
            )))
            .await;
            return true;
        }
        if let Err(e) = track.unload_lv2_plugin_instance(instance_id) {
            self.notify_clients(Err(e)).await;
            return true;
        }
        tracing::info!(%track_name, instance_id, "Engine TrackUnloadLv2PluginInstance complete");
        false
    }

    pub(crate) async fn handle_track_get_plugin_graph(&mut self, action: Action) -> bool {
        let Action::TrackGetPluginGraph {
            ref track_name,
            include_state,
        } = action
        else {
            return false;
        };

        let start = std::time::Instant::now();
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let (plugins, connections, connectable_connections) = {
            let track = track.lock();
            let plugins_start = std::time::Instant::now();
            let plugins = track.plugin_graph_plugins(include_state);
            let plugins_ms = plugins_start.elapsed().as_millis();
            let conns_start = std::time::Instant::now();
            let connections = track.plugin_graph_connections();
            let conns_ms = conns_start.elapsed().as_millis();
            let cc_start = std::time::Instant::now();
            let connectable_connections = track.connectable_connections();
            let cc_ms = cc_start.elapsed().as_millis();
            tracing::debug!(
                %track_name,
                include_state,
                plugins_ms,
                conns_ms,
                cc_ms,
                "TrackGetPluginGraph collected graph data"
            );
            (plugins, connections, connectable_connections)
        };
        let total_ms = start.elapsed().as_millis();
        tracing::debug!(
            %track_name,
            total_ms,
            plugins = plugins.len(),
            connections = connections.len(),
            connectable = connectable_connections.len(),
            "TrackGetPluginGraph responding"
        );
        self.notify_clients(Ok(Action::TrackPluginGraph {
            track_name: track_name.clone(),
            plugins,
            connections,
            connectable_connections,
        }))
        .await;
        true
    }

    pub(crate) async fn handle_track_connect_plugin_audio(&mut self, a: Action) -> bool {
        let Action::TrackConnectPluginAudio {
            ref track_name,
            ref from_node,
            from_port,
            ref to_node,
            to_port,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "plugin routing changes")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) = track.lock().connect_plugin_audio(
            from_node.clone(),
            from_port,
            to_node.clone(),
            to_port,
        ) {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_track_connect_plugin_midi(&mut self, a: Action) -> bool {
        let Action::TrackConnectPluginMidi {
            ref track_name,
            ref from_node,
            from_port,
            ref to_node,
            to_port,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "plugin routing changes")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) =
            track
                .lock()
                .connect_plugin_midi(from_node.clone(), from_port, to_node.clone(), to_port)
        {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_track_disconnect_plugin_audio(&mut self, a: Action) -> bool {
        let Action::TrackDisconnectPluginAudio {
            ref track_name,
            ref from_node,
            from_port,
            ref to_node,
            to_port,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "plugin routing changes")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) = track.lock().disconnect_plugin_audio(
            from_node.clone(),
            from_port,
            to_node.clone(),
            to_port,
        ) {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_track_disconnect_plugin_midi(&mut self, a: Action) -> bool {
        let Action::TrackDisconnectPluginMidi {
            ref track_name,
            ref from_node,
            from_port,
            ref to_node,
            to_port,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "plugin routing changes")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) = track.lock().disconnect_plugin_midi(
            from_node.clone(),
            from_port,
            to_node.clone(),
            to_port,
        ) {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_track_set_plugin_resource_dir(&mut self, a: Action) -> bool {
        let Action::TrackSetPluginResourceDir {
            ref track_name,
            instance_id,
            ref format,
            ref directory,
        } = a
        else {
            return false;
        };

        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let dir = std::path::Path::new(directory);
        let result = if format.eq_ignore_ascii_case("CLAP") {
            track.lock().set_clap_plugin_resource_dir(instance_id, dir)
        } else if format.eq_ignore_ascii_case("LV2") {
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                track.lock().set_lv2_plugin_resource_dir(instance_id, dir)
            }
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            Err("LV2 is not supported on this platform".to_string())
        } else {
            Err(format!(
                "Unsupported plugin format for resource dir: {format}"
            ))
        };
        if let Err(e) = result {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_clip_set_plugin_resource_dir(&mut self, a: Action) -> bool {
        let Action::ClipSetPluginResourceDir {
            ref track_name,
            clip_idx,
            instance_id,
            ref format,
            ref directory,
        } = a
        else {
            return false;
        };

        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let dir = std::path::Path::new(directory);
        let mut track = track.lock();
        let result = if format.eq_ignore_ascii_case("CLAP") {
            track.clip_set_clap_plugin_resource_dir(clip_idx, instance_id, dir)
        } else if format.eq_ignore_ascii_case("LV2") {
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                track.clip_set_lv2_plugin_resource_dir(clip_idx, instance_id, dir)
            }
            #[cfg(not(all(unix, not(target_os = "macos"))))]
            Err("LV2 is not supported on this platform".to_string())
        } else {
            Err(format!(
                "Unsupported plugin format for resource dir: {format}"
            ))
        };
        if let Err(e) = result {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_clip_clap_file_references(&mut self, a: Action) -> bool {
        let Action::ClipClapFileReferences {
            ref track_name,
            clip_idx,
            instance_id,
            refs: _,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                let mut track = track.lock();
                let refs = track
                    .clip_clap_file_references(clip_idx, instance_id)
                    .unwrap_or_else(|e| {
                        tracing::warn!(
                            track_name = %track_name,
                            clip_idx,
                            instance_id,
                            error = %e,
                            "Failed to enumerate clip CLAP file references"
                        );
                        Vec::new()
                    });
                self.notify_clients(Ok(Action::ClipClapFileReferences {
                    track_name: track_name.clone(),
                    clip_idx,
                    instance_id,
                    refs,
                }))
                .await;
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        };
        false
    }

    pub(crate) async fn handle_track_set_clap_parameter(&mut self, a: Action) -> bool {
        let Action::TrackSetClapParameter {
            ref track_name,
            instance_id,
            param_id,
            value,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "CLAP parameter changes")
            .await
        {
            return true;
        }
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                if let Err(e) = track
                    .lock()
                    .set_clap_parameter(instance_id, param_id, value)
                {
                    self.notify_clients(Err(e)).await;
                    return true;
                }
                self.notify_clients(Ok(a.clone())).await;
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        }

        false
    }

    pub(crate) async fn handle_clip_set_clap_parameter(&mut self, a: Action) -> bool {
        let Action::ClipSetClapParameter {
            ref track_name,
            clip_idx,
            instance_id,
            param_id,
            value,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "CLAP parameter changes")
            .await
        {
            return true;
        }
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                if let Err(e) =
                    track
                        .lock()
                        .clip_set_clap_parameter(clip_idx, instance_id, param_id, value)
                {
                    self.notify_clients(Err(e)).await;
                    return true;
                }
                self.notify_clients(Ok(a.clone())).await;
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        }

        false
    }

    pub(crate) async fn handle_track_set_clap_parameter_at(&mut self, a: Action) -> bool {
        let Action::TrackSetClapParameterAt {
            ref track_name,
            instance_id,
            param_id,
            value,
            frame,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "CLAP parameter changes")
            .await
        {
            return true;
        }
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                if let Err(e) =
                    track
                        .lock()
                        .set_clap_parameter_at(instance_id, param_id, value, frame)
                {
                    self.notify_clients(Err(e)).await;
                    return true;
                }
                self.notify_clients(Ok(a.clone())).await;
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        }

        false
    }

    pub(crate) async fn handle_track_begin_clap_parameter_edit(&mut self, a: Action) -> bool {
        let Action::TrackBeginClapParameterEdit {
            ref track_name,
            instance_id,
            param_id,
            frame,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "CLAP parameter edit gestures")
            .await
        {
            return true;
        }
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                if let Err(e) = track
                    .lock()
                    .begin_clap_parameter_edit(instance_id, param_id, frame)
                {
                    self.notify_clients(Err(e)).await;
                    return true;
                }
                self.notify_clients(Ok(a.clone())).await;
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        }

        false
    }

    pub(crate) async fn handle_track_end_clap_parameter_edit(&mut self, a: Action) -> bool {
        let Action::TrackEndClapParameterEdit {
            ref track_name,
            instance_id,
            param_id,
            frame,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "CLAP parameter edit gestures")
            .await
        {
            return true;
        }
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                if let Err(e) = track
                    .lock()
                    .end_clap_parameter_edit(instance_id, param_id, frame)
                {
                    self.notify_clients(Err(e)).await;
                    return true;
                }
                self.notify_clients(Ok(a.clone())).await;
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        }

        false
    }

    pub(crate) async fn handle_track_clap_snapshot_state(&mut self, a: Action) -> bool {
        let Action::TrackClapSnapshotState {
            ref track_name,
            instance_id,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                let plugin_id = track
                    .lock()
                    .clap_plugins
                    .iter()
                    .find(|instance| instance.id == instance_id)
                    .map(|instance| instance.processor.plugin_id().to_string())
                    .unwrap_or_default();
                match track.lock().clap_snapshot_state(instance_id) {
                    Ok(state) => {
                        self.notify_clients(Ok(Action::TrackClapStateSnapshot {
                            track_name: track_name.clone(),
                            instance_id,
                            plugin_id,
                            state,
                        }))
                        .await;
                    }
                    Err(e) => {
                        self.notify_clients(Err(e)).await;
                    }
                }
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        };
        false
    }

    pub(crate) async fn handle_track_clap_restore_state(&mut self, a: Action) -> bool {
        let Action::TrackClapRestoreState {
            ref track_name,
            instance_id,
            ref state,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "CLAP state restore")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let track = track.lock();
        if track.audio.processing() {
            self.notify_clients(Err(format!(
                            "Track '{}' is currently processing audio; stop playback before restoring CLAP state",
                            track_name
                        )))
                        .await;
            return true;
        }
        if let Err(e) = track.clap_restore_state(instance_id, state) {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_clip_clap_restore_state(&mut self, a: Action) -> bool {
        let Action::ClipClapRestoreState {
            ref track_name,
            clip_idx,
            instance_id,
            ref state,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "CLAP state restore")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let mut track = track.lock();
        if track.audio.processing() {
            self.notify_clients(Err(format!(
                            "Track '{}' is currently processing audio; stop playback before restoring CLAP state",
                            track_name
                        )))
                        .await;
            return true;
        }
        if let Err(e) = track.clip_clap_restore_state(clip_idx, instance_id, state) {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_track_snapshot_all_clap_states(&mut self, a: Action) -> bool {
        let Action::TrackSnapshotAllClapStates { ref track_name } = a else {
            return false;
        };

        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let instances: Vec<_> = {
            let locked = track.lock();
            locked
                .clap_plugins
                .iter()
                .map(|i| (i.id, i.processor.plugin_id().to_string()))
                .collect()
        };
        for (instance_id, plugin_id) in instances {
            match track.lock().clap_snapshot_state(instance_id) {
                Ok(state) => {
                    self.notify_clients(Ok(Action::TrackClapStateSnapshot {
                        track_name: track_name.clone(),
                        instance_id,
                        plugin_id,
                        state,
                    }))
                    .await;
                }
                Err(_e) => {}
            }
        }
        self.notify_clients(Ok(Action::TrackSnapshotAllClapStatesDone {
            track_name: track_name.clone(),
        }))
        .await;

        false
    }

    pub(crate) async fn handle_track_set_vst3_parameter(&mut self, a: Action) -> bool {
        let Action::TrackSetVst3Parameter {
            ref track_name,
            instance_id,
            param_id,
            value,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "VST3 parameter changes")
            .await
        {
            return true;
        }
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                if let Err(e) = track
                    .lock()
                    .set_vst3_parameter(instance_id, param_id, value)
                {
                    self.notify_clients(Err(e)).await;
                    return true;
                }
                self.notify_clients(Ok(a.clone())).await;
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        }

        false
    }

    pub(crate) async fn handle_track_set_plugin_bypassed(&mut self, a: Action) -> bool {
        let Action::TrackSetPluginBypassed {
            ref track_name,
            instance_id,
            ref format,
            bypassed,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                let result = match format.as_str() {
                    "CLAP" => track.lock().set_clap_plugin_bypassed(instance_id, bypassed),
                    "VST3" => track.lock().set_vst3_plugin_bypassed(instance_id, bypassed),
                    #[cfg(all(unix, not(target_os = "macos")))]
                    "LV2" => track.lock().set_lv2_plugin_bypassed(instance_id, bypassed),
                    _ => Err(format!("Unknown plugin format for bypass: {format}")),
                };
                if let Err(e) = result {
                    self.notify_clients(Err(e)).await;
                    return true;
                }
                self.notify_clients(Ok(a.clone())).await;
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        };
        false
    }

    pub(crate) async fn handle_track_connect_vst3_audio(&mut self, a: Action) -> bool {
        let Action::TrackConnectVst3Audio {
            ref track_name,
            ref from_node,
            from_port,
            ref to_node,
            to_port,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "VST3 routing changes")
            .await
        {
            return true;
        }
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                if let Err(e) = track
                    .lock()
                    .connect_vst3_audio(from_node, from_port, to_node, to_port)
                {
                    self.notify_clients(Err(e)).await;
                    return true;
                }
                self.notify_clients(Ok(a.clone())).await;
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        }

        false
    }

    pub(crate) async fn handle_track_disconnect_vst3_audio(&mut self, a: Action) -> bool {
        let Action::TrackDisconnectVst3Audio {
            ref track_name,
            ref from_node,
            from_port,
            ref to_node,
            to_port,
        } = a
        else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "VST3 routing changes")
            .await
        {
            return true;
        }
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                if let Err(e) = track
                    .lock()
                    .disconnect_vst3_audio(from_node, from_port, to_node, to_port)
                {
                    self.notify_clients(Err(e)).await;
                    return true;
                }
                self.notify_clients(Ok(a.clone())).await;
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        }

        false
    }

    pub(crate) async fn handle_track_clear_plugins(&mut self, a: Action) -> bool {
        let Action::TrackClearPlugins { ref track_name } = a else {
            return false;
        };

        if self
            .reject_if_track_frozen(track_name, "plugin graph editing")
            .await
        {
            return true;
        }
        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        track.lock().clear_plugins();
        self.notify_clients(Ok(Action::Log {
            source: "engine".to_string(),
            message: format!("Cleared plugins from track '{track_name}'"),
        }))
        .await;

        false
    }

    pub(crate) async fn handle_track_clap_file_references(&mut self, a: Action) -> bool {
        let Action::TrackClapFileReferences {
            ref track_name,
            instance_id,
            refs: _,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                let refs = track.lock().clap_file_references(instance_id).unwrap_or_else(|e| {
                            tracing::warn!(track_name = %track_name, instance_id, error = %e, "Failed to enumerate CLAP file references");
                            Vec::new()
                        });
                self.notify_clients(Ok(Action::TrackClapFileReferences {
                    track_name: track_name.clone(),
                    instance_id,
                    refs,
                }))
                .await;
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        };
        false
    }

    pub(crate) async fn handle_track_update_clap_file_reference(&mut self, a: Action) -> bool {
        let Action::TrackUpdateClapFileReference {
            ref track_name,
            instance_id,
            index,
            ref path,
        } = a
        else {
            return false;
        };

        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) = track
            .lock()
            .update_clap_file_reference(instance_id, index, path)
        {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_clip_update_clap_file_reference(&mut self, a: Action) -> bool {
        let Action::ClipUpdateClapFileReference {
            ref track_name,
            clip_idx,
            instance_id,
            index,
            ref path,
        } = a
        else {
            return false;
        };

        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) =
            track
                .lock()
                .clip_update_clap_file_reference(clip_idx, instance_id, index, path)
        {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_track_get_clap_parameters(&mut self, a: Action) -> bool {
        let Action::TrackGetClapParameters {
            ref track_name,
            instance_id,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => match track.lock().get_clap_parameters(instance_id) {
                Ok(parameters) => {
                    self.notify_clients(Ok(Action::TrackClapParameters {
                        track_name: track_name.clone(),
                        instance_id,
                        parameters,
                    }))
                    .await;
                }
                Err(e) => {
                    self.notify_clients(Err(e)).await;
                }
            },
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        };
        false
    }

    pub(crate) async fn handle_clip_clap_snapshot_state(&mut self, a: Action) -> bool {
        let Action::ClipClapSnapshotState {
            ref track_name,
            clip_idx,
            instance_id,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => match track.lock().clip_clap_snapshot_state(clip_idx, instance_id) {
                Ok((plugin_id, state)) => {
                    self.notify_clients(Ok(Action::ClipClapStateSnapshot {
                        track_name: track_name.clone(),
                        clip_idx,
                        instance_id,
                        plugin_id,
                        state,
                    }))
                    .await;
                }
                Err(e) => {
                    self.notify_clients(Err(e)).await;
                }
            },
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        };
        false
    }

    pub(crate) async fn handle_track_get_vst3_graph(&mut self, a: Action) -> bool {
        let Action::TrackGetVst3Graph { ref track_name } = a else {
            return false;
        };

        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                let t = track.lock();
                let plugins = t.vst3_graph_plugins();
                let connections = t.vst3_graph_connections();
                self.notify_clients(Ok(Action::TrackVst3Graph {
                    track_name: track_name.clone(),
                    plugins,
                    connections,
                }))
                .await;
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        }

        false
    }

    pub(crate) async fn handle_track_get_vst3_parameters(&mut self, a: Action) -> bool {
        let Action::TrackGetVst3Parameters {
            ref track_name,
            instance_id,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => match track.lock().get_vst3_parameters(instance_id) {
                Ok(parameters) => {
                    self.notify_clients(Ok(Action::TrackVst3Parameters {
                        track_name: track_name.clone(),
                        instance_id,
                        parameters,
                    }))
                    .await;
                }
                Err(e) => {
                    self.notify_clients(Err(e)).await;
                }
            },
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        };
        false
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) async fn handle_track_get_lv2_plugin_controls(&mut self, a: Action) -> bool {
        let Action::TrackGetLv2PluginControls {
            ref track_name,
            instance_id,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => match track.lock().get_lv2_plugin_controls(instance_id) {
                Ok(controls) => {
                    self.notify_clients(Ok(Action::TrackLv2PluginControls {
                        track_name: track_name.clone(),
                        instance_id,
                        controls,
                        instance_access_handle: None,
                    }))
                    .await;
                }
                Err(e) => {
                    self.notify_clients(Err(e)).await;
                }
            },
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        };
        false
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) async fn handle_track_lv2_snapshot_state(&mut self, a: Action) -> bool {
        let Action::TrackLv2SnapshotState {
            ref track_name,
            instance_id,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => match track.lock().lv2_snapshot_state(instance_id) {
                Ok(state) => {
                    self.notify_clients(Ok(Action::TrackLv2StateSnapshot {
                        track_name: track_name.clone(),
                        instance_id,
                        state,
                    }))
                    .await;
                }
                Err(e) => {
                    self.notify_clients(Err(e)).await;
                }
            },
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        };
        false
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) async fn handle_clip_lv2_snapshot_state(&mut self, a: Action) -> bool {
        let Action::ClipLv2SnapshotState {
            ref track_name,
            clip_idx,
            instance_id,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => match track.lock().clip_lv2_snapshot_state(clip_idx, instance_id) {
                Ok(state) => {
                    self.notify_clients(Ok(Action::ClipLv2StateSnapshot {
                        track_name: track_name.clone(),
                        clip_idx,
                        instance_id,
                        state,
                    }))
                    .await;
                }
                Err(e) => {
                    self.notify_clients(Err(e)).await;
                }
            },
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        };
        false
    }

    pub(crate) async fn handle_track_vst3_snapshot_state(&mut self, a: Action) -> bool {
        let Action::TrackVst3SnapshotState {
            ref track_name,
            instance_id,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => match track.lock().vst3_snapshot_state(instance_id) {
                Ok(state) => {
                    self.notify_clients(Ok(Action::TrackVst3StateSnapshot {
                        track_name: track_name.clone(),
                        instance_id,
                        state,
                    }))
                    .await;
                }
                Err(e) => {
                    self.notify_clients(Err(e)).await;
                }
            },
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        };
        false
    }

    pub(crate) async fn handle_clip_vst3_snapshot_state(&mut self, a: Action) -> bool {
        let Action::ClipVst3SnapshotState {
            ref track_name,
            clip_idx,
            instance_id,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => match track.lock().clip_vst3_snapshot_state(clip_idx, instance_id) {
                Ok(state) => {
                    self.notify_clients(Ok(Action::ClipVst3StateSnapshot {
                        track_name: track_name.clone(),
                        clip_idx,
                        instance_id,
                        state,
                    }))
                    .await;
                }
                Err(e) => {
                    self.notify_clients(Err(e)).await;
                }
            },
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        };
        false
    }

    pub(crate) async fn handle_track_get_clap_note_names(&mut self, a: Action) -> bool {
        let Action::TrackGetClapNoteNames { ref track_name } = a else {
            return false;
        };

        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let note_names = track.lock().get_clap_note_names();
        self.notify_clients(Ok(Action::TrackClapNoteNames {
            track_name: track_name.clone(),
            note_names,
        }))
        .await;

        false
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) async fn handle_track_get_lv2_midnam(&mut self, a: Action) -> bool {
        let Action::TrackGetLv2Midnam { ref track_name } = a else {
            return false;
        };

        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        let note_names = track.lock().get_lv2_midnam();
        self.notify_clients(Ok(Action::TrackLv2Midnam {
            track_name: track_name.clone(),
            note_names,
        }))
        .await;

        false
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) async fn handle_list_lv2_plugins(&mut self, a: Action) -> bool {
        let Action::ListLv2Plugins = a else {
            return false;
        };

        match crate::plugins::scan_plugins::<crate::plugins::types::Lv2PluginInfo>("lv2") {
            Ok(plugins) => {
                self.notify_clients(Ok(Action::Lv2Plugins(plugins))).await;
            }
            Err(e) => {
                tracing::error!("LV2 plugin scan failed: {e}");
                self.notify_clients(Ok(Action::Lv2PluginsUnavailable { error: e }))
                    .await;
            }
        }
        true
    }

    pub(crate) async fn handle_list_vst3_plugins(&mut self, a: Action) -> bool {
        let Action::ListVst3Plugins = a else {
            return false;
        };

        match crate::plugins::scan_plugins::<crate::plugins::types::Vst3PluginInfo>("vst3") {
            Ok(plugins) => {
                self.notify_clients(Ok(Action::Vst3Plugins(plugins))).await;
            }
            Err(e) => {
                tracing::error!("VST3 plugin scan failed: {e}");
                self.notify_clients(Ok(Action::Vst3PluginsUnavailable { error: e }))
                    .await;
            }
        }
        true
    }

    pub(crate) async fn handle_list_clap_plugins(&mut self, a: Action) -> bool {
        let Action::ListClapPlugins = a else {
            return false;
        };

        match crate::plugins::scan_plugins::<crate::plugins::types::ClapPluginInfo>("clap") {
            Ok(plugins) => {
                self.notify_clients(Ok(Action::ClapPlugins(plugins))).await;
            }
            Err(e) => {
                tracing::error!("CLAP plugin scan failed: {e}");
                self.notify_clients(Ok(Action::ClapPluginsUnavailable { error: e }))
                    .await;
            }
        }
        true
    }

    pub(crate) async fn handle_list_clap_plugins_with_capabilities(&mut self, a: Action) -> bool {
        let Action::ListClapPluginsWithCapabilities = a else {
            return false;
        };

        match crate::plugins::scan_plugins::<crate::plugins::types::ClapPluginInfo>("clap") {
            Ok(plugins) => {
                self.notify_clients(Ok(Action::ClapPlugins(plugins))).await;
            }
            Err(e) => {
                tracing::error!("CLAP plugin scan failed: {e}");
                self.notify_clients(Ok(Action::ClapPluginsUnavailable { error: e }))
                    .await;
            }
        }
        true
    }

    pub(crate) async fn handle_track_show_clap_gui(&mut self, a: Action) -> bool {
        let Action::TrackShowClapGui {
            ref track_name,
            instance_id,
        } = a
        else {
            return false;
        };

        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) = track.lock().show_clap_gui(instance_id) {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_track_show_vst3_gui(&mut self, a: Action) -> bool {
        let Action::TrackShowVst3Gui {
            ref track_name,
            instance_id,
        } = a
        else {
            return false;
        };

        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) = track.lock().show_vst3_gui(instance_id) {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) async fn handle_track_show_lv2_gui(&mut self, a: Action) -> bool {
        let Action::TrackShowLv2Gui {
            ref track_name,
            instance_id,
        } = a
        else {
            return false;
        };

        let track = match self.track_handle_or_err(track_name) {
            Ok(track) => track,
            Err(e) => {
                self.notify_clients(Err(e)).await;
                return true;
            }
        };
        if let Err(e) = track.lock().show_lv2_gui(instance_id) {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_track_vst3_restore_state(&mut self, a: Action) -> bool {
        let Action::TrackVst3RestoreState {
            ref track_name,
            instance_id,
            ref state,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                if let Err(e) = track.lock().vst3_restore_state(instance_id, state) {
                    self.notify_clients(Err(e)).await;
                    return true;
                }
                self.notify_clients(Ok(a.clone())).await;
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        };
        false
    }
}
