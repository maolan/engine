use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

use super::init;
use super::message::Message;
use super::meter::{MeterSnapshot, SessionRuntimeSnapshot, TransportSnapshot};
use super::triple_buffer::TripleBufferConsumer;
use tokio::sync::mpsc::{Receiver, Sender, channel};

#[derive(Debug, Clone)]
pub struct Client {
    pub sender: Sender<Message>,
    meter_consumer: Arc<Mutex<TripleBufferConsumer<MeterSnapshot>>>,
    transport_consumer: Arc<Mutex<TripleBufferConsumer<TransportSnapshot>>>,
    session_runtime_consumer: Arc<Mutex<TripleBufferConsumer<SessionRuntimeSnapshot>>>,
    _handle: Arc<JoinHandle<()>>,
}

impl Default for Client {
    fn default() -> Self {
        let (sender, handle, meter_consumer, transport_consumer, session_runtime_consumer) = init();
        Self {
            sender,
            meter_consumer: Arc::new(Mutex::new(meter_consumer)),
            transport_consumer: Arc::new(Mutex::new(transport_consumer)),
            session_runtime_consumer: Arc::new(Mutex::new(session_runtime_consumer)),
            _handle: Arc::new(handle),
        }
    }
}

impl Client {
    pub async fn subscribe(&self) -> Receiver<Message> {
        let (tx, rx) = channel::<Message>(32);
        self.sender
            .send(Message::Channel(tx))
            .await
            .expect("Failed to subscribe to engine");
        rx
    }

    pub async fn send(&self, message: Message) -> Result<(), String> {
        self.sender
            .send(message)
            .await
            .map_err(|e| format!("Failed to send message from client: {:?}", e))
    }

    pub fn meter_snapshot(&self) -> Option<MeterSnapshot> {
        let mut consumer = self
            .meter_consumer
            .lock()
            .expect("meter snapshot consumer lock poisoned");
        consumer.refresh().then(|| consumer.read_buffer().clone())
    }

    pub fn transport_snapshot(&self) -> Option<TransportSnapshot> {
        let mut consumer = self
            .transport_consumer
            .lock()
            .expect("transport snapshot consumer lock poisoned");
        consumer.refresh().then(|| consumer.read_buffer().clone())
    }

    pub fn session_runtime_snapshot(&self) -> Option<SessionRuntimeSnapshot> {
        let mut consumer = self
            .session_runtime_consumer
            .lock()
            .expect("session runtime snapshot consumer lock poisoned");
        consumer.refresh().then(|| consumer.read_buffer().clone())
    }
}
