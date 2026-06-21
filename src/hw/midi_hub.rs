use crate::message::HwMidiEvent;
use crate::midi::io::MidiEvent;
use nix::libc;
use std::{
    fs::File,
    io::{ErrorKind, Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::OpenOptionsExt,
    os::unix::io::RawFd,
    time::{Duration, Instant},
};

#[derive(Debug, Default)]
pub struct MidiHub {
    inputs: Vec<MidiInputDevice>,
    outputs: Vec<MidiOutputDevice>,
    input_waiter: Option<MidiInputWaiter>,
}

impl MidiHub {
    pub fn open_input(&mut self, path: &str) -> Result<(), String> {
        if self.inputs.iter().any(|input| input.path == path) {
            return Ok(());
        }
        let file = File::options()
            .read(true)
            .write(false)
            .custom_flags(libc::O_RDONLY | libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| format!("Failed to open MIDI device '{path}': {e}"))?;
        self.inputs
            .push(MidiInputDevice::new(path.to_string(), file));
        if self.input_waiter.is_none() {
            self.input_waiter = MidiInputWaiter::new().ok();
        }
        if let Some(waiter) = self.input_waiter.as_mut()
            && let Some(input) = self.inputs.last()
            && waiter.add_fd(input.file.as_raw_fd()).is_err()
        {}
        Ok(())
    }

    pub fn open_output(&mut self, path: &str) -> Result<(), String> {
        if self.outputs.iter().any(|output| output.path == path) {
            return Ok(());
        }
        let file = File::options()
            .read(false)
            .write(true)
            .custom_flags(libc::O_WRONLY | libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| format!("Failed to open MIDI output '{path}': {e}"))?;
        self.outputs
            .push(MidiOutputDevice::new(path.to_string(), file));
        Ok(())
    }

    pub fn read_events(&mut self) -> Vec<HwMidiEvent> {
        let mut events = Vec::with_capacity(32);
        self.read_events_into(&mut events);
        events
    }

    pub fn read_events_into(&mut self, out: &mut Vec<HwMidiEvent>) {
        out.clear();
        for input in &mut self.inputs {
            input.read_events_into(out);
        }
    }

    pub fn wait_ready_blocking(&mut self) -> Option<Vec<i32>> {
        self.input_waiter
            .as_mut()
            .and_then(|waiter| waiter.wait_ready_blocking())
    }

    pub fn read_events_for_fds(&mut self, ready_fds: &[i32], out: &mut Vec<HwMidiEvent>) {
        out.clear();
        if ready_fds.is_empty() {
            return;
        }
        for input in &mut self.inputs {
            if ready_fds.contains(&input.file.as_raw_fd()) {
                input.read_events_into(out);
            }
        }
    }

    pub fn read_events_blocking_into(&mut self, out: &mut Vec<HwMidiEvent>) {
        let ready_fds = self.wait_ready_blocking();
        match ready_fds {
            Some(ready) => self.read_events_for_fds(&ready, out),
            None => self.read_events_into(out),
        }
    }

    pub fn wake_input_waiter(&mut self) {
        if let Some(waiter) = self.input_waiter.as_mut() {
            waiter.wake();
        }
    }

    pub fn close_input_waiter(&mut self) {
        if let Some(waiter) = self.input_waiter.as_mut() {
            waiter.close();
        }
    }

    pub fn write_events(&mut self, events: &[HwMidiEvent]) {
        if events.is_empty() {
            return;
        }
        for output in &mut self.outputs {
            output.write_events(events);
        }
    }

    pub fn write_events_blocking(&mut self, events: &[HwMidiEvent], timeout: Duration) {
        if events.is_empty() {
            return;
        }
        for output in &mut self.outputs {
            output.write_events_blocking(events, timeout);
        }
    }

    pub fn output_devices(&self) -> Vec<String> {
        self.outputs
            .iter()
            .map(|output| output.path.clone())
            .collect()
    }
}

#[derive(Debug)]
struct MidiInputDevice {
    path: String,
    file: File,
    parser: MidiParser,
}

#[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "macos"))]
#[derive(Debug)]
struct MidiInputWaiter {
    kq: i32,
    events: Vec<libc::kevent>,
    wake_read_fd: RawFd,
    wake_write_fd: RawFd,
}

#[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "macos"))]
impl MidiInputWaiter {
    fn new() -> Result<Self, String> {
        let kq = unsafe { libc::kqueue() };
        if kq < 0 {
            return Err(format!(
                "kqueue failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            kq,
            events: (0..32)
                .map(|_| unsafe { std::mem::zeroed::<libc::kevent>() })
                .collect(),
            wake_read_fd: -1,
            wake_write_fd: -1,
        })
    }

    fn add_fd(&mut self, fd: i32) -> Result<(), String> {
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = fd as _;
        ev.filter = libc::EVFILT_READ;
        ev.flags = libc::EV_ADD | libc::EV_ENABLE;
        let rc =
            unsafe { libc::kevent(self.kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if rc < 0 {
            return Err(format!(
                "kevent EV_ADD failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn ensure_wake_fd(&mut self) -> Result<(), String> {
        if self.wake_read_fd >= 0 && self.wake_write_fd >= 0 {
            return Ok(());
        }
        let mut fds = [0_i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(format!("pipe failed: {}", std::io::Error::last_os_error()));
        }
        self.wake_read_fd = fds[0];
        self.wake_write_fd = fds[1];
        self.add_fd(self.wake_read_fd)
    }

    fn drain_wake_fd(&self) {
        if self.wake_read_fd < 0 {
            return;
        }
        let mut buf = [0_u8; 32];
        loop {
            let n = unsafe { libc::read(self.wake_read_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            if (n as usize) < buf.len() {
                break;
            }
        }
    }

    fn wait_ready_blocking(&mut self) -> Option<Vec<i32>> {
        if self.ensure_wake_fd().is_err() {
            return None;
        }
        let n = unsafe {
            libc::kevent(
                self.kq,
                std::ptr::null(),
                0,
                self.events.as_mut_ptr(),
                self.events.len() as i32,
                std::ptr::null(),
            )
        };
        if n < 0 {
            return None;
        }
        let mut ready = Vec::with_capacity(n as usize);
        for ev in self.events.iter().take(n as usize) {
            let fd = ev.ident as i32;
            if fd == self.wake_read_fd {
                self.drain_wake_fd();
            } else {
                ready.push(fd);
            }
        }
        Some(ready)
    }

    fn wake(&mut self) {
        if self.ensure_wake_fd().is_err() {
            return;
        }
        let one = [1_u8; 1];
        let _ = unsafe { libc::write(self.wake_write_fd, one.as_ptr().cast(), one.len()) };
    }

    fn close(&mut self) {
        unsafe {
            if self.kq >= 0 {
                libc::close(self.kq);
                self.kq = -1;
            }
            if self.wake_read_fd >= 0 {
                libc::close(self.wake_read_fd);
                self.wake_read_fd = -1;
            }
            if self.wake_write_fd >= 0 {
                libc::close(self.wake_write_fd);
                self.wake_write_fd = -1;
            }
        }
    }
}

#[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "macos"))]
impl Drop for MidiInputWaiter {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "macos"))]
unsafe impl Send for MidiInputWaiter {}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct MidiInputWaiter {
    epfd: i32,
    events: Vec<libc::epoll_event>,
    wake_read_fd: RawFd,
    wake_write_fd: RawFd,
}

#[cfg(target_os = "linux")]
impl MidiInputWaiter {
    fn new() -> Result<Self, String> {
        let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epfd < 0 {
            return Err(format!(
                "epoll_create1 failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self {
            epfd,
            events: vec![libc::epoll_event { events: 0, u64: 0 }; 32],
            wake_read_fd: -1,
            wake_write_fd: -1,
        })
    }

    fn add_fd(&mut self, fd: i32) -> Result<(), String> {
        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: fd as u64,
        };
        let rc = unsafe { libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_ADD, fd, &mut ev) };
        if rc < 0 {
            return Err(format!(
                "epoll_ctl ADD failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    fn ensure_wake_fd(&mut self) -> Result<(), String> {
        if self.wake_read_fd >= 0 && self.wake_write_fd >= 0 {
            return Ok(());
        }
        let mut fds = [0_i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(format!("pipe failed: {}", std::io::Error::last_os_error()));
        }
        self.wake_read_fd = fds[0];
        self.wake_write_fd = fds[1];
        self.add_fd(self.wake_read_fd)
    }

    fn drain_wake_fd(&self) {
        if self.wake_read_fd < 0 {
            return;
        }
        let mut buf = [0_u8; 32];
        loop {
            let n = unsafe { libc::read(self.wake_read_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                break;
            }
            if (n as usize) < buf.len() {
                break;
            }
        }
    }

    fn wait_ready_blocking(&mut self) -> Option<Vec<i32>> {
        if self.ensure_wake_fd().is_err() {
            return None;
        }
        let n = unsafe {
            libc::epoll_wait(
                self.epfd,
                self.events.as_mut_ptr(),
                self.events.len() as i32,
                -1,
            )
        };
        if n < 0 {
            return None;
        }
        let mut ready = Vec::with_capacity(n as usize);
        for ev in self.events.iter().take(n as usize) {
            let fd = ev.u64 as i32;
            if fd == self.wake_read_fd {
                self.drain_wake_fd();
            } else {
                ready.push(fd);
            }
        }
        Some(ready)
    }

    fn wake(&mut self) {
        if self.ensure_wake_fd().is_err() {
            return;
        }
        let one = [1_u8; 1];
        let _ = unsafe { libc::write(self.wake_write_fd, one.as_ptr().cast(), one.len()) };
    }

    fn close(&mut self) {
        unsafe {
            if self.epfd >= 0 {
                libc::close(self.epfd);
                self.epfd = -1;
            }
            if self.wake_read_fd >= 0 {
                libc::close(self.wake_read_fd);
                self.wake_read_fd = -1;
            }
            if self.wake_write_fd >= 0 {
                libc::close(self.wake_write_fd);
                self.wake_write_fd = -1;
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for MidiInputWaiter {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(target_os = "linux")]
unsafe impl Send for MidiInputWaiter {}

#[derive(Debug)]
struct MidiOutputDevice {
    path: String,
    file: File,
}

impl MidiOutputDevice {
    fn new(path: String, file: File) -> Self {
        Self { path, file }
    }

    fn write_events(&mut self, events: &[HwMidiEvent]) {
        for event in events {
            if event.device != self.path {
                continue;
            }
            let midi_event = &event.event;
            if midi_event.data.is_empty() {
                continue;
            }
            if self.file.write_all(&midi_event.data).is_err() {
                break;
            }
        }
    }

    fn write_events_blocking(&mut self, events: &[HwMidiEvent], timeout: Duration) {
        for event in events {
            if event.device != self.path {
                continue;
            }
            let midi_event = &event.event;
            if midi_event.data.is_empty() {
                continue;
            }
            let deadline = Instant::now() + timeout;
            let mut data = &midi_event.data[..];
            while !data.is_empty() {
                match self.file.write(data) {
                    Ok(0) => {
                        break;
                    }
                    Ok(n) => {
                        data = &data[n..];
                    }
                    Err(err) if err.kind() == ErrorKind::WouldBlock => {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        let mut pfd = libc::pollfd {
                            fd: self.file.as_raw_fd(),
                            events: libc::POLLOUT,
                            revents: 0,
                        };
                        let ms = remaining.as_millis().min(i32::MAX as u128) as i32;
                        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
                        if rc < 0 {
                            break;
                        }
                        if rc == 0 || (pfd.revents & libc::POLLOUT) == 0 {
                            // Timeout or device not ready; stop trying this event
                            break;
                        }
                    }
                    Err(_) => {
                        break;
                    }
                }
            }
        }
    }
}

impl MidiInputDevice {
    fn new(path: String, file: File) -> Self {
        Self {
            path,
            file,
            parser: MidiParser::default(),
        }
    }

    fn read_events_into(&mut self, out: &mut Vec<HwMidiEvent>) {
        let mut buf = [0_u8; 256];
        loop {
            match self.file.read(&mut buf) {
                Ok(0) => break,
                Ok(read) => {
                    for byte in &buf[..read] {
                        if is_note_or_controller_status(*byte) {
                            if let Some(data) = self.parser.feed(*byte) {
                                out.push(HwMidiEvent {
                                    device: self.path.clone(),
                                    event: MidiEvent::new(0, data),
                                });
                            }
                            for _ in 0..2 {
                                let mut data = [0_u8; 1];
                                match self.file.read(&mut data) {
                                    Ok(1) => {
                                        if let Some(msg) = self.parser.feed(data[0]) {
                                            out.push(HwMidiEvent {
                                                device: self.path.clone(),
                                                event: MidiEvent::new(0, msg),
                                            });
                                        }
                                    }
                                    _ => break,
                                }
                            }
                            continue;
                        }
                        if let Some(data) = self.parser.feed(*byte) {
                            out.push(HwMidiEvent {
                                device: self.path.clone(),
                                event: MidiEvent::new(0, data),
                            });
                        }
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                Err(_) => {
                    break;
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct MidiParser {
    status: Option<u8>,
    needed: usize,
    data: [u8; 2],
    len: usize,
    in_sysex: bool,
    sysex: Vec<u8>,
}

impl MidiParser {
    fn feed(&mut self, byte: u8) -> Option<Vec<u8>> {
        if byte & 0x80 != 0 {
            if self.in_sysex {
                if byte == 0xF7 {
                    self.sysex.push(byte);
                    self.in_sysex = false;
                    return Some(std::mem::take(&mut self.sysex));
                }

                if byte >= 0xF8 {
                    return Some(vec![byte]);
                }

                self.in_sysex = false;
                self.sysex.clear();
            }
            if byte >= 0xF8 {
                return Some(vec![byte]);
            }
            if byte == 0xF0 {
                self.in_sysex = true;
                self.sysex.clear();
                self.sysex.push(byte);
                self.status = None;
                self.needed = 0;
                self.len = 0;
                return None;
            }
            self.status = Some(byte);
            self.len = 0;
            self.needed = status_data_len(byte);
            if self.needed == 0 {
                return Some(vec![byte]);
            }
            return None;
        }

        if self.in_sysex {
            self.sysex.push(byte);
            return None;
        }

        let status = self.status?;
        if self.len < self.data.len() {
            self.data[self.len] = byte;
        }
        self.len += 1;
        if self.len < self.needed {
            return None;
        }

        let mut message = Vec::with_capacity(1 + self.needed);
        message.push(status);
        message.extend_from_slice(&self.data[..self.needed]);
        self.len = 0;
        if status >= 0xF0 {
            self.status = None;
            self.needed = 0;
        }
        Some(message)
    }
}

fn is_note_or_controller_status(byte: u8) -> bool {
    matches!(byte & 0xF0, 0x80 | 0x90 | 0xB0)
}

fn status_data_len(status: u8) -> usize {
    match status {
        0x80..=0x8F | 0x90..=0x9F | 0xA0..=0xAF | 0xB0..=0xBF | 0xE0..=0xEF => 2,
        0xC0..=0xDF => 1,
        0xF1 | 0xF3 => 1,
        0xF2 => 2,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::MidiParser;

    #[test]
    fn parser_collects_sysex_message() {
        let mut parser = MidiParser::default();
        let bytes = [0xF0, 0x7D, 0x01, 0x02, 0xF7];
        let mut out = Vec::new();
        for b in bytes {
            if let Some(msg) = parser.feed(b) {
                out.push(msg);
            }
        }
        assert_eq!(out, vec![vec![0xF0, 0x7D, 0x01, 0x02, 0xF7]]);
    }

    #[test]
    fn parser_keeps_realtime_while_in_sysex() {
        let mut parser = MidiParser::default();
        let bytes = [0xF0, 0x7D, 0xF8, 0x01, 0xF7];
        let mut out = Vec::new();
        for b in bytes {
            if let Some(msg) = parser.feed(b) {
                out.push(msg);
            }
        }
        assert_eq!(out, vec![vec![0xF8], vec![0xF0, 0x7D, 0x01, 0xF7]]);
    }
}
