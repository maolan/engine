use crate::{
    hw::traits::HwMidiHub,
    message::{HwMidiEvent, Message, TempoPoint},
    midi::{clock, mtc},
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc::Sender;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MasterConfig {
    pub rate: f64,
    pub tempo: f64,
    pub points: Vec<TempoPoint>,
    pub devices: Vec<String>,
    pub clock: bool,
    pub mtc: Option<mtc::FrameRate>,
    pub offset: f64,
}

#[derive(Debug)]
struct Master {
    config: MasterConfig,
    start: Instant,
    position: usize,
    cursor: usize,
    clock: clock::Generator,
    mtc: Option<mtc::Generator>,
}

impl Master {
    fn new(config: MasterConfig, start: Instant, position: usize) -> Self {
        let mtc = config.mtc.map(|rate| {
            let mut generator = mtc::Generator::new(config.rate, rate, position);
            generator.set_offset_seconds(config.offset);
            generator.reset(position);
            generator
        });
        Self {
            clock: clock::Generator::new(config.rate),
            config,
            start,
            position,
            cursor: position,
            mtc,
        }
    }

    fn fill(&mut self, queue: &mut BTreeMap<Instant, Vec<HwMidiEvent>>, now: Instant) {
        let elapsed = now.saturating_duration_since(self.start).as_secs_f64();
        let end = self
            .position
            .saturating_add(((elapsed + 0.004) * self.config.rate).ceil() as usize);
        let frames = end.saturating_sub(self.cursor);
        if frames == 0 {
            return;
        }
        let mut events = if self.config.clock {
            self.clock
                .generate(self.cursor, frames, &self.config.points, self.config.tempo)
        } else {
            Vec::new()
        };
        if let Some(mtc) = &mut self.mtc {
            events.extend(mtc.generate(self.cursor, frames));
        }
        for event in events {
            let offset = self.cursor - self.position + event.frame as usize;
            let deadline = self.start + Duration::from_secs_f64(offset as f64 / self.config.rate);
            let batch = queue.entry(deadline).or_default();
            for device in &self.config.devices {
                batch.push(HwMidiEvent {
                    device: device.clone(),
                    event: event.clone(),
                });
            }
        }
        self.cursor = end;
    }
}

enum Command {
    Master(Option<(MasterConfig, Instant, usize)>),
    Immediate(Vec<HwMidiEvent>),
    Shutdown(mpsc::Sender<()>),
    ClearTrack,
    Track(Instant, f64, Vec<HwMidiEvent>),
}

#[derive(Clone, Debug)]
pub(crate) struct MidiService(mpsc::Sender<Command>);

impl MidiService {
    pub fn clear_track(&self) {
        let _ = self.0.send(Command::ClearTrack);
    }
    pub fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        if self.0.send(Command::Shutdown(tx)).is_ok() {
            let _ = rx.recv();
        }
    }

    pub fn configure(&self, config: Option<MasterConfig>, position: usize) {
        let _ = self.0.send(Command::Master(
            config.map(|c| (c, Instant::now(), position)),
        ));
    }
    pub fn immediate(&self, events: Vec<HwMidiEvent>) {
        let _ = self.0.send(Command::Immediate(events));
    }
    pub fn track(&self, rate: f64, events: Vec<HwMidiEvent>) {
        let _ = self.0.send(Command::Track(Instant::now(), rate, events));
    }

    pub fn start<H: HwMidiHub + Send + 'static>(
        hub: Arc<Mutex<H>>,
        engine: Sender<Message>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let input_hub = hub.clone();
        let input_stop = stop.clone();
        std::thread::Builder::new()
            .name("maolan-midi-in".into())
            .spawn(move || {
                let mut input = Vec::with_capacity(64);
                while !input_stop.load(Ordering::Acquire) {
                    input_hub
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .read_events_into(&mut input);
                    let received = Instant::now();
                    if !input.is_empty()
                        && engine
                            .blocking_send(Message::TimedMidiInput(
                                received,
                                std::mem::take(&mut input),
                            ))
                            .is_err()
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
            .expect("spawn OSS MIDI input thread");
        std::thread::Builder::new()
            .name("maolan-midi-out".into())
            .spawn(move || {
                let mut master: Option<Master> = None;
                let mut sync = BTreeMap::new();
                let mut track: BTreeMap<Instant, Vec<HwMidiEvent>> = BTreeMap::new();
                loop {
                    let now = Instant::now();
                    if let Some(master) = &mut master {
                        master.fill(&mut sync, now);
                    }
                    let mut due = Vec::new();
                    while sync.first_key_value().is_some_and(|(t, _)| *t <= now) {
                        due.extend(sync.pop_first().unwrap().1);
                    }
                    if !due.is_empty() {
                        hub.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .write_events_blocking(&due, Duration::ZERO);
                        due.clear();
                    }
                    while track.first_key_value().is_some_and(|(t, _)| *t <= now) {
                        due.extend(track.pop_first().unwrap().1);
                    }
                    if !due.is_empty() {
                        hub.lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .write_events_blocking(&due, Duration::from_millis(10));
                    }
                    let interval = if master.is_some() {
                        Duration::from_millis(4)
                    } else {
                        Duration::from_millis(50)
                    };
                    let wait = sync
                        .keys()
                        .next()
                        .into_iter()
                        .chain(track.keys().next())
                        .min()
                        .map_or(interval, |t| {
                            interval.min(t.saturating_duration_since(Instant::now()))
                        });
                    match rx.recv_timeout(wait) {
                        Ok(Command::ClearTrack) => track.clear(),
                        Ok(Command::Shutdown(done)) => {
                            let _ = done.send(());
                            break;
                        }
                        Ok(Command::Master(config)) => {
                            sync.clear();
                            master = config.map(|(c, t, p)| Master::new(c, t, p));
                        }
                        Ok(Command::Immediate(events)) => hub
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .write_events_blocking(&events, Duration::from_millis(10)),
                        Ok(Command::Track(at, rate, events)) => {
                            for event in events {
                                let deadline = at
                                    + Duration::from_secs_f64(f64::from(event.event.frame) / rate);
                                track.entry(deadline).or_default().push(event);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => (),
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .expect("spawn OSS MIDI output thread");
        Self(tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_deadlines_follow_tempo_map_without_render_callbacks() {
        let start = Instant::now();
        let config = MasterConfig {
            rate: 48_000.0,
            tempo: 120.0,
            points: vec![TempoPoint {
                sample: 24_000,
                bpm: 300.0,
            }],
            devices: vec!["roland".into()],
            clock: true,
            mtc: Some(mtc::FrameRate::Fps25),
            offset: 0.0,
        };
        let mut master = Master::new(config, start, 0);
        let mut queue = BTreeMap::new();
        for ms in [0, 1, 17, 100, 499, 990, 1000] {
            master.fill(&mut queue, start + Duration::from_millis(ms));
        }
        let times = |status| {
            queue
                .iter()
                .filter(|(at, _)| **at < start + Duration::from_secs(1))
                .flat_map(|(at, batch)| {
                    batch
                        .iter()
                        .filter(move |e| e.event.data[0] == status)
                        .map(move |_| *at - start)
                })
                .collect::<Vec<_>>()
        };
        let clock = times(0xf8);
        assert_eq!(clock.len(), 84);
        assert_eq!(clock[24], Duration::from_millis(500));
        assert_eq!(clock[27], Duration::from_millis(525));
        assert_eq!(times(0xf1).len(), 100);
    }
}
