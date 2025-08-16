mod audio;
pub mod client;
mod worker;

use self::audio::track::Track;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

pub enum Message {
    Add,
    Quit,
    Play,
    Ready(usize),
    Process(usize),
    Finished(usize),
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
    state: Arc<Mutex<State>>,
    rx: Receiver<Message>,
    tx: Sender<Message>,
    workers: Vec<WorkerData>,
    track_counter: usize,
}

impl Engine {
    pub fn new(rx: Receiver<Message>, tx: Sender<Message>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::new())),
            rx,
            tx,
            workers: vec![],
            track_counter: 0,
        }
    }

    pub fn init(&mut self) {
        let max_threads = num_cpus::get();
        for id in 0..max_threads {
            let (tx, rx) = channel::<Message>();
            let tx_thread = self.tx.clone();
            let state = self.state.clone();
            let handler = thread::spawn(move || {
                let wrk = worker::Worker::new(id, rx, tx_thread, state);
                wrk.work();
            });
            self.workers.push(WorkerData::new(tx.clone(), handler));
        }
    }

    pub fn work(&mut self) {
        let mut ready_workers: Vec<usize> = vec![];
        for message in &self.rx {
            match message {
                Message::Play => match self.state.lock() {
                    Ok(_state) => {
                        // Find track ID to be processed
                        match self.workers[0].tx.send(Message::Process(0)) {
                            Ok(_) => {}
                            Err(e) => {
                                println!("Error sending track id: {e}");
                            }
                        }
                    }
                    Err(e) => {
                        println!("Error sending track to be processed: {e}");
                    }
                },
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
                    let id = self.track_counter;
                    match self.state.lock() {
                        Ok(mut state) => {
                            state.audio.tracks.push(Track::new(id));
                        }
                        Err(e) => {
                            println!("Error while adding track: {e}");
                        }
                    }
                    self.track_counter += 1;
                }
                _ => {}
            }
        }
    }

    pub fn state(&self) -> Arc<Mutex<State>> {
        self.state.clone()
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
