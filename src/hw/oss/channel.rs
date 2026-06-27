use super::Audio;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

pub struct OSSChannel<'a> {
    pub(super) capture: &'a mut Audio,
    pub(super) playback: &'a mut Audio,
    pub(super) stop_requested: &'a AtomicBool,
}

impl<'a> OSSChannel<'a> {
    pub fn run_cycle(&mut self) -> std::io::Result<()> {
        self.check_config()?;
        if self.stop_requested.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "OSS duplex cycle stopped",
            ));
        }

        self.playback.process(self.stop_requested)?;
        self.capture.process(self.stop_requested)?;

        if let Some(now) = self.capture.frame_clock.now() {
            self.capture.frame_stamp = now;
            self.playback.frame_stamp = now;
        }

        Ok(())
    }

    pub fn run_cycle_with_assist(&mut self, assist_lock: &Mutex<()>) -> std::io::Result<()> {
        let _guard = assist_lock.lock().expect("OSS assist mutex poisoned");
        self.run_cycle()
    }

    pub fn run_assist_step(&mut self) -> std::io::Result<bool> {
        self.run_cycle().map(|_| true)
    }

    pub fn run_assist_step_with_lock(&mut self, assist_lock: &Mutex<()>) -> std::io::Result<bool> {
        let _guard = assist_lock.lock().expect("OSS assist mutex poisoned");
        self.run_assist_step()
    }

    fn check_config(&self) -> std::io::Result<()> {
        if !self.capture.input || self.playback.input {
            return Err(std::io::Error::other(
                "OSSChannel expects (capture=input, playback=output)",
            ));
        }
        Ok(())
    }
}

unsafe impl Send for Audio {}
unsafe impl Sync for Audio {}
