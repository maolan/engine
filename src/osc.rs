use crate::message::{Action, Message};
use std::{
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::sync::mpsc::Sender;

const OSC_LISTEN_ADDR: &str = "0.0.0.0:9000";
const WAKEUP_PACKET: &[u8] = b"\0";

pub struct OscServer {
    stop: Arc<AtomicBool>,
    listen_addr: SocketAddr,
    handle: Option<thread::JoinHandle<()>>,
}

impl OscServer {
    pub fn start(tx: Sender<Message>) -> Result<Self, String> {
        Self::start_on_addr(tx, OSC_LISTEN_ADDR)
    }

    pub fn start_on_addr<A: ToSocketAddrs>(tx: Sender<Message>, addr: A) -> Result<Self, String> {
        let bind_addr = addr
            .to_socket_addrs()
            .map_err(|e| format!("Failed to resolve OSC socket address: {e}"))?
            .next()
            .ok_or_else(|| "Failed to resolve OSC socket address".to_string())?;
        let socket = UdpSocket::bind(bind_addr)
            .map_err(|e| format!("Failed to bind OSC socket on {bind_addr}: {e}"))?;
        socket
            .set_read_timeout(Some(Duration::from_millis(250)))
            .map_err(|e| format!("Failed to configure OSC socket timeout: {e}"))?;
        let listen_addr = socket
            .local_addr()
            .map_err(|e| format!("Failed to read OSC socket address: {e}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let handle = thread::spawn(move || {
            let mut buf = [0_u8; 2048];
            while !stop_thread.load(Ordering::Relaxed) {
                match socket.recv_from(&mut buf) {
                    Ok((len, _)) => {
                        let Some(action) = parse_osc_action(&buf[..len]) else {
                            continue;
                        };
                        if tx.blocking_send(Message::Request(action)).is_err() {
                            break;
                        }
                    }
                    Err(err)
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            || err.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => {
                        break;
                    }
                }
            }
        });

        Ok(Self {
            stop,
            listen_addr,
            handle: Some(handle),
        })
    }

    #[cfg(test)]
    pub fn listen_addr(&self) -> SocketAddr {
        self.listen_addr
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Send a dummy packet to wake up the receiver thread so it notices the
        // stop flag and exits promptly, instead of blocking in recv_from until
        // the read timeout expires.
        if let Ok(wake) = UdpSocket::bind("127.0.0.1:0") {
            let _ = wake.send_to(WAKEUP_PACKET, self.listen_addr);
        }
        if let Some(handle) = self.handle.take() {
            drop(handle.join());
        }
    }
}

impl Drop for OscServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn parse_osc_action(packet: &[u8]) -> Option<Action> {
    let (address, next) = parse_osc_string(packet, 0)?;
    let (type_tags, arg_offset) = parse_osc_string(packet, next)?;
    if !type_tags.starts_with(',') {
        return None;
    }

    let args = OscArgs {
        packet,
        type_tags: &type_tags.as_bytes()[1..],
        offset: arg_offset,
    };

    match address.as_str() {
        "/transport/play" if args.is_empty() => Some(Action::Play),
        "/transport/stop" if args.is_empty() => Some(Action::Stop),
        "/transport/pause" if args.is_empty() => Some(Action::Pause),
        "/transport/start" | "/transport/jump_to_start" | "/transport/start_of_session"
            if args.is_empty() =>
        {
            Some(Action::TransportPosition(0))
        }
        "/transport/end" | "/transport/jump_to_end" | "/transport/end_of_session"
            if args.is_empty() =>
        {
            Some(Action::JumpToEnd)
        }
        "/maolan/session/launch" => {
            let mut args = args;
            let track_name = args.next_string()?;
            let scene_index = args.next_int()? as usize;
            Some(Action::Session(crate::message::SessionAction::LaunchClip {
                track_name,
                scene_index,
                clip_id: String::new(),
                launch_quantization: crate::message::LaunchQuantization::Bar,
                loop_enabled: true,
                loop_start_samples: 0,
                loop_end_samples: 0,
            }))
        }
        "/maolan/session/stop" => {
            let mut args = args;
            let track_name = args.next_string()?;
            let scene_index = args.next_int()? as usize;
            Some(Action::Session(crate::message::SessionAction::StopClip {
                track_name,
                scene_index,
                launch_quantization: crate::message::LaunchQuantization::Bar,
            }))
        }
        "/maolan/session/scene" => {
            let mut args = args;
            let scene_index = args.next_int()? as usize;
            Some(Action::Session(
                crate::message::SessionAction::LaunchScene {
                    scene_index,
                    launch_quantization: crate::message::LaunchQuantization::Bar,
                },
            ))
        }
        "/maolan/session/stopall" if args.is_empty() => {
            Some(Action::Session(crate::message::SessionAction::StopAllClips))
        }
        _ => None,
    }
}

fn parse_osc_string(packet: &[u8], offset: usize) -> Option<(String, usize)> {
    if offset >= packet.len() {
        return None;
    }
    let end = packet[offset..].iter().position(|byte| *byte == 0)? + offset;
    let value = std::str::from_utf8(&packet[offset..end]).ok()?.to_string();
    let next = (end + 4) & !3;
    if next > packet.len() {
        return None;
    }
    Some((value, next))
}

struct OscArgs<'a> {
    packet: &'a [u8],
    type_tags: &'a [u8],
    offset: usize,
}

impl<'a> OscArgs<'a> {
    fn is_empty(&self) -> bool {
        self.type_tags.is_empty()
    }

    fn next_string(&mut self) -> Option<String> {
        let tag = self.type_tags.first()?;
        if *tag != b's' {
            return None;
        }
        self.type_tags = &self.type_tags[1..];
        let (value, next) = parse_osc_string(self.packet, self.offset)?;
        self.offset = next;
        Some(value)
    }

    fn next_int(&mut self) -> Option<i32> {
        let tag = self.type_tags.first()?;
        if *tag != b'i' {
            return None;
        }
        self.type_tags = &self.type_tags[1..];
        if self.offset.saturating_add(4) > self.packet.len() {
            return None;
        }
        let value = i32::from_be_bytes([
            self.packet[self.offset],
            self.packet[self.offset + 1],
            self.packet[self.offset + 2],
            self.packet[self.offset + 3],
        ]);
        self.offset += 4;
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::parse_osc_action;
    use crate::message::Action;

    fn osc_packet(address: &str) -> Vec<u8> {
        fn push_padded_string(buf: &mut Vec<u8>, value: &str) {
            buf.extend_from_slice(value.as_bytes());
            buf.push(0);
            while !buf.len().is_multiple_of(4) {
                buf.push(0);
            }
        }

        let mut buf = Vec::new();
        push_padded_string(&mut buf, address);
        push_padded_string(&mut buf, ",");
        buf
    }

    fn osc_packet_with_args(address: &str, type_tags: &str, args: &[OscArg]) -> Vec<u8> {
        fn push_padded_string(buf: &mut Vec<u8>, value: &str) {
            buf.extend_from_slice(value.as_bytes());
            buf.push(0);
            while !buf.len().is_multiple_of(4) {
                buf.push(0);
            }
        }

        let mut buf = Vec::new();
        push_padded_string(&mut buf, address);
        push_padded_string(&mut buf, &format!(",{}", type_tags));
        for arg in args {
            match arg {
                OscArg::String(s) => push_padded_string(&mut buf, s),
                OscArg::Int(i) => buf.extend_from_slice(&i.to_be_bytes()),
            }
        }
        buf
    }

    #[derive(Debug, Clone)]
    enum OscArg {
        String(String),
        Int(i32),
    }

    #[test]
    fn parses_basic_transport_messages() {
        assert!(matches!(
            parse_osc_action(&osc_packet("/transport/play")),
            Some(Action::Play)
        ));
        assert!(matches!(
            parse_osc_action(&osc_packet("/transport/pause")),
            Some(Action::Pause)
        ));
        assert!(matches!(
            parse_osc_action(&osc_packet("/transport/stop")),
            Some(Action::Stop)
        ));
        assert!(matches!(
            parse_osc_action(&osc_packet("/transport/start")),
            Some(Action::TransportPosition(0))
        ));
        assert!(matches!(
            parse_osc_action(&osc_packet("/transport/end")),
            Some(Action::JumpToEnd)
        ));
    }

    #[test]
    fn keeps_compatibility_transport_jump_aliases() {
        assert!(matches!(
            parse_osc_action(&osc_packet("/transport/jump_to_start")),
            Some(Action::TransportPosition(0))
        ));
        assert!(matches!(
            parse_osc_action(&osc_packet("/transport/jump_to_end")),
            Some(Action::JumpToEnd)
        ));
    }

    #[test]
    fn rejects_removed_short_aliases() {
        assert!(parse_osc_action(&osc_packet("/start")).is_none());
        assert!(parse_osc_action(&osc_packet("/stop")).is_none());
        assert!(parse_osc_action(&osc_packet("/pause")).is_none());
        assert!(parse_osc_action(&osc_packet("/jump_to_start")).is_none());
        assert!(parse_osc_action(&osc_packet("/jump_to_end")).is_none());
    }

    #[test]
    fn parses_session_launch_message() {
        let packet = osc_packet_with_args(
            "/maolan/session/launch",
            "si",
            &[OscArg::String("kick".to_string()), OscArg::Int(2)],
        );
        assert!(matches!(
            parse_osc_action(&packet),
            Some(Action::Session(crate::message::SessionAction::LaunchClip {
                track_name,
                scene_index,
                clip_id,
                ..
            })) if track_name == "kick" && scene_index == 2 && clip_id.is_empty()
        ));
    }

    #[test]
    fn parses_session_stop_message() {
        let packet = osc_packet_with_args(
            "/maolan/session/stop",
            "si",
            &[OscArg::String("snare".to_string()), OscArg::Int(1)],
        );
        assert!(matches!(
            parse_osc_action(&packet),
            Some(Action::Session(crate::message::SessionAction::StopClip {
                track_name,
                scene_index,
                ..
            })) if track_name == "snare" && scene_index == 1
        ));
    }

    #[test]
    fn parses_session_scene_message() {
        let packet = osc_packet_with_args("/maolan/session/scene", "i", &[OscArg::Int(3)]);
        assert!(matches!(
            parse_osc_action(&packet),
            Some(Action::Session(crate::message::SessionAction::LaunchScene {
                scene_index,
                ..
            })) if scene_index == 3
        ));
    }

    #[test]
    fn parses_session_stopall_message() {
        assert!(matches!(
            parse_osc_action(&osc_packet("/maolan/session/stopall")),
            Some(Action::Session(crate::message::SessionAction::StopAllClips))
        ));
    }
}
