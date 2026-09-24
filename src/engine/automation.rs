use super::*;
use crate::message::{OfflineAutomationLane, OfflineAutomationPoint};
use mixosc::parameters::{OscValue, build_set};

impl Engine {
    pub(crate) fn compute_modulator_values(
        &self,
        sample: usize,
    ) -> Arc<std::collections::HashMap<usize, f32>> {
        let sample_rate = self.sample_rate();
        let (bpm, tsig_num, tsig_denom) = self.transport.timing_at_sample(sample);
        let values: std::collections::HashMap<usize, f32> = self
            .automation
            .modulators
            .iter()
            .filter(|m| m.enabled)
            .map(|m| {
                (
                    m.id,
                    m.value_at(sample, sample_rate, bpm, tsig_num, tsig_denom),
                )
            })
            .collect();
        Arc::new(values)
    }

    pub(crate) fn apply_modulators(&mut self, sample: usize) -> Vec<Action> {
        use crate::modulator::ModulatorTarget;
        let values = self.compute_modulator_values(sample);
        self.automation.modulator_values = Some(values.clone());
        let mut echoes = Vec::new();
        let mut per_track: HashMap<String, (Option<f32>, Option<f32>)> = HashMap::new();
        let mut clap_params: HashMap<(String, usize, u32), f64> = HashMap::new();
        let mut vst3_params: HashMap<(String, usize, u32), f32> = HashMap::new();
        #[cfg(unix)]
        let mut lv2_params: HashMap<(String, usize, u32), f32> = HashMap::new();
        let mut midi_cc_events: HashMap<String, Vec<MidiEvent>> = HashMap::new();

        let map_f32 = |value: f32, min: f32, max: f32| -> f32 {
            crate::modulator::map_value(value, min, max)
        };
        let map_f64 = |value: f32, min: f64, max: f64| -> f64 {
            crate::modulator::map_value_f64(value, min, max)
        };

        for m in &self.automation.modulators {
            if !m.enabled {
                continue;
            }
            let Some(&value) = values.get(&m.id) else {
                continue;
            };
            for target in &m.targets {
                match target {
                    ModulatorTarget::TrackVolume {
                        track_name,
                        min,
                        max,
                    } => {
                        let clamped = map_f32(value, *min, *max);
                        per_track.entry(track_name.clone()).or_default().0 = Some(clamped);
                    }
                    ModulatorTarget::TrackBalance {
                        track_name,
                        min,
                        max,
                    } => {
                        let clamped = map_f32(value, *min, *max);
                        per_track.entry(track_name.clone()).or_default().1 = Some(clamped);
                    }
                    ModulatorTarget::HwOutVolume { min, max } => {
                        let clamped = map_f32(value, *min, *max);
                        if (self.meters.hw_out_level_db - clamped).abs() > f32::EPSILON {
                            self.meters.hw_out_level_db = clamped;
                            echoes
                                .push(Action::TrackAutomationLevel("hw:out".to_string(), clamped));
                        }
                    }
                    ModulatorTarget::HwOutBalance { min, max } => {
                        let next = map_f32(value, *min, *max).clamp(-1.0, 1.0);
                        if (self.meters.hw_out_balance - next).abs() > f32::EPSILON {
                            self.meters.hw_out_balance = next;
                            echoes.push(Action::TrackAutomationBalance("hw:out".to_string(), next));
                        }
                    }
                    ModulatorTarget::ClapParameter {
                        track_name,
                        instance_id,
                        param_id,
                        min,
                        max,
                    } => {
                        let param_value = map_f64(value, *min, *max);
                        clap_params
                            .insert((track_name.clone(), *instance_id, *param_id), param_value);
                    }
                    ModulatorTarget::Vst3Parameter {
                        track_name,
                        instance_id,
                        param_id,
                        min,
                        max,
                    } => {
                        let param_value = map_f32(value, *min, *max);
                        vst3_params
                            .insert((track_name.clone(), *instance_id, *param_id), param_value);
                    }
                    #[cfg(unix)]
                    ModulatorTarget::Lv2Parameter {
                        track_name,
                        instance_id,
                        index,
                        min,
                        max,
                    } => {
                        let param_value = map_f32(value, *min, *max);
                        lv2_params.insert((track_name.clone(), *instance_id, *index), param_value);
                    }
                    ModulatorTarget::MidiCc {
                        track_name,
                        channel,
                        cc,
                    } => {
                        let cc_value = (value * 127.0).round() as u8;
                        midi_cc_events
                            .entry(track_name.clone())
                            .or_default()
                            .push(MidiEvent::new(
                                0,
                                vec![0xB0 | (*channel).min(15), (*cc).min(127), cc_value],
                            ));
                    }
                }
            }
        }
        let state = self.state_snapshot.load_full();
        for (track_name, (level, balance)) in per_track {
            if let Some(level) = level
                && let Some(track) = state.tracks.get(&track_name).cloned()
            {
                let t = track.lock();
                if (t.level() - level).abs() > f32::EPSILON {
                    t.set_level(level);
                    echoes.push(Action::TrackAutomationLevel(track_name.clone(), level));
                }
            }
            if let Some(balance) = balance
                && let Some(track) = state.tracks.get(&track_name).cloned()
            {
                let t = track.lock();
                let next = balance.clamp(-1.0, 1.0);
                if (t.balance() - next).abs() > f32::EPSILON {
                    t.set_balance(next);
                    echoes.push(Action::TrackAutomationBalance(track_name.clone(), next));
                }
            }
        }

        for (track_name, events) in midi_cc_events {
            if let Some(track) = state.tracks.get(&track_name).cloned() {
                track.lock().rt.pending_modulator_midi_events.extend(events);
            }
        }

        for ((track_name, instance_id, param_id), value) in clap_params {
            if let Some(track) = state.tracks.get(&track_name).cloned()
                && track
                    .lock()
                    .set_clap_parameter(instance_id, param_id, value)
                    .is_ok()
            {
                echoes.push(Action::TrackSetClapParameter {
                    track_name,
                    instance_id,
                    param_id,
                    value,
                });
            }
        }
        for ((track_name, instance_id, param_id), value) in vst3_params {
            if let Some(track) = state.tracks.get(&track_name).cloned()
                && track
                    .lock()
                    .set_vst3_parameter(instance_id, param_id, value)
                    .is_ok()
            {
                echoes.push(Action::TrackSetVst3Parameter {
                    track_name,
                    instance_id,
                    param_id,
                    value,
                });
            }
        }
        #[cfg(unix)]
        for ((track_name, instance_id, index), value) in lv2_params {
            if let Some(track) = state.tracks.get(&track_name).cloned()
                && track
                    .lock()
                    .set_lv2_control_value(instance_id, index as usize, f64::from(value))
                    .is_ok()
            {
                echoes.push(Action::TrackSetLv2ControlValue {
                    track_name,
                    instance_id,
                    index,
                    value,
                });
            }
        }

        echoes
    }

    /// Evaluates MixOSC automation lanes for tracks bound to a mixer and sends
    /// the current values over UDP/OSC. Called once per hardware cycle.
    pub(crate) fn apply_mixosc_automation(&mut self, sample: usize) {
        if !self.transport.playing {
            return;
        }

        let socket = match self.automation.mixosc_socket.as_ref() {
            Some(socket) => socket,
            None => match UdpSocket::bind("0.0.0.0:0") {
                Ok(socket) => {
                    self.automation.mixosc_socket = Some(socket);
                    self.automation.mixosc_socket.as_ref().unwrap()
                }
                Err(err) => {
                    tracing::warn!(%err, "Failed to bind MixOSC output socket");
                    return;
                }
            },
        };

        let state = self.state_snapshot.load_full();
        for (track_name, track) in state.tracks.iter() {
            let track_lock = track.lock();
            let Some(track_addr) = track_lock.mixosc_addr.as_ref() else {
                continue;
            };
            let lanes: Vec<crate::message::OfflineAutomationLane> =
                crate::engine::parse_automation_lanes(&track_lock.automation_lanes);
            for lane in lanes {
                if !lane.visible {
                    continue;
                }
                let crate::message::OfflineAutomationTarget::MixOsc {
                    addr: ref lane_addr,
                    path: ref lane_path,
                } = lane.target
                else {
                    continue;
                };
                if lane_addr != track_addr {
                    continue;
                }
                let Some(value) = lane.value_at(sample) else {
                    continue;
                };
                let key = (track_addr.clone(), lane_path.clone());
                if let Some(last) = self.automation.mixosc_last_values.get(&key)
                    && (last - value).abs() < f32::EPSILON
                {
                    continue;
                }
                self.automation.mixosc_last_values.insert(key, value);
                let packet = build_set(lane_path, OscValue::Float(value));
                if let Err(err) = socket.send_to(&packet, track_addr) {
                    tracing::debug!(
                        %err,
                        %track_name,
                        %track_addr,
                        %lane_path,
                        "Failed to send MixOSC packet"
                    );
                }
            }
        }
    }

    pub(crate) fn parse_automation_lanes(value: &serde_json::Value) -> Vec<OfflineAutomationLane> {
        parse_automation_lanes(value)
    }

    pub(crate) async fn handle_track_automation_insert_point(&mut self, a: Action) -> bool {
        let Action::TrackAutomationInsertPoint {
            ref track_name,
            ref target,
            sample,
            value,
        } = a
        else {
            return false;
        };

        if let Some(track) = self
            .state_snapshot
            .load_full()
            .tracks
            .get(track_name)
            .cloned()
        {
            let mut track = track.lock();
            let mut lanes = Self::parse_automation_lanes(&track.automation_lanes);
            let lane = match lanes.iter_mut().find(|lane| lane.target == *target) {
                Some(lane) => lane,
                None => {
                    lanes.push(OfflineAutomationLane {
                        target: target.clone(),
                        visible: true,
                        points: vec![],
                    });
                    lanes.last_mut().expect("just pushed")
                }
            };
            if let Some(point) = lane.points.iter_mut().find(|p| p.sample == sample) {
                point.value = value;
            } else {
                lane.points.push(OfflineAutomationPoint { sample, value });
                lane.points.sort_unstable_by_key(|p| p.sample);
            }
            track.automation_lanes = serde_json::to_value(&lanes).unwrap_or_default();
        }

        false
    }

    pub(crate) async fn handle_track_automation_toggle_lane(&mut self, a: Action) -> bool {
        let Action::TrackAutomationToggleLane {
            ref track_name,
            ref target,
        } = a
        else {
            return false;
        };

        if let Some(track) = self
            .state_snapshot
            .load_full()
            .tracks
            .get(track_name)
            .cloned()
        {
            let mut track = track.lock();
            let mut lanes = Self::parse_automation_lanes(&track.automation_lanes);
            if let Some(lane) = lanes.iter_mut().find(|lane| lane.target == *target) {
                lane.visible = !lane.visible;
            } else {
                lanes.push(OfflineAutomationLane {
                    target: target.clone(),
                    visible: true,
                    points: vec![],
                });
            }
            track.automation_lanes = serde_json::to_value(&lanes).unwrap_or_default();
        }

        false
    }

    pub(crate) async fn handle_track_automation_delete_point(&mut self, a: Action) -> bool {
        let Action::TrackAutomationDeletePoint {
            ref track_name,
            ref target,
            sample,
        } = a
        else {
            return false;
        };

        if let Some(track) = self
            .state_snapshot
            .load_full()
            .tracks
            .get(track_name)
            .cloned()
        {
            let mut track = track.lock();
            let mut lanes = Self::parse_automation_lanes(&track.automation_lanes);
            if let Some(lane) = lanes.iter_mut().find(|lane| lane.target == *target) {
                lane.points.retain(|point| point.sample != sample);
            }
            track.automation_lanes = serde_json::to_value(&lanes).unwrap_or_default();
        }

        false
    }
}
impl Engine {
    /// Modulator and track automation request arms.
    pub(crate) async fn handle_automation_request(&mut self, a: Action) -> bool {
        match a {
            Action::SetModulators(ref modulators) => {
                self.automation.modulators = modulators.clone();
                let echoes = self.apply_modulators(self.active_transport_sample());
                for action in echoes {
                    self.notify_clients(Ok(action)).await;
                }
            }
            Action::SetTrackAutomationLanes {
                ref track_name,
                ref lanes,
                mode,
            } => {
                if let Some(track) = self.state_snapshot.load_full().tracks.get(track_name) {
                    let mut track = track.lock();
                    track.automation_lanes = lanes.clone();
                    track.set_automation_mode(mode);
                }
            }
            Action::TrackAutomationToggleLane { .. } => {
                if Self::box_bool(self.handle_track_automation_toggle_lane(a.clone())).await {
                    return true;
                }
            }
            Action::TrackAutomationInsertPoint { .. } => {
                if Self::box_bool(self.handle_track_automation_insert_point(a.clone())).await {
                    return true;
                }
            }
            Action::TrackAutomationDeletePoint { .. } => {
                if Self::box_bool(self.handle_track_automation_delete_point(a.clone())).await {
                    return true;
                }
            }
            Action::TrackAutomationSetMode {
                ref track_name,
                mode,
            } => {
                if let Some(track) = self
                    .state_snapshot
                    .load_full()
                    .tracks
                    .get(track_name)
                    .cloned()
                {
                    track.lock().set_automation_mode(mode);
                }
            }
            Action::TrackAutomationLevel(ref name, level) => {
                tracing::debug!(%name, level, "engine received TrackAutomationLevel");
                if name == "hw:out" {
                    self.meters.hw_out_level_db = level;
                } else if let Some(track) = self.state_snapshot.load_full().tracks.get(name) {
                    track.lock().set_level(level);
                }
            }
            Action::TrackAutomationBalance(ref name, balance) => {
                if name == "hw:out" {
                    self.meters.hw_out_balance = balance.clamp(-1.0, 1.0);
                } else if let Some(track) = self.state_snapshot.load_full().tracks.get(name) {
                    track.lock().set_balance(balance);
                }
            }
            _ => {}
        }
        false
    }
}

// ---------- Undo/history support (colocated in Phase 4; formerly the crate::history matches) ----------
/// Whether `action` is an undoable command owned by this feature
/// (colocated from `crate::history::should_record` in Phase 4).
pub(crate) fn undo_should_record(action: &Action) -> bool {
    matches!(
        action,
        Action::SetModulators(_)
            | Action::SetTrackAutomationLanes { .. }
            | Action::TrackAutomationToggleLane { .. }
            | Action::TrackAutomationInsertPoint { .. }
            | Action::TrackAutomationDeletePoint { .. }
            | Action::TrackAutomationSetMode { .. }
    )
}

/// State-based inverse constructor for this feature's commands
/// (colocated from `crate::history::create_inverse_action` in Phase 4).
pub(crate) fn undo_inverse(action: &Action, state: &State) -> Option<Action> {
    match action {
        Action::SetTrackAutomationLanes {
            track_name,
            lanes: _,
            mode: _,
        } => {
            let track = state.tracks.get(track_name)?;
            let track_lock = track.lock();
            Some(Action::SetTrackAutomationLanes {
                track_name: track_name.clone(),
                lanes: track_lock.automation_lanes.clone(),
                mode: track_lock.automation_mode(),
            })
        }

        Action::TrackAutomationToggleLane { track_name, target } => {
            let track = state.tracks.get(track_name)?;
            let track_lock = track.lock();
            let _lanes = parse_automation_lanes(&track_lock.automation_lanes);
            Some(Action::TrackAutomationToggleLane {
                track_name: track_name.clone(),
                target: target.clone(),
            })
        }

        Action::TrackAutomationInsertPoint {
            track_name,
            target,
            sample,
            value: _,
        } => {
            let track = state.tracks.get(track_name)?;
            let track_lock = track.lock();
            let lanes = parse_automation_lanes(&track_lock.automation_lanes);
            let old_value = lanes
                .iter()
                .find(|lane| lane.target == *target)
                .and_then(|lane| lane.points.iter().find(|p| p.sample == *sample))
                .map(|p| p.value);
            match old_value {
                Some(value) => Some(Action::TrackAutomationInsertPoint {
                    track_name: track_name.clone(),
                    target: target.clone(),
                    sample: *sample,
                    value,
                }),
                None => Some(Action::TrackAutomationDeletePoint {
                    track_name: track_name.clone(),
                    target: target.clone(),
                    sample: *sample,
                }),
            }
        }

        Action::TrackAutomationDeletePoint {
            track_name,
            target,
            sample,
        } => {
            let track = state.tracks.get(track_name)?;
            let track_lock = track.lock();
            let lanes = parse_automation_lanes(&track_lock.automation_lanes);
            let old_value = lanes
                .iter()
                .find(|lane| lane.target == *target)
                .and_then(|lane| lane.points.iter().find(|p| p.sample == *sample))
                .map(|p| p.value);
            old_value.map(|value| Action::TrackAutomationInsertPoint {
                track_name: track_name.clone(),
                target: target.clone(),
                sample: *sample,
                value,
            })
        }

        Action::TrackAutomationSetMode {
            track_name,
            mode: _,
        } => {
            let track = state.tracks.get(track_name)?;
            let track_lock = track.lock();
            Some(Action::TrackAutomationSetMode {
                track_name: track_name.clone(),
                mode: track_lock.automation_mode(),
            })
        }
        _ => None,
    }
}

/// Multi-action inverse constructor for this feature's commands
/// (colocated from `crate::history::create_inverse_actions` in Phase 4).
pub(crate) fn undo_inverse_actions(action: &Action, state: &State) -> Option<Vec<Action>> {
    undo_inverse(action, state).map(|a| vec![a])
}

impl Engine {
    /// Engine-state inverse for the modulator list (colocated from
    /// `prepare_inverse_actions` in Phase 4).
    pub(crate) fn undo_engine_state_inverse_automation(
        &self,
        action: &Action,
    ) -> Option<Vec<Action>> {
        match action {
            Action::SetModulators(_) => Some(vec![Action::SetModulators(
                self.automation.modulators.clone(),
            )]),
            _ => None,
        }
    }
}
