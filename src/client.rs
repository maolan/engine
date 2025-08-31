use super::{Message, TrackData};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

#[derive(Debug)]
pub struct Client {
    tx: Sender<Message>,
    tracks: Arc<Mutex<Vec<TrackData>>>,
    thread: JoinHandle<()>,
}

impl Client {
    pub fn new(
        tx: Sender<Message>,
        tracks: Arc<Mutex<Vec<TrackData>>>,
        thread: JoinHandle<()>,
    ) -> Self {
        Self { tx, tracks, thread }
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

    pub fn state(&self) -> (Arc<Mutex<Vec<TrackData>>>,) {
        return (self.tracks.clone(),);
    }
}
