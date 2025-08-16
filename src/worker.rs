use super::Message;
use super::State;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub struct Worker {
    id: usize,
    rx: Receiver<Message>,
    tx: Sender<Message>,
    state: Arc<Mutex<State>>,
}

impl Worker {
    pub fn new(id: usize, rx: Receiver<Message>, tx: Sender<Message>, state: Arc<Mutex<State>>) -> Worker {
        let worker = Worker { id, rx, tx, state };
        worker.send(Message::Ready(id));
        worker
    }

    pub fn send(&self, message: Message) {
        let _ = self.tx.send(message);
    }

    pub fn work(&self) {
        for message in &self.rx {
            match message {
                Message::Quit => {
                    return;
                }
                Message::Process(id) => match self.state.lock() {
                    Ok(mut state) => {
                        let track = &mut state.audio.tracks[id];
                        track.process();
                        let _ = self.tx.send(Message::Finished(self.id));
                    }
                    Err(e) => {
                        println!("Track invalid: {}", e);
                    }
                },
                _ => {}
            }
        }
    }
}
