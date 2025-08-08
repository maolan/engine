use super::{Message, State};
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

#[derive(Debug)]
pub struct Client {
    tx: Sender<Message>,
    thread: JoinHandle<()>,
    state: *const State,
}

impl Client {
    pub fn new(tx: Sender<Message>, thread: JoinHandle<()>, state: *const State) -> Self {
        Self { tx, thread, state }
    }

    pub fn send(&self, message: Message) {
        let _ = self.tx.send(message);
    }

    pub fn quit(self) {
        self.send(Message::Quit);
        let _ = self.thread.join();
    }

    pub fn add(&self) {
        self.send(Message::Add);
    }
}
