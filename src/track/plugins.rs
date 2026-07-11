#[cfg(target_os = "macos")]
use crate::clap::ClapMidiOutputEvent;
use crate::message::{PluginGraphConnection, PluginGraphNode, PluginGraphPlugin, PluginKind};
use crate::mutex::UnsafeMutex;

use super::*;
use crate::{
    audio::io::AudioIO,
    midi::io::{MIDIIO, MidiEvent},
};
use crate::{kind::Kind, routing};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
    sync::Arc,
};

impl Track {
    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn load_lv2_plugin(&mut self, uri: &str, instance_id: Option<usize>) -> Result<(), String> {
        let buffer_size = self
            .audio
            .ins
            .first()
            .map(|io| io.buffer.lock().len())
            .or_else(|| self.audio.outs.first().map(|io| io.buffer.lock().len()))
            .unwrap_or(0);
        let host_binary = crate::plugins::ipc::find_plugin_host_binary()
            .ok_or_else(|| "maolan-plugin-host binary not found".to_string())?;
        let processor = crate::lv2_proc::Lv2Processor::new(
            self.sample_rate,
            buffer_size,
            uri,
            self.audio.ins.len().max(1),
            self.audio.outs.len().max(1),
            host_binary,
        )?;
        let id = instance_id
            .filter(|&id| {
                !self.vst3_plugins.iter().any(|i| i.id == id)
                    && !self.clap_plugins.iter().any(|i| i.id == id)
                    && !self.lv2_instance_id_exists(id)
            })
            .unwrap_or_else(|| self.alloc_plugin_instance_id());
        self.next_lv2_instance_id = self.next_lv2_instance_id.max(id.saturating_add(1));
        self.next_plugin_instance_id = self.next_plugin_instance_id.max(id.saturating_add(1));
        self.lv2_plugins.push(Lv2Instance {
            id,
            processor: Arc::new(UnsafeMutex::new(processor)),
        });
        self.invalidate_audio_route_cache();
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn unload_lv2_plugin(&mut self, uri: &str) -> Result<(), String> {
        let Some(index) = self
            .lv2_plugins
            .iter()
            .position(|instance| instance.processor.lock().uri() == uri)
        else {
            return Err(format!(
                "Track '{}' does not have LV2 plugin loaded: {uri}",
                self.name
            ));
        };
        self.remove_lv2_instance(index);
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn unload_lv2_plugin_instance(&mut self, instance_id: usize) -> Result<(), String> {
        tracing::info!(track = %self.name, instance_id, "unload_lv2_plugin_instance start");
        let Some(index) = self
            .lv2_plugins
            .iter()
            .position(|instance| instance.id == instance_id)
        else {
            return Err(format!(
                "Track '{}' does not have LV2 instance id: {}",
                self.name, instance_id
            ));
        };
        self.remove_lv2_instance(index);
        tracing::info!(track = %self.name, instance_id, "unload_lv2_plugin_instance done");
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn get_lv2_plugin_controls(
        &self,
        instance_id: usize,
    ) -> Result<Vec<crate::message::Lv2ControlPortInfo>, String> {
        let instance = self
            .lv2_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| {
                format!(
                    "Track '{}' does not have LV2 instance id: {}",
                    self.name, instance_id
                )
            })?;
        instance.processor.lock().control_ports()
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) fn remove_lv2_instance(&mut self, index: usize) {
        tracing::info!(track = %self.name, index, "remove_lv2_instance start");
        let removed = self.lv2_plugins.remove(index);
        let removed_id = removed.id;
        for port in removed.processor.lock().audio_inputs() {
            Self::disconnect_all(port);
        }
        for port in removed.processor.lock().audio_outputs() {
            Self::disconnect_all(port);
        }
        self.plugin_midi_connections.retain(|conn| {
            conn.from_node != PluginGraphNode::Lv2PluginInstance(removed_id)
                && conn.to_node != PluginGraphNode::Lv2PluginInstance(removed_id)
        });
        self.invalidate_audio_route_cache();
        tracing::info!(track = %self.name, removed_id, "remove_lv2_instance done");
    }

    pub(crate) fn prune_plugin_midi_connections(&mut self, node: PluginGraphNode) {
        self.plugin_midi_connections
            .retain(|conn| conn.from_node != node && conn.to_node != node);
    }

    pub(crate) fn push_plugin_graph_plugin(
        plugins: &mut Vec<PluginGraphPlugin>,
        plugin: PluginGraphPlugin,
    ) {
        plugins.push(plugin);
    }

    pub fn plugin_graph_plugins(&self, include_state: bool) -> Vec<PluginGraphPlugin> {
        let mut plugins = Vec::new();
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            let proc = instance.processor.lock();
            Self::push_plugin_graph_plugin(
                &mut plugins,
                PluginGraphPlugin {
                    #[cfg(all(unix, not(target_os = "macos")))]
                    node: PluginGraphNode::Lv2PluginInstance(instance.id),
                    instance_id: instance.id,
                    format: "LV2".to_string(),
                    uri: proc.uri().to_string(),
                    plugin_id: proc.uri().to_string(),
                    name: proc.name().to_string(),
                    main_audio_inputs: proc.main_audio_input_count(),
                    main_audio_outputs: proc.main_audio_output_count(),
                    audio_inputs: proc.audio_inputs().len(),
                    audio_outputs: proc.audio_outputs().len(),
                    midi_inputs: proc.midi_input_count(),
                    midi_outputs: proc.midi_output_count(),
                    state: include_state
                        .then(|| serde_json::to_value(proc.snapshot_state()).ok())
                        .flatten(),
                    bypassed: proc.is_bypassed(),
                },
            );
        }
        for instance in &self.vst3_plugins {
            let proc = instance.processor.lock();
            Self::push_plugin_graph_plugin(
                &mut plugins,
                PluginGraphPlugin {
                    node: PluginGraphNode::Vst3PluginInstance(instance.id),
                    instance_id: instance.id,
                    format: "VST3".to_string(),
                    uri: proc.plugin_id().to_string(),
                    plugin_id: proc.plugin_id().to_string(),
                    name: proc.name().to_string(),
                    main_audio_inputs: proc.main_audio_input_count(),
                    main_audio_outputs: proc.main_audio_output_count(),
                    audio_inputs: proc.audio_inputs().len(),
                    audio_outputs: proc.audio_outputs().len(),
                    midi_inputs: proc.midi_input_count(),
                    midi_outputs: proc.midi_output_count(),
                    state: None,
                    bypassed: proc.is_bypassed(),
                },
            );
        }
        for instance in &self.clap_plugins {
            let proc = instance.processor.lock();
            Self::push_plugin_graph_plugin(
                &mut plugins,
                PluginGraphPlugin {
                    node: PluginGraphNode::ClapPluginInstance(instance.id),
                    instance_id: instance.id,
                    format: "CLAP".to_string(),
                    uri: proc.plugin_id().to_string(),
                    plugin_id: proc.plugin_id().to_string(),
                    name: proc.name().to_string(),
                    main_audio_inputs: proc.main_audio_input_count(),
                    main_audio_outputs: proc.main_audio_output_count(),
                    audio_inputs: proc.audio_inputs().len(),
                    audio_outputs: proc.audio_outputs().len(),
                    midi_inputs: proc.midi_input_count(),
                    midi_outputs: proc.midi_output_count(),
                    state: include_state
                        .then(|| {
                            proc.snapshot_state()
                                .ok()
                                .and_then(|state| serde_json::to_value(state).ok())
                        })
                        .flatten(),
                    bypassed: proc.is_bypassed(),
                },
            );
        }
        plugins
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn set_lv2_plugin_bypassed(
        &self,
        instance_id: usize,
        bypassed: bool,
    ) -> Result<(), String> {
        let Some(instance) = self
            .lv2_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
        else {
            return Err(format!(
                "Track '{}' does not have LV2 instance id: {}",
                self.name, instance_id
            ));
        };
        instance.processor.lock().set_bypassed(bypassed);
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn set_lv2_control_value(
        &self,
        instance_id: usize,
        index: usize,
        param_value: f64,
    ) -> Result<(), String> {
        let Some(instance) = self
            .lv2_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
        else {
            return Err(format!(
                "Track '{}' does not have LV2 instance id: {}",
                self.name, instance_id
            ));
        };
        instance
            .processor
            .lock()
            .set_parameter(index as u32, param_value)
    }

    pub(crate) fn normalize_clap_path(path: &str) -> String {
        if let Some(pos) = path.rfind("::") {
            format!("{}::{}", &path[..pos], &path[pos + 2..])
        } else if let Some(pos) = path.rfind('#') {
            format!("{}::{}", &path[..pos], &path[pos + 1..])
        } else {
            path.to_string()
        }
    }

    pub fn load_clap_plugin(
        &mut self,
        plugin_spec: &str,
        instance_id: Option<usize>,
    ) -> Result<(), String> {
        let normalized = Self::normalize_clap_path(plugin_spec);
        let bundle_path = normalized
            .split_once("::")
            .map(|(path, _)| path)
            .unwrap_or(&normalized);
        let path = Path::new(bundle_path);
        if !path.exists() {
            return Err(format!("CLAP plugin not found: {plugin_spec}"));
        }
        if !crate::clap::is_supported_clap_binary(path) {
            return Err(format!("Not a CLAP plugin path: {plugin_spec}"));
        }
        let id = instance_id
            .filter(|&id| {
                !self.vst3_plugins.iter().any(|i| i.id == id)
                    && !self.clap_plugins.iter().any(|i| i.id == id)
                    && !self.lv2_instance_id_exists(id)
            })
            .unwrap_or_else(|| self.alloc_plugin_instance_id());
        self.next_clap_instance_id = self.next_clap_instance_id.max(id.saturating_add(1));
        self.next_plugin_instance_id = self.next_plugin_instance_id.max(id.saturating_add(1));
        let buffer_size = self
            .audio
            .ins
            .first()
            .map(|io| io.buffer.lock().len())
            .or_else(|| self.audio.outs.first().map(|io| io.buffer.lock().len()))
            .unwrap_or(0);
        let input_count = self.audio.ins.len().max(1);
        let output_count = self.audio.outs.len().max(1);
        let host_binary = crate::plugins::ipc::find_plugin_host_binary()
            .ok_or_else(|| "maolan-plugin-host binary not found".to_string())?;
        let processor = Arc::new(UnsafeMutex::new(crate::clap_proc::ClapProcessor::new(
            self.sample_rate,
            buffer_size,
            plugin_spec,
            input_count,
            output_count,
            host_binary,
        )?));
        self.clap_plugins.push(ClapInstance::new(id, processor));
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn unload_clap_plugin(&mut self, plugin_id: &str) -> Result<(), String> {
        let Some(index) = self
            .clap_plugins
            .iter()
            .position(|instance| instance.processor.lock().plugin_id() == plugin_id)
        else {
            return Err(format!(
                "Track '{}' does not have CLAP plugin loaded: {}",
                self.name, plugin_id
            ));
        };
        let removed_id = self.clap_plugins[index].id;
        self.clap_plugins.remove(index);
        self.prune_plugin_midi_connections(PluginGraphNode::ClapPluginInstance(removed_id));
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn unload_clap_plugin_instance(&mut self, instance_id: usize) -> Result<(), String> {
        let Some(index) = self
            .clap_plugins
            .iter()
            .position(|instance| instance.id == instance_id)
        else {
            return Err(format!(
                "Track '{}' does not have CLAP instance id: {}",
                self.name, instance_id
            ));
        };
        self.clap_plugins.remove(index);
        self.prune_plugin_midi_connections(PluginGraphNode::ClapPluginInstance(instance_id));
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn show_clap_gui(&self, instance_id: usize) -> Result<(), String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            let processor = instance.processor.lock();
            processor.gui_set_parent_x11(0)?;
            processor.gui_set_floating_mode(true)?;
            return processor.gui_show();
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn show_vst3_gui(&self, instance_id: usize) -> Result<(), String> {
        if let Some(instance) = self.vst3_plugins.iter().find(|i| i.id == instance_id) {
            let processor = instance.processor.lock();
            processor.gui_set_floating_mode(true)?;
            return processor.gui_show();
        }
        Err(format!(
            "Track '{}' does not have VST3 instance id: {}",
            self.name, instance_id
        ))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn show_lv2_gui(&self, instance_id: usize) -> Result<(), String> {
        if let Some(instance) = self.lv2_plugins.iter().find(|i| i.id == instance_id) {
            let processor = instance.processor.lock();
            processor.gui_set_floating_mode(true)?;
            return processor.gui_show();
        }
        Err(format!(
            "Track '{}' does not have LV2 instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn set_clap_plugin_bypassed(
        &self,
        instance_id: usize,
        bypassed: bool,
    ) -> Result<(), String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            instance.processor.lock().set_bypassed(bypassed);
            return Ok(());
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn set_clap_parameter(
        &self,
        instance_id: usize,
        param_id: u32,
        value: f64,
    ) -> Result<(), String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().set_parameter(param_id, value);
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn clip_set_clap_parameter(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
        param_id: u32,
        value: f64,
    ) -> Result<(), String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip CLAP instance {} not found", instance_id))?;
        instance.processor.lock().set_parameter(param_id, value)
    }

    pub fn set_clap_parameter_at(
        &self,
        instance_id: usize,
        param_id: u32,
        value: f64,
        frame: u32,
    ) -> Result<(), String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            return instance
                .processor
                .lock()
                .set_parameter_at(param_id, value, frame);
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn begin_clap_parameter_edit(
        &self,
        instance_id: usize,
        param_id: u32,
        frame: u32,
    ) -> Result<(), String> {
        let instance = self
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| {
                format!(
                    "Track '{}' does not have CLAP instance id: {}",
                    self.name, instance_id
                )
            })?;
        instance
            .processor
            .lock()
            .begin_parameter_edit_at(param_id, frame)
    }

    pub fn end_clap_parameter_edit(
        &self,
        instance_id: usize,
        param_id: u32,
        frame: u32,
    ) -> Result<(), String> {
        let instance = self
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| {
                format!(
                    "Track '{}' does not have CLAP instance id: {}",
                    self.name, instance_id
                )
            })?;
        instance
            .processor
            .lock()
            .end_parameter_edit_at(param_id, frame)
    }

    pub fn get_clap_parameters(
        &self,
        instance_id: usize,
    ) -> Result<Vec<crate::clap::ClapParameterInfo>, String> {
        let instance = self
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| {
                format!(
                    "Track '{}' does not have CLAP instance id: {}",
                    self.name, instance_id
                )
            })?;
        Ok(instance.processor.lock().parameter_infos())
    }

    pub fn get_clap_note_names(&self) -> std::collections::HashMap<u8, String> {
        let mut result = std::collections::HashMap::new();
        for instance in &self.clap_plugins {
            match instance.processor.lock().note_names() {
                Ok(names) => {
                    for (k, v) in names {
                        result.insert(k, v);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        track = %self.name,
                        error = %e,
                        "Failed to read CLAP note names"
                    );
                }
            }
        }
        result
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn get_lv2_midnam(&self) -> std::collections::HashMap<u8, String> {
        let mut result = std::collections::HashMap::new();
        for instance in &self.lv2_plugins {
            match instance.processor.lock().note_names() {
                Ok(names) => {
                    for (k, v) in names {
                        result.insert(k, v);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        track = %self.name,
                        error = %e,
                        "Failed to read LV2 midnam note names"
                    );
                }
            }
        }
        result
    }

    pub fn clap_snapshot_state(
        &self,
        instance_id: usize,
    ) -> Result<crate::clap::ClapPluginState, String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().snapshot_state();
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn clip_clap_snapshot_state(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
    ) -> Result<(String, crate::clap::ClapPluginState), String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip CLAP instance {} not found", instance_id))?;
        let state = instance.processor.lock().snapshot_state()?;
        Ok((instance.processor.lock().plugin_id().to_string(), state))
    }

    pub fn clap_restore_state(
        &self,
        instance_id: usize,
        state: &crate::clap::ClapPluginState,
    ) -> Result<(), String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().restore_state(state);
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn clip_clap_restore_state(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
        state: &crate::clap::ClapPluginState,
    ) -> Result<(), String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip CLAP instance {} not found", instance_id))?;
        instance.processor.lock().restore_state(state)
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn lv2_snapshot_state(&self, instance_id: usize) -> Result<Vec<u8>, String> {
        if let Some(instance) = self.lv2_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().snapshot_state();
        }
        Err(format!(
            "Track '{}' does not have LV2 instance id: {}",
            self.name, instance_id
        ))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn lv2_restore_state(&self, instance_id: usize, state: &[u8]) -> Result<(), String> {
        if let Some(instance) = self.lv2_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().restore_state(state);
        }
        Err(format!(
            "Track '{}' does not have LV2 instance id: {}",
            self.name, instance_id
        ))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn clip_lv2_snapshot_state(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
    ) -> Result<Vec<u8>, String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .lv2_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip LV2 instance {} not found", instance_id))?;
        instance.processor.lock().snapshot_state()
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub fn clip_lv2_restore_state(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
        state: &[u8],
    ) -> Result<(), String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .lv2_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip LV2 instance {} not found", instance_id))?;
        instance.processor.lock().restore_state(state)
    }

    pub fn clap_snapshot_all_states(&self) -> Vec<(usize, String, crate::clap::ClapPluginState)> {
        self.clap_plugins
            .iter()
            .filter_map(|instance| {
                let proc = instance.processor.lock();
                proc.snapshot_state()
                    .ok()
                    .map(|state| (instance.id, proc.plugin_id().to_string(), state))
            })
            .collect()
    }

    pub fn take_dirty_clap_instances(&self) -> Vec<usize> {
        self.clap_plugins
            .iter()
            .filter_map(|instance| {
                if instance.processor.take_state_dirty() {
                    Some(instance.id)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn set_clap_plugin_resource_dir(
        &self,
        instance_id: usize,
        dir: &std::path::Path,
    ) -> Result<(), String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().set_resource_directory(dir);
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn set_lv2_plugin_resource_dir(
        &self,
        instance_id: usize,
        dir: &std::path::Path,
    ) -> Result<(), String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        if let Some(instance) = self.lv2_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().set_resource_directory(dir);
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        let _ = dir;
        Err(format!(
            "Track '{}' does not have LV2 instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn clap_file_references(
        &self,
        instance_id: usize,
    ) -> Result<Vec<maolan_plugin_protocol::protocol::FileReference>, String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().file_references();
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn update_clap_file_reference(
        &self,
        instance_id: usize,
        index: u32,
        path: &str,
    ) -> Result<(), String> {
        if let Some(instance) = self.clap_plugins.iter().find(|i| i.id == instance_id) {
            return instance.processor.lock().update_file_reference(index, path);
        }
        Err(format!(
            "Track '{}' does not have CLAP instance id: {}",
            self.name, instance_id
        ))
    }

    pub fn clip_set_clap_plugin_resource_dir(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
        dir: &std::path::Path,
    ) -> Result<(), String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip CLAP instance {} not found", instance_id))?;
        instance.processor.lock().set_resource_directory(dir)
    }

    pub fn clip_set_lv2_plugin_resource_dir(
        &mut self,
        _clip_idx: usize,
        _instance_id: usize,
        _dir: &std::path::Path,
    ) -> Result<(), String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let channels = self.audio.ins.len().max(1);
            let runtime = self.ensure_clip_plugin_runtime(_clip_idx, channels)?;
            let instance = runtime
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == _instance_id)
                .ok_or_else(|| format!("Clip LV2 instance {} not found", _instance_id))?;
            instance.processor.lock().set_resource_directory(_dir)
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        Err("LV2 is not supported on this platform".to_string())
    }

    pub fn clip_clap_file_references(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
    ) -> Result<Vec<maolan_plugin_protocol::protocol::FileReference>, String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip CLAP instance {} not found", instance_id))?;
        instance.processor.lock().file_references()
    }

    pub fn clip_update_clap_file_reference(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
        index: u32,
        path: &str,
    ) -> Result<(), String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip CLAP instance {} not found", instance_id))?;
        instance.processor.lock().update_file_reference(index, path)
    }

    pub fn load_vst3_plugin(
        &mut self,
        plugin_id: &str,
        plugin_path: &str,
        instance_id: Option<usize>,
    ) -> Result<(), String> {
        let buffer_size = self
            .audio
            .ins
            .first()
            .map(|io| io.buffer.lock().len())
            .or_else(|| self.audio.outs.first().map(|io| io.buffer.lock().len()))
            .unwrap_or(64)
            .max(1);
        let input_count = self.audio.ins.len().max(1);
        let output_count = self.audio.outs.len().max(1);
        let host_binary = crate::plugins::ipc::find_plugin_host_binary()
            .ok_or_else(|| "maolan-plugin-host binary not found".to_string())?;
        let processor = crate::vst3_proc::Vst3Processor::new(
            self.sample_rate,
            buffer_size,
            plugin_path,
            plugin_id,
            input_count,
            output_count,
            host_binary,
        )?;
        let id = instance_id
            .filter(|&id| {
                !self.vst3_plugins.iter().any(|i| i.id == id)
                    && !self.clap_plugins.iter().any(|i| i.id == id)
                    && !self.lv2_instance_id_exists(id)
            })
            .unwrap_or_else(|| self.alloc_plugin_instance_id());
        self.next_vst3_instance_id = self.next_vst3_instance_id.max(id.saturating_add(1));
        self.next_plugin_instance_id = self.next_plugin_instance_id.max(id.saturating_add(1));
        self.vst3_plugins.push(Vst3Instance {
            id,
            processor: Arc::new(UnsafeMutex::new(processor)),
        });
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn unload_vst3_plugin(&mut self, plugin_id: &str) -> Result<(), String> {
        let Some(index) = self
            .vst3_plugins
            .iter()
            .position(|instance| instance.processor.lock().plugin_id() == plugin_id)
        else {
            return Err(format!(
                "Track '{}' does not have VST3 plugin loaded: {}",
                self.name, plugin_id
            ));
        };
        let removed = self.vst3_plugins.remove(index);
        let removed_id = removed.id;
        for port in removed.processor.lock().audio_inputs() {
            Self::disconnect_all(port);
        }
        for port in removed.processor.lock().audio_outputs() {
            Self::disconnect_all(port);
        }
        self.prune_plugin_midi_connections(PluginGraphNode::Vst3PluginInstance(removed_id));
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn unload_vst3_plugin_instance(&mut self, instance_id: usize) -> Result<(), String> {
        let Some(index) = self
            .vst3_plugins
            .iter()
            .position(|instance| instance.id == instance_id)
        else {
            return Err(format!(
                "Track '{}' does not have VST3 instance id: {}",
                self.name, instance_id
            ));
        };
        let removed = self.vst3_plugins.remove(index);
        for port in removed.processor.lock().audio_inputs() {
            Self::disconnect_all(port);
        }
        for port in removed.processor.lock().audio_outputs() {
            Self::disconnect_all(port);
        }
        self.prune_plugin_midi_connections(PluginGraphNode::Vst3PluginInstance(instance_id));
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn clear_plugins(&mut self) {
        let clap_ids: Vec<usize> = self.clap_plugins.iter().map(|i| i.id).collect();
        for id in clap_ids {
            let _ = self.unload_clap_plugin_instance(id);
        }
        let vst3_ids: Vec<usize> = self.vst3_plugins.iter().map(|i| i.id).collect();
        for id in vst3_ids {
            let _ = self.unload_vst3_plugin_instance(id);
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let lv2_ids: Vec<usize> = self.lv2_plugins.iter().map(|i| i.id).collect();
            for id in lv2_ids {
                let _ = self.unload_lv2_plugin_instance(id);
            }
        }
        self.plugin_midi_connections.clear();
        self.invalidate_audio_route_cache();
        self.invalidate_midi_route_cache();
    }

    pub fn vst3_graph_plugins(&self) -> Vec<crate::message::Vst3GraphPlugin> {
        use crate::message::Vst3GraphPlugin;

        self.vst3_plugins
            .iter()
            .map(|instance| {
                let proc = instance.processor.lock();
                Vst3GraphPlugin {
                    instance_id: instance.id,
                    name: proc.name().to_string(),
                    path: proc.path().to_string(),
                    audio_inputs: proc.audio_inputs().len(),
                    audio_outputs: proc.audio_outputs().len(),
                    parameters: proc.parameter_infos(),
                }
            })
            .collect()
    }

    pub fn vst3_graph_connections(&self) -> Vec<crate::message::Vst3GraphConnection> {
        use crate::kind::Kind;
        use crate::message::{Vst3GraphConnection, Vst3GraphNode};

        let mut connections = Vec::new();

        for instance in &self.vst3_plugins {
            let proc = instance.processor.lock();
            for (port_idx, input) in proc.audio_inputs().iter().enumerate() {
                let conns = input.connections.lock();
                for conn in conns.iter() {
                    let from_node = self.find_vst3_audio_source_node(conn.as_ref());
                    if let Some((node, from_port)) = from_node {
                        connections.push(Vst3GraphConnection {
                            from_node: node,
                            from_port,
                            to_node: Vst3GraphNode::PluginInstance(instance.id),
                            to_port: port_idx,
                            kind: Kind::Audio,
                        });
                    }
                }
            }

            for (port_idx, output) in proc.audio_outputs().iter().enumerate() {
                let conns = output.connections.lock();
                for conn in conns.iter() {
                    if self.audio.outs.iter().any(|out| Arc::ptr_eq(out, conn)) {
                        let to_port = self
                            .audio
                            .outs
                            .iter()
                            .position(|out| Arc::ptr_eq(out, conn))
                            .unwrap();

                        connections.push(Vst3GraphConnection {
                            from_node: Vst3GraphNode::PluginInstance(instance.id),
                            from_port: port_idx,
                            to_node: Vst3GraphNode::TrackOutput,
                            to_port,
                            kind: Kind::Audio,
                        });
                    }
                }
            }
        }

        connections
    }

    pub(crate) fn find_vst3_audio_source_node(
        &self,
        audio_io: &crate::audio::io::AudioIO,
    ) -> Option<(crate::message::Vst3GraphNode, usize)> {
        use crate::message::Vst3GraphNode;

        for (idx, input) in self.audio.ins.iter().enumerate() {
            if std::ptr::eq(input.as_ref(), audio_io) {
                return Some((Vst3GraphNode::TrackInput, idx));
            }
        }

        for instance in &self.vst3_plugins {
            for (port_idx, output) in instance.processor.lock().audio_outputs().iter().enumerate() {
                if std::ptr::eq(output.as_ref(), audio_io) {
                    return Some((Vst3GraphNode::PluginInstance(instance.id), port_idx));
                }
            }
        }

        None
    }

    pub fn set_vst3_plugin_bypassed(
        &self,
        instance_id: usize,
        bypassed: bool,
    ) -> Result<(), String> {
        let instance = self
            .vst3_plugins
            .iter()
            .find(|i| i.id == instance_id)
            .ok_or_else(|| format!("VST3 instance {} not found", instance_id))?;
        instance.processor.lock().set_bypassed(bypassed);
        Ok(())
    }

    pub fn set_vst3_parameter(
        &mut self,
        instance_id: usize,
        param_id: u32,
        value: f32,
    ) -> Result<(), String> {
        let instance = self
            .vst3_plugins
            .iter()
            .find(|i| i.id == instance_id)
            .ok_or_else(|| format!("VST3 instance {} not found", instance_id))?;

        instance
            .processor
            .lock()
            .set_parameter(param_id, value as f64)
    }

    pub fn get_vst3_parameters(
        &self,
        instance_id: usize,
    ) -> Result<Vec<crate::vst3::port::ParameterInfo>, String> {
        let instance = self
            .vst3_plugins
            .iter()
            .find(|i| i.id == instance_id)
            .ok_or_else(|| format!("VST3 instance {} not found", instance_id))?;

        Ok(instance.processor.lock().parameter_infos())
    }

    pub fn vst3_snapshot_state(
        &self,
        instance_id: usize,
    ) -> Result<crate::vst3::state::Vst3PluginState, String> {
        let instance = self
            .vst3_plugins
            .iter()
            .find(|i| i.id == instance_id)
            .ok_or_else(|| format!("VST3 instance {} not found", instance_id))?;

        instance.processor.lock().snapshot_state()
    }

    pub fn clip_vst3_snapshot_state(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
    ) -> Result<crate::vst3::state::Vst3PluginState, String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .vst3_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip VST3 instance {} not found", instance_id))?;
        instance.processor.lock().snapshot_state()
    }

    pub fn clip_vst3_restore_state(
        &mut self,
        clip_idx: usize,
        instance_id: usize,
        state: &crate::vst3::state::Vst3PluginState,
    ) -> Result<(), String> {
        let channels = self.audio.ins.len().max(1);
        let runtime = self.ensure_clip_plugin_runtime(clip_idx, channels)?;
        let instance = runtime
            .vst3_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .ok_or_else(|| format!("Clip VST3 instance {} not found", instance_id))?;
        instance.processor.lock().restore_state(state)
    }

    pub fn vst3_restore_state(
        &mut self,
        instance_id: usize,
        state: &crate::vst3::state::Vst3PluginState,
    ) -> Result<(), String> {
        let instance = self
            .vst3_plugins
            .iter()
            .find(|i| i.id == instance_id)
            .ok_or_else(|| format!("VST3 instance {} not found", instance_id))?;

        instance.processor.lock().restore_state(state)
    }

    pub fn connect_vst3_audio(
        &mut self,
        from_node: &crate::message::Vst3GraphNode,
        from_port: usize,
        to_node: &crate::message::Vst3GraphNode,
        to_port: usize,
    ) -> Result<(), String> {
        use crate::message::Vst3GraphNode;

        let from_io = match from_node {
            Vst3GraphNode::TrackInput => self
                .audio
                .ins
                .get(from_port)
                .ok_or("Invalid track input port")?
                .clone(),
            Vst3GraphNode::PluginInstance(id) => {
                let instance = self
                    .vst3_plugins
                    .iter()
                    .find(|i| i.id == *id)
                    .ok_or("VST3 instance not found")?;
                instance
                    .processor
                    .lock()
                    .audio_outputs()
                    .get(from_port)
                    .ok_or("Invalid plugin output port")?
                    .clone()
            }
            Vst3GraphNode::TrackOutput => {
                return Err("Cannot connect from track output".to_string());
            }
        };

        let to_io = match to_node {
            Vst3GraphNode::PluginInstance(id) => {
                let instance = self
                    .vst3_plugins
                    .iter()
                    .find(|i| i.id == *id)
                    .ok_or("VST3 instance not found")?;
                instance
                    .processor
                    .lock()
                    .audio_inputs()
                    .get(to_port)
                    .ok_or("Invalid plugin input port")?
            }
            Vst3GraphNode::TrackOutput => self
                .audio
                .outs
                .get(to_port)
                .ok_or("Invalid track output port")?,
            Vst3GraphNode::TrackInput => return Err("Cannot connect to track input".to_string()),
        };

        to_io.connections.lock().push(from_io);
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn disconnect_vst3_audio(
        &mut self,
        from_node: &crate::message::Vst3GraphNode,
        from_port: usize,
        to_node: &crate::message::Vst3GraphNode,
        to_port: usize,
    ) -> Result<(), String> {
        use crate::message::Vst3GraphNode;

        let from_io = match from_node {
            Vst3GraphNode::TrackInput => self
                .audio
                .ins
                .get(from_port)
                .ok_or("Invalid track input port")?
                .clone(),
            Vst3GraphNode::PluginInstance(id) => {
                let instance = self
                    .vst3_plugins
                    .iter()
                    .find(|i| i.id == *id)
                    .ok_or("VST3 instance not found")?;
                instance
                    .processor
                    .lock()
                    .audio_outputs()
                    .get(from_port)
                    .ok_or("Invalid plugin output port")?
                    .clone()
            }
            Vst3GraphNode::TrackOutput => {
                return Err("Cannot disconnect from track output".to_string());
            }
        };

        let to_io = match to_node {
            Vst3GraphNode::PluginInstance(id) => {
                let instance = self
                    .vst3_plugins
                    .iter()
                    .find(|i| i.id == *id)
                    .ok_or("VST3 instance not found")?;
                instance
                    .processor
                    .lock()
                    .audio_inputs()
                    .get(to_port)
                    .ok_or("Invalid plugin input port")?
            }
            Vst3GraphNode::TrackOutput => self
                .audio
                .outs
                .get(to_port)
                .ok_or("Invalid track output port")?,
            Vst3GraphNode::TrackInput => return Err("Cannot disconnect to track input".to_string()),
        };

        to_io
            .connections
            .lock()
            .retain(|conn| !Arc::ptr_eq(conn, &from_io));
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub(crate) fn plugin_process_order(&self) -> Vec<(PluginKind, usize)> {
        let mut entries: Vec<(PluginGraphNode, PluginKind, usize)> = Vec::new();
        for (idx, instance) in self.clap_plugins.iter().enumerate() {
            entries.push((
                PluginGraphNode::ClapPluginInstance(instance.id),
                PluginKind::Clap,
                idx,
            ));
        }
        for (idx, instance) in self.vst3_plugins.iter().enumerate() {
            entries.push((
                PluginGraphNode::Vst3PluginInstance(instance.id),
                PluginKind::Vst3,
                idx,
            ));
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for (idx, instance) in self.lv2_plugins.iter().enumerate() {
            entries.push((
                PluginGraphNode::Lv2PluginInstance(instance.id),
                PluginKind::Lv2,
                idx,
            ));
        }

        let node_to_index: HashMap<PluginGraphNode, usize> = entries
            .iter()
            .enumerate()
            .map(|(idx, (node, _, _))| (node.clone(), idx))
            .collect();
        let count = entries.len();
        let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); count];
        let mut in_degree = vec![0usize; count];
        for conn in self.plugin_graph_connections() {
            if let Some(&from_idx) = node_to_index.get(&conn.from_node)
                && let Some(&to_idx) = node_to_index.get(&conn.to_node)
            {
                adjacency[from_idx].push(to_idx);
                in_degree[to_idx] += 1;
            }
        }

        let mut queue: VecDeque<usize> = in_degree
            .iter()
            .enumerate()
            .filter(|(_, d)| **d == 0)
            .map(|(idx, _)| idx)
            .collect();
        let mut order = Vec::with_capacity(count);
        while let Some(idx) = queue.pop_front() {
            order.push((entries[idx].1, entries[idx].2));
            for &next in &adjacency[idx] {
                in_degree[next] = in_degree[next].saturating_sub(1);
                if in_degree[next] == 0 {
                    queue.push_back(next);
                }
            }
        }

        if order.len() < count {
            // Cycle or disconnected graph: fall back to type ordering so every
            // plugin still gets a chance to run.
            order.clear();
            for (idx, _) in self.clap_plugins.iter().enumerate() {
                order.push((PluginKind::Clap, idx));
            }
            for (idx, _) in self.vst3_plugins.iter().enumerate() {
                order.push((PluginKind::Vst3, idx));
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            for (idx, _) in self.lv2_plugins.iter().enumerate() {
                order.push((PluginKind::Lv2, idx));
            }
        }
        order
    }

    pub fn connect_plugin_audio(
        &mut self,
        from_node: PluginGraphNode,
        from_port: usize,
        to_node: PluginGraphNode,
        to_port: usize,
    ) -> Result<(), String> {
        let source = self.plugin_source_io(&from_node, from_port)?;
        let target = self.plugin_target_io(&to_node, to_port)?;
        if routing::would_create_cycle(&from_node, &to_node, |node| {
            self.plugin_connected_neighbors(Kind::Audio, node)
        }) {
            return Err("Circular routing is not allowed!".to_string());
        }
        if matches!(from_node, PluginGraphNode::TrackInput) {
            Self::connect_directed_audio(&source, &target);
        } else {
            AudioIO::connect(&source, &target);
        }
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn disconnect_plugin_audio(
        &mut self,
        from_node: PluginGraphNode,
        from_port: usize,
        to_node: PluginGraphNode,
        to_port: usize,
    ) -> Result<(), String> {
        let source = self.plugin_source_io(&from_node, from_port)?;
        let target = self.plugin_target_io(&to_node, to_port)?;
        AudioIO::disconnect(&source, &target)?;
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn connect_plugin_midi(
        &mut self,
        from_node: PluginGraphNode,
        from_port: usize,
        to_node: PluginGraphNode,
        to_port: usize,
    ) -> Result<(), String> {
        self.validate_plugin_midi_source(&from_node, from_port)?;
        self.validate_plugin_midi_target(&to_node, to_port)?;
        if from_node == to_node && from_port == to_port {
            return Err("Cannot connect a MIDI port to itself".to_string());
        }
        if routing::would_create_cycle(&from_node, &to_node, |node| {
            self.plugin_connected_neighbors(Kind::MIDI, node)
        }) {
            return Err("Circular routing is not allowed!".to_string());
        }

        let source = self.plugin_midi_source_io(&from_node, from_port)?;
        let target = self.plugin_midi_target_io(&to_node, to_port)?;
        MIDIIO::connect(&source, &target);

        if !(matches!(from_node, PluginGraphNode::TrackInput)
            && matches!(to_node, PluginGraphNode::TrackOutput))
        {
            let new_conn = PluginGraphConnection {
                from_node,
                from_port,
                to_node,
                to_port,
                kind: Kind::MIDI,
            };
            if !self.plugin_midi_connections.iter().any(|c| c == &new_conn) {
                self.plugin_midi_connections.push(new_conn);
            }
        }

        self.invalidate_midi_route_cache();
        Ok(())
    }

    pub fn disconnect_plugin_midi(
        &mut self,
        from_node: PluginGraphNode,
        from_port: usize,
        to_node: PluginGraphNode,
        to_port: usize,
    ) -> Result<(), String> {
        let source = self.plugin_midi_source_io(&from_node, from_port)?;
        let target = self.plugin_midi_target_io(&to_node, to_port)?;
        MIDIIO::disconnect(&source, &target)?;

        if !(matches!(from_node, PluginGraphNode::TrackInput)
            && matches!(to_node, PluginGraphNode::TrackOutput))
        {
            let before = self.plugin_midi_connections.len();
            self.plugin_midi_connections.retain(|c| {
                !(c.kind == Kind::MIDI
                    && c.from_node == from_node
                    && c.from_port == from_port
                    && c.to_node == to_node
                    && c.to_port == to_port)
            });
            if self.plugin_midi_connections.len() == before {
                return Err("MIDI plugin graph connection not found".to_string());
            }
        }

        self.invalidate_midi_route_cache();
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) fn lv2_audio_output_io(
        &self,
        instance_id: usize,
        _port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            self.lv2_plugins
                .iter()
                .find(|instance| instance.id == instance_id)
                .and_then(|instance| {
                    instance
                        .processor
                        .lock()
                        .audio_outputs()
                        .get(_port)
                        .cloned()
                })
                .ok_or_else(|| format!("Plugin instance {instance_id} output port {_port} missing"))
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            Err("LV2 plugins are not supported on this platform".to_string())
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) fn lv2_audio_input_io(
        &self,
        instance_id: usize,
        _port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            self.lv2_plugins
                .iter()
                .find(|instance| instance.id == instance_id)
                .and_then(|instance| instance.processor.lock().audio_inputs().get(_port).cloned())
                .ok_or_else(|| format!("Plugin instance {instance_id} input port {_port} missing"))
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            Err("LV2 plugins are not supported on this platform".to_string())
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) fn lv2_validate_midi_output(
        &self,
        instance_id: usize,
        _port: usize,
    ) -> Result<(), String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            self.lv2_plugins
                .iter()
                .find(|instance| instance.id == instance_id)
                .and_then(|instance| {
                    (_port < instance.processor.lock().midi_output_count()).then_some(())
                })
                .ok_or_else(|| {
                    format!("Plugin instance {instance_id} MIDI output port {_port} missing")
                })
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            Err("LV2 plugins are not supported on this platform".to_string())
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) fn lv2_validate_midi_input(
        &self,
        instance_id: usize,
        _port: usize,
    ) -> Result<(), String> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            self.lv2_plugins
                .iter()
                .find(|instance| instance.id == instance_id)
                .and_then(|instance| {
                    (_port < instance.processor.lock().midi_input_count()).then_some(())
                })
                .ok_or_else(|| {
                    format!("Plugin instance {instance_id} MIDI input port {_port} missing")
                })
        }
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        {
            Err("LV2 plugins are not supported on this platform".to_string())
        }
    }

    pub(crate) fn vst3_audio_output_io(
        &self,
        instance_id: usize,
        port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        self.vst3_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| instance.processor.lock().audio_outputs().get(port).cloned())
            .ok_or_else(|| format!("VST3 instance {instance_id} output port {port} missing"))
    }

    pub(crate) fn vst3_audio_input_io(
        &self,
        instance_id: usize,
        port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        self.vst3_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| instance.processor.lock().audio_inputs().get(port).cloned())
            .ok_or_else(|| format!("VST3 instance {instance_id} input port {port} missing"))
    }

    pub(crate) fn clap_audio_output_io(
        &self,
        instance_id: usize,
        port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        self.clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| instance.processor.lock().audio_outputs().get(port).cloned())
            .ok_or_else(|| format!("CLAP instance {instance_id} output port {port} missing"))
    }

    pub(crate) fn clap_audio_input_io(
        &self,
        instance_id: usize,
        port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        self.clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| instance.processor.lock().audio_inputs().get(port).cloned())
            .ok_or_else(|| format!("CLAP instance {instance_id} input port {port} missing"))
    }

    pub(crate) fn vst3_validate_midi_output(
        &self,
        instance_id: usize,
        port: usize,
    ) -> Result<(), String> {
        self.vst3_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| {
                (port < instance.processor.lock().midi_output_count()).then_some(())
            })
            .ok_or_else(|| format!("VST3 instance {instance_id} MIDI output port {port} missing"))
    }

    pub(crate) fn clap_validate_midi_output(
        &self,
        instance_id: usize,
        port: usize,
    ) -> Result<(), String> {
        self.clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| {
                (port < instance.processor.lock().midi_output_count()).then_some(())
            })
            .ok_or_else(|| format!("CLAP instance {instance_id} MIDI output port {port} missing"))
    }

    pub(crate) fn vst3_validate_midi_input(
        &self,
        instance_id: usize,
        port: usize,
    ) -> Result<(), String> {
        self.vst3_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| {
                (port < instance.processor.lock().midi_input_count()).then_some(())
            })
            .ok_or_else(|| format!("VST3 instance {instance_id} MIDI input port {port} missing"))
    }

    pub(crate) fn clap_validate_midi_input(
        &self,
        instance_id: usize,
        port: usize,
    ) -> Result<(), String> {
        self.clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| {
                (port < instance.processor.lock().midi_input_count()).then_some(())
            })
            .ok_or_else(|| format!("CLAP instance {instance_id} MIDI input port {port} missing"))
    }

    pub(crate) fn clap_midi_output_io(
        &self,
        instance_id: usize,
        port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        self.clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| {
                instance
                    .processor
                    .lock()
                    .midi_output_ports()
                    .get(port)
                    .cloned()
            })
            .ok_or_else(|| format!("CLAP instance {instance_id} MIDI output port {port} missing"))
    }

    pub(crate) fn clap_midi_input_io(
        &self,
        instance_id: usize,
        port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        self.clap_plugins
            .iter()
            .find(|instance| instance.id == instance_id)
            .and_then(|instance| {
                instance
                    .processor
                    .lock()
                    .midi_input_ports()
                    .get(port)
                    .cloned()
            })
            .ok_or_else(|| format!("CLAP instance {instance_id} MIDI input port {port} missing"))
    }

    pub(crate) fn vst3_midi_output_io(
        &self,
        _instance_id: usize,
        _port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        Err("VST3 MIDI output ports not yet implemented".to_string())
    }

    pub(crate) fn vst3_midi_input_io(
        &self,
        _instance_id: usize,
        _port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        Err("VST3 MIDI input ports not yet implemented".to_string())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) fn lv2_midi_output_io(
        &self,
        _instance_id: usize,
        _port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        Err("LV2 MIDI output ports not yet implemented".to_string())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) fn lv2_midi_input_io(
        &self,
        _instance_id: usize,
        _port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        Err("LV2 MIDI input ports not yet implemented".to_string())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    pub(crate) fn lv2_instance_id_exists(&self, id: usize) -> bool {
        self.lv2_plugins.iter().any(|i| i.id == id)
    }

    #[cfg(not(all(unix, not(target_os = "macos"))))]
    pub(crate) fn lv2_instance_id_exists(&self, _id: usize) -> bool {
        false
    }

    pub fn plugin_source_io(
        &self,
        node: &PluginGraphNode,
        port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        match node {
            PluginGraphNode::TrackInput => self
                .audio
                .ins
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track input port {port} not found")),
            PluginGraphNode::TrackOutput => Err("Track output node cannot be source".to_string()),
            PluginGraphNode::ClapPluginInstance(instance_id) => {
                self.clap_audio_output_io(*instance_id, port)
            }
            PluginGraphNode::Vst3PluginInstance(instance_id) => {
                self.vst3_audio_output_io(*instance_id, port)
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(instance_id) => {
                self.lv2_audio_output_io(*instance_id, port)
            }
        }
    }

    pub fn plugin_target_io(
        &self,
        node: &PluginGraphNode,
        port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        match node {
            PluginGraphNode::TrackInput => Err("Track input node cannot be target".to_string()),
            PluginGraphNode::TrackOutput => self
                .audio
                .outs
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track output port {port} not found")),
            PluginGraphNode::ClapPluginInstance(instance_id) => {
                self.clap_audio_input_io(*instance_id, port)
            }
            PluginGraphNode::Vst3PluginInstance(instance_id) => {
                self.vst3_audio_input_io(*instance_id, port)
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(instance_id) => {
                self.lv2_audio_input_io(*instance_id, port)
            }
        }
    }

    pub fn plugin_midi_source_io(
        &self,
        node: &PluginGraphNode,
        port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        match node {
            PluginGraphNode::TrackInput => self
                .midi
                .ins
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track MIDI input port {port} not found")),
            PluginGraphNode::TrackOutput => {
                Err("Track output node cannot be MIDI source".to_string())
            }
            PluginGraphNode::ClapPluginInstance(instance_id) => {
                self.clap_midi_output_io(*instance_id, port)
            }
            PluginGraphNode::Vst3PluginInstance(instance_id) => {
                self.vst3_midi_output_io(*instance_id, port)
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(instance_id) => {
                self.lv2_midi_output_io(*instance_id, port)
            }
        }
    }

    pub fn plugin_midi_target_io(
        &self,
        node: &PluginGraphNode,
        port: usize,
    ) -> Result<Arc<UnsafeMutex<Box<MIDIIO>>>, String> {
        match node {
            PluginGraphNode::TrackInput => {
                Err("Track input node cannot be MIDI target".to_string())
            }
            PluginGraphNode::TrackOutput => self
                .midi
                .outs
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track MIDI output port {port} not found")),
            PluginGraphNode::ClapPluginInstance(instance_id) => {
                self.clap_midi_input_io(*instance_id, port)
            }
            PluginGraphNode::Vst3PluginInstance(instance_id) => {
                self.vst3_midi_input_io(*instance_id, port)
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(instance_id) => {
                self.lv2_midi_input_io(*instance_id, port)
            }
        }
    }

    pub(crate) fn validate_plugin_midi_source(
        &self,
        node: &PluginGraphNode,
        port: usize,
    ) -> Result<(), String> {
        match node {
            PluginGraphNode::TrackInput => self
                .midi
                .ins
                .get(port)
                .map(|_| ())
                .ok_or_else(|| format!("Track MIDI input port {port} not found")),
            PluginGraphNode::TrackOutput => {
                Err("Track output node cannot be MIDI source".to_string())
            }
            PluginGraphNode::ClapPluginInstance(instance_id) => {
                self.clap_validate_midi_output(*instance_id, port)
            }
            PluginGraphNode::Vst3PluginInstance(instance_id) => {
                self.vst3_validate_midi_output(*instance_id, port)
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(instance_id) => {
                self.lv2_validate_midi_output(*instance_id, port)
            }
        }
    }

    pub(crate) fn validate_plugin_midi_target(
        &self,
        node: &PluginGraphNode,
        port: usize,
    ) -> Result<(), String> {
        match node {
            PluginGraphNode::TrackInput => {
                Err("Track input node cannot be MIDI target".to_string())
            }
            PluginGraphNode::TrackOutput => self
                .midi
                .outs
                .get(port)
                .map(|_| ())
                .ok_or_else(|| format!("Track MIDI output port {port} not found")),
            PluginGraphNode::ClapPluginInstance(instance_id) => {
                self.clap_validate_midi_input(*instance_id, port)
            }
            PluginGraphNode::Vst3PluginInstance(instance_id) => {
                self.vst3_validate_midi_input(*instance_id, port)
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(instance_id) => {
                self.lv2_validate_midi_input(*instance_id, port)
            }
        }
    }

    pub(crate) fn plugin_connected_neighbors(
        &self,
        kind: Kind,
        current_node: &PluginGraphNode,
    ) -> Vec<PluginGraphNode> {
        let mut nodes = HashSet::new();
        for conn in self.plugin_graph_connections() {
            if conn.kind == kind && &conn.from_node == current_node {
                nodes.insert(conn.to_node);
            }
        }
        nodes.into_iter().collect()
    }

    pub fn push_hw_midi_events(&mut self, events: &[MidiEvent]) {
        let Some(input) = self.midi.ins.first() else {
            return;
        };
        if events.is_empty() {
            return;
        }
        input.lock().buffer.extend_from_slice(events);
    }

    pub fn push_hw_midi_events_to_port(&mut self, port: usize, events: &[MidiEvent]) {
        let Some(input) = self.midi.ins.get(port) else {
            return;
        };
        if events.is_empty() {
            return;
        }
        input.lock().buffer.extend_from_slice(events);
    }

    pub(crate) fn collect_track_input_midi_events(&mut self) -> Vec<Vec<MidiEvent>> {
        let mut events: Vec<Vec<MidiEvent>> = Vec::with_capacity(self.midi.ins.len());
        self.record_tap_midi_in.clear();
        let midi_disk_active = self.midi_disk_monitor.iter().any(|&m| m);
        let clip_playback_active = midi_disk_active && self.clip_playback_enabled;
        for (lane, input) in self.midi.ins.iter().enumerate() {
            let input_lock = input.lock();
            self.record_tap_midi_in
                .extend(input_lock.buffer.iter().cloned());
            let monitor = self.midi_input_monitor.get(lane).copied().unwrap_or(false);
            if clip_playback_active && !monitor {
                input_lock.buffer.clear();
            } else if (monitor || self.record_tap_enabled)
                && let Some(Some(channel)) = self.midi_lane_channels.get(lane)
            {
                input_lock
                    .buffer
                    .retain(|event| Self::event_matches_midi_channel(event, *channel));
            }
            input_lock.buffer.sort_by_key(|event| event.frame);
            input_lock.mark_finished();
            events.push(input_lock.buffer.clone());
        }
        self.record_tap_midi_in.sort_by_key(|e| e.frame);
        events
    }

    pub(crate) fn event_matches_midi_channel(event: &MidiEvent, channel: u8) -> bool {
        let Some(status) = event.data.first().copied() else {
            return true;
        };
        if !(0x80..=0xEF).contains(&status) {
            return true;
        }
        (status & 0x0F) == channel.min(15)
    }

    pub(crate) fn route_track_inputs_to_track_outputs(&mut self, _input_events: &[Vec<MidiEvent>]) {
        for out in &self.midi.outs {
            out.lock().buffer.clear();
        }
        if !self.output_enabled || self.is_folder {
            return;
        }
        for out in &self.midi.outs {
            out.lock().process();
        }
    }

    pub(crate) fn route_modulator_midi_to_track_outputs(&mut self) {
        if self.pending_modulator_midi_events.is_empty() {
            return;
        }
        let events = std::mem::take(&mut self.pending_modulator_midi_events);
        if !self.output_enabled {
            return;
        }
        for out in &self.midi.outs {
            out.lock().buffer.extend_from_slice(&events);
        }
    }

    pub(crate) fn route_automation_midi_to_track_outputs(&mut self) {
        if self.pending_automation_midi_events.is_empty() {
            return;
        }
        let events = std::mem::take(&mut self.pending_automation_midi_events);
        if !self.output_enabled {
            return;
        }
        for out in &self.midi.outs {
            out.lock().buffer.extend_from_slice(&events);
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn route_plugin_midi_to_track_outputs(&self, plugin_events: &[MidiEvent]) {
        if !self.output_enabled || plugin_events.is_empty() {
            return;
        }
        for out in &self.midi.outs {
            out.lock().buffer.extend_from_slice(plugin_events);
        }
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn route_clap_midi_to_track_outputs(&self, plugin_events: &[ClapMidiOutputEvent]) {
        if !self.output_enabled || plugin_events.is_empty() {
            return;
        }
        for event in plugin_events {
            let port = event.port.min(self.midi.outs.len().saturating_sub(1));
            let Some(out) = self.midi.outs.get(port) else {
                continue;
            };
            out.lock().buffer.push(event.event.clone());
        }
    }

    pub(crate) fn process_track_plugins_in_graph_order(&mut self, frames: usize) {
        let track_input_events = self.folder_input_midi_events.clone();
        let order = self.plugin_process_order();
        let mut processed = HashSet::<(PluginKind, usize)>::new();
        self.folder_processed_midi_plugins.clear();
        self.folder_plugin_midi_node_events.clear();
        let echoed = self.echoed_parameter_updates.lock();
        echoed.clear();
        let track_name = self.name.clone();

        while processed.len() < order.len() {
            let mut progressed = false;
            for &(kind, idx) in &order {
                if processed.contains(&(kind, idx)) {
                    continue;
                }
                match kind {
                    PluginKind::Clap => {
                        let processor = self.clap_plugins[idx].processor.lock();
                        if !processor.audio_inputs().iter().all(|input| input.ready()) {
                            continue;
                        }
                        let node = PluginGraphNode::ClapPluginInstance(self.clap_plugins[idx].id);
                        for input in processor.audio_inputs() {
                            input.process();
                        }
                        self.plugin_midi_input_events(
                            &node,
                            processor.midi_input_count(),
                            &track_input_events,
                            &self.folder_plugin_midi_node_events,
                        );
                        let outputs = processor.process_with_midi(
                            frames,
                            &[],
                            crate::plugins::types::ClapTransportInfo {
                                transport_sample: self.transport_sample,
                                playing: (self.disk_monitor.iter().any(|&m| m)
                                    || self.midi_disk_monitor.iter().any(|&m| m))
                                    && self.clip_playback_enabled,
                                loop_enabled: self.loop_enabled,
                                loop_range_samples: self.loop_range_samples,
                                bpm: self.tempo_bpm,
                                tsig_num: self.tsig_num,
                                tsig_denom: self.tsig_denom,
                            },
                        );
                        for ev in processor.drain_echoed_parameters() {
                            echoed.push(crate::message::Action::TrackSetClapParameter {
                                track_name: track_name.clone(),
                                instance_id: self.clap_plugins[idx].id,
                                param_id: ev.param_index,
                                value: ev.value as f64,
                            });
                        }
                        for evt in outputs {
                            self.folder_plugin_midi_node_events
                                .entry((node.clone(), evt.port))
                                .or_default()
                                .push(evt.event);
                        }
                        self.folder_processed_midi_plugins.insert(node);
                    }
                    PluginKind::Vst3 => {
                        let processor = self.vst3_plugins[idx].processor.lock();
                        if !processor.audio_inputs().iter().all(|input| input.ready()) {
                            continue;
                        }
                        let node = PluginGraphNode::Vst3PluginInstance(self.vst3_plugins[idx].id);
                        for input in processor.audio_inputs() {
                            input.process();
                        }
                        let midi_inputs = self.plugin_midi_input_events(
                            &node,
                            processor.midi_input_count(),
                            &track_input_events,
                            &self.folder_plugin_midi_node_events,
                        );
                        let vst3_input = midi_inputs.first().cloned().unwrap_or_default();
                        let outputs = processor.process_with_midi(frames, &vst3_input);
                        for ev in processor.drain_echoed_parameters() {
                            echoed.push(crate::message::Action::TrackSetVst3Parameter {
                                track_name: track_name.clone(),
                                instance_id: self.vst3_plugins[idx].id,
                                param_id: ev.param_index,
                                value: ev.value,
                            });
                        }
                        if !outputs.is_empty() {
                            self.folder_plugin_midi_node_events
                                .insert((node.clone(), 0), outputs);
                        }
                        self.folder_processed_midi_plugins.insert(node);
                    }
                    #[cfg(all(unix, not(target_os = "macos")))]
                    PluginKind::Lv2 => {
                        let processor = self.lv2_plugins[idx].processor.lock();
                        if !processor.audio_inputs().iter().all(|input| input.ready()) {
                            continue;
                        }
                        let node = PluginGraphNode::Lv2PluginInstance(self.lv2_plugins[idx].id);
                        for input in processor.audio_inputs() {
                            input.process();
                        }
                        let midi_inputs = self.plugin_midi_input_events(
                            &node,
                            processor.midi_input_count(),
                            &track_input_events,
                            &self.folder_plugin_midi_node_events,
                        );
                        let lv2_input = midi_inputs.first().cloned().unwrap_or_default();
                        let outputs = processor.process_with_midi(frames, &lv2_input);
                        for ev in processor.drain_echoed_parameters() {
                            echoed.push(crate::message::Action::TrackSetLv2ControlValue {
                                track_name: track_name.clone(),
                                instance_id: self.lv2_plugins[idx].id,
                                index: ev.param_index,
                                value: ev.value,
                            });
                        }
                        if !outputs.is_empty() {
                            self.folder_plugin_midi_node_events
                                .insert((node.clone(), 0), outputs);
                        }
                        self.folder_processed_midi_plugins.insert(node);
                    }
                }
                processed.insert((kind, idx));
                progressed = true;
            }
            if !progressed {
                break;
            }
        }
    }

    pub(crate) fn plugin_midi_ready(
        &self,
        node: &PluginGraphNode,
        processed: &HashSet<PluginGraphNode>,
    ) -> bool {
        self.plugin_midi_connections
            .iter()
            .filter(|conn| {
                if conn.kind != Kind::MIDI || &conn.to_node != node {
                    return false;
                }
                let is_plugin = matches!(
                    conn.from_node,
                    PluginGraphNode::ClapPluginInstance(_) | PluginGraphNode::Vst3PluginInstance(_)
                );
                #[cfg(all(unix, not(target_os = "macos")))]
                let is_plugin =
                    is_plugin || matches!(conn.from_node, PluginGraphNode::Lv2PluginInstance(_));
                is_plugin
            })
            .all(|conn| processed.contains(&conn.from_node))
    }

    pub(crate) fn plugin_midi_input_events(
        &self,
        node: &PluginGraphNode,
        midi_inputs: usize,
        _track_input_events: &[Vec<MidiEvent>],
        _node_events: &HashMap<(PluginGraphNode, usize), Vec<MidiEvent>>,
    ) -> Vec<Vec<MidiEvent>> {
        let ports = self.plugin_midi_input_ports_for_node(node);
        let mut per_port: Vec<Vec<MidiEvent>> = ports
            .iter()
            .map(|port| {
                let lock = port.lock();
                lock.process();
                lock.buffer.clone()
            })
            .collect();
        if per_port.len() < midi_inputs {
            per_port.resize_with(midi_inputs, Vec::new);
        }
        per_port
    }

    pub(crate) fn plugin_midi_input_ports_for_node(
        &self,
        node: &PluginGraphNode,
    ) -> Vec<Arc<UnsafeMutex<Box<MIDIIO>>>> {
        match node {
            PluginGraphNode::ClapPluginInstance(instance_id) => self
                .clap_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .map(|instance| instance.processor.lock().midi_input_ports().to_vec())
                .unwrap_or_default(),
            PluginGraphNode::Vst3PluginInstance(instance_id) => self
                .vst3_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .map(|instance| instance.processor.lock().midi_input_ports().to_vec())
                .unwrap_or_default(),
            #[cfg(all(unix, not(target_os = "macos")))]
            PluginGraphNode::Lv2PluginInstance(instance_id) => self
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .map(|instance| instance.processor.lock().midi_input_ports().to_vec())
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    pub(crate) fn route_plugin_midi_to_track_outputs_graph(
        &self,
        _track_input_events: &[Vec<MidiEvent>],
        node_events: &HashMap<(PluginGraphNode, usize), Vec<MidiEvent>>,
    ) {
        if !self.output_enabled {
            return;
        }
        for conn in self
            .plugin_midi_connections
            .iter()
            .filter(|conn| conn.kind == Kind::MIDI && conn.to_node == PluginGraphNode::TrackOutput)
        {
            // Track input -> output is handled by MIDIIO::process on the output.
            // CLAP plugin outputs already feed track outputs through their MIDIIO ports.
            if conn.from_node == PluginGraphNode::TrackInput
                || matches!(conn.from_node, PluginGraphNode::ClapPluginInstance(_))
            {
                continue;
            }
            let Some(out) = self.midi.outs.get(conn.to_port) else {
                continue;
            };
            if let Some(events) = node_events.get(&(conn.from_node.clone(), conn.from_port)) {
                out.lock().buffer.extend_from_slice(events);
            }
        }
    }

    pub(crate) fn clear_local_midi_inputs(&self) {
        for input in &self.midi.ins {
            input.lock().buffer.clear();
        }
    }

    pub(crate) fn collect_hw_midi_output_events(&mut self) {
        self.pending_hw_midi_out_events.clear();
        for (port, out) in self.midi.outs.iter().enumerate() {
            self.pending_hw_midi_out_events.extend(
                out.lock()
                    .buffer
                    .iter()
                    .cloned()
                    .map(|event| HwMidiOutEvent { port, event }),
            );
        }
    }

    pub fn take_hw_midi_out_events(&mut self) -> Vec<HwMidiOutEvent> {
        std::mem::take(&mut self.pending_hw_midi_out_events)
    }
}
