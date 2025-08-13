mod audio;
pub mod client;
mod worker;

use self::audio::track::Track;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};

pub enum Message {
    Add,
    Quit,
    Ready(usize),
    Process(*mut Track),
}

#[derive(Debug)]
pub struct State {
    pub audio: audio::State,
}

impl State {
    pub fn new() -> Self {
        State {
            audio: audio::State::new(),
        }
    }
}

#[derive(Debug)]
struct WorkerData {
    tx: Sender<Message>,
    handle: JoinHandle<()>,
}

impl WorkerData {
    pub fn new(tx: Sender<Message>, handle: JoinHandle<()>) -> Self {
        Self { tx, handle }
    }
}

#[derive(Debug)]
pub struct Engine {
    state: State,
    rx: Receiver<Message>,
    tx: Sender<Message>,
    workers: Vec<WorkerData>,
}

impl Engine {
    pub fn new(rx: Receiver<Message>, tx: Sender<Message>) -> Self {
        Self {
            state: State::new(),
            rx,
            tx,
            workers: vec![],
        }
    }

    pub fn init(&mut self) {
        let max_threads = num_cpus::get();
        for id in 0..max_threads {
            let (tx, rx) = channel::<Message>();
            let tx_thread = self.tx.clone();
            let handler = thread::spawn(move || {
                let wrk = worker::Worker::new(id, rx, tx_thread);
                wrk.work();
            });
            self.workers.push(WorkerData::new(tx.clone(), handler));
        }
    }

    pub fn work(&mut self) {
        let mut ready_workers: Vec<usize> = vec![];
        for message in &self.rx {
            match message {
                Message::Quit => {
                    while self.workers.len() > 0 {
                        let worker = self.workers.remove(0);
                        let _ = worker.tx.send(Message::Quit);
                        let _ = worker.handle.join();
                    }
                    return;
                }
                Message::Ready(id) => {
                    ready_workers.push(id);
                }
                Message::Add => {
                    self.state.audio.tracks.push(Track::new());
                }
                _ => {}
            }
        }
    }

    pub fn state(&self) -> *const State {
        let raw = &self.state;
        return raw;
    }
}

pub fn init() -> client::Client {
    let (tx, rx) = channel::<Message>();
    let mut engine = Engine::new(rx, tx.clone());
    let state = engine.state();
    let handle = thread::spawn(move || {
        engine.init();
        engine.work();
    });
    let client = client::Client::new(tx.clone(), handle, state);
    client
}
