use super::*;
use crate::message::Action;

impl Engine {
    /// Hand a bounce job to its reserved worker, recording the worker→track
    /// mapping so the terminal `Ready(id)` cleans the job up on any path.
    pub(crate) async fn send_bounce_job(
        &mut self,
        worker_index: usize,
        job: crate::message::OfflineBounceWork,
    ) {
        let track_name = job.track_name.clone();
        self.dispatch
            .bounce_worker_tracks
            .insert(worker_index, track_name.clone());
        let worker = &self.workers[worker_index];
        if let Err(e) = worker.tx.send(Message::ProcessOfflineBounce(job)).await {
            self.dispatch.bounce_worker_tracks.remove(&worker_index);
            self.dispatch.offline_bounce_jobs.remove(&track_name);
            self.push_ready_worker(worker_index);
            self.notify_clients(Err(format!("Failed to schedule offline bounce: {e}")))
                .await;
        }
    }

    /// Requests queued while a bounce was running are replayed once the last
    /// bounce job is gone (and plan cycles resume).
    pub(crate) async fn drain_pending_requests_if_idle(&mut self) {
        if self.dispatch.offline_bounce_jobs.is_empty() {
            while let Some(next) = self.dispatch.pending_requests.pop_front() {
                self.handle_request(next).await;
            }
        }
    }

    pub(crate) async fn handle_track_offline_bounce(&mut self, action: Action) {
        let Action::TrackOfflineBounce {
            track_name,
            output_path,
            start_sample,
            length_samples,
            automation_lanes,
            apply_fader,
        } = action
        else {
            return;
        };
        if self.dispatch.offline_bounce_jobs.contains_key(&track_name) {
            self.notify_clients(Err(format!(
                "Offline bounce for track '{}' is already in progress",
                track_name
            )))
            .await;
            return;
        }
        if let Err(e) = self.track_handle_or_err(&track_name) {
            self.notify_clients(Err(e)).await;
            return;
        }
        if length_samples == 0 {
            self.notify_clients(Err(format!(
                "Track '{}' has no renderable content for offline bounce",
                track_name
            )))
            .await;
            return;
        }
        let Some(worker_index) = self.take_ready_worker_index() else {
            self.dispatch
                .pending_requests
                .push_front(Action::TrackOfflineBounce {
                    track_name,
                    output_path,
                    start_sample,
                    length_samples,
                    automation_lanes,
                    apply_fader,
                });
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.dispatch.offline_bounce_jobs.insert(
            track_name.clone(),
            OfflineBounceJob {
                cancel: cancel.clone(),
            },
        );
        let job = crate::message::OfflineBounceWork {
            state: self.state_snapshot.load_full(),
            track_name,
            output_path,
            start_sample,
            length_samples,
            tempo_bpm: self.transport.tempo_bpm,
            tsig_num: self.transport.tsig_num,
            tsig_denom: self.transport.tsig_denom,
            automation_lanes,
            cancel,
            apply_fader,
        };
        if self.executor.cycle_complete() {
            self.send_bounce_job(worker_index, job).await;
        } else {
            // A plan cycle is in flight; starting the bounce now would race
            // its workers on the live track bodies. The job is registered,
            // so new cycles are suspended from now on, and the work is
            // handed over when the cycle completes (on_all_tracks_finished).
            self.dispatch
                .pending_bounce_starts
                .push((worker_index, job));
        }
    }
}
impl Engine {
    /// Offline bounce request arms.
    pub(crate) async fn handle_bounce_request(&mut self, a: Action) -> bool {
        match a {
            Action::TrackOfflineBounce { .. } => {
                self.handle_track_offline_bounce(a.clone()).await;
                return true;
            }
            Action::TrackOfflineBounceCancel { .. } => {}
            Action::TrackOfflineBounceCancelAll => {}
            Action::TrackOfflineBounceCanceled { .. } => {}
            Action::TrackOfflineBounceProgress { .. } => {}
            _ => {}
        }
        false
    }
}
