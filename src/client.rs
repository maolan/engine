use std::sync::Arc;
use tokio::task::JoinHandle;

use super::init;
use super::message::Message;
use super::meter::{MeterSnapshot, SessionRuntimeSnapshot, TransportSnapshot};
use super::triple_buffer::TripleBufferConsumer;
use tokio::sync::mpsc::{Receiver, Sender, channel};

#[derive(Debug, Clone)]
pub struct Client {
    pub sender: Sender<Message>,
    meter_consumer: Arc<TripleBufferConsumer<MeterSnapshot>>,
    transport_consumer: Arc<TripleBufferConsumer<TransportSnapshot>>,
    session_runtime_consumer: Arc<TripleBufferConsumer<SessionRuntimeSnapshot>>,
    _handle: Arc<JoinHandle<()>>,
}

impl Default for Client {
    fn default() -> Self {
        let (sender, handle, meter_consumer, transport_consumer, session_runtime_consumer) = init();
        Self {
            sender,
            meter_consumer: Arc::new(meter_consumer),
            transport_consumer: Arc::new(transport_consumer),
            session_runtime_consumer: Arc::new(session_runtime_consumer),
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
        self.meter_consumer.read_latest_clone()
    }

    pub fn transport_snapshot(&self) -> Option<TransportSnapshot> {
        self.transport_consumer.read_latest_clone()
    }

    pub fn session_runtime_snapshot(&self) -> Option<SessionRuntimeSnapshot> {
        self.session_runtime_consumer.read_latest_clone()
    }
}
