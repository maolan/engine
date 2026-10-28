use super::*;
use crate::message::{Action, PendingBounceWork, STEMS_BOUNCE_JOB_KEY};

impl Engine {
    /// Hand a bounce job to its reserved worker, recording the worker→track
    /// mapping so the terminal `Ready(id)` cleans the job up on any path.
    pub(crate) async fn send_bounce_job(&mut self, worker_index: usize, job: PendingBounceWork) {
        let job_key = job.job_key().to_string();
        self.dispatch
            .bounce_worker_tracks
            .insert(worker_index, job_key.clone());
        let message = job.into_message();
        let worker = &self.workers[worker_index];
        if let Err(e) = worker.tx.send(message).await {
            self.dispatch.bounce_worker_tracks.remove(&worker_index);
            self.dispatch.offline_bounce_jobs.remove(&job_key);
            self.push_ready_worker(worker_index);
            self.notify_clients(Err(format!("Failed to schedule offline bounce: {e}")))
                .await;
        }
    }

    /// Replay requests only while the shared track graph has no bounce owner.
    pub(crate) async fn drain_pending_requests_if_idle(&mut self) {
        // Bound retries: a bounce with no available worker requeues itself.
        for _ in 0..self.dispatch.pending_requests.len() {
            if !self.dispatch.offline_bounce_jobs.is_empty() {
                break;
            }
            let Some(next) = self.dispatch.pending_requests.pop_front() else {
                break;
            };
            self.handle_request(next).await;
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
        // Every bounce renders the whole graph through shared Track handles.
        // Different target tracks therefore cannot be bounced concurrently.
        let worker_index = if self.dispatch.offline_bounce_jobs.is_empty() {
            self.take_ready_worker_index()
        } else {
            None
        };
        let Some(worker_index) = worker_index else {
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
        if self.executor.cycle_complete()
            && !(self.rt_inline_enabled && self.transport.awaiting_hwfinished)
        {
            self.send_bounce_job(worker_index, PendingBounceWork::Track(job))
                .await;
        } else {
            // A plan cycle is in flight; starting the bounce now would race
            // its workers on the live track bodies. The job is registered,
            // so new cycles are suspended from now on, and the work is
            // handed over when the cycle completes (on_all_tracks_finished).
            self.dispatch
                .pending_bounce_starts
                .push((worker_index, PendingBounceWork::Track(job)));
        }
    }

    /// Multi-stem variant of [`Engine::handle_track_offline_bounce`]: renders
    /// every stem in a single pass, so the job owns the graph just like a
    /// single-track bounce and is registered under a reserved key.
    pub(crate) async fn handle_stems_offline_bounce(&mut self, action: Action) {
        let Action::StemsOfflineBounce { stems } = action else {
            return;
        };
        if self
            .dispatch
            .offline_bounce_jobs
            .contains_key(STEMS_BOUNCE_JOB_KEY)
        {
            self.notify_clients(Err(
                "An offline stems bounce is already in progress".to_string()
            ))
            .await;
            return;
        }
        if stems.is_empty() {
            self.notify_clients(Err("No stems requested for offline bounce".to_string()))
                .await;
            return;
        }
        for stem in &stems {
            if let Err(e) = self.track_handle_or_err(&stem.track_name) {
                self.notify_clients(Err(e)).await;
                return;
            }
            if stem.length_samples == 0 {
                self.notify_clients(Err(format!(
                    "Track '{}' has no renderable content for offline stems bounce",
                    stem.track_name
                )))
                .await;
                return;
            }
        }
        let worker_index = if self.dispatch.offline_bounce_jobs.is_empty() {
            self.take_ready_worker_index()
        } else {
            None
        };
        let Some(worker_index) = worker_index else {
            self.dispatch
                .pending_requests
                .push_front(Action::StemsOfflineBounce { stems });
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.dispatch.offline_bounce_jobs.insert(
            STEMS_BOUNCE_JOB_KEY.to_string(),
            OfflineBounceJob {
                cancel: cancel.clone(),
            },
        );
        let job = crate::message::OfflineStemsBounceWork {
            state: self.state_snapshot.load_full(),
            stems,
            tempo_bpm: self.transport.tempo_bpm,
            tsig_num: self.transport.tsig_num,
            tsig_denom: self.transport.tsig_denom,
            cancel,
        };
        if self.executor.cycle_complete()
            && !(self.rt_inline_enabled && self.transport.awaiting_hwfinished)
        {
            self.send_bounce_job(worker_index, PendingBounceWork::Stems(job))
                .await;
        } else {
            self.dispatch
                .pending_bounce_starts
                .push((worker_index, PendingBounceWork::Stems(job)));
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
            Action::StemsOfflineBounce { .. } => {
                self.handle_stems_offline_bounce(a.clone()).await;
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
