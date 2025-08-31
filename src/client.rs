use super::Message;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;

#[derive(Debug)]
pub struct Client {
    tx: Sender<Message>,
    thread: JoinHandle<()>,
}

impl Client {
    pub fn new(tx: Sender<Message>, thread: JoinHandle<()>) -> Self {
        Self { tx, thread }
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

    pub fn play(&self) {
        self.send(Message::Play);
    }
}
