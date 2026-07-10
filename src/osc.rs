use crate::connectable::ConnectableRef;
use crate::kind::Kind;
use crate::message::{
    Action, AudioClipData, ClipMoveFrom, ClipMoveTo, GlobalMidiLearnTarget, LaunchQuantization,
    Message, MidiClipData, MidiControllerData, MidiLearnBinding, MidiNoteData, MidiRawEventData,
    OfflineAutomationLane, OfflineAutomationTarget, PitchCorrectionPointData, PluginGraphNode,
    SessionMidiLearnTarget, TempoPoint, TimeSignaturePoint, TrackAutomationMode, TrackColor,
    TrackMidiLearnTarget,
};
use crate::modulator::Modulator;
use std::{
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::sync::mpsc::Sender;

const OSC_LISTEN_ADDR: &str = "0.0.0.0:9000";
const WAKEUP_PACKET: &[u8] = b"\0";

pub struct OscServer {
    stop: Arc<AtomicBool>,
    listen_addr: SocketAddr,
    handle: Option<thread::JoinHandle<()>>,
}

impl OscServer {
    pub fn start(tx: Sender<Message>) -> Result<Self, String> {
        Self::start_on_addr(tx, OSC_LISTEN_ADDR)
    }

    pub fn start_on_addr<A: ToSocketAddrs>(tx: Sender<Message>, addr: A) -> Result<Self, String> {
        let bind_addr = addr
            .to_socket_addrs()
            .map_err(|e| format!("Failed to resolve OSC socket address: {e}"))?
            .next()
            .ok_or_else(|| "Failed to resolve OSC socket address".to_string())?;
        let socket = UdpSocket::bind(bind_addr)
            .map_err(|e| format!("Failed to bind OSC socket on {bind_addr}: {e}"))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(|e| format!("Failed to configure OSC socket timeout: {e}"))?;
        let listen_addr = socket
            .local_addr()
            .map_err(|e| format!("Failed to read OSC socket address: {e}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = thread::spawn(move || {
            let mut buf = [0_u8; 2048];
            while !stop_thread.load(Ordering::Relaxed) {
                match socket.recv_from(&mut buf) {
                    Ok((len, src_addr)) => {
                        let packet = &buf[..len];
                        match parse_osc_request(packet) {
                            Ok(action) => {
                                if tx
                                    .blocking_send(Message::OscRequest {
                                        action,
                                        reply_to: src_addr,
                                    })
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(reason) => {
                                let reply = build_error_packet(&reason);
                                let _ = socket.send_to(&reply, src_addr);
                            }
                        }
                    }
                    Err(err)
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            || err.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => {
                        break;
                    }
                }
            }
        });

        Ok(Self {
            stop,
            listen_addr,
            handle: Some(handle),
        })
    }

    #[cfg(test)]
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Send a dummy packet to wake up the receiver thread so it notices the
        // stop flag and exits promptly, instead of blocking in recv_from until
        // the read timeout expires.
        if let Ok(wake) = UdpSocket::bind("127.0.0.1:0") {
            let _ = wake.send_to(WAKEUP_PACKET, self.listen_addr);
        }
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
}

impl Drop for OscServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Parses an OSC packet into an engine action.
fn parse_osc_request(packet: &[u8]) -> Result<Action, String> {
    let (address, next) =
        parse_osc_string(packet, 0).ok_or_else(|| "Malformed OSC address".to_string())?;
    let (type_tags, arg_offset) = parse_osc_string(packet, next)
        .ok_or_else(|| "Malformed OSC type tag string".to_string())?;
    if !type_tags.starts_with(',') {
        return Err("OSC type tag string must start with ','".to_string());
    }

    let args = OscArgs {
        packet,
        type_tags: &type_tags.as_bytes()[1..],
        offset: arg_offset,
    };

    dispatch_address(&address, args)
}

fn dispatch_address(address: &str, mut args: OscArgs<'_>) -> Result<Action, String> {
    match address {
        // Transport
        "/transport/play" => no_args(args, Action::Play),
        "/transport/stop" => no_args(args, Action::Stop),
        "/transport/pause" => no_args(args, Action::Pause),
        "/transport/start" | "/transport/jump_to_start" | "/transport/start_of_session" => {
            no_args(args, Action::TransportPosition(0))
        }
        "/transport/end" | "/transport/jump_to_end" | "/transport/end_of_session" => {
            no_args(args, Action::JumpToEnd)
        }
        "/transport/position" => {
            let sample = args.next_int()? as usize;
            Ok(Action::TransportPosition(sample))
        }
        "/transport/record" => {
            let enabled = args.next_bool()?;
            Ok(Action::SetRecordEnabled(enabled))
        }
        "/transport/loop_enable" => {
            let enabled = args.next_bool()?;
            Ok(Action::SetLoopEnabled(enabled))
        }
        "/transport/loop_range" => {
            let start = args.next_int()? as usize;
            let end = args.next_int()? as usize;
            Ok(Action::SetLoopRange(Some((start, end))))
        }
        "/transport/loop_range/clear" => no_args(args, Action::SetLoopRange(None)),
        "/transport/punch_enable" => {
            let enabled = args.next_bool()?;
            Ok(Action::SetPunchEnabled(enabled))
        }
        "/transport/punch_range" => {
            let start = args.next_int()? as usize;
            let end = args.next_int()? as usize;
            Ok(Action::SetPunchRange(Some((start, end))))
        }
        "/transport/punch_range/clear" => no_args(args, Action::SetPunchRange(None)),
        "/transport/tempo" => {
            let bpm = args.next_double_or_float()?;
            Ok(Action::SetTempo(bpm))
        }
        "/transport/time_signature" => {
            let numerator = args.next_int()? as u16;
            let denominator = args.next_int()? as u16;
            Ok(Action::SetTimeSignature {
                numerator,
                denominator,
            })
        }

        // Session
        "/session/launch" => {
            let track_name = args.next_string()?;
            let scene_index = args.next_int()? as usize;
            Ok(Action::Session(crate::message::SessionAction::LaunchClip {
                track_name,
                scene_index,
                clip_id: String::new(),
                launch_quantization: LaunchQuantization::Bar,
                loop_enabled: true,
                loop_start_samples: 0,
                loop_end_samples: 0,
            }))
        }
        "/session/stop" => {
            let track_name = args.next_string()?;
            let scene_index = args.next_int()? as usize;
            Ok(Action::Session(crate::message::SessionAction::StopClip {
                track_name,
                scene_index,
                launch_quantization: LaunchQuantization::Bar,
            }))
        }
        "/session/scene" => {
            let scene_index = args.next_int()? as usize;
            Ok(Action::Session(
                crate::message::SessionAction::LaunchScene {
                    scene_index,
                    launch_quantization: LaunchQuantization::Bar,
                },
            ))
        }
        "/session/stopall" => no_args(
            args,
            Action::Session(crate::message::SessionAction::StopAllClips),
        ),
        "/session/stop_scene" => {
            let scene_index = args.next_int()? as usize;
            Ok(Action::Session(crate::message::SessionAction::StopScene {
                scene_index,
                launch_quantization: LaunchQuantization::Bar,
            }))
        }

        // Track management
        "/track/add" => {
            let name = args.next_string()?;
            let audio_ins = args.next_int()? as usize;
            let midi_ins = args.next_int()? as usize;
            let audio_outs = args.next_int()? as usize;
            let midi_outs = args.next_int()? as usize;
            let folder = args.next_bool()?;
            Ok(Action::AddTrack {
                name,
                audio_ins,
                midi_ins,
                audio_outs,
                midi_outs,
                folder,
            })
        }
        "/track/remove" => {
            let name = args.next_string()?;
            Ok(Action::RemoveTrack(name))
        }
        "/track/rename" => {
            let old_name = args.next_string()?;
            let new_name = args.next_string()?;
            Ok(Action::RenameTrack { old_name, new_name })
        }
        "/track/set_folder" => {
            let track_name = args.next_string()?;
            let is_folder = args.next_bool()?;
            Ok(Action::TrackSetFolder {
                track_name,
                is_folder,
            })
        }
        "/track/toggle_folder" => {
            let track_name = args.next_string()?;
            Ok(Action::TrackToggleFolder { track_name })
        }
        "/track/set_parent" => {
            let track_name = args.next_string()?;
            let parent_name = args.next_string()?;
            let parent_name = if parent_name.is_empty() {
                None
            } else {
                Some(parent_name)
            };
            Ok(Action::TrackSetParent {
                track_name,
                parent_name,
            })
        }

        // Track mixing
        "/track/level" => {
            let track_name = args.next_string()?;
            let level = args.next_float()?;
            Ok(Action::TrackLevel(track_name, level))
        }
        "/track/balance" => {
            let track_name = args.next_string()?;
            let balance = args.next_float()?;
            Ok(Action::TrackBalance(track_name, balance))
        }
        "/track/mute" => {
            let track_name = args.next_string()?;
            let _enabled = args.next_bool()?;
            Ok(Action::TrackToggleMute(track_name))
        }
        "/track/solo" => {
            let track_name = args.next_string()?;
            let _enabled = args.next_bool()?;
            Ok(Action::TrackToggleSolo(track_name))
        }
        "/track/arm" => {
            let track_name = args.next_string()?;
            let _enabled = args.next_bool()?;
            Ok(Action::TrackToggleArm(track_name))
        }
        "/track/phase" => {
            let track_name = args.next_string()?;
            let _enabled = args.next_bool()?;
            Ok(Action::TrackTogglePhase(track_name))
        }
        "/track/master" => {
            let track_name = args.next_string()?;
            let _enabled = args.next_bool()?;
            Ok(Action::TrackToggleMaster(track_name))
        }
        "/track/automation_level" => {
            let track_name = args.next_string()?;
            let level = args.next_float()?;
            Ok(Action::TrackAutomationLevel(track_name, level))
        }
        "/track/automation_balance" => {
            let track_name = args.next_string()?;
            let balance = args.next_float()?;
            Ok(Action::TrackAutomationBalance(track_name, balance))
        }
        "/track/midi_cc" => {
            let track_name = args.next_string()?;
            let channel = args.next_int()? as u8;
            let cc = args.next_int()? as u8;
            let value = args.next_int()? as u8;
            Ok(Action::TrackMidiCc {
                track_name,
                channel,
                cc,
                value,
            })
        }

        // Routing
        "/connect" => parse_connect(args, true),
        "/disconnect" => parse_connect(args, false),

        // Plugins
        "/plugin/load" => {
            let track_name = args.next_string()?;
            let format = args.next_string()?;
            let path = args.next_string()?;
            parse_plugin_load(track_name, &format, path)
        }
        "/plugin/unload" => {
            let track_name = args.next_string()?;
            let format = args.next_string()?;
            let path = args.next_string()?;
            parse_plugin_unload(track_name, &format, path)
        }
        "/plugin/unload_instance" => {
            let track_name = args.next_string()?;
            let format = args.next_string()?;
            let instance_id = args.next_int()? as usize;
            parse_plugin_unload_instance(track_name, &format, instance_id)
        }
        "/plugin/bypass" => {
            let track_name = args.next_string()?;
            let format = args.next_string()?;
            let instance_id = args.next_int()? as usize;
            let bypassed = args.next_bool()?;
            Ok(Action::TrackSetPluginBypassed {
                track_name,
                instance_id,
                format,
                bypassed,
            })
        }
        "/plugin/show_gui" => {
            let track_name = args.next_string()?;
            let format = args.next_string()?;
            let instance_id = args.next_int()? as usize;
            parse_plugin_show_gui(track_name, &format, instance_id)
        }
        "/plugin/snapshot_state" => {
            let track_name = args.next_string()?;
            let format = args.next_string()?;
            let instance_id = args.next_int()? as usize;
            parse_plugin_snapshot_state(track_name, &format, instance_id)
        }
        "/plugin/restore_state" => {
            let track_name = args.next_string()?;
            let format = args.next_string()?;
            let instance_id = args.next_int()? as usize;
            let state_json = args.next_string()?;
            parse_plugin_restore_state(track_name, &format, instance_id, &state_json)
        }
        "/plugin/set_resource_dir" => {
            let track_name = args.next_string()?;
            let format = args.next_string()?;
            let instance_id = args.next_int()? as usize;
            let directory = args.next_string()?;
            Ok(Action::TrackSetPluginResourceDir {
                track_name,
                instance_id,
                format,
                directory,
            })
        }
        "/plugin/update_file_reference" => {
            let track_name = args.next_string()?;
            let _format = args.next_string()?;
            let instance_id = args.next_int()? as usize;
            let index = args.next_int()? as u32;
            let path = args.next_string()?;
            Ok(Action::TrackUpdateClapFileReference {
                track_name,
                instance_id,
                index,
                path,
            })
        }
        "/plugin/snapshot_all_states" => {
            let track_name = args.next_string()?;
            Ok(Action::TrackSnapshotAllClapStates { track_name })
        }
        "/plugin/set_param_at" => parse_plugin_set_param_at(args),
        "/plugin/begin_param_edit" => parse_clap_param_edit(args, true),
        "/plugin/end_param_edit" => parse_clap_param_edit(args, false),

        // Automation
        "/automation/mode" => {
            let track_name = args.next_string()?;
            let mode = parse_automation_mode(&args.next_string()?)?;
            Ok(Action::TrackAutomationSetMode { track_name, mode })
        }
        "/automation/toggle_lane" => {
            let track_name = args.next_string()?;
            let target = parse_automation_target(&args.next_string()?)?;
            Ok(Action::TrackAutomationToggleLane { track_name, target })
        }
        "/automation/insert_point" => {
            let track_name = args.next_string()?;
            let target = parse_automation_target(&args.next_string()?)?;
            let sample = args.next_int()? as usize;
            let value = args.next_float()?;
            Ok(Action::TrackAutomationInsertPoint {
                track_name,
                target,
                sample,
                value,
            })
        }
        "/automation/delete_point" => {
            let track_name = args.next_string()?;
            let target = parse_automation_target(&args.next_string()?)?;
            let sample = args.next_int()? as usize;
            Ok(Action::TrackAutomationDeletePoint {
                track_name,
                target,
                sample,
            })
        }
        "/automation/set_lanes" => {
            let track_name = args.next_string()?;
            let mode = parse_automation_mode(&args.next_string()?)?;
            let lanes = parse_json(&args.next_string()?)?;
            Ok(Action::SetTrackAutomationLanes {
                track_name,
                lanes,
                mode,
            })
        }

        // Queries
        "/query/tracks" => no_args(args, Action::RequestTrackList),
        "/query/transport" => no_args(args, Action::RequestTransportState),
        "/query/plugins" => {
            let track_name = args.next_string()?;
            Ok(Action::TrackGetPluginGraph {
                track_name,
                include_state: false,
            })
        }
        "/query/meters" => no_args(args, Action::RequestMeterSnapshot),
        "/query/plugin_parameters" => {
            let track_name = args.next_string()?;
            let format = args.next_string()?;
            let instance_id = args.next_int()? as usize;
            parse_plugin_parameters_query(track_name, &format, instance_id)
        }
        "/query/clip_plugin_parameters" => {
            let track_name = args.next_string()?;
            let format = args.next_string()?;
            let clip_idx = args.next_int()? as usize;
            let instance_id = args.next_int()? as usize;
            parse_clip_plugin_parameters_query(track_name, &format, clip_idx, instance_id)
        }
        "/query/clap_plugins" => no_args(args, Action::ListClapPlugins),
        "/query/clap_plugins_with_capabilities" => {
            no_args(args, Action::ListClapPluginsWithCapabilities)
        }
        "/query/vst3_plugins" => no_args(args, Action::ListVst3Plugins),
        #[cfg(all(unix, not(target_os = "macos")))]
        "/query/lv2_plugins" => no_args(args, Action::ListLv2Plugins),
        "/query/clap_note_names" => {
            let track_name = args.next_string()?;
            Ok(Action::TrackGetClapNoteNames { track_name })
        }
        "/query/vst3_graph" => {
            let track_name = args.next_string()?;
            Ok(Action::TrackGetVst3Graph { track_name })
        }
        "/query/diagnostics" => no_args(args, Action::RequestSessionDiagnostics),
        "/query/midi_learn_report" => no_args(args, Action::RequestMidiLearnMappingsReport),

        // Transport extras
        "/transport/session_play" => no_args(args, Action::SessionPlay),
        "/transport/metronome" => {
            let enabled = args.next_bool()?;
            Ok(Action::SetMetronomeEnabled(enabled))
        }
        "/transport/clip_playback" => {
            let enabled = args.next_bool()?;
            Ok(Action::SetClipPlaybackEnabled(enabled))
        }
        "/transport/session_clip_playback" => {
            let enabled = args.next_bool()?;
            Ok(Action::SetSessionClipPlaybackEnabled(enabled))
        }
        "/transport/position_at" => {
            let sample = args.next_int()? as usize;
            let after_frames = args.next_int()? as usize;
            Ok(Action::TransportPositionAt {
                sample,
                after_frames,
            })
        }
        "/transport/panic" => no_args(args, Action::Panic),
        "/transport/tempo_map" => {
            let json = args.next_string()?;
            parse_tempo_map(&json)
        }
        "/step_recording" => {
            let enabled = args.next_bool()?;
            Ok(Action::SetStepRecording(enabled))
        }

        // Session
        "/session/path" => {
            let path = args.next_string()?;
            Ok(Action::SetSessionPath(path))
        }

        // Clip operations
        "/clip/add" => parse_add_clip(args),
        "/clip/remove" => {
            let track_name = args.next_string()?;
            let kind = parse_kind(&args.next_string()?)?;
            let indices = parse_int_list(&args.next_string()?)?;
            Ok(Action::RemoveClip {
                track_name,
                kind,
                clip_indices: indices,
            })
        }
        "/clip/move" => {
            let kind = parse_kind(&args.next_string()?)?;
            let from_track = args.next_string()?;
            let from_index = args.next_int()? as usize;
            let to_track = args.next_string()?;
            let to_offset = args.next_int()? as usize;
            let to_channel = args.next_int()? as usize;
            let copy = args.next_bool()?;
            Ok(Action::ClipMove {
                kind,
                from: ClipMoveFrom {
                    track_name: from_track,
                    clip_index: from_index,
                },
                to: ClipMoveTo {
                    track_name: to_track,
                    sample_offset: to_offset,
                    input_channel: to_channel,
                },
                copy,
            })
        }
        "/clip/fade" => {
            let track_name = args.next_string()?;
            let clip_index = args.next_int()? as usize;
            let kind = parse_kind(&args.next_string()?)?;
            let fade_enabled = args.next_bool()?;
            let fade_in_samples = args.next_int()? as usize;
            let fade_out_samples = args.next_int()? as usize;
            Ok(Action::SetClipFade {
                track_name,
                clip_index,
                kind,
                fade_enabled,
                fade_in_samples,
                fade_out_samples,
            })
        }
        "/clip/bounds" => {
            let track_name = args.next_string()?;
            let clip_index = args.next_int()? as usize;
            let kind = parse_kind(&args.next_string()?)?;
            let start = args.next_int()? as usize;
            let length = args.next_int()? as usize;
            let offset = args.next_int()? as usize;
            Ok(Action::SetClipBounds {
                track_name,
                clip_index,
                kind,
                start,
                length,
                offset,
            })
        }
        "/clip/mute" => {
            let track_name = args.next_string()?;
            let clip_index = args.next_int()? as usize;
            let kind = parse_kind(&args.next_string()?)?;
            let muted = args.next_bool()?;
            Ok(Action::SetClipMuted {
                track_name,
                clip_index,
                kind,
                muted,
            })
        }
        "/clip/rename" => {
            let track_name = args.next_string()?;
            let clip_index = args.next_int()? as usize;
            let kind = parse_kind(&args.next_string()?)?;
            let new_name = args.next_string()?;
            Ok(Action::RenameClip {
                track_name,
                kind,
                clip_index,
                new_name,
            })
        }
        "/clip/source_name" => {
            let track_name = args.next_string()?;
            let clip_index = args.next_int()? as usize;
            let kind = parse_kind(&args.next_string()?)?;
            let name = args.next_string()?;
            Ok(Action::SetClipSourceName {
                track_name,
                kind,
                clip_index,
                name,
            })
        }
        "/clip/plugin_graph_json" => {
            let track_name = args.next_string()?;
            let clip_index = args.next_int()? as usize;
            let json = args.next_string()?;
            let plugin_graph_json = if json.is_empty() {
                None
            } else {
                Some(parse_json(&json)?)
            };
            Ok(Action::SetClipPluginGraphJson {
                track_name,
                clip_index,
                plugin_graph_json,
            })
        }
        "/clip/pitch_correction" => parse_clip_pitch_correction(args),
        "/clip_group/add" => {
            let track_name = args.next_string()?;
            let kind = parse_kind(&args.next_string()?)?;
            let audio_json = args.next_string()?;
            let midi_json = args.next_string()?;
            let audio_clip = if audio_json.is_empty() {
                None
            } else {
                Some(parse_audio_clip_data(&audio_json)?)
            };
            let midi_clip = if midi_json.is_empty() {
                None
            } else {
                Some(parse_midi_clip_data(&midi_json)?)
            };
            Ok(Action::AddGroupedClip {
                track_name,
                kind,
                audio_clip,
                midi_clip,
            })
        }

        // MIDI editing
        "/midi/insert_notes" => parse_insert_notes(args),
        "/midi/delete_notes" => parse_delete_notes(args),
        "/midi/modify_notes" => parse_modify_notes(args),
        "/midi/insert_controllers" => parse_insert_controllers(args),
        "/midi/delete_controllers" => parse_delete_controllers(args),
        "/midi/modify_controllers" => parse_modify_controllers(args),
        "/midi/sysex" => parse_sysex_events(args),
        "/midi/step_record" => {
            let device = args.next_string()?;
            let channel = args.next_int()? as u8;
            let pitch = args.next_int()? as u8;
            let velocity = args.next_int()? as u8;
            Ok(Action::StepRecordMidiNote {
                device,
                channel,
                pitch,
                velocity,
            })
        }

        // Plugin graph connections
        "/plugin/connect_audio" => parse_plugin_graph_connection(args, true, Kind::Audio),
        "/plugin/disconnect_audio" => parse_plugin_graph_connection(args, false, Kind::Audio),
        "/plugin/connect_midi" => parse_plugin_graph_connection(args, true, Kind::MIDI),
        "/plugin/disconnect_midi" => parse_plugin_graph_connection(args, false, Kind::MIDI),

        // VST3 internal graph connections
        "/vst3/connect_audio" => parse_vst3_graph_connection(args, true),
        "/vst3/disconnect_audio" => parse_vst3_graph_connection(args, false),

        // Plugin parameters
        "/plugin/set_param" => parse_plugin_set_param(args),
        "/clip_plugin/set_param" => parse_clip_plugin_set_param(args),
        "/clip_plugin/snapshot_state" => {
            let track_name = args.next_string()?;
            let format = args.next_string()?;
            let clip_idx = args.next_int()? as usize;
            let instance_id = args.next_int()? as usize;
            parse_clip_plugin_snapshot_state(track_name, &format, clip_idx, instance_id)
        }
        "/clip_plugin/restore_state" => {
            let track_name = args.next_string()?;
            let format = args.next_string()?;
            let clip_idx = args.next_int()? as usize;
            let instance_id = args.next_int()? as usize;
            let state_json = args.next_string()?;
            parse_clip_plugin_restore_state(track_name, &format, clip_idx, instance_id, &state_json)
        }
        "/clip_plugin/set_resource_dir" => {
            let track_name = args.next_string()?;
            let format = args.next_string()?;
            let clip_idx = args.next_int()? as usize;
            let instance_id = args.next_int()? as usize;
            let directory = args.next_string()?;
            Ok(Action::ClipSetPluginResourceDir {
                track_name,
                clip_idx,
                instance_id,
                format,
                directory,
            })
        }
        "/clip_plugin/update_file_reference" => {
            let track_name = args.next_string()?;
            let _format = args.next_string()?;
            let clip_idx = args.next_int()? as usize;
            let instance_id = args.next_int()? as usize;
            let index = args.next_int()? as u32;
            let path = args.next_string()?;
            Ok(Action::ClipUpdateClapFileReference {
                track_name,
                clip_idx,
                instance_id,
                index,
                path,
            })
        }

        // MIDI learn
        "/midi_learn/arm_track" => {
            let track_name = args.next_string()?;
            let target = parse_track_midi_learn_target(&args.next_string()?)?;
            Ok(Action::TrackArmMidiLearn { track_name, target })
        }
        "/midi_learn/arm_global" => {
            let target = parse_global_midi_learn_target(&args.next_string()?)?;
            Ok(Action::GlobalArmMidiLearn { target })
        }
        "/midi_learn/arm_session" => {
            let target = parse_session_midi_learn_target(&args.next_string()?)?;
            Ok(Action::SessionArmMidiLearn { target })
        }
        "/midi_learn/bind_track" => {
            let track_name = args.next_string()?;
            let target = parse_track_midi_learn_target(&args.next_string()?)?;
            let binding_json = args.next_string()?;
            let binding = parse_midi_learn_binding(&binding_json)?;
            Ok(Action::TrackSetMidiLearnBinding {
                track_name,
                target,
                binding,
            })
        }
        "/midi_learn/bind_global" => {
            let target = parse_global_midi_learn_target(&args.next_string()?)?;
            let binding_json = args.next_string()?;
            let binding = parse_midi_learn_binding(&binding_json)?;
            Ok(Action::SetGlobalMidiLearnBinding { target, binding })
        }
        "/midi_learn/bind_session" => {
            let target = parse_session_midi_learn_target(&args.next_string()?)?;
            let binding_json = args.next_string()?;
            let binding = parse_midi_learn_binding(&binding_json)?;
            Ok(Action::SetSessionMidiLearnBinding { target, binding })
        }
        "/midi_learn/clear" => no_args(args, Action::ClearAllMidiLearnBindings),

        // Modulators
        "/modulators" => {
            let json = args.next_string()?;
            let modulators: Vec<Modulator> =
                serde_json::from_str(&json).map_err(|e| format!("Invalid modulators JSON: {e}"))?;
            Ok(Action::SetModulators(modulators))
        }

        // Audio/MIDI devices
        "/device/audio_open" => parse_open_audio_device(args),
        "/device/midi_in_open" => {
            let device = args.next_string()?;
            Ok(Action::OpenMidiInputDevice(device))
        }
        "/device/midi_out_open" => {
            let device = args.next_string()?;
            Ok(Action::OpenMidiOutputDevice(device))
        }
        "/device/jack/add_audio_in" => no_args(args, Action::JackAddAudioInputPort),
        "/device/jack/remove_audio_in" => {
            let port = args.next_int()? as usize;
            Ok(Action::JackRemoveAudioInputPort(port))
        }
        "/device/jack/add_audio_out" => no_args(args, Action::JackAddAudioOutputPort),
        "/device/jack/remove_audio_out" => {
            let port = args.next_int()? as usize;
            Ok(Action::JackRemoveAudioOutputPort(port))
        }

        // Offline bounce
        "/bounce/start" => parse_offline_bounce(args),
        "/bounce/cancel" => {
            let track_name = args.next_string()?;
            Ok(Action::TrackOfflineBounceCancel { track_name })
        }
        "/bounce/cancel_all" => no_args(args, Action::TrackOfflineBounceCancelAll),

        // Misc track actions
        "/track/add_audio_input" => {
            let track_name = args.next_string()?;
            Ok(Action::TrackAddAudioInput(track_name))
        }
        "/track/remove_audio_input" => {
            let track_name = args.next_string()?;
            Ok(Action::TrackRemoveAudioInput(track_name))
        }
        "/track/add_audio_output" => {
            let track_name = args.next_string()?;
            Ok(Action::TrackAddAudioOutput(track_name))
        }
        "/track/remove_audio_output" => {
            let track_name = args.next_string()?;
            Ok(Action::TrackRemoveAudioOutput(track_name))
        }
        "/track/toggle_input_monitor" => {
            let track_name = args.next_string()?;
            let lane = args.next_int()? as usize;
            Ok(Action::TrackToggleInputMonitor { track_name, lane })
        }
        "/track/toggle_disk_monitor" => {
            let track_name = args.next_string()?;
            let lane = args.next_int()? as usize;
            Ok(Action::TrackToggleDiskMonitor { track_name, lane })
        }
        "/track/toggle_midi_input_monitor" => {
            let track_name = args.next_string()?;
            let lane = args.next_int()? as usize;
            Ok(Action::TrackToggleMidiInputMonitor { track_name, lane })
        }
        "/track/toggle_midi_disk_monitor" => {
            let track_name = args.next_string()?;
            let lane = args.next_int()? as usize;
            Ok(Action::TrackToggleMidiDiskMonitor { track_name, lane })
        }
        "/track/midi_lane_channel" => {
            let track_name = args.next_string()?;
            let lane = args.next_int()? as usize;
            let channel = args.next_int()?;
            let channel = if !(0..=15).contains(&channel) {
                None
            } else {
                Some(channel as u8)
            };
            Ok(Action::TrackSetMidiLaneChannel {
                track_name,
                lane,
                channel,
            })
        }
        "/track/frozen" => {
            let track_name = args.next_string()?;
            let frozen = args.next_bool()?;
            Ok(Action::TrackSetFrozen { track_name, frozen })
        }
        "/track/color" => {
            let track_name = args.next_string()?;
            let r = args.next_float()?;
            let g = args.next_float()?;
            let b = args.next_float()?;
            let a = args.next_float().unwrap_or(1.0);
            Ok(Action::TrackSetColor {
                track_name,
                color: Some(TrackColor { r, g, b, a }),
            })
        }
        "/track/color/clear" => {
            let track_name = args.next_string()?;
            Ok(Action::TrackSetColor {
                track_name,
                color: None,
            })
        }
        "/track/session_slot" => {
            let track_name = args.next_string()?;
            let scene_index = args.next_int()? as usize;
            let clip_id = args.next_string()?;
            let clip_id = if clip_id.is_empty() {
                None
            } else {
                Some(clip_id)
            };
            Ok(Action::TrackSetSessionSlot {
                track_name,
                scene_index,
                clip_id,
            })
        }
        "/track/session_slot_play_enabled" => {
            let track_name = args.next_string()?;
            let scene_index = args.next_int()? as usize;
            let enabled = args.next_bool()?;
            Ok(Action::TrackSetSessionSlotPlayEnabled {
                track_name,
                scene_index,
                enabled,
            })
        }
        "/track/clear_default_passthrough" => {
            let track_name = args.next_string()?;
            Ok(Action::TrackClearDefaultPassthrough { track_name })
        }
        "/track/clear_plugins" => {
            let track_name = args.next_string()?;
            Ok(Action::TrackClearPlugins { track_name })
        }
        "/track/connect_audio" => parse_track_connectable(args, true, Kind::Audio),
        "/track/disconnect_audio" => parse_track_connectable(args, false, Kind::Audio),
        "/track/connect_midi" => parse_track_connectable(args, true, Kind::MIDI),
        "/track/disconnect_midi" => parse_track_connectable(args, false, Kind::MIDI),

        // Piano key
        "/piano_key" => {
            let track_name = args.next_string()?;
            let note = args.next_int()? as u8;
            let velocity = args.next_int()? as u8;
            let on = args.next_bool()?;
            Ok(Action::PianoKey {
                track_name,
                note,
                velocity,
                on,
            })
        }

        _ => Err(format!("Unsupported OSC address: {address}")),
    }
}

fn no_args(args: OscArgs<'_>, action: Action) -> Result<Action, String> {
    if !args.is_empty() {
        return Err("Expected no arguments".to_string());
    }
    Ok(action)
}

fn parse_connect(mut args: OscArgs<'_>, connect: bool) -> Result<Action, String> {
    let from_track = args.next_string()?;
    let from_port = args.next_int()? as usize;
    let to_track = args.next_string()?;
    let to_port = args.next_int()? as usize;
    let kind = parse_kind(&args.next_string()?)?;
    if connect {
        Ok(Action::Connect {
            from_track,
            from_port,
            to_track,
            to_port,
            kind,
        })
    } else {
        Ok(Action::Disconnect {
            from_track,
            from_port,
            to_track,
            to_port,
            kind,
        })
    }
}

fn parse_kind(value: &str) -> Result<Kind, String> {
    match value {
        "audio" => Ok(Kind::Audio),
        "midi" => Ok(Kind::MIDI),
        _ => Err(format!("Expected 'audio' or 'midi', got '{value}'")),
    }
}

fn parse_plugin_load(track_name: String, format: &str, path: String) -> Result<Action, String> {
    match format {
        "clap" => Ok(Action::TrackLoadClapPlugin {
            track_name,
            plugin_id: path,
            instance_id: None,
        }),
        "vst3" => Ok(Action::TrackLoadVst3Plugin {
            track_name,
            plugin_id: path,
            instance_id: None,
        }),
        #[cfg(all(unix, not(target_os = "macos")))]
        "lv2" => Ok(Action::TrackLoadLv2Plugin {
            track_name,
            plugin_uri: path,
            instance_id: None,
        }),
        _ => Err(format!("Unsupported plugin format: {format}")),
    }
}

fn parse_plugin_unload(track_name: String, format: &str, path: String) -> Result<Action, String> {
    match format {
        "clap" => Ok(Action::TrackUnloadClapPlugin {
            track_name,
            plugin_id: path,
        }),
        "vst3" => Ok(Action::TrackUnloadVst3Plugin {
            track_name,
            plugin_id: path,
        }),
        #[cfg(all(unix, not(target_os = "macos")))]
        "lv2" => Ok(Action::TrackUnloadLv2Plugin {
            track_name,
            plugin_uri: path,
        }),
        _ => Err(format!("Unsupported plugin format: {format}")),
    }
}

fn parse_plugin_unload_instance(
    track_name: String,
    format: &str,
    instance_id: usize,
) -> Result<Action, String> {
    match format {
        "clap" => Ok(Action::TrackUnloadClapPluginInstance {
            track_name,
            instance_id,
        }),
        "vst3" => Ok(Action::TrackUnloadVst3PluginInstance {
            track_name,
            instance_id,
        }),
        #[cfg(all(unix, not(target_os = "macos")))]
        "lv2" => Ok(Action::TrackUnloadLv2PluginInstance {
            track_name,
            instance_id,
        }),
        _ => Err(format!("Unsupported plugin format: {format}")),
    }
}

fn parse_plugin_parameters_query(
    track_name: String,
    format: &str,
    instance_id: usize,
) -> Result<Action, String> {
    match format {
        "clap" => Ok(Action::TrackGetClapParameters {
            track_name,
            instance_id,
        }),
        "vst3" => Ok(Action::TrackGetVst3Parameters {
            track_name,
            instance_id,
        }),
        #[cfg(all(unix, not(target_os = "macos")))]
        "lv2" => Ok(Action::TrackGetLv2PluginControls {
            track_name,
            instance_id,
        }),
        _ => Err(format!("Unsupported plugin format: {format}")),
    }
}

fn parse_clip_plugin_parameters_query(
    track_name: String,
    format: &str,
    clip_idx: usize,
    instance_id: usize,
) -> Result<Action, String> {
    match format {
        #[cfg(all(unix, not(target_os = "macos")))]
        "lv2" => Ok(Action::ClipGetLv2PluginControls {
            track_name,
            clip_idx,
            instance_id,
        }),
        _ => Err(format!(
            "Unsupported clip plugin parameter query format: {format}"
        )),
    }
}

fn parse_plugin_show_gui(
    track_name: String,
    format: &str,
    instance_id: usize,
) -> Result<Action, String> {
    match format {
        "clap" => Ok(Action::TrackShowClapGui {
            track_name,
            instance_id,
        }),
        "vst3" => Ok(Action::TrackShowVst3Gui {
            track_name,
            instance_id,
        }),
        #[cfg(all(unix, not(target_os = "macos")))]
        "lv2" => Ok(Action::TrackShowLv2Gui {
            track_name,
            instance_id,
        }),
        _ => Err(format!("Unsupported plugin format: {format}")),
    }
}

fn parse_plugin_snapshot_state(
    track_name: String,
    format: &str,
    instance_id: usize,
) -> Result<Action, String> {
    match format {
        "clap" => Ok(Action::TrackClapSnapshotState {
            track_name,
            instance_id,
        }),
        "vst3" => Ok(Action::TrackVst3SnapshotState {
            track_name,
            instance_id,
        }),
        #[cfg(all(unix, not(target_os = "macos")))]
        "lv2" => Ok(Action::TrackLv2SnapshotState {
            track_name,
            instance_id,
        }),
        _ => Err(format!("Unsupported plugin format: {format}")),
    }
}

fn parse_plugin_restore_state(
    track_name: String,
    format: &str,
    instance_id: usize,
    state_json: &str,
) -> Result<Action, String> {
    match format {
        "clap" => {
            let state = parse_clap_state(state_json)?;
            Ok(Action::TrackClapRestoreState {
                track_name,
                instance_id,
                state,
            })
        }
        "vst3" => {
            let state = parse_vst3_state(state_json)?;
            Ok(Action::TrackVst3RestoreState {
                track_name,
                instance_id,
                state,
            })
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        "lv2" => Ok(Action::TrackSetLv2PluginState {
            track_name,
            instance_id,
            state: parse_state_bytes(state_json)?,
        }),
        _ => Err(format!("Unsupported plugin format: {format}")),
    }
}

fn parse_clip_plugin_snapshot_state(
    track_name: String,
    format: &str,
    clip_idx: usize,
    instance_id: usize,
) -> Result<Action, String> {
    match format {
        "clap" => Ok(Action::ClipClapSnapshotState {
            track_name,
            clip_idx,
            instance_id,
        }),
        "vst3" => Ok(Action::ClipVst3SnapshotState {
            track_name,
            clip_idx,
            instance_id,
        }),
        #[cfg(all(unix, not(target_os = "macos")))]
        "lv2" => Ok(Action::ClipLv2SnapshotState {
            track_name,
            clip_idx,
            instance_id,
        }),
        _ => Err(format!(
            "Unsupported clip plugin state snapshot format: {format}"
        )),
    }
}

fn parse_clip_plugin_restore_state(
    track_name: String,
    format: &str,
    clip_idx: usize,
    instance_id: usize,
    state_json: &str,
) -> Result<Action, String> {
    match format {
        "clap" => {
            let state = parse_clap_state(state_json)?;
            Ok(Action::ClipClapRestoreState {
                track_name,
                clip_idx,
                instance_id,
                state,
            })
        }
        "vst3" => {
            let state = parse_vst3_state(state_json)?;
            Ok(Action::ClipVst3RestoreState {
                track_name,
                clip_idx,
                instance_id,
                state,
            })
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        "lv2" => Ok(Action::ClipSetLv2PluginState {
            track_name,
            clip_idx,
            instance_id,
            state: parse_state_bytes(state_json)?,
        }),
        _ => Err(format!(
            "Unsupported clip plugin state restore format: {format}"
        )),
    }
}

fn parse_state_bytes(json: &str) -> Result<Vec<u8>, String> {
    let value: serde_json::Value = parse_json(json)?;
    Ok(value
        .get("bytes")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64())
                .map(|v| v as u8)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default())
}

fn parse_clap_state(json: &str) -> Result<crate::clap::ClapPluginState, String> {
    let bytes = parse_state_bytes(json)?;
    Ok(crate::clap::ClapPluginState { bytes })
}

fn parse_vst3_state(json: &str) -> Result<crate::vst3::state::Vst3PluginState, String> {
    let value: serde_json::Value = parse_json(json)?;
    let plugin_id = value
        .get("plugin_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let bytes_from_array = |key: &str| -> Result<Vec<u8>, String> {
        value
            .get(key)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_u64())
                    .map(|v| v as u8)
                    .collect::<Vec<_>>()
            })
            .ok_or_else(|| format!("Missing or invalid {key} byte array"))
    };
    let component_state = bytes_from_array("component_state")?;
    let controller_state = bytes_from_array("controller_state")?;
    Ok(crate::vst3::state::Vst3PluginState {
        plugin_id,
        component_state,
        controller_state,
    })
}

fn parse_tempo_map(json: &str) -> Result<Action, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("Invalid tempo map JSON: {e}"))?;
    let tempo_points = value
        .get("tempo_points")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    Some(TempoPoint {
                        sample: p.get("sample")?.as_u64()? as usize,
                        bpm: p.get("bpm")?.as_f64()?,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let time_signature_points = value
        .get("time_signature_points")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    Some(TimeSignaturePoint {
                        sample: p.get("sample")?.as_u64()? as usize,
                        numerator: p.get("numerator")?.as_u64()? as u16,
                        denominator: p.get("denominator")?.as_u64()? as u16,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Action::SetTempoMap {
        tempo_points,
        time_signature_points,
    })
}

fn parse_add_clip(mut args: OscArgs<'_>) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let name = args.next_string()?;
    let start = args.next_int()? as usize;
    let length = args.next_int()? as usize;
    let offset = args.next_int()? as usize;
    let input_channel = args.next_int()? as usize;
    let muted = args.next_bool()?;
    let fade_enabled = args.next_bool()?;
    let fade_in_samples = args.next_int()? as usize;
    let fade_out_samples = args.next_int()? as usize;
    let kind = parse_kind(&args.next_string()?)?;
    let source_name = args.next_string().ok().filter(|s| !s.is_empty());
    let preview_name = args.next_string().ok().filter(|s| !s.is_empty());
    Ok(Action::AddClip {
        clip_id: String::new(),
        name,
        track_name,
        start,
        length,
        offset,
        input_channel,
        muted,
        peaks_file: None,
        kind,
        fade_enabled,
        fade_in_samples,
        fade_out_samples,
        source_name,
        source_offset: None,
        source_length: None,
        preview_name,
        pitch_correction_points: Vec::new(),
        pitch_correction_frame_likeness: None,
        pitch_correction_inertia_ms: None,
        pitch_correction_formant_compensation: None,
        plugin_graph_json: None,
    })
}

fn parse_int_list(value: &str) -> Result<Vec<usize>, String> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|s| {
            s.trim()
                .parse()
                .map_err(|_| format!("Invalid integer: {s}"))
        })
        .collect()
}

fn parse_json(json: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(json).map_err(|e| format!("Invalid JSON: {e}"))
}

fn parse_audio_clip_data(json: &str) -> Result<AudioClipData, String> {
    serde_json::from_str(json).map_err(|e| format!("Invalid audio clip JSON: {e}"))
}

fn parse_midi_clip_data(json: &str) -> Result<MidiClipData, String> {
    serde_json::from_str(json).map_err(|e| format!("Invalid MIDI clip JSON: {e}"))
}

fn parse_clip_pitch_correction(mut args: OscArgs<'_>) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let clip_index = args.next_int()? as usize;
    let json = args.next_string()?;
    if json.is_empty() {
        return Ok(Action::SetClipPitchCorrection {
            track_name,
            clip_index,
            preview_name: None,
            source_name: None,
            source_offset: None,
            source_length: None,
            pitch_correction_points: Vec::new(),
            pitch_correction_frame_likeness: None,
            pitch_correction_inertia_ms: None,
            pitch_correction_formant_compensation: None,
        });
    }
    let value: serde_json::Value = parse_json(&json)?;
    let preview_name = value
        .get("preview_name")
        .and_then(|v| v.as_str())
        .map(String::from);
    let source_name = value
        .get("source_name")
        .and_then(|v| v.as_str())
        .map(String::from);
    let source_offset = value
        .get("source_offset")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let source_length = value
        .get("source_length")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let pitch_correction_frame_likeness = value
        .get("frame_likeness")
        .and_then(|v| v.as_f64())
        .map(|v| v as f32);
    let pitch_correction_inertia_ms = value
        .get("inertia_ms")
        .and_then(|v| v.as_u64())
        .map(|v| v as u16);
    let pitch_correction_formant_compensation =
        value.get("formant_compensation").and_then(|v| v.as_bool());
    let pitch_correction_points = value
        .get("points")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    Some(PitchCorrectionPointData {
                        start_sample: p.get("start_sample")?.as_u64()? as usize,
                        length_samples: p.get("length_samples")?.as_u64()? as usize,
                        detected_midi_pitch: p.get("detected_midi_pitch")?.as_f64()? as f32,
                        target_midi_pitch: p.get("target_midi_pitch")?.as_f64()? as f32,
                        clarity: p.get("clarity")?.as_f64()? as f32,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Action::SetClipPitchCorrection {
        track_name,
        clip_index,
        preview_name,
        source_name,
        source_offset,
        source_length,
        pitch_correction_points,
        pitch_correction_frame_likeness,
        pitch_correction_inertia_ms,
        pitch_correction_formant_compensation,
    })
}

fn parse_midi_note_data(value: &serde_json::Value) -> Option<MidiNoteData> {
    Some(MidiNoteData {
        start_sample: value.get("start_sample")?.as_u64()? as usize,
        length_samples: value.get("length_samples")?.as_u64()? as usize,
        pitch: value.get("pitch")?.as_u64()? as u8,
        velocity: value.get("velocity")?.as_u64()? as u8,
        channel: value.get("channel")?.as_u64()? as u8,
    })
}

fn parse_midi_controller_data(value: &serde_json::Value) -> Option<MidiControllerData> {
    Some(MidiControllerData {
        sample: value.get("sample")?.as_u64()? as usize,
        controller: value.get("controller")?.as_u64()? as u8,
        value: value.get("value")?.as_u64()? as u8,
        channel: value.get("channel")?.as_u64()? as u8,
    })
}

fn parse_indexed_midi_notes(json: &str) -> Result<Vec<(usize, MidiNoteData)>, String> {
    let value: serde_json::Value = parse_json(json)?;
    let arr = value
        .as_array()
        .ok_or("Expected JSON array of indexed notes")?;
    arr.iter()
        .map(|entry| {
            let index = entry
                .get("index")
                .and_then(|v| v.as_u64())
                .ok_or("Missing note index")? as usize;
            let note = parse_midi_note_data(entry).ok_or("Invalid note data")?;
            Ok((index, note))
        })
        .collect()
}

fn parse_indexed_midi_controllers(json: &str) -> Result<Vec<(usize, MidiControllerData)>, String> {
    let value: serde_json::Value = parse_json(json)?;
    let arr = value
        .as_array()
        .ok_or("Expected JSON array of indexed controllers")?;
    arr.iter()
        .map(|entry| {
            let index = entry
                .get("index")
                .and_then(|v| v.as_u64())
                .ok_or("Missing controller index")? as usize;
            let ctrl = parse_midi_controller_data(entry).ok_or("Invalid controller data")?;
            Ok((index, ctrl))
        })
        .collect()
}

fn parse_insert_notes(mut args: OscArgs<'_>) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let clip_index = args.next_int()? as usize;
    let notes = parse_indexed_midi_notes(&args.next_string()?)?;
    Ok(Action::InsertMidiNotes {
        track_name,
        clip_index,
        notes,
    })
}

fn parse_delete_notes(mut args: OscArgs<'_>) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let clip_index = args.next_int()? as usize;
    let value: serde_json::Value = parse_json(&args.next_string()?)?;
    let note_indices = value
        .get("indices")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64())
                .map(|v| v as usize)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let deleted_notes = value
        .get("deleted")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let index = entry.get("index")?.as_u64()? as usize;
                    let note = parse_midi_note_data(entry)?;
                    Some((index, note))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Action::DeleteMidiNotes {
        track_name,
        clip_index,
        note_indices,
        deleted_notes,
    })
}

fn parse_modify_notes(mut args: OscArgs<'_>) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let clip_index = args.next_int()? as usize;
    let value: serde_json::Value = parse_json(&args.next_string()?)?;
    let note_indices = value
        .get("indices")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64())
                .map(|v| v as usize)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let new_notes = value
        .get("new")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(parse_midi_note_data)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let old_notes = value
        .get("old")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(parse_midi_note_data)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Action::ModifyMidiNotes {
        track_name,
        clip_index,
        note_indices,
        new_notes,
        old_notes,
    })
}

fn parse_insert_controllers(mut args: OscArgs<'_>) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let clip_index = args.next_int()? as usize;
    let controllers = parse_indexed_midi_controllers(&args.next_string()?)?;
    Ok(Action::InsertMidiControllers {
        track_name,
        clip_index,
        controllers,
    })
}

fn parse_delete_controllers(mut args: OscArgs<'_>) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let clip_index = args.next_int()? as usize;
    let value: serde_json::Value = parse_json(&args.next_string()?)?;
    let controller_indices = value
        .get("indices")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64())
                .map(|v| v as usize)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let deleted_controllers = value
        .get("deleted")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let index = entry.get("index")?.as_u64()? as usize;
                    let ctrl = parse_midi_controller_data(entry)?;
                    Some((index, ctrl))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Action::DeleteMidiControllers {
        track_name,
        clip_index,
        controller_indices,
        deleted_controllers,
    })
}

fn parse_modify_controllers(mut args: OscArgs<'_>) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let clip_index = args.next_int()? as usize;
    let value: serde_json::Value = parse_json(&args.next_string()?)?;
    let controller_indices = value
        .get("indices")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64())
                .map(|v| v as usize)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let new_controllers = value
        .get("new")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(parse_midi_controller_data)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let old_controllers = value
        .get("old")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(parse_midi_controller_data)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(Action::ModifyMidiControllers {
        track_name,
        clip_index,
        controller_indices,
        new_controllers,
        old_controllers,
    })
}

fn parse_sysex_events(mut args: OscArgs<'_>) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let clip_index = args.next_int()? as usize;
    let json = args.next_string()?;
    let value: serde_json::Value = parse_json(&json)?;
    let events = value
        .as_array()
        .ok_or("Expected JSON array of SysEx events")?;
    let new_sysex_events = events
        .iter()
        .map(|entry| {
            let sample = entry
                .get("sample")
                .and_then(|v| v.as_u64())
                .ok_or("Missing SysEx sample")? as usize;
            let data = entry
                .get("data")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_u64())
                        .map(|v| v as u8)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            Ok(MidiRawEventData { sample, data })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Action::SetMidiSysExEvents {
        track_name,
        clip_index,
        new_sysex_events,
        old_sysex_events: Vec::new(),
    })
}

fn parse_plugin_graph_node(value: &str) -> Result<PluginGraphNode, String> {
    match value {
        "track_input" => Ok(PluginGraphNode::TrackInput),
        "track_output" => Ok(PluginGraphNode::TrackOutput),
        _ => {
            if let Some(rest) = value.strip_prefix("clap_") {
                let id = rest
                    .parse()
                    .map_err(|_| format!("Invalid clap instance id: {rest}"))?;
                return Ok(PluginGraphNode::ClapPluginInstance(id));
            }
            if let Some(rest) = value.strip_prefix("vst3_") {
                let id = rest
                    .parse()
                    .map_err(|_| format!("Invalid vst3 instance id: {rest}"))?;
                return Ok(PluginGraphNode::Vst3PluginInstance(id));
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            if let Some(rest) = value.strip_prefix("lv2_") {
                let id = rest
                    .parse()
                    .map_err(|_| format!("Invalid lv2 instance id: {rest}"))?;
                return Ok(PluginGraphNode::Lv2PluginInstance(id));
            }
            Err(format!("Unsupported plugin graph node: {value}"))
        }
    }
}

fn parse_plugin_graph_connection(
    mut args: OscArgs<'_>,
    connect: bool,
    kind: Kind,
) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let from_node = parse_plugin_graph_node(&args.next_string()?)?;
    let from_port = args.next_int()? as usize;
    let to_node = parse_plugin_graph_node(&args.next_string()?)?;
    let to_port = args.next_int()? as usize;
    if connect {
        match kind {
            Kind::Audio => Ok(Action::TrackConnectPluginAudio {
                track_name,
                from_node,
                from_port,
                to_node,
                to_port,
            }),
            Kind::MIDI => Ok(Action::TrackConnectPluginMidi {
                track_name,
                from_node,
                from_port,
                to_node,
                to_port,
            }),
        }
    } else {
        match kind {
            Kind::Audio => Ok(Action::TrackDisconnectPluginAudio {
                track_name,
                from_node,
                from_port,
                to_node,
                to_port,
            }),
            Kind::MIDI => Ok(Action::TrackDisconnectPluginMidi {
                track_name,
                from_node,
                from_port,
                to_node,
                to_port,
            }),
        }
    }
}

fn parse_connectable_ref(value: &str) -> Result<ConnectableRef, String> {
    match value {
        "track_input" => Ok(ConnectableRef::TrackInput),
        "track_output" => Ok(ConnectableRef::TrackOutput),
        _ => {
            if let Some(rest) = value.strip_prefix("child:") {
                return Ok(ConnectableRef::ChildTrack(rest.to_string()));
            }
            if let Some(rest) = value.strip_prefix("clap_") {
                let id = rest
                    .parse()
                    .map_err(|_| format!("Invalid clap instance id: {rest}"))?;
                return Ok(ConnectableRef::ClapPlugin(id));
            }
            if let Some(rest) = value.strip_prefix("vst3_") {
                let id = rest
                    .parse()
                    .map_err(|_| format!("Invalid vst3 instance id: {rest}"))?;
                return Ok(ConnectableRef::Vst3Plugin(id));
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            if let Some(rest) = value.strip_prefix("lv2_") {
                let id = rest
                    .parse()
                    .map_err(|_| format!("Invalid lv2 instance id: {rest}"))?;
                return Ok(ConnectableRef::Lv2Plugin(id));
            }
            Err(format!("Unsupported connectable ref: {value}"))
        }
    }
}

fn parse_track_connectable(
    mut args: OscArgs<'_>,
    connect: bool,
    kind: Kind,
) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let from = parse_connectable_ref(&args.next_string()?)?;
    let from_port = args.next_int()? as usize;
    let to = parse_connectable_ref(&args.next_string()?)?;
    let to_port = args.next_int()? as usize;
    if connect {
        match kind {
            Kind::Audio => Ok(Action::TrackConnectAudio {
                track_name,
                from,
                from_port,
                to,
                to_port,
            }),
            Kind::MIDI => Ok(Action::TrackConnectMidi {
                track_name,
                from,
                from_port,
                to,
                to_port,
            }),
        }
    } else {
        match kind {
            Kind::Audio => Ok(Action::TrackDisconnectAudio {
                track_name,
                from,
                from_port,
                to,
                to_port,
            }),
            Kind::MIDI => Ok(Action::TrackDisconnectMidi {
                track_name,
                from,
                from_port,
                to,
                to_port,
            }),
        }
    }
}

fn parse_plugin_set_param(mut args: OscArgs<'_>) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let format = args.next_string()?;
    let instance_id = args.next_int()? as usize;
    let param_id = args.next_int()? as u32;
    match format.as_str() {
        "clap" => {
            let value = args.next_double_or_float()?;
            Ok(Action::TrackSetClapParameter {
                track_name,
                instance_id,
                param_id,
                value,
            })
        }
        "vst3" => {
            let value = args.next_float()?;
            Ok(Action::TrackSetVst3Parameter {
                track_name,
                instance_id,
                param_id,
                value,
            })
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        "lv2" => {
            let index = param_id;
            let value = args.next_float()?;
            Ok(Action::TrackSetLv2ControlValue {
                track_name,
                instance_id,
                index,
                value,
            })
        }
        _ => Err(format!("Unsupported plugin format: {format}")),
    }
}

fn parse_plugin_set_param_at(mut args: OscArgs<'_>) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let format = args.next_string()?;
    let instance_id = args.next_int()? as usize;
    let param_id = args.next_int()? as u32;
    let value = args.next_double_or_float()?;
    let frame = args.next_int()? as u32;
    if format != "clap" {
        return Err(format!(
            "set_param_at only supports CLAP plugins, got: {format}"
        ));
    }
    Ok(Action::TrackSetClapParameterAt {
        track_name,
        instance_id,
        param_id,
        value,
        frame,
    })
}

fn parse_clap_param_edit(mut args: OscArgs<'_>, begin: bool) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let format = args.next_string()?;
    let instance_id = args.next_int()? as usize;
    let param_id = args.next_int()? as u32;
    let frame = args.next_int()? as u32;
    if format != "clap" {
        return Err(format!(
            "parameter edit gestures only support CLAP plugins, got: {format}"
        ));
    }
    if begin {
        Ok(Action::TrackBeginClapParameterEdit {
            track_name,
            instance_id,
            param_id,
            frame,
        })
    } else {
        Ok(Action::TrackEndClapParameterEdit {
            track_name,
            instance_id,
            param_id,
            frame,
        })
    }
}

fn parse_vst3_graph_node(value: &str) -> Result<crate::message::Vst3GraphNode, String> {
    match value {
        "track_input" => Ok(crate::message::Vst3GraphNode::TrackInput),
        "track_output" => Ok(crate::message::Vst3GraphNode::TrackOutput),
        _ => {
            if let Some(rest) = value.strip_prefix("vst3_") {
                let id = rest
                    .parse()
                    .map_err(|_| format!("Invalid vst3 instance id: {rest}"))?;
                return Ok(crate::message::Vst3GraphNode::PluginInstance(id));
            }
            Err(format!("Unsupported VST3 graph node: {value}"))
        }
    }
}

fn parse_vst3_graph_connection(mut args: OscArgs<'_>, connect: bool) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let from_node = parse_vst3_graph_node(&args.next_string()?)?;
    let from_port = args.next_int()? as usize;
    let to_node = parse_vst3_graph_node(&args.next_string()?)?;
    let to_port = args.next_int()? as usize;
    if connect {
        Ok(Action::TrackConnectVst3Audio {
            track_name,
            from_node,
            from_port,
            to_node,
            to_port,
        })
    } else {
        Ok(Action::TrackDisconnectVst3Audio {
            track_name,
            from_node,
            from_port,
            to_node,
            to_port,
        })
    }
}

fn parse_clip_plugin_set_param(mut args: OscArgs<'_>) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let format = args.next_string()?;
    let clip_idx = args.next_int()? as usize;
    let instance_id = args.next_int()? as usize;
    let param_id = args.next_int()? as u32;
    match format.as_str() {
        "clap" => {
            let value = args.next_double_or_float()?;
            Ok(Action::ClipSetClapParameter {
                track_name,
                clip_idx,
                instance_id,
                param_id,
                value,
            })
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        "lv2" => {
            let index = param_id;
            let value = args.next_float()?;
            Ok(Action::ClipSetLv2ControlValue {
                track_name,
                clip_idx,
                instance_id,
                index,
                value,
            })
        }
        _ => Err(format!("Unsupported clip plugin format: {format}")),
    }
}

fn parse_track_midi_learn_target(value: &str) -> Result<TrackMidiLearnTarget, String> {
    match value {
        "volume" => Ok(TrackMidiLearnTarget::Volume),
        "balance" => Ok(TrackMidiLearnTarget::Balance),
        "mute" => Ok(TrackMidiLearnTarget::Mute),
        "solo" => Ok(TrackMidiLearnTarget::Solo),
        "arm" => Ok(TrackMidiLearnTarget::Arm),
        "input_monitor" => Ok(TrackMidiLearnTarget::InputMonitor),
        "disk_monitor" => Ok(TrackMidiLearnTarget::DiskMonitor),
        _ => Err(format!("Unsupported track MIDI learn target: {value}")),
    }
}

fn parse_global_midi_learn_target(value: &str) -> Result<GlobalMidiLearnTarget, String> {
    match value {
        "play_pause" => Ok(GlobalMidiLearnTarget::PlayPause),
        "stop" => Ok(GlobalMidiLearnTarget::Stop),
        "record_toggle" => Ok(GlobalMidiLearnTarget::RecordToggle),
        _ => Err(format!("Unsupported global MIDI learn target: {value}")),
    }
}

fn parse_session_midi_learn_target(value: &str) -> Result<SessionMidiLearnTarget, String> {
    if let Some(rest) = value.strip_prefix("slot:") {
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        if parts.len() == 2 {
            let scene_index = parts[1]
                .parse()
                .map_err(|_| format!("Invalid scene index: {}", parts[1]))?;
            return Ok(SessionMidiLearnTarget::Slot {
                track_name: parts[0].to_string(),
                scene_index,
            });
        }
    } else if let Some(rest) = value.strip_prefix("scene:") {
        let scene_index = rest
            .parse()
            .map_err(|_| format!("Invalid scene index: {rest}"))?;
        return Ok(SessionMidiLearnTarget::Scene(scene_index));
    } else if let Some(rest) = value.strip_prefix("stop_track:") {
        return Ok(SessionMidiLearnTarget::StopTrack(rest.to_string()));
    } else if value == "stop_all" {
        return Ok(SessionMidiLearnTarget::StopAll);
    }
    Err(format!("Unsupported session MIDI learn target: {value}"))
}

fn parse_midi_learn_binding(json: &str) -> Result<Option<MidiLearnBinding>, String> {
    if json.is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value = parse_json(json)?;
    let device = value
        .get("device")
        .and_then(|v| v.as_str())
        .map(String::from);
    let channel = value
        .get("channel")
        .and_then(|v| v.as_u64())
        .ok_or("Missing MIDI learn channel")? as u8;
    let cc = value
        .get("cc")
        .and_then(|v| v.as_u64())
        .ok_or("Missing MIDI learn cc")? as u8;
    Ok(Some(MidiLearnBinding {
        device,
        channel,
        cc,
    }))
}

fn parse_open_audio_device(mut args: OscArgs<'_>) -> Result<Action, String> {
    let json = args.next_string()?;
    let value: serde_json::Value = parse_json(&json)?;
    let get_usize = |key: &str| -> Result<usize, String> {
        value
            .get(key)
            .and_then(|v| v.as_u64())
            .ok_or_else(|| format!("Missing or invalid {key}"))
            .map(|v| v as usize)
    };
    let get_i32 = |key: &str| -> Result<i32, String> {
        value
            .get(key)
            .and_then(|v| v.as_i64())
            .or_else(|| value.get(key).and_then(|v| v.as_u64()).map(|v| v as i64))
            .ok_or_else(|| format!("Missing or invalid {key}"))
            .map(|v| v as i32)
    };
    let get_bool = |key: &str| -> Result<bool, String> {
        value
            .get(key)
            .and_then(|v| v.as_bool())
            .ok_or_else(|| format!("Missing or invalid {key}"))
    };
    let get_string = |key: &str| -> Result<String, String> {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| format!("Missing or invalid {key}"))
    };
    let input_device = value
        .get("input_device")
        .and_then(|v| v.as_str())
        .map(String::from);
    Ok(Action::OpenAudioDevice {
        device: get_string("device")?,
        input_device,
        sample_rate_hz: get_i32("sample_rate_hz")?,
        bits: get_i32("bits")?,
        exclusive: get_bool("exclusive")?,
        period_frames: get_usize("period_frames")?,
        nperiods: get_usize("nperiods")?,
        sync_mode: get_bool("sync_mode")?,
        actual_period_frames: get_usize("actual_period_frames")?,
        input_channels: get_usize("input_channels")?,
        output_channels: get_usize("output_channels")?,
        bytes_per_frame: get_usize("bytes_per_frame")?,
    })
}

fn parse_offline_bounce(mut args: OscArgs<'_>) -> Result<Action, String> {
    let track_name = args.next_string()?;
    let output_path = args.next_string()?;
    let start_sample = args.next_int()? as usize;
    let length_samples = args.next_int()? as usize;
    let lanes_json = args.next_string()?;
    let apply_fader = args.next_bool()?;
    let automation_lanes: Vec<OfflineAutomationLane> = if lanes_json.is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(&lanes_json)
            .map_err(|e| format!("Invalid automation lanes JSON: {e}"))?
    };
    Ok(Action::TrackOfflineBounce {
        track_name,
        output_path,
        start_sample,
        length_samples,
        automation_lanes,
        apply_fader,
    })
}

fn parse_automation_mode(value: &str) -> Result<TrackAutomationMode, String> {
    match value {
        "read" => Ok(TrackAutomationMode::Read),
        "touch" => Ok(TrackAutomationMode::Touch),
        "latch" => Ok(TrackAutomationMode::Latch),
        "write" => Ok(TrackAutomationMode::Write),
        _ => Err(format!("Unknown automation mode: {value}")),
    }
}

fn parse_automation_target(value: &str) -> Result<OfflineAutomationTarget, String> {
    match value {
        "volume" => Ok(OfflineAutomationTarget::Volume),
        "balance" => Ok(OfflineAutomationTarget::Balance),
        _ => {
            if let Some(rest) = value.strip_prefix("midi_cc_") {
                let parts: Vec<&str> = rest.split('_').collect();
                if parts.len() == 2 {
                    let channel: u8 = parts[0]
                        .parse()
                        .map_err(|_| format!("Invalid MIDI channel: {}", parts[0]))?;
                    let cc: u8 = parts[1]
                        .parse()
                        .map_err(|_| format!("Invalid MIDI CC: {}", parts[1]))?;
                    if (1..=16).contains(&channel) && cc <= 127 {
                        return Ok(OfflineAutomationTarget::MidiCc {
                            channel: channel - 1,
                            cc,
                        });
                    }
                }
            }
            Err(format!("Unsupported automation target: {value}"))
        }
    }
}

fn parse_osc_string(packet: &[u8], offset: usize) -> Option<(String, usize)> {
    if offset >= packet.len() {
        return None;
    }
    let end = packet[offset..].iter().position(|byte| *byte == 0)? + offset;
    let value = std::str::from_utf8(&packet[offset..end]).ok()?.to_string();
    let next = (end + 4) & !3;
    if next > packet.len() {
        return None;
    }
    Some((value, next))
}

struct OscArgs<'a> {
    packet: &'a [u8],
    type_tags: &'a [u8],
    offset: usize,
}

impl<'a> OscArgs<'a> {
    fn is_empty(&self) -> bool {
        self.type_tags.is_empty()
    }

    fn ensure_tag(&mut self, expected: u8) -> Result<(), String> {
        let tag = self
            .type_tags
            .first()
            .copied()
            .ok_or("Missing OSC argument")?;
        if tag != expected {
            return Err(format!(
                "Expected OSC argument type '{}', got '{}'",
                expected as char, tag as char
            ));
        }
        self.type_tags = &self.type_tags[1..];
        Ok(())
    }

    fn next_string(&mut self) -> Result<String, String> {
        self.ensure_tag(b's')?;
        let (value, next) = parse_osc_string(self.packet, self.offset)
            .ok_or_else(|| "Malformed OSC string argument".to_string())?;
        self.offset = next;
        Ok(value)
    }

    fn next_int(&mut self) -> Result<i32, String> {
        self.ensure_tag(b'i')?;
        if self.offset.saturating_add(4) > self.packet.len() {
            return Err("Truncated OSC int argument".to_string());
        }
        let value = i32::from_be_bytes([
            self.packet[self.offset],
            self.packet[self.offset + 1],
            self.packet[self.offset + 2],
            self.packet[self.offset + 3],
        ]);
        self.offset += 4;
        Ok(value)
    }

    fn next_float(&mut self) -> Result<f32, String> {
        self.ensure_tag(b'f')?;
        if self.offset.saturating_add(4) > self.packet.len() {
            return Err("Truncated OSC float argument".to_string());
        }
        let value = f32::from_be_bytes([
            self.packet[self.offset],
            self.packet[self.offset + 1],
            self.packet[self.offset + 2],
            self.packet[self.offset + 3],
        ]);
        self.offset += 4;
        Ok(value)
    }

    fn next_double_or_float(&mut self) -> Result<f64, String> {
        let tag = self
            .type_tags
            .first()
            .copied()
            .ok_or("Missing OSC argument")?;
        match tag {
            b'd' => {
                self.type_tags = &self.type_tags[1..];
                if self.offset.saturating_add(8) > self.packet.len() {
                    return Err("Truncated OSC double argument".to_string());
                }
                let value = f64::from_be_bytes([
                    self.packet[self.offset],
                    self.packet[self.offset + 1],
                    self.packet[self.offset + 2],
                    self.packet[self.offset + 3],
                    self.packet[self.offset + 4],
                    self.packet[self.offset + 5],
                    self.packet[self.offset + 6],
                    self.packet[self.offset + 7],
                ]);
                self.offset += 8;
                Ok(value)
            }
            b'f' => Ok(self.next_float()? as f64),
            _ => Err(format!("Expected double or float, got '{}'", tag as char)),
        }
    }

    fn next_bool(&mut self) -> Result<bool, String> {
        let tag = self
            .type_tags
            .first()
            .copied()
            .ok_or("Missing OSC argument")?;
        match tag {
            b'T' => {
                self.type_tags = &self.type_tags[1..];
                Ok(true)
            }
            b'F' => {
                self.type_tags = &self.type_tags[1..];
                Ok(false)
            }
            b'i' => Ok(self.next_int()? != 0),
            _ => Err(format!("Expected bool (T/F/i), got '{}'", tag as char)),
        }
    }
}

// ---------------------------------------------------------------------------
// OSC packet encoding for replies
// ---------------------------------------------------------------------------

pub fn encode_osc_string(buf: &mut Vec<u8>, value: &str) {
    buf.extend_from_slice(value.as_bytes());
    buf.push(0);
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

pub fn encode_osc_int(buf: &mut Vec<u8>, value: i32) {
    buf.extend_from_slice(&value.to_be_bytes());
}

pub fn encode_osc_float(buf: &mut Vec<u8>, value: f32) {
    buf.extend_from_slice(&value.to_be_bytes());
}

pub fn build_osc_packet(address: &str, type_tags: &str, args: &[OscArg]) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_osc_string(&mut buf, address);
    encode_osc_string(&mut buf, &format!(",{type_tags}"));
    for arg in args {
        match arg {
            OscArg::String(s) => encode_osc_string(&mut buf, s),
            OscArg::Int(i) => encode_osc_int(&mut buf, *i),
            OscArg::Float(f) => encode_osc_float(&mut buf, *f),
        }
    }
    buf
}

pub fn build_error_packet(reason: &str) -> Vec<u8> {
    build_osc_packet("/error", "s", &[OscArg::String(reason.to_string())])
}

#[derive(Debug, Clone)]
pub enum OscArg {
    String(String),
    Int(i32),
    Float(f32),
}

#[cfg(test)]
mod tests {
    use super::{OscArg, build_error_packet, build_osc_packet, parse_osc_request};
    use crate::connectable::ConnectableRef;
    use crate::kind::Kind;
    use crate::message::{
        Action, OfflineAutomationTarget, PluginGraphNode, TrackAutomationMode, TrackMidiLearnTarget,
    };

    fn osc_packet(address: &str) -> Vec<u8> {
        build_osc_packet(address, "", &[])
    }

    fn osc_packet_with_args(address: &str, type_tags: &str, args: &[OscArg]) -> Vec<u8> {
        build_osc_packet(address, type_tags, args)
    }

    #[test]
    fn parses_basic_transport_messages() {
        assert!(matches!(
            parse_osc_request(&osc_packet("/transport/play")).unwrap(),
            Action::Play
        ));
        assert!(matches!(
            parse_osc_request(&osc_packet("/transport/pause")).unwrap(),
            Action::Pause
        ));
        assert!(matches!(
            parse_osc_request(&osc_packet("/transport/stop")).unwrap(),
            Action::Stop
        ));
        assert!(matches!(
            parse_osc_request(&osc_packet("/transport/start")).unwrap(),
            Action::TransportPosition(0)
        ));
        assert!(matches!(
            parse_osc_request(&osc_packet("/transport/end")).unwrap(),
            Action::JumpToEnd
        ));
    }

    #[test]
    fn keeps_compatibility_transport_jump_aliases() {
        assert!(matches!(
            parse_osc_request(&osc_packet("/transport/jump_to_start")).unwrap(),
            Action::TransportPosition(0)
        ));
        assert!(matches!(
            parse_osc_request(&osc_packet("/transport/jump_to_end")).unwrap(),
            Action::JumpToEnd
        ));
    }

    #[test]
    fn rejects_removed_short_aliases() {
        assert!(parse_osc_request(&osc_packet("/start")).is_err());
        assert!(parse_osc_request(&osc_packet("/stop")).is_err());
        assert!(parse_osc_request(&osc_packet("/pause")).is_err());
        assert!(parse_osc_request(&osc_packet("/jump_to_start")).is_err());
        assert!(parse_osc_request(&osc_packet("/jump_to_end")).is_err());
    }

    #[test]
    fn parses_session_messages() {
        let packet = osc_packet_with_args(
            "/session/launch",
            "si",
            &[OscArg::String("kick".to_string()), OscArg::Int(2)],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::Session(crate::message::SessionAction::LaunchClip {
                track_name,
                scene_index,
                clip_id,
                ..
            }) if track_name == "kick" && scene_index == 2 && clip_id.is_empty()
        ));

        let packet = osc_packet_with_args(
            "/session/stop",
            "si",
            &[OscArg::String("snare".to_string()), OscArg::Int(1)],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::Session(crate::message::SessionAction::StopClip {
                track_name,
                scene_index,
                ..
            }) if track_name == "snare" && scene_index == 1
        ));

        let packet = osc_packet_with_args("/session/scene", "i", &[OscArg::Int(3)]);
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::Session(crate::message::SessionAction::LaunchScene {
                scene_index,
                ..
            }) if scene_index == 3
        ));

        assert!(matches!(
            parse_osc_request(&osc_packet("/session/stopall")).unwrap(),
            Action::Session(crate::message::SessionAction::StopAllClips)
        ));
    }

    #[test]
    fn parses_transport_position() {
        let packet = osc_packet_with_args("/transport/position", "i", &[OscArg::Int(44100)]);
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TransportPosition(44100)
        ));
    }

    #[test]
    fn parses_transport_record() {
        let packet = osc_packet_with_args("/transport/record", "i", &[OscArg::Int(1)]);
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::SetRecordEnabled(true)
        ));
    }

    #[test]
    fn parses_transport_tempo_and_time_signature() {
        let packet = osc_packet_with_args("/transport/tempo", "f", &[OscArg::Float(128.5)]);
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::SetTempo(bpm) if (bpm - 128.5).abs() < f64::EPSILON
        ));

        let packet = osc_packet_with_args(
            "/transport/time_signature",
            "ii",
            &[OscArg::Int(7), OscArg::Int(8)],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::SetTimeSignature {
                numerator: 7,
                denominator: 8
            }
        ));
    }

    #[test]
    fn parses_track_add() {
        let packet = osc_packet_with_args(
            "/track/add",
            "siiiii",
            &[
                OscArg::String("vox".to_string()),
                OscArg::Int(2),
                OscArg::Int(0),
                OscArg::Int(2),
                OscArg::Int(0),
                OscArg::Int(0),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::AddTrack {
                name,
                audio_ins: 2,
                midi_ins: 0,
                audio_outs: 2,
                midi_outs: 0,
                folder: false,
            } if name == "vox"
        ));
    }

    #[test]
    fn parses_track_management() {
        let packet =
            osc_packet_with_args("/track/remove", "s", &[OscArg::String("old".to_string())]);
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::RemoveTrack(name) if name == "old"
        ));

        let packet = osc_packet_with_args(
            "/track/rename",
            "ss",
            &[
                OscArg::String("old".to_string()),
                OscArg::String("new".to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::RenameTrack { old_name, new_name } if old_name == "old" && new_name == "new"
        ));

        let packet = osc_packet_with_args(
            "/track/set_parent",
            "ss",
            &[
                OscArg::String("child".to_string()),
                OscArg::String("parent".to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackSetParent { track_name, parent_name: Some(parent) }
                if track_name == "child" && parent == "parent"
        ));
    }

    #[test]
    fn parses_track_mixing() {
        let packet = osc_packet_with_args(
            "/track/level",
            "sf",
            &[OscArg::String("drums".to_string()), OscArg::Float(-6.0)],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackLevel(name, level) if name == "drums" && (level - -6.0).abs() < f32::EPSILON
        ));

        let packet = osc_packet_with_args(
            "/track/mute",
            "si",
            &[OscArg::String("drums".to_string()), OscArg::Int(1)],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackToggleMute(name) if name == "drums"
        ));
    }

    #[test]
    fn parses_routing() {
        let packet = osc_packet_with_args(
            "/connect",
            "sisis",
            &[
                OscArg::String("a".to_string()),
                OscArg::Int(0),
                OscArg::String("b".to_string()),
                OscArg::Int(0),
                OscArg::String("audio".to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::Connect { from_track, from_port, to_track, to_port, kind }
                if from_track == "a" && from_port == 0 && to_track == "b" && to_port == 0
                    && kind == Kind::Audio
        ));

        let packet = osc_packet_with_args(
            "/disconnect",
            "sisis",
            &[
                OscArg::String("a".to_string()),
                OscArg::Int(0),
                OscArg::String("b".to_string()),
                OscArg::Int(0),
                OscArg::String("midi".to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::Disconnect { from_track, from_port, to_track, to_port, kind }
                if from_track == "a" && from_port == 0 && to_track == "b" && to_port == 0
                    && kind == Kind::MIDI
        ));
    }

    #[test]
    fn parses_plugin_commands() {
        let packet = osc_packet_with_args(
            "/plugin/load",
            "sss",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("clap".to_string()),
                OscArg::String("rs.maolan.widener".to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackLoadClapPlugin { track_name, plugin_id, instance_id: None }
                if track_name == "drums" && plugin_id == "rs.maolan.widener"
        ));

        let packet = osc_packet_with_args(
            "/plugin/bypass",
            "ssii",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("clap".to_string()),
                OscArg::Int(3),
                OscArg::Int(1),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackSetPluginBypassed { track_name, instance_id, format, bypassed }
                if track_name == "drums" && instance_id == 3 && format == "clap" && bypassed
        ));
    }

    #[test]
    fn parses_automation_commands() {
        let packet = osc_packet_with_args(
            "/automation/mode",
            "ss",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("touch".to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackAutomationSetMode { track_name, mode: TrackAutomationMode::Touch }
                if track_name == "drums"
        ));

        let packet = osc_packet_with_args(
            "/automation/insert_point",
            "ssif",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("midi_cc_1_7".to_string()),
                OscArg::Int(44100),
                OscArg::Float(64.0),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackAutomationInsertPoint { track_name, target, sample, value }
                if track_name == "drums"
                    && matches!(target, OfflineAutomationTarget::MidiCc { channel: 0, cc: 7 })
                    && sample == 44100
                    && (value - 64.0).abs() < f32::EPSILON
        ));
    }

    #[test]
    fn parses_query_addresses() {
        assert!(matches!(
            parse_osc_request(&osc_packet("/query/tracks")).unwrap(),
            Action::RequestTrackList
        ));
        assert!(matches!(
            parse_osc_request(&osc_packet("/query/transport")).unwrap(),
            Action::RequestTransportState
        ));

        let packet = osc_packet_with_args(
            "/query/plugins",
            "s",
            &[OscArg::String("drums".to_string())],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackGetPluginGraph { track_name, include_state: false }
                if track_name == "drums"
        ));
    }

    #[test]
    fn parses_clip_commands() {
        let packet = osc_packet_with_args(
            "/clip/add",
            "ssiiiiiiiisss",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("take1".to_string()),
                OscArg::Int(0),
                OscArg::Int(44100),
                OscArg::Int(0),
                OscArg::Int(0),
                OscArg::Int(0),
                OscArg::Int(1),
                OscArg::Int(100),
                OscArg::Int(100),
                OscArg::String("audio".to_string()),
                OscArg::String("".to_string()),
                OscArg::String("".to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::AddClip { track_name, name, kind: Kind::Audio, .. }
                if track_name == "drums" && name == "take1"
        ));

        let packet = osc_packet_with_args(
            "/clip/remove",
            "sss",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("audio".to_string()),
                OscArg::String("0,2".to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::RemoveClip { track_name, kind: Kind::Audio, clip_indices }
                if track_name == "drums" && clip_indices == vec![0, 2]
        ));

        let packet = osc_packet_with_args(
            "/clip/move",
            "ssisiii",
            &[
                OscArg::String("audio".to_string()),
                OscArg::String("drums".to_string()),
                OscArg::Int(0),
                OscArg::String("bass".to_string()),
                OscArg::Int(88200),
                OscArg::Int(0),
                OscArg::Int(1),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::ClipMove { kind: Kind::Audio, from, to, copy }
                if from.track_name == "drums" && from.clip_index == 0
                    && to.track_name == "bass" && to.sample_offset == 88200 && copy
        ));
    }

    #[test]
    fn parses_midi_edit_commands() {
        let json = r#"[{"index":0,"start_sample":0,"length_samples":1000,"pitch":60,"velocity":100,"channel":0}]"#;
        let packet = osc_packet_with_args(
            "/midi/insert_notes",
            "sis",
            &[
                OscArg::String("piano".to_string()),
                OscArg::Int(0),
                OscArg::String(json.to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::InsertMidiNotes { track_name, clip_index, notes }
                if track_name == "piano" && clip_index == 0 && notes.len() == 1
        ));
    }

    #[test]
    fn parses_plugin_graph_and_parameter_commands() {
        let packet = osc_packet_with_args(
            "/plugin/connect_audio",
            "ssisisi",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("track_input".to_string()),
                OscArg::Int(0),
                OscArg::String("clap_0".to_string()),
                OscArg::Int(0),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackConnectPluginAudio { track_name, from_node, from_port, to_node, to_port }
                if track_name == "drums" && from_node == PluginGraphNode::TrackInput
                    && from_port == 0 && matches!(to_node, PluginGraphNode::ClapPluginInstance(0))
                    && to_port == 0
        ));

        let packet = osc_packet_with_args(
            "/plugin/set_param",
            "ssiif",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("clap".to_string()),
                OscArg::Int(0),
                OscArg::Int(7),
                OscArg::Float(0.5),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackSetClapParameter { track_name, instance_id, param_id, value }
                if track_name == "drums" && instance_id == 0 && param_id == 7
                    && (value - 0.5).abs() < f64::EPSILON
        ));
    }

    #[test]
    fn parses_track_connectable_commands() {
        let packet = osc_packet_with_args(
            "/track/connect_audio",
            "ssisi",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("child:sub".to_string()),
                OscArg::Int(0),
                OscArg::String("track_output".to_string()),
                OscArg::Int(0),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackConnectAudio { track_name, from, from_port, to, to_port }
                if track_name == "drums"
                    && from == ConnectableRef::ChildTrack("sub".to_string())
                    && from_port == 0
                    && to == ConnectableRef::TrackOutput
                    && to_port == 0
        ));

        let packet = osc_packet_with_args(
            "/track/disconnect_midi",
            "ssisi",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("clap_0".to_string()),
                OscArg::Int(0),
                OscArg::String("track_output".to_string()),
                OscArg::Int(0),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackDisconnectMidi { track_name, from, from_port, to, to_port }
                if track_name == "drums"
                    && from == ConnectableRef::ClapPlugin(0)
                    && from_port == 0
                    && to == ConnectableRef::TrackOutput
                    && to_port == 0
        ));
    }

    #[test]
    fn parses_transport_extra_commands() {
        let packet = osc_packet_with_args(
            "/transport/tempo_map",
            "s",
            &[OscArg::String(
                r#"{"tempo_points":[{"sample":0,"bpm":128.0}]}"#.to_string(),
            )],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::SetTempoMap { tempo_points, .. }
                if tempo_points.len() == 1 && (tempo_points[0].bpm - 128.0).abs() < f64::EPSILON
        ));

        assert!(matches!(
            parse_osc_request(&osc_packet("/transport/panic")).unwrap(),
            Action::Panic
        ));
    }

    #[test]
    fn parses_midi_learn_commands() {
        let packet = osc_packet_with_args(
            "/midi_learn/bind_track",
            "sss",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("volume".to_string()),
                OscArg::String(r#"{"device":"X","channel":1,"cc":7}"#.to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackSetMidiLearnBinding { track_name, target, binding }
                if track_name == "drums" && matches!(target, TrackMidiLearnTarget::Volume)
                    && binding.is_some()
        ));
    }

    #[test]
    fn parses_device_and_bounce_commands() {
        let json = r#"{"device":"hw:0","sample_rate_hz":48000,"bits":32,"exclusive":false,"period_frames":256,"nperiods":2,"sync_mode":false,"actual_period_frames":256,"input_channels":2,"output_channels":2,"bytes_per_frame":8}"#;
        let packet = osc_packet_with_args(
            "/device/audio_open",
            "s",
            &[OscArg::String(json.to_string())],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::OpenAudioDevice { device, .. } if device == "hw:0"
        ));

        let packet = osc_packet_with_args(
            "/bounce/start",
            "ssiisi",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("/tmp/bounce.wav".to_string()),
                OscArg::Int(0),
                OscArg::Int(44100),
                OscArg::String("".to_string()),
                OscArg::Int(1),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackOfflineBounce { track_name, output_path, .. }
                if track_name == "drums" && output_path == "/tmp/bounce.wav"
        ));
    }

    #[test]
    fn rejects_unknown_address() {
        assert!(parse_osc_request(&osc_packet("/unknown")).is_err());
    }

    #[test]
    fn error_packet_contains_reason() {
        let packet = build_error_packet("bad args");
        assert!(packet.starts_with(b"/error\0"));
        // Type tag starts after the padded address (8 bytes).
        assert_eq!(&packet[8..12], b",s\0\0");
        assert!(packet[12..].starts_with(b"bad args"));
    }

    #[test]
    fn parses_session_stop_scene() {
        let packet = osc_packet_with_args("/session/stop_scene", "i", &[OscArg::Int(2)]);
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::Session(crate::message::SessionAction::StopScene {
                scene_index,
                ..
            }) if scene_index == 2
        ));
    }

    #[test]
    fn parses_step_recording() {
        let packet = osc_packet_with_args("/step_recording", "i", &[OscArg::Int(1)]);
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::SetStepRecording(true)
        ));
    }

    #[test]
    fn parses_track_automation_and_midi_cc() {
        let packet = osc_packet_with_args(
            "/track/automation_level",
            "sf",
            &[OscArg::String("drums".to_string()), OscArg::Float(-6.0)],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackAutomationLevel(name, level)
                if name == "drums" && (level - -6.0).abs() < f32::EPSILON
        ));

        let packet = osc_packet_with_args(
            "/track/automation_balance",
            "sf",
            &[OscArg::String("drums".to_string()), OscArg::Float(0.25)],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackAutomationBalance(name, balance)
                if name == "drums" && (balance - 0.25).abs() < f32::EPSILON
        ));

        let packet = osc_packet_with_args(
            "/track/midi_cc",
            "siii",
            &[
                OscArg::String("drums".to_string()),
                OscArg::Int(1),
                OscArg::Int(7),
                OscArg::Int(64),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackMidiCc {
                track_name,
                channel: 1,
                cc: 7,
                value: 64,
            } if track_name == "drums"
        ));
    }

    #[test]
    fn parses_plugin_state_and_resource_commands() {
        let packet = osc_packet_with_args(
            "/plugin/show_gui",
            "ssi",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("clap".to_string()),
                OscArg::Int(0),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackShowClapGui { track_name, instance_id }
                if track_name == "drums" && instance_id == 0
        ));

        let packet = osc_packet_with_args(
            "/plugin/snapshot_state",
            "ssi",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("vst3".to_string()),
                OscArg::Int(0),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackVst3SnapshotState { track_name, instance_id }
                if track_name == "drums" && instance_id == 0
        ));

        let packet = osc_packet_with_args(
            "/plugin/restore_state",
            "ssis",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("clap".to_string()),
                OscArg::Int(0),
                OscArg::String(r#"{"bytes":[1,2,3]}"#.to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackClapRestoreState { track_name, instance_id, .. }
                if track_name == "drums" && instance_id == 0
        ));

        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let packet = osc_packet_with_args(
                "/plugin/snapshot_state",
                "ssi",
                &[
                    OscArg::String("drums".to_string()),
                    OscArg::String("lv2".to_string()),
                    OscArg::Int(0),
                ],
            );
            assert!(matches!(
                parse_osc_request(&packet).unwrap(),
                Action::TrackLv2SnapshotState { track_name, instance_id }
                    if track_name == "drums" && instance_id == 0
            ));

            let packet = osc_packet_with_args(
                "/plugin/restore_state",
                "ssis",
                &[
                    OscArg::String("drums".to_string()),
                    OscArg::String("lv2".to_string()),
                    OscArg::Int(0),
                    OscArg::String(r#"{"bytes":[1,2,3]}"#.to_string()),
                ],
            );
            assert!(matches!(
                parse_osc_request(&packet).unwrap(),
                Action::TrackSetLv2PluginState { track_name, instance_id, state }
                    if track_name == "drums" && instance_id == 0 && state == vec![1, 2, 3]
            ));
        }

        let packet = osc_packet_with_args(
            "/plugin/set_resource_dir",
            "ssis",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("clap".to_string()),
                OscArg::Int(0),
                OscArg::String("/tmp/res".to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackSetPluginResourceDir { track_name, instance_id, directory, .. }
                if track_name == "drums" && instance_id == 0 && directory == "/tmp/res"
        ));

        let packet = osc_packet_with_args(
            "/plugin/update_file_reference",
            "ssiis",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("clap".to_string()),
                OscArg::Int(0),
                OscArg::Int(2),
                OscArg::String("/tmp/sample.wav".to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackUpdateClapFileReference { track_name, instance_id, index, path, .. }
                if track_name == "drums" && instance_id == 0 && index == 2 && path == "/tmp/sample.wav"
        ));
    }

    #[test]
    fn parses_clip_plugin_state_and_resource_commands() {
        let packet = osc_packet_with_args(
            "/clip_plugin/snapshot_state",
            "ssii",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("clap".to_string()),
                OscArg::Int(1),
                OscArg::Int(0),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::ClipClapSnapshotState { track_name, clip_idx, instance_id }
                if track_name == "drums" && clip_idx == 1 && instance_id == 0
        ));

        let packet = osc_packet_with_args(
            "/clip_plugin/restore_state",
            "ssiis",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("clap".to_string()),
                OscArg::Int(1),
                OscArg::Int(0),
                OscArg::String(r#"{"bytes":[]}"#.to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::ClipClapRestoreState { track_name, clip_idx, instance_id, .. }
                if track_name == "drums" && clip_idx == 1 && instance_id == 0
        ));

        let packet = osc_packet_with_args(
            "/clip_plugin/restore_state",
            "ssiis",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("vst3".to_string()),
                OscArg::Int(1),
                OscArg::Int(0),
                OscArg::String(
                    r#"{"plugin_id":"plug","component_state":[1,2],"controller_state":[3]}"#
                        .to_string(),
                ),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::ClipVst3RestoreState { track_name, clip_idx, instance_id, .. }
                if track_name == "drums" && clip_idx == 1 && instance_id == 0
        ));

        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let packet = osc_packet_with_args(
                "/clip_plugin/snapshot_state",
                "ssii",
                &[
                    OscArg::String("drums".to_string()),
                    OscArg::String("lv2".to_string()),
                    OscArg::Int(1),
                    OscArg::Int(0),
                ],
            );
            assert!(matches!(
                parse_osc_request(&packet).unwrap(),
                Action::ClipLv2SnapshotState { track_name, clip_idx, instance_id }
                    if track_name == "drums" && clip_idx == 1 && instance_id == 0
            ));

            let packet = osc_packet_with_args(
                "/clip_plugin/restore_state",
                "ssiis",
                &[
                    OscArg::String("drums".to_string()),
                    OscArg::String("lv2".to_string()),
                    OscArg::Int(1),
                    OscArg::Int(0),
                    OscArg::String(r#"{"bytes":[4,5,6]}"#.to_string()),
                ],
            );
            assert!(matches!(
                parse_osc_request(&packet).unwrap(),
                Action::ClipSetLv2PluginState { track_name, clip_idx, instance_id, state }
                    if track_name == "drums"
                        && clip_idx == 1
                        && instance_id == 0
                        && state == vec![4, 5, 6]
            ));
        }

        let packet = osc_packet_with_args(
            "/clip_plugin/set_resource_dir",
            "ssiis",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("clap".to_string()),
                OscArg::Int(1),
                OscArg::Int(0),
                OscArg::String("/tmp/res".to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::ClipSetPluginResourceDir { track_name, clip_idx, instance_id, directory, .. }
                if track_name == "drums" && clip_idx == 1 && instance_id == 0 && directory == "/tmp/res"
        ));

        let packet = osc_packet_with_args(
            "/clip_plugin/update_file_reference",
            "ssiiis",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("clap".to_string()),
                OscArg::Int(1),
                OscArg::Int(0),
                OscArg::Int(0),
                OscArg::String("/tmp/sample.wav".to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::ClipUpdateClapFileReference { track_name, clip_idx, instance_id, index, path, .. }
                if track_name == "drums" && clip_idx == 1 && instance_id == 0 && index == 0 && path == "/tmp/sample.wav"
        ));
    }

    #[test]
    fn parses_extra_query_addresses() {
        assert!(matches!(
            parse_osc_request(&osc_packet("/query/clap_plugins_with_capabilities")).unwrap(),
            Action::ListClapPluginsWithCapabilities
        ));

        let packet = osc_packet_with_args(
            "/query/clap_note_names",
            "s",
            &[OscArg::String("drums".to_string())],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackGetClapNoteNames { track_name } if track_name == "drums"
        ));

        let packet = osc_packet_with_args(
            "/query/vst3_graph",
            "s",
            &[OscArg::String("drums".to_string())],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackGetVst3Graph { track_name } if track_name == "drums"
        ));

        assert!(matches!(
            parse_osc_request(&osc_packet("/query/diagnostics")).unwrap(),
            Action::RequestSessionDiagnostics
        ));

        assert!(matches!(
            parse_osc_request(&osc_packet("/query/midi_learn_report")).unwrap(),
            Action::RequestMidiLearnMappingsReport
        ));
    }

    #[test]
    fn parses_automation_set_lanes() {
        let packet = osc_packet_with_args(
            "/automation/set_lanes",
            "sss",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("latch".to_string()),
                OscArg::String(r#"[]"#.to_string()),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::SetTrackAutomationLanes { track_name, mode, .. }
                if track_name == "drums" && mode == TrackAutomationMode::Latch
        ));
    }

    #[test]
    fn parses_clap_parameter_edit_gestures() {
        let packet = osc_packet_with_args(
            "/plugin/set_param_at",
            "ssiifi",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("clap".to_string()),
                OscArg::Int(0),
                OscArg::Int(7),
                OscArg::Float(0.5),
                OscArg::Int(128),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackSetClapParameterAt { track_name, instance_id, param_id, value, frame }
                if track_name == "drums"
                    && instance_id == 0
                    && param_id == 7
                    && (value - 0.5).abs() < f64::EPSILON
                    && frame == 128
        ));

        let packet = osc_packet_with_args(
            "/plugin/begin_param_edit",
            "ssiii",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("clap".to_string()),
                OscArg::Int(0),
                OscArg::Int(7),
                OscArg::Int(64),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackBeginClapParameterEdit { track_name, instance_id, param_id, frame }
                if track_name == "drums" && instance_id == 0 && param_id == 7 && frame == 64
        ));

        let packet = osc_packet_with_args(
            "/plugin/end_param_edit",
            "ssiii",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("clap".to_string()),
                OscArg::Int(0),
                OscArg::Int(7),
                OscArg::Int(192),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackEndClapParameterEdit { track_name, instance_id, param_id, frame }
                if track_name == "drums" && instance_id == 0 && param_id == 7 && frame == 192
        ));

        let packet = osc_packet_with_args(
            "/plugin/snapshot_all_states",
            "s",
            &[OscArg::String("drums".to_string())],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackSnapshotAllClapStates { track_name } if track_name == "drums"
        ));
    }

    #[test]
    fn parses_clip_vst3_snapshot() {
        let packet = osc_packet_with_args(
            "/clip_plugin/snapshot_state",
            "ssii",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("vst3".to_string()),
                OscArg::Int(2),
                OscArg::Int(1),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::ClipVst3SnapshotState { track_name, clip_idx, instance_id }
                if track_name == "drums" && clip_idx == 2 && instance_id == 1
        ));
    }

    #[test]
    fn parses_vst3_graph_connections() {
        let packet = osc_packet_with_args(
            "/vst3/connect_audio",
            "ssisi",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("track_input".to_string()),
                OscArg::Int(0),
                OscArg::String("vst3_3".to_string()),
                OscArg::Int(1),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackConnectVst3Audio { track_name, from_node, from_port, to_node, to_port }
                if track_name == "drums"
                    && from_node == crate::message::Vst3GraphNode::TrackInput
                    && from_port == 0
                    && to_node == crate::message::Vst3GraphNode::PluginInstance(3)
                    && to_port == 1
        ));

        let packet = osc_packet_with_args(
            "/vst3/disconnect_audio",
            "ssisi",
            &[
                OscArg::String("drums".to_string()),
                OscArg::String("vst3_3".to_string()),
                OscArg::Int(1),
                OscArg::String("track_output".to_string()),
                OscArg::Int(0),
            ],
        );
        assert!(matches!(
            parse_osc_request(&packet).unwrap(),
            Action::TrackDisconnectVst3Audio { track_name, from_node, to_node, .. }
                if track_name == "drums"
                    && from_node == crate::message::Vst3GraphNode::PluginInstance(3)
                    && to_node == crate::message::Vst3GraphNode::TrackOutput
        ));
    }
}
