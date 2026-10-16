use super::hw_worker::Backend;
use crate::hw::{config, wasapi, windows};

#[derive(Debug)]
pub struct WindowsBackend;

impl Backend for WindowsBackend {
    type Driver = windows::HwDriver;
    type MidiHub = wasapi::MidiHub;

    const LABEL: &'static str = "Windows audio";
    const WORKER_THREAD_NAME: &'static str = "windows-audio-worker";
    const ASSIST_THREAD_NAME: &'static str = "windows-audio-assist";
    const ASSIST_AUTONOMOUS_ENV: &'static str = config::WASAPI_ASSIST_AUTONOMOUS_ENV;
}

pub type HwWorker = super::hw_worker::HwWorker<WindowsBackend>;
