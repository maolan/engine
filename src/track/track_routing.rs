#[cfg(target_os = "macos")]
use crate::clap::ClapMidiOutputEvent;
use crate::connectable::{ConnectableConnection, ConnectableRef};
use crate::message::{PluginGraphConnection, PluginGraphNode};

use super::*;
use crate::kind::Kind;
use crate::{audio::io::AudioIO, midi::io::MIDIIO};
use std::sync::Arc;

impl Track {
    pub fn clear_default_passthrough(&mut self) {
        for (audio_in, audio_out) in self.audio.ins.iter().zip(self.audio.outs.iter()) {
            let _ = AudioIO::disconnect(audio_in, audio_out);
            let _ = AudioIO::disconnect(audio_out, audio_in);
        }
        for (midi_in, midi_out) in self.midi.ins.iter().zip(self.midi.outs.iter()) {
            let _ = MIDIIO::disconnect(midi_out, midi_in);
        }
        self.invalidate_audio_route_cache();
        self.invalidate_midi_route_cache();
    }

    pub(crate) fn current_buffer_size(&self) -> usize {
        self.audio
            .ins
            .first()
            .map(|io| io.buffer_size())
            .or_else(|| self.audio.outs.first().map(|io| io.buffer_size()))
            .unwrap_or(self.process_block_size())
    }

    pub fn set_force_realtime_domain(&mut self, forced: bool) {
        self.force_realtime_domain = forced;
    }

    pub fn set_shared_realtime_mixed(&mut self, mixed: bool) {
        self.shared_realtime_mixed = mixed;
    }

    pub fn is_realtime_domain(&self) -> bool {
        (self.armed()
            && (self.input_monitor().iter().any(|&m| m)
                || self.midi_input_monitor().iter().any(|&m| m)))
            || self.force_realtime_domain
    }

    pub fn add_audio_input(&mut self) -> Result<(), String> {
        let buffer_size = self.current_buffer_size();
        if buffer_size == 0 {
            return Err(format!("Track '{}' has no audio buffer size", self.name));
        }
        let _ = self.audio.add_input(buffer_size);
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn add_audio_output(&mut self) -> Result<(), String> {
        let buffer_size = self.current_buffer_size();
        if buffer_size == 0 {
            return Err(format!("Track '{}' has no audio buffer size", self.name));
        }
        let _ = self.audio.add_output(buffer_size);
        self.rt.record_tap_outs.push(vec![0.0; buffer_size]);
        self.rt.output_meter_linear_cache.push(0.0);
        self.rt.meter_peak_hold_linear.push(0.0);
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn remove_audio_input(&mut self) -> Result<(), String> {
        if self.audio.ins.len() <= self.primary_audio_ins() {
            return Err(format!(
                "Track '{}' has no removable return inputs",
                self.name
            ));
        }
        if let Some(input) = self.audio.ins.pop() {
            Self::disconnect_all(&input);
            for output in &self.audio.outs {
                output.update_connections(|conns| {
                    conns.retain(|source| !Arc::ptr_eq(source, &input));
                });
            }
            self.invalidate_audio_route_cache();
            Ok(())
        } else {
            Err(format!("Track '{}' input removal failed", self.name))
        }
    }

    pub fn remove_audio_output(
        &mut self,
        hw_outputs: &[Arc<AudioIO>],
        track_inputs: &[Arc<AudioIO>],
    ) -> Result<(), String> {
        if self.audio.outs.len() <= self.primary_audio_outs() {
            return Err(format!(
                "Track '{}' has no removable send outputs",
                self.name
            ));
        }
        let Some(output) = self.audio.outs.pop() else {
            return Err(format!("Track '{}' output removal failed", self.name));
        };
        for target in hw_outputs.iter().chain(track_inputs.iter()) {
            let _ = AudioIO::disconnect(&output, target);
        }
        self.rt.record_tap_outs.truncate(self.audio.outs.len());
        self.rt
            .output_meter_linear_cache
            .truncate(self.audio.outs.len());
        self.rt
            .meter_peak_hold_linear
            .truncate(self.audio.outs.len());
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn plugin_graph_connections(&self) -> Vec<PluginGraphConnection> {
        let mut source_ports: Vec<(PluginGraphNode, usize, Arc<AudioIO>)> = self
            .audio
            .ins
            .iter()
            .enumerate()
            .map(|(idx, io)| (PluginGraphNode::TrackInput, idx, io.clone()))
            .collect();
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            source_ports.extend(instance.processor.audio_outputs().iter().enumerate().map(
                |(idx, io)| {
                    (
                        #[cfg(all(unix, not(target_os = "macos")))]
                        PluginGraphNode::Lv2PluginInstance(instance.id),
                        idx,
                        io.clone(),
                    )
                },
            ));
        }
        for instance in &self.vst3_plugins {
            source_ports.extend(instance.processor.audio_outputs().iter().enumerate().map(
                |(idx, io)| {
                    (
                        PluginGraphNode::Vst3PluginInstance(instance.id),
                        idx,
                        io.clone(),
                    )
                },
            ));
        }
        for instance in &self.clap_plugins {
            source_ports.extend(instance.processor.audio_outputs().iter().enumerate().map(
                |(idx, io)| {
                    (
                        PluginGraphNode::ClapPluginInstance(instance.id),
                        idx,
                        io.clone(),
                    )
                },
            ));
        }

        let mut connections = vec![];
        for (to_port, to_io) in self.audio.outs.iter().enumerate() {
            for conn in to_io.connections().iter() {
                if let Some((from_node, from_port, _)) = source_ports
                    .iter()
                    .find(|(_, _, source_io)| Arc::ptr_eq(source_io, conn))
                {
                    connections.push(PluginGraphConnection {
                        from_node: from_node.clone(),
                        from_port: *from_port,
                        to_node: PluginGraphNode::TrackOutput,
                        to_port,
                        kind: Kind::Audio,
                    });
                }
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            for (to_port, to_io) in instance.processor.audio_inputs().iter().enumerate() {
                for conn in to_io.connections().iter() {
                    if let Some((from_node, from_port, _)) = source_ports
                        .iter()
                        .find(|(_, _, source_io)| Arc::ptr_eq(source_io, conn))
                    {
                        connections.push(PluginGraphConnection {
                            from_node: from_node.clone(),
                            from_port: *from_port,
                            #[cfg(all(unix, not(target_os = "macos")))]
                            to_node: PluginGraphNode::Lv2PluginInstance(instance.id),
                            to_port,
                            kind: Kind::Audio,
                        });
                    }
                }
            }
        }
        for instance in &self.vst3_plugins {
            for (to_port, to_io) in instance.processor.audio_inputs().iter().enumerate() {
                for conn in to_io.connections().iter() {
                    if let Some((from_node, from_port, _)) = source_ports
                        .iter()
                        .find(|(_, _, source_io)| Arc::ptr_eq(source_io, conn))
                    {
                        connections.push(PluginGraphConnection {
                            from_node: from_node.clone(),
                            from_port: *from_port,
                            to_node: PluginGraphNode::Vst3PluginInstance(instance.id),
                            to_port,
                            kind: Kind::Audio,
                        });
                    }
                }
            }
        }
        for instance in &self.clap_plugins {
            for (to_port, to_io) in instance.processor.audio_inputs().iter().enumerate() {
                for conn in to_io.connections().iter() {
                    if let Some((from_node, from_port, _)) = source_ports
                        .iter()
                        .find(|(_, _, source_io)| Arc::ptr_eq(source_io, conn))
                    {
                        connections.push(PluginGraphConnection {
                            from_node: from_node.clone(),
                            from_port: *from_port,
                            to_node: PluginGraphNode::ClapPluginInstance(instance.id),
                            to_port,
                            kind: Kind::Audio,
                        });
                    }
                }
            }
        }
        for (from_port, from_io) in self.midi.ins.iter().enumerate() {
            for conn in from_io.connections() {
                if let Some((to_port, _)) = self
                    .midi
                    .outs
                    .iter()
                    .enumerate()
                    .find(|(_, out_io)| Arc::ptr_eq(out_io, &conn))
                {
                    connections.push(PluginGraphConnection {
                        from_node: PluginGraphNode::TrackInput,
                        from_port,
                        to_node: PluginGraphNode::TrackOutput,
                        to_port,
                        kind: Kind::MIDI,
                    });
                }
            }
        }
        connections.extend(self.plugin_midi_connections.iter().cloned());
        connections
    }

    pub fn connectable_connections(&self) -> Vec<ConnectableConnection> {
        use crate::connectable::{AudioPorts, MidiPorts};
        let mut connections = Vec::new();

        // --- Audio ---
        let mut audio_sources: Vec<(Arc<AudioIO>, ConnectableRef, usize)> = Vec::new();
        for (port, io) in self.audio.ins.iter().enumerate() {
            audio_sources.push((io.clone(), ConnectableRef::TrackInput, port));
        }
        for (port, io) in self.audio.outs.iter().enumerate() {
            audio_sources.push((io.clone(), ConnectableRef::TrackOutput, port));
        }
        for child in &self.child_tracks {
            let child = child.lock();
            let name = child.name.clone();
            for (port, io) in child.audio_outputs().iter().enumerate() {
                audio_sources.push((io.clone(), ConnectableRef::ChildTrack(name.clone()), port));
            }
        }
        for instance in &self.clap_plugins {
            for (port, io) in instance.audio_outputs().iter().enumerate() {
                audio_sources.push((io.clone(), ConnectableRef::ClapPlugin(instance.id), port));
            }
        }
        for instance in &self.vst3_plugins {
            for (port, io) in instance.audio_outputs().iter().enumerate() {
                audio_sources.push((io.clone(), ConnectableRef::Vst3Plugin(instance.id), port));
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            for (port, io) in instance.audio_outputs().iter().enumerate() {
                audio_sources.push((io.clone(), ConnectableRef::Lv2Plugin(instance.id), port));
            }
        }

        let find_audio_source = |io: &Arc<AudioIO>| {
            audio_sources
                .iter()
                .find(|(candidate, _, _)| Arc::ptr_eq(candidate, io))
                .map(|(_, r, p)| (r.clone(), *p))
        };

        let mut report_audio_targets = |targets: Vec<Arc<AudioIO>>, target_ref: ConnectableRef| {
            for (port, target) in targets.iter().enumerate() {
                let source_list = target.connections();
                for source in source_list.iter() {
                    if let Some((from_ref, from_port)) = find_audio_source(source) {
                        connections.push(ConnectableConnection {
                            from: from_ref,
                            from_port,
                            to: target_ref.clone(),
                            to_port: port,
                            kind: Kind::Audio,
                        });
                    }
                }
            }
        };

        report_audio_targets(self.audio_outputs(), ConnectableRef::TrackOutput);
        for child in &self.child_tracks {
            let child = child.lock();
            let name = child.name.clone();
            report_audio_targets(child.audio_inputs(), ConnectableRef::ChildTrack(name));
        }
        for instance in &self.clap_plugins {
            report_audio_targets(
                instance.audio_inputs(),
                ConnectableRef::ClapPlugin(instance.id),
            );
        }
        for instance in &self.vst3_plugins {
            report_audio_targets(
                instance.audio_inputs(),
                ConnectableRef::Vst3Plugin(instance.id),
            );
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            report_audio_targets(
                instance.audio_inputs(),
                ConnectableRef::Lv2Plugin(instance.id),
            );
        }

        // --- MIDI ---
        type MidiSource = (Arc<MIDIIO>, ConnectableRef, usize);
        let mut midi_sources: Vec<MidiSource> = Vec::new();
        for (port, io) in self.midi.ins.iter().enumerate() {
            midi_sources.push((io.clone(), ConnectableRef::TrackInput, port));
        }
        for (port, io) in self.midi.outs.iter().enumerate() {
            midi_sources.push((io.clone(), ConnectableRef::TrackOutput, port));
        }
        for child in &self.child_tracks {
            let child = child.lock();
            let name = child.name.clone();
            for (port, io) in child.midi_outputs().iter().enumerate() {
                midi_sources.push((io.clone(), ConnectableRef::ChildTrack(name.clone()), port));
            }
        }
        for instance in &self.clap_plugins {
            for (port, io) in instance.midi_outputs().iter().enumerate() {
                midi_sources.push((io.clone(), ConnectableRef::ClapPlugin(instance.id), port));
            }
        }
        for instance in &self.vst3_plugins {
            for (port, io) in instance.midi_outputs().iter().enumerate() {
                midi_sources.push((io.clone(), ConnectableRef::Vst3Plugin(instance.id), port));
            }
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            for (port, io) in instance.midi_outputs().iter().enumerate() {
                midi_sources.push((io.clone(), ConnectableRef::Lv2Plugin(instance.id), port));
            }
        }

        let find_midi_source = |io: &Arc<MIDIIO>| {
            midi_sources
                .iter()
                .find(|(candidate, _, _)| Arc::ptr_eq(candidate, io))
                .map(|(_, r, p)| (r.clone(), *p))
        };

        let mut report_midi_targets = |targets: Vec<Arc<MIDIIO>>, target_ref: ConnectableRef| {
            for (port, target) in targets.iter().enumerate() {
                let source_list = target.sources();
                for source in source_list {
                    if let Some((from_ref, from_port)) = find_midi_source(&source) {
                        connections.push(ConnectableConnection {
                            from: from_ref,
                            from_port,
                            to: target_ref.clone(),
                            to_port: port,
                            kind: Kind::MIDI,
                        });
                    }
                }
            }
        };

        report_midi_targets(self.midi_outputs(), ConnectableRef::TrackOutput);
        for child in &self.child_tracks {
            let child = child.lock();
            let name = child.name.clone();
            report_midi_targets(child.midi_inputs(), ConnectableRef::ChildTrack(name));
        }
        for instance in &self.clap_plugins {
            report_midi_targets(
                instance.midi_inputs(),
                ConnectableRef::ClapPlugin(instance.id),
            );
        }
        for instance in &self.vst3_plugins {
            report_midi_targets(
                instance.midi_inputs(),
                ConnectableRef::Vst3Plugin(instance.id),
            );
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            report_midi_targets(
                instance.midi_inputs(),
                ConnectableRef::Lv2Plugin(instance.id),
            );
        }

        connections
    }

    pub(crate) fn connectable_audio_output(
        &self,
        connectable: &ConnectableRef,
        port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        use crate::connectable::AudioPorts;
        match connectable {
            ConnectableRef::TrackInput => {
                Err("Track input cannot be used as an audio source".to_string())
            }
            ConnectableRef::TrackOutput => self
                .audio_outputs()
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track output audio port {port} not found")),
            ConnectableRef::ChildTrack(name) => self
                .child_tracks
                .iter()
                .find(|child| child.lock().name == *name)
                .and_then(|child| child.lock().audio_outputs().get(port).cloned())
                .ok_or_else(|| format!("Child track '{name}' audio output port {port} not found")),
            ConnectableRef::ClapPlugin(instance_id) => self
                .clap_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.audio_outputs().get(port).cloned())
                .ok_or_else(|| format!("CLAP plugin audio output port {port} not found")),
            ConnectableRef::Vst3Plugin(instance_id) => self
                .vst3_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.audio_outputs().get(port).cloned())
                .ok_or_else(|| format!("VST3 plugin audio output port {port} not found")),
            #[cfg(all(unix, not(target_os = "macos")))]
            ConnectableRef::Lv2Plugin(instance_id) => self
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.audio_outputs().get(port).cloned())
                .ok_or_else(|| format!("LV2 plugin audio output port {port} not found")),
        }
    }

    pub(crate) fn connectable_audio_input(
        &self,
        connectable: &ConnectableRef,
        port: usize,
    ) -> Result<Arc<AudioIO>, String> {
        use crate::connectable::AudioPorts;
        match connectable {
            ConnectableRef::TrackOutput => self
                .audio_outputs()
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track output audio port {port} not found")),
            ConnectableRef::TrackInput => self
                .audio_inputs()
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track input audio port {port} not found")),
            ConnectableRef::ChildTrack(name) => self
                .child_tracks
                .iter()
                .find(|child| child.lock().name == *name)
                .and_then(|child| child.lock().audio_inputs().get(port).cloned())
                .ok_or_else(|| format!("Child track '{name}' audio input port {port} not found")),
            ConnectableRef::ClapPlugin(instance_id) => self
                .clap_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.audio_inputs().get(port).cloned())
                .ok_or_else(|| format!("CLAP plugin audio input port {port} not found")),
            ConnectableRef::Vst3Plugin(instance_id) => self
                .vst3_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.audio_inputs().get(port).cloned())
                .ok_or_else(|| format!("VST3 plugin audio input port {port} not found")),
            #[cfg(all(unix, not(target_os = "macos")))]
            ConnectableRef::Lv2Plugin(instance_id) => self
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.audio_inputs().get(port).cloned())
                .ok_or_else(|| format!("LV2 plugin audio input port {port} not found")),
        }
    }

    pub(crate) fn connectable_midi_output(
        &self,
        connectable: &ConnectableRef,
        port: usize,
    ) -> Result<Arc<MIDIIO>, String> {
        use crate::connectable::MidiPorts;
        match connectable {
            ConnectableRef::TrackInput => {
                Err("Track input cannot be used as a MIDI source".to_string())
            }
            ConnectableRef::TrackOutput => self
                .midi_outputs()
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track output MIDI port {port} not found")),
            ConnectableRef::ChildTrack(name) => self
                .child_tracks
                .iter()
                .find(|child| child.lock().name == *name)
                .and_then(|child| child.lock().midi_outputs().get(port).cloned())
                .ok_or_else(|| format!("Child track '{name}' MIDI output port {port} not found")),
            ConnectableRef::ClapPlugin(instance_id) => self
                .clap_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.midi_outputs().get(port).cloned())
                .ok_or_else(|| format!("CLAP plugin MIDI output port {port} not found")),
            ConnectableRef::Vst3Plugin(instance_id) => self
                .vst3_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.midi_outputs().get(port).cloned())
                .ok_or_else(|| format!("VST3 plugin MIDI output port {port} not found")),
            #[cfg(all(unix, not(target_os = "macos")))]
            ConnectableRef::Lv2Plugin(instance_id) => self
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.midi_outputs().get(port).cloned())
                .ok_or_else(|| format!("LV2 plugin MIDI output port {port} not found")),
        }
    }

    pub(crate) fn connectable_midi_input(
        &self,
        connectable: &ConnectableRef,
        port: usize,
    ) -> Result<Arc<MIDIIO>, String> {
        use crate::connectable::MidiPorts;
        match connectable {
            ConnectableRef::TrackOutput => self
                .midi_outputs()
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track output MIDI port {port} not found")),
            ConnectableRef::TrackInput => self
                .midi_inputs()
                .get(port)
                .cloned()
                .ok_or_else(|| format!("Track input MIDI port {port} not found")),
            ConnectableRef::ChildTrack(name) => self
                .child_tracks
                .iter()
                .find(|child| child.lock().name == *name)
                .and_then(|child| child.lock().midi_inputs().get(port).cloned())
                .ok_or_else(|| format!("Child track '{name}' MIDI input port {port} not found")),
            ConnectableRef::ClapPlugin(instance_id) => self
                .clap_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.midi_inputs().get(port).cloned())
                .ok_or_else(|| format!("CLAP plugin MIDI input port {port} not found")),
            ConnectableRef::Vst3Plugin(instance_id) => self
                .vst3_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.midi_inputs().get(port).cloned())
                .ok_or_else(|| format!("VST3 plugin MIDI input port {port} not found")),
            #[cfg(all(unix, not(target_os = "macos")))]
            ConnectableRef::Lv2Plugin(instance_id) => self
                .lv2_plugins
                .iter()
                .find(|instance| instance.id == *instance_id)
                .and_then(|instance| instance.midi_inputs().get(port).cloned())
                .ok_or_else(|| format!("LV2 plugin MIDI input port {port} not found")),
        }
    }

    pub fn connect_audio_connectable(
        &mut self,
        from: ConnectableRef,
        from_port: usize,
        to: ConnectableRef,
        to_port: usize,
    ) -> Result<(), String> {
        let source = self.connectable_audio_output(&from, from_port)?;
        let target = self.connectable_audio_input(&to, to_port)?;
        if from == to && from_port == to_port {
            return Err("Cannot connect an audio port to itself".to_string());
        }
        AudioIO::connect(&source, &target);
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn disconnect_audio_connectable(
        &mut self,
        from: ConnectableRef,
        from_port: usize,
        to: ConnectableRef,
        to_port: usize,
    ) -> Result<(), String> {
        let source = self.connectable_audio_output(&from, from_port)?;
        let target = self.connectable_audio_input(&to, to_port)?;
        AudioIO::disconnect(&source, &target)?;
        self.invalidate_audio_route_cache();
        Ok(())
    }

    pub fn connect_midi_connectable(
        &mut self,
        from: ConnectableRef,
        from_port: usize,
        to: ConnectableRef,
        to_port: usize,
    ) -> Result<(), String> {
        let source = self.connectable_midi_output(&from, from_port)?;
        let target = self.connectable_midi_input(&to, to_port)?;
        if from == to && from_port == to_port {
            return Err("Cannot connect a MIDI port to itself".to_string());
        }
        MIDIIO::connect(&source, &target);
        self.invalidate_midi_route_cache();
        Ok(())
    }

    pub fn disconnect_midi_connectable(
        &mut self,
        from: ConnectableRef,
        from_port: usize,
        to: ConnectableRef,
        to_port: usize,
    ) -> Result<(), String> {
        let source = self.connectable_midi_output(&from, from_port)?;
        let target = self.connectable_midi_input(&to, to_port)?;
        MIDIIO::disconnect(&source, &target)?;
        self.invalidate_midi_route_cache();
        Ok(())
    }

    pub(crate) fn with_default_passthrough(mut self) -> Self {
        self.ensure_default_audio_passthrough();
        self.ensure_default_midi_passthrough();
        self
    }

    pub fn ensure_default_audio_passthrough(&mut self) {
        if self.is_folder {
            self.disconnect_audio_inputs_from_outputs();
            return;
        }
        if self.audio.ins.is_empty() {
            self.invalidate_audio_route_cache();
            return;
        }

        for audio_in in &self.audio.ins {
            audio_in.update_connections(|conns| {
                conns.retain(|conn| !self.audio.outs.iter().any(|out| Arc::ptr_eq(out, conn)));
            });
        }

        for (out_idx, audio_out) in self.audio.outs.iter().enumerate() {
            let source_idx = out_idx.min(self.audio.ins.len().saturating_sub(1));
            let audio_in = &self.audio.ins[source_idx];
            audio_out.update_connections(|conns| {
                conns.retain(|conn| !self.audio.ins.iter().any(|input| Arc::ptr_eq(input, conn)));
                if !conns.iter().any(|conn| Arc::ptr_eq(conn, audio_in)) {
                    conns.push(audio_in.clone());
                }
            });
        }
        self.invalidate_audio_route_cache();
    }

    pub(crate) fn disconnect_audio_inputs_from_outputs(&mut self) {
        for audio_in in &self.audio.ins {
            audio_in.update_connections(|conns| {
                conns.retain(|conn| !self.audio.outs.iter().any(|out| Arc::ptr_eq(out, conn)));
            });
        }
        for audio_out in &self.audio.outs {
            audio_out.update_connections(|conns| {
                conns.retain(|conn| !self.audio.ins.iter().any(|input| Arc::ptr_eq(input, conn)));
            });
        }
        self.invalidate_audio_route_cache();
    }

    pub fn ensure_default_midi_passthrough(&mut self) {
        if self.is_folder {
            self.disconnect_midi_inputs_from_outputs();
            return;
        }
        let count = self.midi.ins.len().min(self.midi.outs.len());
        for port in 0..count {
            let _ = self.connect_plugin_midi(
                PluginGraphNode::TrackInput,
                port,
                PluginGraphNode::TrackOutput,
                port,
            );
        }
    }

    pub(crate) fn disconnect_midi_inputs_from_outputs(&mut self) {
        let count = self.midi.ins.len().min(self.midi.outs.len());
        for port in 0..count {
            let _ = self.disconnect_plugin_midi(
                PluginGraphNode::TrackInput,
                port,
                PluginGraphNode::TrackOutput,
                port,
            );
        }
    }

    pub fn connect_outputs_to_parent(&mut self, parent: &Track) {
        for (out_idx, child_out) in self.audio.outs.iter().enumerate() {
            if let Some(parent_in) = parent.audio.ins.get(out_idx) {
                let already_connected = child_out
                    .connections()
                    .iter()
                    .any(|conn| Arc::ptr_eq(conn, parent_in));
                if !already_connected {
                    AudioIO::connect(child_out, parent_in);
                }
            }
        }
        self.invalidate_audio_route_cache();
    }

    pub fn disconnect_from_parent(&mut self, parent: &Track) {
        // Folder input -> child input
        for (in_idx, child_in) in self.audio.ins.iter().enumerate() {
            if let Some(parent_in) = parent.audio.ins.get(in_idx) {
                let _ = AudioIO::disconnect(parent_in, child_in);
            }
        }
        // Child output -> folder output
        for (out_idx, child_out) in self.audio.outs.iter().enumerate() {
            if let Some(parent_out) = parent.audio.outs.get(out_idx) {
                let _ = AudioIO::disconnect(child_out, parent_out);
            }
        }
        // Folder MIDI input -> child MIDI input
        for (in_idx, child_in) in self.midi.ins.iter().enumerate() {
            if let Some(parent_in) = parent.midi.ins.get(in_idx) {
                let _ = MIDIIO::disconnect(parent_in, child_in);
            }
        }
        // Child MIDI output -> folder MIDI output
        for (out_idx, child_out) in self.midi.outs.iter().enumerate() {
            if let Some(parent_out) = parent.midi.outs.get(out_idx) {
                let _ = MIDIIO::disconnect(child_out, parent_out);
            }
        }
        self.invalidate_audio_route_cache();
        self.invalidate_midi_route_cache();
    }

    pub(crate) fn internal_audio_sources(&self) -> Vec<Arc<AudioIO>> {
        // Folder tracks aggregate their children; their own inputs must not feed their outputs.
        let mut sources = if self.is_folder {
            Vec::new()
        } else {
            self.audio.ins.clone()
        };
        if let Some(src) = self.metronome_source.load_full() {
            sources.push(src);
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        for instance in &self.lv2_plugins {
            sources.extend(instance.processor.audio_outputs().iter().cloned());
        }
        for instance in &self.vst3_plugins {
            sources.extend(instance.processor.audio_outputs().iter().cloned());
        }
        for instance in &self.clap_plugins {
            sources.extend(instance.processor.audio_outputs().iter().cloned());
        }
        for child in &self.child_tracks {
            let child = child.lock();
            sources.extend(child.audio.outs.iter().cloned());
        }
        sources
    }

    pub(crate) fn is_track_input_source(&self, source: &Arc<AudioIO>) -> bool {
        self.audio
            .ins
            .iter()
            .any(|input| Arc::ptr_eq(input, source))
    }

    pub(crate) fn disconnect_all(port: &Arc<AudioIO>) {
        let connections = port.connections();
        for other in connections.iter() {
            let _ = AudioIO::disconnect(other, port);
        }
    }
}
