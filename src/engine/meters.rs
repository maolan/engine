use super::*;
use crate::engine::fields::{MeterDecay, MeterFields};

impl Engine {
    pub(crate) async fn maybe_notify_hw_out_meter(&mut self, _meter_db: Vec<f32>) {
        {}
    }

    pub(crate) async fn apply_hw_out_gain_and_meter(&mut self) {
        let gain = if self.meters.hw_out_muted {
            0.0
        } else {
            10.0_f32.powf(self.meters.hw_out_level_db / 20.0)
        };

        // Send master gain/balance to the driver. If there is no active audio
        // backend there is nothing further to meter.
        if let Some(worker) = &self.hw_worker {
            let _ = worker
                .tx
                .send(Message::HWSetOutputGainBalance {
                    gain,
                    balance: self.meters.hw_out_balance,
                })
                .await;
        } else {
            #[cfg(unix)]
            {
                if let Some(jack) = self.jack_runtime.as_ref() {
                    jack.set_output_gain_linear(gain);
                    jack.set_output_balance(self.meters.hw_out_balance);
                } else {
                    return;
                }
            }
            #[cfg(not(unix))]
            {
                return;
            }
        }

        if self.meters.meter_decay_after_stop.is_some() {
            return;
        }

        let should_notify_interval = self.meters.should_publish_hw_out_meters();
        if !should_notify_interval {
            return;
        }

        let plan = self.executor.plan().clone();
        let peaks_linear = crate::hw::common::output_meter_linear_from_plan(
            &plan,
            gain,
            self.meters.hw_out_balance,
        );
        if self.meters.hw_out_peak_hold_linear.len() != peaks_linear.len() {
            self.meters
                .hw_out_peak_hold_linear
                .resize(peaks_linear.len(), 0.0);
        }
        let mut held_peaks = Vec::with_capacity(peaks_linear.len());
        for (idx, peak_now) in peaks_linear.iter().copied().enumerate() {
            let held = self.meters.hw_out_peak_hold_linear[idx] * 0.92;
            let next = peak_now.max(held);
            self.meters.hw_out_peak_hold_linear[idx] = next;
            held_peaks.push(next);
        }
        let should_notify = self.meters.should_publish_hw_out_linear(&held_peaks);
        let meter_db: Vec<f32> = held_peaks
            .into_iter()
            .map(MeterFields::meter_linear_to_db)
            .collect();
        self.meters.latest_hw_out_meter_db = Arc::new(meter_db.clone());
        if should_notify {
            self.maybe_notify_hw_out_meter(meter_db).await;
        }
    }

    pub(crate) async fn publish_track_meters(&mut self) {
        if !self.meters.should_publish_track_meters() {
            return;
        }
        if self.meters.meter_decay_after_stop.is_some() {
            self.update_meter_decay_after_stop();
            return;
        }
        let tracks: Vec<(String, crate::state::TrackHandle)> = self
            .state_snapshot
            .load_full()
            .tracks
            .iter()
            .map(|(name, track)| (name.clone(), track.clone()))
            .collect();
        let mut snapshot = Vec::with_capacity(tracks.len());
        for (name, track) in &tracks {
            let linear = self
                .meters
                .track_meter_linear_by_track
                .get(name)
                .cloned()
                .unwrap_or_else(|| track.lock().output_meter_linear());
            let output_db = linear
                .iter()
                .copied()
                .map(MeterFields::meter_linear_to_db)
                .collect::<Vec<_>>();
            snapshot.push((name.clone(), output_db));
        }
        for (track_name, output_db) in &snapshot {
            self.notify_event(crate::message::Event::TrackMeters {
                track_name: track_name.clone(),
                output_db: output_db.clone(),
            })
            .await;
        }
        self.meters.latest_track_meter_snapshot = Arc::new(snapshot);
    }

    pub(crate) fn reset_meters_after_stop(&mut self) {
        self.meters.last_hw_out_meter_publish = None;
        self.meters.last_track_meter_publish = None;
        self.meters.last_meter_snapshot_publish = None;
        #[cfg(unix)]
        {
            self.meters.last_hw_out_meter_linear.clear();
        }

        let tracks: Vec<(String, crate::state::TrackHandle)> = self
            .state_snapshot
            .load_full()
            .tracks
            .iter()
            .map(|(name, track)| (name.clone(), track.clone()))
            .collect();
        let mut track_linear = Vec::with_capacity(tracks.len());
        for (name, track) in tracks {
            let linear = self
                .meters
                .track_meter_linear_by_track
                .get(&name)
                .cloned()
                .unwrap_or_else(|| track.lock().output_meter_linear());
            track_linear.push((name, linear));
        }
        let hw_out_linear = if self.meters.hw_out_peak_hold_linear.is_empty() {
            self.meters
                .latest_hw_out_meter_db
                .iter()
                .copied()
                .map(MeterFields::meter_db_to_linear)
                .collect()
        } else {
            self.meters.hw_out_peak_hold_linear.clone()
        };
        self.meters.meter_decay_after_stop = Some(MeterDecay {
            started_at: Instant::now(),
            hw_out_linear,
            track_linear,
        });
        self.update_meter_decay_after_stop();
        self.meters.publish_meter_snapshot();
    }

    pub(crate) fn update_meter_decay_after_stop(&mut self) {
        let Some(decay) = self.meters.meter_decay_after_stop.as_ref() else {
            return;
        };
        let elapsed = decay.started_at.elapsed();
        if elapsed >= MeterFields::METER_DECAY_AFTER_STOP {
            self.finish_meter_decay_after_stop();
            return;
        }

        let remaining =
            1.0 - (elapsed.as_secs_f32() / MeterFields::METER_DECAY_AFTER_STOP.as_secs_f32());
        let hw_out_linear = decay
            .hw_out_linear
            .iter()
            .copied()
            .map(|value| value * remaining)
            .collect::<Vec<_>>();
        self.meters.latest_hw_out_meter_db = Arc::new(
            hw_out_linear
                .iter()
                .copied()
                .map(MeterFields::meter_linear_to_db)
                .collect(),
        );
        self.meters.hw_out_peak_hold_linear = hw_out_linear;

        let mut track_linear_by_track = HashMap::with_capacity(decay.track_linear.len());
        let mut snapshot = Vec::with_capacity(decay.track_linear.len());
        for (name, start_linear) in &decay.track_linear {
            let linear = start_linear
                .iter()
                .copied()
                .map(|value| value * remaining)
                .collect::<Vec<_>>();
            let output_db = linear
                .iter()
                .copied()
                .map(MeterFields::meter_linear_to_db)
                .collect::<Vec<_>>();
            track_linear_by_track.insert(name.clone(), linear);
            snapshot.push((name.clone(), output_db));
        }
        self.meters.track_meter_linear_by_track = track_linear_by_track;
        self.meters.latest_track_meter_snapshot = Arc::new(snapshot);
    }

    pub(crate) fn finish_meter_decay_after_stop(&mut self) {
        self.meters.meter_decay_after_stop = None;
        self.meters.hw_out_peak_hold_linear.fill(0.0);
        let hw_channels = self.meters.latest_hw_out_meter_db.len();
        self.meters.latest_hw_out_meter_db = Arc::new(vec![-90.0; hw_channels]);

        let tracks: Vec<(String, crate::state::TrackHandle)> = self
            .state_snapshot
            .load_full()
            .tracks
            .iter()
            .map(|(name, track)| (name.clone(), track.clone()))
            .collect();
        self.meters.track_meter_linear_by_track.clear();
        let mut snapshot = Vec::with_capacity(tracks.len());
        for (name, track) in tracks {
            let mut t = track.lock();
            t.clear_output_meters();
            let width = t.output_meter_linear().len();
            let zero_linear = vec![0.0; width];
            self.meters
                .track_meter_linear_by_track
                .insert(name.clone(), zero_linear);
            snapshot.push((name, vec![-90.0; width]));
        }
        self.meters.latest_track_meter_snapshot = Arc::new(snapshot);
        self.meters.publish_meter_snapshot();
    }

    pub(crate) fn publish_meter_snapshot_if_due(&mut self) {
        let now = Instant::now();
        if self
            .meters
            .last_meter_snapshot_publish
            .is_some_and(|last| now.duration_since(last) < MeterFields::METER_PUBLISH_INTERVAL)
        {
            return;
        }
        self.meters.last_meter_snapshot_publish = Some(now);
        self.update_meter_decay_after_stop();
        self.meters.publish_meter_snapshot();
    }
}
impl Engine {
    /// Meter request arms.
    pub(crate) async fn handle_meter_request(&mut self, a: Action) -> bool {
        if let Action::RequestMeterSnapshot = a {
            self.update_meter_decay_after_stop();
            self.notify_query_reply(QueryReply::MeterSnapshot {
                hw_out_db: self.meters.latest_hw_out_meter_db.clone(),
                track_meters: self.meters.latest_track_meter_snapshot.clone(),
            })
            .await;
            return true;
        }
        false
    }
}

impl MeterFields {
    pub(crate) fn meter_linear_to_db(peak: f32) -> f32 {
        if peak <= 1.0e-6 {
            -90.0
        } else {
            (20.0 * peak.log10()).clamp(-90.0, 20.0)
        }
    }

    pub(crate) fn meter_db_to_linear(db: f32) -> f32 {
        if db <= -90.0 {
            0.0
        } else {
            10.0_f32.powf(db / 20.0)
        }
    }

    pub(crate) fn should_publish_hw_out_meters(&mut self) -> bool {
        let now = Instant::now();
        match self.last_hw_out_meter_publish {
            Some(last) if now.duration_since(last) < MeterFields::METER_PUBLISH_INTERVAL => false,
            _ => {
                self.last_hw_out_meter_publish = Some(now);
                true
            }
        }
    }

    pub(crate) fn should_publish_track_meters(&mut self) -> bool {
        let now = Instant::now();
        match self.last_track_meter_publish {
            Some(last) if now.duration_since(last) < MeterFields::METER_PUBLISH_INTERVAL => false,
            _ => {
                self.last_track_meter_publish = Some(now);
                true
            }
        }
    }

    pub(crate) fn should_publish_hw_out_linear(&mut self, peaks_linear: &[f32]) -> bool {
        #[cfg(unix)]
        {
            self.hw_out_meter_publish_phase = !self.hw_out_meter_publish_phase;
            if !self.hw_out_meter_publish_phase {
                return false;
            }
            let changed = if self.last_hw_out_meter_linear.len() != peaks_linear.len() {
                true
            } else {
                self.last_hw_out_meter_linear
                    .iter()
                    .zip(peaks_linear.iter())
                    .any(|(prev, next)| {
                        (prev - next).abs() >= MeterFields::HW_OUT_METER_LINEAR_EPSILON
                    })
            };
            if !changed {
                return false;
            }
            self.last_hw_out_meter_linear.clear();
            self.last_hw_out_meter_linear
                .extend_from_slice(peaks_linear);
            true
        }
        #[cfg(not(unix))]
        {
            let _ = peaks_linear;
            false
        }
    }

    pub(crate) fn publish_meter_snapshot(&mut self) {
        let snapshot = self.meter_snapshot_producer.write_buffer();
        snapshot.hw_out_db.clear();
        snapshot
            .hw_out_db
            .extend(self.latest_hw_out_meter_db.iter().copied());
        snapshot.track_meters.clear();
        snapshot
            .track_meters
            .extend(self.latest_track_meter_snapshot.iter().cloned());
        self.meter_snapshot_producer.publish();
    }
}
