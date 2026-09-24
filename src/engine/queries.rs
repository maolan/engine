use super::*;
use crate::message::{Event, QueryReply};
use crate::osc::{OscArg, build_error_packet, build_osc_packet};

impl Engine {
    pub(crate) async fn notify_clients(&mut self, action: Result<Action, String>) {
        self.clients.retain(|client| !client.is_closed());
        for client in self.clients.iter() {
            if client
                .send(Message::Response(action.clone()))
                .await
                .is_err()
            {}
        }
        if let Some(reply_to) = self.osc_reply_target
            && let Err(reason) = &action
        {
            self.send_osc_reply(reply_to, &build_error_packet(reason));
        }
    }

    /// Broadcast an unsolicited state report / event to all clients. The
    /// OSC reply path intentionally handles only the query-answer subset
    /// (see `notify_query_reply`); events that used to produce OSC output
    /// while wrapped in `Ok(Action::...)` were query answers and now live
    /// there. `OfflineBounceFinished` errors keep the error-packet behavior.
    pub(crate) async fn notify_event(&mut self, event: Event) {
        self.clients.retain(|client| !client.is_closed());
        for client in self.clients.iter() {
            if client.send(Message::Event(event.clone())).await.is_err() {}
        }
        if let Some(reply_to) = self.osc_reply_target
            && let Event::OfflineBounceFinished(result) = &event
            && let Err(reason) = result.as_ref()
        {
            self.send_osc_reply(reply_to, &build_error_packet(reason));
        }
    }

    /// Deliver a query answer to all clients, with the OSC mapping for the
    /// state-report subset OSC clients consume.
    pub(crate) async fn notify_query_reply(&mut self, reply: QueryReply) {
        self.clients.retain(|client| !client.is_closed());
        for client in self.clients.iter() {
            if client
                .send(Message::QueryReply(reply.clone()))
                .await
                .is_err()
            {}
        }
        if let Some(reply_to) = self.osc_reply_target {
            match &reply {
                QueryReply::TrackList(names) => {
                    let args: Vec<OscArg> = names
                        .iter()
                        .map(|name| OscArg::String(name.clone()))
                        .collect();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet("/response/tracks", &"s".repeat(names.len()), &args),
                    );
                }
                QueryReply::TransportState {
                    sample,
                    tempo_bpm,
                    playing,
                    paused: _,
                    tsig_num,
                    tsig_denom,
                } => {
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet(
                            "/response/transport",
                            "idffii",
                            &[
                                OscArg::Int(*sample as i32),
                                OscArg::Int(if *playing { 1 } else { 0 }),
                                OscArg::Float(*tempo_bpm as f32),
                                OscArg::Float(0.0), // placeholder for future beat position
                                OscArg::Int(*tsig_num as i32),
                                OscArg::Int(*tsig_denom as i32),
                            ],
                        ),
                    );
                }
                QueryReply::MeterSnapshot {
                    hw_out_db,
                    track_meters,
                    ..
                } => {
                    let mut args: Vec<OscArg> = Vec::new();
                    args.push(OscArg::Int(hw_out_db.len() as i32));
                    for db in hw_out_db.iter() {
                        args.push(OscArg::Float(*db));
                    }
                    args.push(OscArg::Int(track_meters.len() as i32));
                    for (name, channels) in track_meters.iter() {
                        args.push(OscArg::String(name.clone()));
                        args.push(OscArg::Int(channels.len() as i32));
                        for db in channels.iter() {
                            args.push(OscArg::Float(*db));
                        }
                    }
                    let types = args
                        .iter()
                        .map(|a| match a {
                            OscArg::String(_) => 's',
                            OscArg::Int(_) => 'i',
                            OscArg::Float(_) => 'f',
                        })
                        .collect::<String>();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet("/response/meters", &types, &args),
                    );
                }
                QueryReply::TrackPluginGraph {
                    track_name,
                    plugins,
                    connections: _,
                    connectable_connections: _,
                } => {
                    let mut args: Vec<OscArg> = vec![OscArg::String(track_name.clone())];
                    args.push(OscArg::Int(plugins.len() as i32));
                    for plugin in plugins.iter() {
                        args.push(OscArg::Int(plugin.instance_id as i32));
                        args.push(OscArg::String(plugin.format.clone()));
                        args.push(OscArg::String(plugin.uri.clone()));
                        args.push(OscArg::String(plugin.name.clone()));
                        args.push(OscArg::Int(plugin.bypassed as i32));
                    }
                    let types = args
                        .iter()
                        .map(|a| match a {
                            OscArg::String(_) => 's',
                            OscArg::Int(_) => 'i',
                            OscArg::Float(_) => 'f',
                        })
                        .collect::<String>();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet("/response/plugins", &types, &args),
                    );
                }
                QueryReply::ClapPlugins(plugins) => {
                    let args: Vec<OscArg> = plugins
                        .iter()
                        .map(|p| OscArg::String(format!("{}|{}", p.path, p.name)))
                        .collect();
                    let types = "s".repeat(args.len());
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet("/response/clap_plugins", &types, &args),
                    );
                }
                QueryReply::Vst3Plugins(plugins) => {
                    let args: Vec<OscArg> = plugins
                        .iter()
                        .map(|p| OscArg::String(format!("{}|{}", p.id, p.name)))
                        .collect();
                    let types = "s".repeat(args.len());
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet("/response/vst3_plugins", &types, &args),
                    );
                }
                #[cfg(unix)]
                QueryReply::Lv2Plugins(plugins) => {
                    let args: Vec<OscArg> = plugins
                        .iter()
                        .map(|p| OscArg::String(format!("{}|{}", p.uri, p.name)))
                        .collect();
                    let types = "s".repeat(args.len());
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet("/response/lv2_plugins", &types, &args),
                    );
                }
                QueryReply::ClapPluginsUnavailable { error }
                | QueryReply::Vst3PluginsUnavailable { error } => {
                    self.send_osc_reply(reply_to, &build_error_packet(error));
                }
                #[cfg(unix)]
                QueryReply::Lv2PluginsUnavailable { error } => {
                    self.send_osc_reply(reply_to, &build_error_packet(error));
                }
                QueryReply::TrackClapParameters {
                    track_name,
                    instance_id,
                    parameters,
                } => {
                    let json = serde_json::json!(
                        parameters
                            .iter()
                            .map(|p| serde_json::json!({
                                "id": p.id,
                                "name": p.name,
                                "module": p.module,
                                "min_value": p.min_value,
                                "max_value": p.max_value,
                                "default_value": p.default_value,
                            }))
                            .collect::<Vec<_>>()
                    )
                    .to_string();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet(
                            "/response/plugin_parameters",
                            "siss",
                            &[
                                OscArg::String(track_name.clone()),
                                OscArg::Int(*instance_id as i32),
                                OscArg::String("clap".to_string()),
                                OscArg::String(json),
                            ],
                        ),
                    );
                }
                QueryReply::TrackVst3Parameters {
                    track_name,
                    instance_id,
                    parameters,
                } => {
                    let json = serde_json::to_string(parameters).unwrap_or_default();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet(
                            "/response/plugin_parameters",
                            "siss",
                            &[
                                OscArg::String(track_name.clone()),
                                OscArg::Int(*instance_id as i32),
                                OscArg::String("vst3".to_string()),
                                OscArg::String(json),
                            ],
                        ),
                    );
                }
                #[cfg(unix)]
                QueryReply::TrackLv2PluginControls {
                    track_name,
                    instance_id,
                    controls,
                    instance_access_handle: _,
                } => {
                    let json = serde_json::json!(
                        controls
                            .iter()
                            .map(|c| serde_json::json!({
                                "index": c.index,
                                "name": c.name,
                                "min": c.min,
                                "max": c.max,
                                "value": c.value,
                            }))
                            .collect::<Vec<_>>()
                    )
                    .to_string();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet(
                            "/response/plugin_parameters",
                            "siss",
                            &[
                                OscArg::String(track_name.clone()),
                                OscArg::Int(*instance_id as i32),
                                OscArg::String("lv2".to_string()),
                                OscArg::String(json),
                            ],
                        ),
                    );
                }
                QueryReply::TrackClapNoteNames {
                    track_name,
                    note_names,
                } => {
                    let json = serde_json::to_string(note_names).unwrap_or_default();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet(
                            "/response/clap_note_names",
                            "ss",
                            &[OscArg::String(track_name.clone()), OscArg::String(json)],
                        ),
                    );
                }
                #[cfg(unix)]
                QueryReply::TrackLv2Midnam {
                    track_name,
                    note_names,
                } => {
                    let json = serde_json::to_string(note_names).unwrap_or_default();
                    self.send_osc_reply(
                        reply_to,
                        &build_osc_packet(
                            "/response/lv2_midnam",
                            "ss",
                            &[OscArg::String(track_name.clone()), OscArg::String(json)],
                        ),
                    );
                }
                _ => {}
            }
        }
    }

    pub(crate) fn send_osc_reply(&mut self, reply_to: SocketAddr, packet: &[u8]) {
        if self.osc_reply_socket.is_none() {
            self.osc_reply_socket = UdpSocket::bind("0.0.0.0:0").ok();
        }
        if let Some(socket) = self.osc_reply_socket.as_ref() {
            let _ = socket.send_to(packet, reply_to);
        }
    }
}
impl Engine {
    /// Read-only query request arms answered with a client notification.
    pub(crate) async fn handle_query_request(&mut self, a: Action) -> bool {
        match a {
            Action::RequestTrackList => {
                let names: Vec<String> = self
                    .state_snapshot
                    .load_full()
                    .tracks
                    .keys()
                    .cloned()
                    .collect();
                self.notify_query_reply(QueryReply::TrackList(names)).await;
            }
            Action::RequestTransportState => {
                self.notify_query_reply(QueryReply::TransportState {
                    sample: self.transport.transport_sample,
                    tempo_bpm: self.transport.tempo_bpm,
                    playing: self.transport.playing,
                    paused: !self.transport.transport_running && self.transport.playing,
                    tsig_num: self.transport.tsig_num,
                    tsig_denom: self.transport.tsig_denom,
                })
                .await;
            }
            Action::RequestSessionDiagnostics => {
                self.handle_request_session_diagnostics().await;
            }
            Action::RequestMidiLearnMappingsReport => {
                self.handle_request_midi_learn_mappings_report().await;
            }
            _ => {}
        }
        false
    }
}
