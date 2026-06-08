use super::hw_worker::Backend;
use crate::hw::config;
use crate::hw::oss;

#[derive(Debug)]
pub struct OssBackend;

impl Backend for OssBackend {
    type Driver = oss::HwDriver;
    type MidiHub = oss::MidiHub;

    const LABEL: &'static str = "OSS";
    const WORKER_THREAD_NAME: &'static str = "oss-worker";
    const ASSIST_THREAD_NAME: &'static str = "oss-assist";
    const ASSIST_AUTONOMOUS_ENV: &'static str = config::OSS_ASSIST_AUTONOMOUS_ENV;
    const ASSIST_AUTONOMOUS_DEFAULT: bool = true;
    const CYCLE_ON_WORKER_WHEN_ASSIST_AUTONOMOUS: bool = true;
}

pub type HwWorker = super::hw_worker::HwWorker<OssBackend>;
