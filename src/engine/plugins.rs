use super::*;
#[cfg(target_os = "openbsd")]
use crate::hw::sndio::{HwDriver, HwOptions, MidiHub};
use crate::message::{Action, PluginKind};
#[cfg(target_os = "openbsd")]
use crate::workers::sndio_worker::HwWorker;
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
            #[cfg(unix)]
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
        self.notify_event(Event::Log {
            source: "engine".to_string(),
            message: format!("CLAP plugin loaded on track '{track_name}': {resolved_plugin_path}"),
        })
        .await;
        if let Some(instance) = track.clap_plugins.last()
            && let Some(stderr) = instance.processor.take_stderr()
        {
            let source = format!("clap:{resolved_plugin_path}");
            self.spawn_plugin_host_stderr_reader(stderr, source);
            self.notify_event(Event::Log {
                source: "engine".to_string(),
                message: format!("Attached stderr reader for CLAP plugin on track '{track_name}'"),
            })
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
    pub(crate) async fn handle_track_unload_lv2_plugin_instance(
        &mut self,
        track_name: &str,
        instance_id: usize,
    ) -> bool {
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
            track.refresh_clap_audio_ports_from_hosts();
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
        self.notify_query_reply(QueryReply::TrackPluginGraph {
            track_name: track_name.clone(),
            plugins,
            connections,
            connectable_connections,
        })
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
            shared,
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
            track
                .lock()
                .set_clap_plugin_resource_dir(instance_id, dir, shared)
        } else if format.eq_ignore_ascii_case("LV2") {
            #[cfg(unix)]
            {
                track
                    .lock()
                    .set_lv2_plugin_resource_dir(instance_id, dir, shared)
            }
            #[cfg(not(unix))]
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
            shared,
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
            track.clip_set_clap_plugin_resource_dir(clip_idx, instance_id, dir, shared)
        } else if format.eq_ignore_ascii_case("LV2") {
            #[cfg(unix)]
            {
                track.clip_set_lv2_plugin_resource_dir(clip_idx, instance_id, dir, shared)
            }
            #[cfg(not(unix))]
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

    pub(crate) async fn handle_clip_clap_collect_resources(&mut self, a: Action) -> bool {
        let Action::ClipClapCollectResources {
            ref track_name,
            clip_idx,
            instance_id,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                let mut track = track.lock();
                let files = track
                    .clip_clap_collect_resources(clip_idx, instance_id)
                    .unwrap_or_else(|e| {
                        tracing::warn!(
                            track_name = %track_name,
                            clip_idx,
                            instance_id,
                            error = %e,
                            "Failed to collect clip CLAP resources"
                        );
                        Vec::new()
                    });
                self.notify_event(Event::ClipClapResourceFiles {
                    track_name: track_name.clone(),
                    clip_idx,
                    instance_id,
                    files,
                })
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

    pub(crate) async fn handle_clip_get_clap_parameters(&mut self, a: Action) -> bool {
        let Action::ClipGetClapParameters {
            ref track_name,
            clip_idx,
            instance_id,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => match track.lock().clip_get_clap_parameters(clip_idx, instance_id) {
                Ok(parameters) => {
                    self.notify_query_reply(QueryReply::ClipClapParameters {
                        track_name: track_name.clone(),
                        clip_idx,
                        instance_id,
                        parameters,
                    })
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
                        self.notify_event(Event::TrackClapStateSnapshot {
                            track_name: track_name.clone(),
                            instance_id,
                            plugin_id,
                            state: Box::new(state),
                        })
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
                    self.notify_event(Event::TrackClapStateSnapshot {
                        track_name: track_name.clone(),
                        instance_id,
                        plugin_id,
                        state: Box::new(state),
                    })
                    .await;
                }
                Err(_e) => {}
            }
        }
        self.notify_event(Event::TrackSnapshotAllClapStatesDone {
            track_name: track_name.clone(),
        })
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
                    #[cfg(unix)]
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
        self.notify_event(Event::Log {
            source: "engine".to_string(),
            message: format!("Cleared plugins from track '{track_name}'"),
        })
        .await;

        false
    }

    pub(crate) async fn handle_track_clap_collect_resources(&mut self, a: Action) -> bool {
        let Action::TrackClapCollectResources {
            ref track_name,
            instance_id,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                let files = track.lock().clap_collect_resources(instance_id).unwrap_or_else(|e| {
                            tracing::warn!(track_name = %track_name, instance_id, error = %e, "Failed to collect CLAP resources");
                            Vec::new()
                        });
                self.notify_event(Event::TrackClapResourceFiles {
                    track_name: track_name.clone(),
                    instance_id,
                    files,
                })
                .await;
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        };
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
                    self.notify_query_reply(QueryReply::TrackClapParameters {
                        track_name: track_name.clone(),
                        instance_id,
                        parameters,
                    })
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
                    self.notify_event(Event::ClipClapStateSnapshot {
                        track_name: track_name.clone(),
                        clip_idx,
                        instance_id,
                        plugin_id,
                        state: Box::new(state),
                    })
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
                self.notify_query_reply(QueryReply::TrackVst3Graph {
                    track_name: track_name.clone(),
                    plugins,
                    connections,
                })
                .await;
            }
            Err(e) => {
                self.notify_clients(Err(e)).await;
            }
        }

        false
    }

    pub(crate) async fn handle_clip_set_vst3_parameter(&mut self, a: Action) -> bool {
        let Action::ClipSetVst3Parameter {
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
            .reject_if_track_frozen(track_name, "VST3 parameter changes")
            .await
        {
            return true;
        }
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                if let Err(e) =
                    track
                        .lock()
                        .clip_set_vst3_parameter(clip_idx, instance_id, param_id, value)
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
                    self.notify_query_reply(QueryReply::TrackVst3Parameters {
                        track_name: track_name.clone(),
                        instance_id,
                        parameters,
                    })
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

    pub(crate) async fn handle_clip_get_vst3_parameters(&mut self, a: Action) -> bool {
        let Action::ClipGetVst3Parameters {
            ref track_name,
            clip_idx,
            instance_id,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => match track.lock().clip_get_vst3_parameters(clip_idx, instance_id) {
                Ok(parameters) => {
                    self.notify_query_reply(QueryReply::ClipVst3Parameters {
                        track_name: track_name.clone(),
                        clip_idx,
                        instance_id,
                        parameters,
                    })
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

    #[cfg(unix)]
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
                    self.notify_query_reply(QueryReply::TrackLv2PluginControls {
                        track_name: track_name.clone(),
                        instance_id,
                        controls,
                        instance_access_handle: None,
                    })
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

    #[cfg(unix)]
    pub(crate) async fn handle_clip_get_lv2_plugin_controls(&mut self, a: Action) -> bool {
        let Action::ClipGetLv2PluginControls {
            ref track_name,
            clip_idx,
            instance_id,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => match track
                .lock()
                .clip_get_lv2_plugin_controls(clip_idx, instance_id)
            {
                Ok(controls) => {
                    self.notify_query_reply(QueryReply::ClipLv2PluginControls {
                        track_name: track_name.clone(),
                        clip_idx,
                        instance_id,
                        controls,
                        instance_access_handle: None,
                    })
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

    #[cfg(unix)]
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
                    self.notify_event(Event::TrackLv2StateSnapshot {
                        track_name: track_name.clone(),
                        instance_id,
                        state,
                    })
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

    #[cfg(unix)]
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
                    self.notify_event(Event::ClipLv2StateSnapshot {
                        track_name: track_name.clone(),
                        clip_idx,
                        instance_id,
                        state,
                    })
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
                    self.notify_event(Event::TrackVst3StateSnapshot {
                        track_name: track_name.clone(),
                        instance_id,
                        state: Box::new(state),
                    })
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
                    self.notify_event(Event::ClipVst3StateSnapshot {
                        track_name: track_name.clone(),
                        clip_idx,
                        instance_id,
                        state: Box::new(state),
                    })
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
        self.notify_query_reply(QueryReply::TrackClapNoteNames {
            track_name: track_name.clone(),
            note_names,
        })
        .await;

        false
    }

    #[cfg(unix)]
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
        self.notify_query_reply(QueryReply::TrackLv2Midnam {
            track_name: track_name.clone(),
            note_names,
        })
        .await;

        false
    }

    #[cfg(unix)]
    pub(crate) async fn handle_list_lv2_plugins(&mut self, a: Action) -> bool {
        let Action::ListLv2Plugins = a else {
            return false;
        };

        match crate::plugins::scan_plugins::<crate::plugins::types::Lv2PluginInfo>("lv2") {
            Ok(plugins) => {
                self.notify_query_reply(QueryReply::Lv2Plugins(plugins))
                    .await;
            }
            Err(e) => {
                tracing::error!("LV2 plugin scan failed: {e}");
                self.notify_query_reply(QueryReply::Lv2PluginsUnavailable { error: e })
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
                self.notify_query_reply(QueryReply::Vst3Plugins(plugins))
                    .await;
            }
            Err(e) => {
                tracing::error!("VST3 plugin scan failed: {e}");
                self.notify_query_reply(QueryReply::Vst3PluginsUnavailable { error: e })
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
                self.notify_query_reply(QueryReply::ClapPlugins(plugins))
                    .await;
            }
            Err(e) => {
                tracing::error!("CLAP plugin scan failed: {e}");
                self.notify_query_reply(QueryReply::ClapPluginsUnavailable { error: e })
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
                self.notify_query_reply(QueryReply::ClapPlugins(plugins))
                    .await;
            }
            Err(e) => {
                tracing::error!("CLAP plugin scan failed: {e}");
                self.notify_query_reply(QueryReply::ClapPluginsUnavailable { error: e })
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
        self.notify_clients(Ok(a.clone())).await;

        false
    }

    pub(crate) async fn handle_clip_show_clap_gui(&mut self, a: Action) -> bool {
        let Action::ClipShowClapGui {
            ref track_name,
            clip_idx,
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
        if let Err(e) = track.lock().clip_show_clap_gui(clip_idx, instance_id) {
            self.notify_clients(Err(e)).await;
            return true;
        }

        false
    }

    pub(crate) async fn handle_clip_show_vst3_gui(&mut self, a: Action) -> bool {
        let Action::ClipShowVst3Gui {
            ref track_name,
            clip_idx,
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
        if let Err(e) = track.lock().clip_show_vst3_gui(clip_idx, instance_id) {
            self.notify_clients(Err(e)).await;
            return true;
        }
        self.notify_clients(Ok(a.clone())).await;

        false
    }

    #[cfg(unix)]
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
        self.notify_clients(Ok(a.clone())).await;

        false
    }

    #[cfg(unix)]
    pub(crate) async fn handle_clip_show_lv2_gui(&mut self, a: Action) -> bool {
        let Action::ClipShowLv2Gui {
            ref track_name,
            clip_idx,
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
        if let Err(e) = track.lock().clip_show_lv2_gui(clip_idx, instance_id) {
            self.notify_clients(Err(e)).await;
            return true;
        }
        self.notify_clients(Ok(a.clone())).await;

        false
    }

    #[cfg(unix)]
    pub(crate) async fn handle_track_set_lv2_plugin_state(&mut self, a: Action) -> bool {
        let Action::TrackSetLv2PluginState {
            ref track_name,
            instance_id,
            ref state,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                if let Err(e) = track.lock().lv2_restore_state(instance_id, state) {
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

    #[cfg(unix)]
    pub(crate) async fn handle_track_set_lv2_control_value(&mut self, a: Action) -> bool {
        let Action::TrackSetLv2ControlValue {
            ref track_name,
            instance_id,
            index,
            value,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                if let Err(e) = track.lock().set_lv2_control_value(
                    instance_id,
                    index as usize,
                    f64::from(value),
                ) {
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

    #[cfg(unix)]
    pub(crate) async fn handle_clip_set_lv2_control_value(&mut self, a: Action) -> bool {
        let Action::ClipSetLv2ControlValue {
            ref track_name,
            clip_idx,
            instance_id,
            index,
            value,
        } = a
        else {
            return false;
        };
        match self.track_handle_or_err(track_name) {
            Ok(track) => {
                if let Err(e) = track.lock().clip_set_lv2_control_value(
                    clip_idx,
                    instance_id,
                    index as usize,
                    f64::from(value),
                ) {
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

impl Engine {
    pub(crate) async fn handle_plugin_request(&mut self, a: Action) -> bool {
        match a {
            Action::TrackClearPlugins { .. } => {
                if Self::box_bool(self.handle_track_clear_plugins(a.clone())).await {
                    return true;
                }
            }
            #[cfg(unix)]
            Action::TrackSetLv2PluginState { .. } => {
                if Self::box_bool(self.handle_track_set_lv2_plugin_state(a.clone())).await {
                    return true;
                }
            }
            #[cfg(unix)]
            Action::ClipSetLv2PluginState { ref track_name, .. } => {
                self.notify_clients(Err(format!(
                    "Track '{}': clip LV2 plugin state changes are not supported",
                    track_name
                )))
                .await;
            }
            Action::TrackGetClapNoteNames { .. } => {
                if Self::box_bool(self.handle_track_get_clap_note_names(a.clone())).await {
                    return true;
                }
            }
            #[cfg(unix)]
            Action::TrackGetLv2Midnam { .. } => {
                if Self::box_bool(self.handle_track_get_lv2_midnam(a.clone())).await {
                    return true;
                }
            }
            Action::TrackGetPluginGraph { .. } => {
                if Self::box_bool(self.handle_track_get_plugin_graph(a.clone())).await {
                    return true;
                }
            }
            Action::TrackConnectPluginAudio { .. } => {
                if Self::box_bool(self.handle_track_connect_plugin_audio(a.clone())).await {
                    return true;
                }
            }
            Action::TrackConnectPluginMidi { .. } => {
                if Self::box_bool(self.handle_track_connect_plugin_midi(a.clone())).await {
                    return true;
                }
            }
            Action::TrackDisconnectPluginAudio { .. } => {
                if Self::box_bool(self.handle_track_disconnect_plugin_audio(a.clone())).await {
                    return true;
                }
            }
            Action::TrackDisconnectPluginMidi { .. } => {
                if Self::box_bool(self.handle_track_disconnect_plugin_midi(a.clone())).await {
                    return true;
                }
            }
            Action::TrackConnectAudio { .. } => {
                if Self::box_bool(self.handle_track_connect_audio(a.clone())).await {
                    return true;
                }
            }
            Action::TrackDisconnectAudio { .. } => {
                if Self::box_bool(self.handle_track_disconnect_audio(a.clone())).await {
                    return true;
                }
            }
            Action::TrackConnectMidi { .. } => {
                if Self::box_bool(self.handle_track_connect_midi(a.clone())).await {
                    return true;
                }
            }
            Action::TrackDisconnectMidi { .. } => {
                if Self::box_bool(self.handle_track_disconnect_midi(a.clone())).await {
                    return true;
                }
            }
            #[cfg(unix)]
            Action::ListLv2Plugins => {
                if Self::box_bool(self.handle_list_lv2_plugins(a.clone())).await {
                    return true;
                }
            }
            Action::ListVst3Plugins => {
                if Self::box_bool(self.handle_list_vst3_plugins(a.clone())).await {
                    return true;
                }
            }
            Action::ListClapPlugins => {
                if Self::box_bool(self.handle_list_clap_plugins(a.clone())).await {
                    return true;
                }
            }
            Action::ListClapPluginsWithCapabilities => {
                if self
                    .handle_list_clap_plugins_with_capabilities(a.clone())
                    .await
                {
                    return true;
                }
            }
            Action::TrackLoadClapPlugin {
                ref track_name,
                ref plugin_id,
                instance_id,
            } => {
                if self
                    .handle_track_load_clap_plugin(
                        track_name.as_str(),
                        plugin_id.as_str(),
                        instance_id,
                    )
                    .await
                {
                    return true;
                }
            }
            Action::TrackUnloadClapPlugin {
                ref track_name,
                ref plugin_id,
            } => {
                if self
                    .handle_track_unload_clap_plugin(track_name.as_str(), plugin_id.as_str())
                    .await
                {
                    return true;
                }
            }
            Action::TrackUnloadClapPluginInstance {
                ref track_name,
                instance_id,
            } => {
                if self
                    .handle_track_unload_clap_plugin_instance(track_name.as_str(), instance_id)
                    .await
                {
                    return true;
                }
            }
            Action::TrackShowClapGui { .. } => {
                if Self::box_bool(self.handle_track_show_clap_gui(a.clone())).await {
                    return true;
                }
            }
            Action::ClipShowClapGui { .. } => {
                if Self::box_bool(self.handle_clip_show_clap_gui(a.clone())).await {
                    return true;
                }
            }
            Action::TrackLoadVst3Plugin {
                ref track_name,
                ref plugin_id,
                instance_id,
            } => {
                if self
                    .handle_track_load_vst3_plugin(
                        track_name.as_str(),
                        plugin_id.as_str(),
                        instance_id,
                    )
                    .await
                {
                    return true;
                }
            }
            Action::TrackUnloadVst3Plugin {
                ref track_name,
                ref plugin_id,
            } => {
                if self
                    .handle_track_unload_vst3_plugin(track_name.as_str(), plugin_id.as_str())
                    .await
                {
                    return true;
                }
            }
            Action::TrackUnloadVst3PluginInstance {
                ref track_name,
                instance_id,
            } => {
                if self
                    .handle_track_unload_vst3_plugin_instance(track_name.as_str(), instance_id)
                    .await
                {
                    return true;
                }
            }
            Action::TrackShowVst3Gui { .. } => {
                if Self::box_bool(self.handle_track_show_vst3_gui(a.clone())).await {
                    return true;
                }
            }
            Action::ClipShowVst3Gui { .. } => {
                if Self::box_bool(self.handle_clip_show_vst3_gui(a.clone())).await {
                    return true;
                }
            }
            #[cfg(unix)]
            Action::TrackLoadLv2Plugin {
                ref track_name,
                ref plugin_uri,
                instance_id,
            } => {
                if self
                    .handle_track_load_lv2_plugin(
                        track_name.as_str(),
                        plugin_uri.as_str(),
                        instance_id,
                    )
                    .await
                {
                    return true;
                }
            }
            #[cfg(unix)]
            Action::TrackUnloadLv2Plugin {
                ref track_name,
                ref plugin_uri,
            } => {
                if self
                    .handle_track_unload_lv2_plugin(track_name.as_str(), plugin_uri.as_str())
                    .await
                {
                    return true;
                }
            }
            #[cfg(unix)]
            Action::TrackUnloadLv2PluginInstance {
                ref track_name,
                instance_id,
            } => {
                if self
                    .handle_track_unload_lv2_plugin_instance(track_name.as_str(), instance_id)
                    .await
                {
                    return true;
                }
            }
            #[cfg(unix)]
            Action::TrackShowLv2Gui { .. } => {
                if Self::box_bool(self.handle_track_show_lv2_gui(a.clone())).await {
                    return true;
                }
            }
            #[cfg(unix)]
            Action::ClipShowLv2Gui { .. } => {
                if Self::box_bool(self.handle_clip_show_lv2_gui(a.clone())).await {
                    return true;
                }
            }
            Action::TrackSetPluginResourceDir { .. } => {
                if Self::box_bool(self.handle_track_set_plugin_resource_dir(a.clone())).await {
                    return true;
                }
            }
            Action::TrackClapCollectResources { .. } => {
                if Self::box_bool(self.handle_track_clap_collect_resources(a.clone())).await {
                    return true;
                }
            }
            Action::ClipSetPluginResourceDir { .. } => {
                if Self::box_bool(self.handle_clip_set_plugin_resource_dir(a.clone())).await {
                    return true;
                }
            }
            Action::ClipClapCollectResources { .. } => {
                if Self::box_bool(self.handle_clip_clap_collect_resources(a.clone())).await {
                    return true;
                }
            }
            Action::TrackSetClapParameter { .. } => {
                if Self::box_bool(self.handle_track_set_clap_parameter(a.clone())).await {
                    return true;
                }
            }
            Action::ClipSetClapParameter { .. } => {
                if Self::box_bool(self.handle_clip_set_clap_parameter(a.clone())).await {
                    return true;
                }
            }
            Action::ClipGetClapParameters { .. } => {
                if Self::box_bool(self.handle_clip_get_clap_parameters(a.clone())).await {
                    return true;
                }
            }
            Action::TrackSetClapParameterAt { .. } => {
                if Self::box_bool(self.handle_track_set_clap_parameter_at(a.clone())).await {
                    return true;
                }
            }
            Action::TrackBeginClapParameterEdit { .. } => {
                if Self::box_bool(self.handle_track_begin_clap_parameter_edit(a.clone())).await {
                    return true;
                }
            }
            Action::TrackEndClapParameterEdit { .. } => {
                if Self::box_bool(self.handle_track_end_clap_parameter_edit(a.clone())).await {
                    return true;
                }
            }
            Action::TrackGetClapParameters { .. } => {
                if Self::box_bool(self.handle_track_get_clap_parameters(a.clone())).await {
                    return true;
                }
            }
            Action::TrackClapSnapshotState { .. } => {
                if Self::box_bool(self.handle_track_clap_snapshot_state(a.clone())).await {
                    return true;
                }
            }
            Action::ClipClapSnapshotState { .. } => {
                if Self::box_bool(self.handle_clip_clap_snapshot_state(a.clone())).await {
                    return true;
                }
            }
            Action::TrackClapRestoreState { .. } => {
                if Self::box_bool(self.handle_track_clap_restore_state(a.clone())).await {
                    return true;
                }
            }
            Action::ClipClapRestoreState { .. } => {
                if Self::box_bool(self.handle_clip_clap_restore_state(a.clone())).await {
                    return true;
                }
            }
            Action::TrackSnapshotAllClapStates { .. } => {
                if Self::box_bool(self.handle_track_snapshot_all_clap_states(a.clone())).await {
                    return true;
                }
            }
            Action::TrackGetVst3Graph { .. } => {
                if Self::box_bool(self.handle_track_get_vst3_graph(a.clone())).await {
                    return true;
                }
            }
            Action::TrackSetVst3Parameter { .. } => {
                if Self::box_bool(self.handle_track_set_vst3_parameter(a.clone())).await {
                    return true;
                }
            }
            Action::ClipSetVst3Parameter { .. } => {
                if Self::box_bool(self.handle_clip_set_vst3_parameter(a.clone())).await {
                    return true;
                }
            }
            Action::TrackSetPluginBypassed { .. } => {
                if Self::box_bool(self.handle_track_set_plugin_bypassed(a.clone())).await {
                    return true;
                }
            }
            Action::TrackGetVst3Parameters { .. } => {
                if Self::box_bool(self.handle_track_get_vst3_parameters(a.clone())).await {
                    return true;
                }
            }
            Action::ClipGetVst3Parameters { .. } => {
                if Self::box_bool(self.handle_clip_get_vst3_parameters(a.clone())).await {
                    return true;
                }
            }
            #[cfg(unix)]
            Action::TrackSetLv2ControlValue { .. } => {
                if Self::box_bool(self.handle_track_set_lv2_control_value(a.clone())).await {
                    return true;
                }
            }
            #[cfg(unix)]
            Action::ClipSetLv2ControlValue { .. } => {
                if Self::box_bool(self.handle_clip_set_lv2_control_value(a.clone())).await {
                    return true;
                }
            }
            #[cfg(unix)]
            Action::TrackGetLv2PluginControls { .. } => {
                if Self::box_bool(self.handle_track_get_lv2_plugin_controls(a.clone())).await {
                    return true;
                }
            }
            #[cfg(unix)]
            Action::ClipGetLv2PluginControls { .. } => {
                if Self::box_bool(self.handle_clip_get_lv2_plugin_controls(a.clone())).await {
                    return true;
                }
            }
            #[cfg(unix)]
            Action::TrackLv2SnapshotState { .. } => {
                if Self::box_bool(self.handle_track_lv2_snapshot_state(a.clone())).await {
                    return true;
                }
            }
            #[cfg(unix)]
            Action::ClipLv2SnapshotState { .. } => {
                if Self::box_bool(self.handle_clip_lv2_snapshot_state(a.clone())).await {
                    return true;
                }
            }
            Action::TrackVst3SnapshotState { .. } => {
                if Self::box_bool(self.handle_track_vst3_snapshot_state(a.clone())).await {
                    return true;
                }
            }
            Action::ClipVst3SnapshotState { .. } => {
                if Self::box_bool(self.handle_clip_vst3_snapshot_state(a.clone())).await {
                    return true;
                }
            }
            Action::TrackVst3RestoreState { .. } => {
                if Self::box_bool(self.handle_track_vst3_restore_state(a.clone())).await {
                    return true;
                }
            }
            Action::TrackConnectVst3Audio { .. } => {
                if Self::box_bool(self.handle_track_connect_vst3_audio(a.clone())).await {
                    return true;
                }
            }
            Action::TrackDisconnectVst3Audio { .. }
                if Self::box_bool(self.handle_track_disconnect_vst3_audio(a.clone())).await =>
            {
                return true;
            }
            _ => {}
        }
        false
    }
}

impl Engine {
    pub(crate) fn spawn_plugin_host_stderr_reader(
        &self,
        stderr: std::process::ChildStderr,
        source: String,
    ) {
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                if let Ok(line) = line
                    && !line.is_empty()
                {
                    let _ = tx.blocking_send(Message::Request(Action::Log {
                        source: source.clone(),
                        message: line,
                    }));
                }
            }
        });
    }

    pub(crate) async fn publish_clap_state_dirty(&mut self) {
        let tracks: Vec<(String, crate::state::TrackHandle)> = self
            .state_snapshot
            .load_full()
            .tracks
            .iter()
            .map(|(name, track)| (name.clone(), track.clone()))
            .collect();
        for (track_name, track) in &tracks {
            let dirty = track.lock().take_dirty_clap_instances();
            for instance_id in dirty {
                self.notify_event(Event::TrackClapStateDirty {
                    track_name: track_name.clone(),
                    instance_id,
                })
                .await;
            }
        }
    }
}

impl Engine {
    pub(crate) async fn poll_stopped_plugin_parameter_echoes(&mut self) {
        if self.transport.playing || self.transport.transport_running {
            return;
        }

        let state = self.state_snapshot.load_full();
        let mut updates = Vec::new();
        for track in state.tracks.values() {
            updates.extend(track.lock().drain_plugin_parameter_echoes());
        }
        drop(state);

        for action in updates {
            self.notify_clients(Ok(action)).await;
        }
    }
}
