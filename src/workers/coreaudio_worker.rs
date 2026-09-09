use super::hw_worker::Backend;
use crate::hw::config;
use crate::hw::coreaudio;
use crate::hw::coremidi;

#[derive(Debug)]
pub struct CoreaudioBackend;

impl Backend for CoreaudioBackend {
    type Driver = coreaudio::HwDriver;
    type MidiHub = coremidi::MidiHub;

    const LABEL: &'static str = "CoreAudio";
    const WORKER_THREAD_NAME: &'static str = "coreaudio-worker";
    const ASSIST_THREAD_NAME: &'static str = "coreaudio-assist";
    const ASSIST_AUTONOMOUS_ENV: &'static str = config::COREAUDIO_ASSIST_AUTONOMOUS_ENV;
}

pub type HwWorker = super::hw_worker::HwWorker<CoreaudioBackend>;
