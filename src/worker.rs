use super::Message;
use std::sync::mpsc::{Receiver, Sender};

#[derive(Debug)]
pub struct Worker {
    id: usize,
    rx: Receiver<Message>,
    tx: Sender<Message>,
}

impl Worker {
    pub fn new(id: usize, rx: Receiver<Message>, tx: Sender<Message>) -> Worker {
        let worker = Worker { id, rx, tx };
        worker.send(Message::Ready(id));
        worker
    }

    pub fn send(&self, message: Message) {
        let _ = self.tx.send(message);
    }

    pub fn work(self) {
        for message in &self.rx {
            match message {
                Message::Quit => {
                    return;
                }
                Message::Process(track) => {
                    unsafe {(*track).process()};
                }
                _ => {}
            }
        }
    }
}
