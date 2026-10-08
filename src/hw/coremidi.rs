//! Hand-rolled CoreMIDI hub (replaces the `midir` crate used by the WASAPI
//! backend's MidiHub).
//!
//! Same shape as `wasapi::MidiHub`: prefixed device ids
//! (`coremidi:in:<index>:<name>` / `coremidi:out:<index>:<name>`), an
//! `input_events` queue fed by the receive block, and `MIDISend` for
//! output.
//!
//! Input uses the block-based `MIDIReceiveBlock` API
//! (`MIDIInputPortCreateWithBlock`), not the classic `MIDIReadProc`: on
//! recent macOS the read-proc path is no longer invoked at all, and block
//! delivery requires a running CoreFoundation run loop. A single
//! process-wide daemon thread runs `CFRunLoopRun` (started lazily on the
//! first input open); delivery does not depend on which thread created the
//! client or port.
//!
//! Ownership invariants (all `unsafe` below hinges on these):
//! - Each open input holds one `Arc<InputCallbackContext>`; the receive
//!   block holds a clone, so the context stays alive until `close` disposes
//!   the port (after which no block can run) and the device struct drops.
//! - The block only locks the shared event queue; it never calls back into
//!   hub code.

use crate::message::HwMidiEvent;
use crate::midi::io::MidiEvent;
use block::{Block, ConcreteBlock, RcBlock};
use std::ffi::{CStr, CString, c_char, c_void};
use std::mem::offset_of;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::error;

const MIDI_IN_PREFIX: &str = "coremidi:in:";
const MIDI_OUT_PREFIX: &str = "coremidi:out:";
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// Maximum payload of a single `MIDIPacket`; larger sysex messages are
/// truncated to what one packet can carry.
const MIDI_PACKET_DATA_LEN: usize = 256;
/// Stack packet list sized for a handful of short messages per `MIDISend`.
const SEND_LIST_PACKETS: usize = 4;

type MidiObjectRef = u32;
type MidiClientRef = u32;
type MidiPortRef = u32;
type MidiEndpointRef = u32;
type OsStatus = i32;
type ItemCount = usize;
type ByteCount = usize;
type MidiTimeStamp = u64;

// CoreMIDI declares its packet structures under `#pragma pack(4)` (see
// CoreMIDI.h): `MIDIPacket.timeStamp` is an *unaligned* u64, `length` sits
// at offset 8, `data` at offset 10, and a `MIDIPacketList`'s first packet
// starts at offset 4, immediately after `numPackets`. Rust's default
// alignment would insert padding (timestamp at 0/8-aligned, data at 16) and
// corrupt both reception and sending, so the packed layout is mandatory.
// Never take references to fields of these structs; use `addr_of!` plus
// unaligned reads.
#[repr(C, packed(4))]
struct MidiPacket {
    time_stamp: MidiTimeStamp,
    length: u16,
    data: [u8; MIDI_PACKET_DATA_LEN],
}

#[repr(C, packed(4))]
struct MidiPacketList {
    num_packets: u32,
    packet: [MidiPacket; 1],
}

const fn midi_packet_stride() -> usize {
    (offset_of!(MidiPacket, data) + MIDI_PACKET_DATA_LEN + 3) & !3
}

const SEND_LIST_CAPACITY: usize =
    offset_of!(MidiPacketList, packet) + midi_packet_stride() * SEND_LIST_PACKETS + 8;

struct InputCallbackContext {
    device: String,
    queue: Arc<Mutex<Vec<HwMidiEvent>>>,
}

#[link(name = "CoreMIDI", kind = "framework")]
unsafe extern "C" {
    static kMIDIPropertyName: *const c_void;
    fn MIDIGetNumberOfSources() -> ItemCount;
    fn MIDIGetSource(index: ItemCount) -> MidiEndpointRef;
    fn MIDIGetNumberOfDestinations() -> ItemCount;
    fn MIDIGetDestination(index: ItemCount) -> MidiEndpointRef;
    fn MIDIObjectGetStringProperty(
        object: MidiObjectRef,
        property_id: *const c_void,
        value: *mut *const c_void,
    ) -> OsStatus;
    fn MIDIClientCreate(
        name: *const c_void,
        notify_proc: *const c_void,
        notify_ref_con: *mut c_void,
        out_client: *mut MidiClientRef,
    ) -> OsStatus;
    fn MIDIInputPortCreateWithBlock(
        client: MidiClientRef,
        port_name: *const c_void,
        out_port: *mut MidiPortRef,
        read_block: *const c_void,
    ) -> OsStatus;
    fn MIDIOutputPortCreate(
        client: MidiClientRef,
        port_name: *const c_void,
        out_port: *mut MidiPortRef,
    ) -> OsStatus;
    fn MIDIPortConnectSource(
        port: MidiPortRef,
        source: MidiEndpointRef,
        conn_ref_con: *mut c_void,
    ) -> OsStatus;
    fn MIDIPortDisconnectSource(port: MidiPortRef, source: MidiEndpointRef) -> OsStatus;
    fn MIDISend(
        port: MidiPortRef,
        destination: MidiEndpointRef,
        packet_list: *const MidiPacketList,
    ) -> OsStatus;
    fn MIDIPacketListInit(list: *mut MidiPacketList) -> *mut MidiPacket;
    fn MIDIPacketListAdd(
        list: *mut MidiPacketList,
        list_capacity: ByteCount,
        cur_packet: *mut MidiPacket,
        time_stamp: MidiTimeStamp,
        data_len: ByteCount,
        data: *const u8,
    ) -> *mut MidiPacket;
    fn MIDIPortDispose(port: MidiPortRef) -> OsStatus;
    fn MIDIClientDispose(client: MidiClientRef) -> OsStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        c_string: *const c_char,
        encoding: u32,
    ) -> *const c_void;
    fn CFStringGetLength(the_string: *const c_void) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(
        the_string: *const c_void,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> bool;
    fn CFRunLoopRun() -> ();
    fn CFRunLoopGetCurrent() -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

fn os_error(context: &str, status: OsStatus) -> String {
    format!("{context} failed with OSStatus {status}")
}

/// CoreMIDI on recent macOS no longer delivers classic `MIDIReadProc`
/// input at all; even the block-based `MIDIReceiveBlock` API only
/// dispatches while a CoreFoundation run loop is running. Delivery does not
/// depend on which thread created the client/port (verified on macOS 26),
/// so a single process-wide daemon thread running `CFRunLoopRun` serves all
/// hubs. Started lazily on the first input open; lives for the process.
mod runloop {
    use super::*;
    use std::sync::{Mutex, Once};

    static START: Once = Once::new();
    static RUNLOOP: Mutex<usize> = Mutex::new(0);

    pub(crate) fn ensure() {
        START.call_once(|| {
            std::thread::Builder::new()
                .name("coremidi-runloop".to_string())
                .spawn(|| {
                    // SAFETY: publishes this thread's run loop so tests and
                    // shutdown paths can stop it; then runs until stopped.
                    unsafe {
                        *RUNLOOP.lock().expect("runloop lock") = CFRunLoopGetCurrent() as usize;
                        CFRunLoopRun();
                    }
                })
                .expect("spawn coremidi-runloop");
            while *RUNLOOP.lock().expect("runloop lock") == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        });
    }
}

fn cf_string_from_rust(text: &str) -> Result<*const c_void, String> {
    let ctext = CString::new(text).map_err(|e| e.to_string())?;
    // SAFETY: `ctext` is a valid NUL-terminated UTF-8 buffer.
    let cf = unsafe {
        CFStringCreateWithCString(ptr::null(), ctext.as_ptr(), K_CF_STRING_ENCODING_UTF8)
    };
    if cf.is_null() {
        return Err(format!("CFStringCreateWithCString failed for '{text}'"));
    }
    Ok(cf)
}

unsafe fn cf_string_to_rust(cf: *const c_void) -> Option<String> {
    if cf.is_null() {
        return None;
    }
    // SAFETY: `cf` is a valid CFStringRef.
    let length = unsafe { CFStringGetLength(cf) };
    // SAFETY: `length` came from the same CFString.
    let capacity =
        unsafe { CFStringGetMaximumSizeForEncoding(length, K_CF_STRING_ENCODING_UTF8) } + 1;
    let mut buffer = vec![0_u8; capacity.max(1) as usize];
    // SAFETY: `buffer` is writable for its full length.
    let converted = unsafe {
        CFStringGetCString(
            cf,
            buffer.as_mut_ptr().cast::<c_char>(),
            buffer.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
        )
    };
    // SAFETY: `cf` is a CFString we own a reference to.
    unsafe {
        CFRelease(cf);
    }
    if !converted {
        return None;
    }
    // SAFETY: CFStringGetCString wrote a NUL-terminated string into the buffer.
    let cstr = unsafe { CStr::from_ptr(buffer.as_ptr().cast::<c_char>()) };
    Some(cstr.to_string_lossy().into_owned())
}

fn endpoint_name(endpoint: MidiEndpointRef) -> Option<String> {
    let mut raw: *const c_void = ptr::null();
    // SAFETY: `raw` outlives the call; `kMIDIPropertyName` is the
    // CoreMIDI-provided CFString property constant.
    let status = unsafe { MIDIObjectGetStringProperty(endpoint, kMIDIPropertyName, &mut raw) };
    if status != 0 {
        return None;
    }
    // SAFETY: `raw` is a CFStringRef owned by us on success.
    unsafe { cf_string_to_rust(raw) }.filter(|name| !name.is_empty())
}

fn create_client(name: &str) -> Result<MidiClientRef, String> {
    let name_cf = cf_string_from_rust(name)?;
    let mut client: MidiClientRef = 0;
    // SAFETY: `name_cf` is valid for the call and released below; `client`
    // is an out-parameter.
    let status = unsafe { MIDIClientCreate(name_cf, ptr::null(), ptr::null_mut(), &mut client) };
    // SAFETY: `name_cf` is no longer needed after client creation.
    unsafe {
        CFRelease(name_cf);
    }
    if status != 0 || client == 0 {
        return Err(os_error("MIDIClientCreate", status));
    }
    Ok(client)
}

fn create_port_name(name: &str) -> Result<*const c_void, String> {
    cf_string_from_rust(name)
}

pub fn list_midi_input_devices() -> Vec<String> {
    // SAFETY: pure count query.
    let count = unsafe { MIDIGetNumberOfSources() };
    let mut devices = Vec::new();
    for index in 0..count {
        // SAFETY: `index` is within the queried count.
        let endpoint = unsafe { MIDIGetSource(index) };
        if endpoint == 0 {
            continue;
        }
        if let Some(name) = endpoint_name(endpoint) {
            devices.push(format!("{MIDI_IN_PREFIX}{index}:{name}"));
        }
    }
    devices
}

pub fn list_midi_output_devices() -> Vec<String> {
    // SAFETY: pure count query.
    let count = unsafe { MIDIGetNumberOfDestinations() };
    let mut devices = Vec::new();
    for index in 0..count {
        // SAFETY: `index` is within the queried count.
        let endpoint = unsafe { MIDIGetDestination(index) };
        if endpoint == 0 {
            continue;
        }
        if let Some(name) = endpoint_name(endpoint) {
            devices.push(format!("{MIDI_OUT_PREFIX}{index}:{name}"));
        }
    }
    devices
}

struct MidiInputDevice {
    device: String,
    client: MidiClientRef,
    port: MidiPortRef,
    source: MidiEndpointRef,
    // Ownership-only clone of the receive block's context: kept alive until
    // `close` disposes the port (after which no block can run).
    _context: Arc<InputCallbackContext>,
    // Keeps the heap-copied receive block alive for the port's lifetime.
    _block: RcBlock<(*const MidiPacketList, *mut c_void), ()>,
}

// Safety: the `RcBlock` is an opaque heap block owned by this device; the
// code never invokes it directly (only CoreMIDI does, on the run-loop
// thread) and `close` drops it only after disconnect/dispose guarantee no
// further invocations. Moving the device between threads does not race
// with either side.
unsafe impl Send for MidiInputDevice {}

impl MidiInputDevice {
    fn close(&mut self) {
        // SAFETY: the port/client are valid open refs owned by this device;
        // disposing them guarantees no further block invocations, so the
        // shared context is reclaimed when this struct drops.
        unsafe {
            let _ = MIDIPortDisconnectSource(self.port, self.source);
            let _ = MIDIPortDispose(self.port);
            let _ = MIDIClientDispose(self.client);
        }
    }
}

struct MidiOutputDevice {
    device: String,
    client: MidiClientRef,
    port: MidiPortRef,
    destination: MidiEndpointRef,
}

// Safety: CoreMIDI port/client/destination refs are documented as usable
// from any thread. Every access (send, dispose) is serialized through the
// hub's output mutex, and the blocking-writer thread is joined before the
// refs are disposed in `Drop`.
unsafe impl Send for MidiOutputDevice {}

enum BlockingWriterMsg {
    Write(Vec<HwMidiEvent>, std::sync::mpsc::Sender<()>),
    Shutdown,
}

/// Helper thread for `write_events_blocking`: `MIDISend` has no deadline
/// parameter and may block internally (e.g. when a destination's queue is
/// full), so blocking writes run here where the caller can bound the wait.
struct BlockingWriter {
    tx: std::sync::mpsc::Sender<BlockingWriterMsg>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl BlockingWriter {
    fn spawn(outputs: Arc<Mutex<Vec<MidiOutputDevice>>>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<BlockingWriterMsg>();
        let handle = std::thread::Builder::new()
            .name("maolan-coremidi-blocking-writer".to_string())
            .spawn(move || {
                while let Ok(msg) = rx.recv() {
                    match msg {
                        BlockingWriterMsg::Write(events, ack) => {
                            if let Ok(mut outputs) = outputs.lock() {
                                send_output_events(&mut outputs, &events);
                            }
                            let _ = ack.send(());
                        }
                        BlockingWriterMsg::Shutdown => break,
                    }
                }
            })
            .expect("failed to spawn CoreMIDI blocking writer thread");
        Self {
            tx,
            handle: Some(handle),
        }
    }

    fn shutdown(&mut self) {
        let _ = self.tx.send(BlockingWriterMsg::Shutdown);
        if let Some(handle) = self.handle.take()
            && let Err(e) = handle.join()
        {
            error!("CoreMIDI blocking writer thread panicked: {e:?}");
        }
    }
}

impl MidiOutputDevice {
    fn close(&mut self) {
        // SAFETY: the port/client are valid open refs owned by this device.
        unsafe {
            let _ = MIDIPortDispose(self.port);
            let _ = MIDIClientDispose(self.client);
        }
    }
}

#[derive(Default)]
pub struct MidiHub {
    inputs: Vec<MidiInputDevice>,
    outputs: Arc<Mutex<Vec<MidiOutputDevice>>>,
    input_events: Arc<Mutex<Vec<HwMidiEvent>>>,
    blocking_writer: Option<BlockingWriter>,
}

impl MidiHub {
    pub fn open_input(&mut self, device: &str) -> Result<(), String> {
        if self.inputs.iter().any(|d| d.device == device) {
            return Ok(());
        }

        let index = parse_prefixed_index(device, MIDI_IN_PREFIX)?;
        // SAFETY: `index` is bounds-checked against a fresh count below.
        let source = unsafe { MIDIGetSource(index) };
        if source == 0 {
            return Err(format!("MIDI input device index out of range: {index}"));
        }

        runloop::ensure();

        let client = create_client("maolan-midi-in")?;

        let context = Arc::new(InputCallbackContext {
            device: device.to_string(),
            queue: self.input_events.clone(),
        });
        let block = {
            let context = context.clone();
            ConcreteBlock::new(
                move |packet_list: *const MidiPacketList, _src_conn: *mut c_void| {
                    // SAFETY: `packet_list` is a valid MIDIPacketList owned by
                    // CoreMIDI for the duration of the call; `context` is
                    // kept alive by the device's RcBlock until the port is
                    // disposed.
                    unsafe { collect_packets(&context, packet_list) };
                },
            )
            .copy()
        };
        let port_name = create_port_name("maolan-midi-input")?;
        let mut port: MidiPortRef = 0;
        // SAFETY: `client` is valid; `port_name` is valid for the call and
        // released below; the copied block is consumed by CoreMIDI and also
        // retained in the device struct.
        let status = unsafe {
            MIDIInputPortCreateWithBlock(
                client,
                port_name,
                &mut port,
                &*block as *const Block<(*const MidiPacketList, *mut c_void), ()> as *const c_void,
            )
        };
        // SAFETY: `port_name` is no longer needed after port creation.
        unsafe {
            CFRelease(port_name);
        }
        if status != 0 || port == 0 {
            let _ = unsafe { MIDIClientDispose(client) };
            return Err(os_error("MIDIInputPortCreateWithBlock", status));
        }

        // SAFETY: `port` and `source` are valid; the connection context is
        // unused (events carry the device string from the captured context).
        let status = unsafe { MIDIPortConnectSource(port, source, ptr::null_mut()) };
        if status != 0 {
            unsafe {
                let _ = MIDIPortDispose(port);
                let _ = MIDIClientDispose(client);
            }
            return Err(os_error("MIDIPortConnectSource", status));
        }

        self.inputs.push(MidiInputDevice {
            device: device.to_string(),
            client,
            port,
            source,
            _context: context,
            _block: block,
        });
        Ok(())
    }

    pub fn open_output(&mut self, device: &str) -> Result<(), String> {
        if self
            .outputs
            .lock()
            .is_ok_and(|outputs| outputs.iter().any(|d| d.device == device))
        {
            return Ok(());
        }

        let index = parse_prefixed_index(device, MIDI_OUT_PREFIX)?;
        // SAFETY: `index` is bounds-checked against a fresh count below.
        let destination = unsafe { MIDIGetDestination(index) };
        if destination == 0 {
            return Err(format!("MIDI output device index out of range: {index}"));
        }

        let client = create_client("maolan-midi-out")?;

        let port_name = create_port_name("maolan-midi-output")?;
        let mut port: MidiPortRef = 0;
        // SAFETY: `client` is valid; `port_name` is valid for the call and
        // released below; `port` is an out-parameter.
        let status = unsafe { MIDIOutputPortCreate(client, port_name, &mut port) };
        // SAFETY: `port_name` is no longer needed after port creation.
        unsafe {
            CFRelease(port_name);
        }
        if status != 0 || port == 0 {
            let _ = unsafe { MIDIClientDispose(client) };
            return Err(os_error("MIDIOutputPortCreate", status));
        }

        if let Ok(mut outputs) = self.outputs.lock() {
            outputs.push(MidiOutputDevice {
                device: device.to_string(),
                client,
                port,
                destination,
            });
        }
        Ok(())
    }

    pub fn read_events_into(&mut self, out: &mut Vec<HwMidiEvent>) {
        out.clear();
        let Ok(mut queue) = self.input_events.lock() else {
            return;
        };
        out.extend(queue.drain(..));
    }

    pub fn write_events(&mut self, events: &[HwMidiEvent]) {
        if events.is_empty() {
            return;
        }
        if let Ok(mut outputs) = self.outputs.lock() {
            send_output_events(&mut outputs, events);
        }
    }

    /// Write events, waiting up to `timeout` for the sends to complete.
    /// `MIDISend` has no deadline parameter and may block internally, so the
    /// sends run on a dedicated helper thread; on timeout this returns with
    /// the events still delivered once the thread unblocks.
    pub fn write_events_blocking(&mut self, events: &[HwMidiEvent], timeout: Duration) {
        if events.is_empty() {
            return;
        }
        if self.blocking_writer.is_none() {
            self.blocking_writer = Some(BlockingWriter::spawn(self.outputs.clone()));
        }
        let writer = self.blocking_writer.as_mut().expect("initialized above");
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        if writer
            .tx
            .send(BlockingWriterMsg::Write(events.to_vec(), ack_tx))
            .is_err()
        {
            // Writer thread is gone; deliver directly as a fallback.
            if let Ok(mut outputs) = self.outputs.lock() {
                send_output_events(&mut outputs, events);
            }
            return;
        }
        let _ = ack_rx.recv_timeout(timeout);
    }

    pub fn close_all(&mut self) {
        while let Some(mut input) = self.inputs.pop() {
            input.close();
        }
        if let Ok(mut outputs) = self.outputs.lock() {
            while let Some(mut output) = outputs.pop() {
                output.close();
            }
        }
    }

    pub fn output_devices(&self) -> Vec<String> {
        self.outputs
            .lock()
            .map(|outputs| outputs.iter().map(|output| output.device.clone()).collect())
            .unwrap_or_default()
    }
}

/// Send each event to every matching open output, one `MIDISend` per event.
fn send_output_events(outputs: &mut [MidiOutputDevice], events: &[HwMidiEvent]) {
    for output in outputs.iter_mut() {
        for event in events {
            if event.device != output.device || event.event.data.is_empty() {
                continue;
            }
            let data_len = event.event.data.len().min(MIDI_PACKET_DATA_LEN);
            // 8-byte alignment satisfies the packed(4) packet layout.
            let mut buffer = [0_u8; SEND_LIST_CAPACITY];
            let list = buffer.as_mut_ptr().cast::<MidiPacketList>();
            // SAFETY: `list` points at the head of the aligned send
            // buffer which is sized for SEND_LIST_PACKETS packets.
            let mut packet = unsafe { MIDIPacketListInit(list) };
            if packet.is_null() {
                continue;
            }
            // SAFETY: capacity matches the buffer; data is readable for
            // data_len bytes.
            packet = unsafe {
                MIDIPacketListAdd(
                    list,
                    buffer.len(),
                    packet,
                    0,
                    data_len,
                    event.event.data.as_ptr(),
                )
            };
            if packet.is_null() {
                error!("MIDI write dropped for {}: packet list full", output.device);
                continue;
            }
            // SAFETY: `list` is a well-formed packet list; port and
            // destination are valid open refs.
            let status = unsafe { MIDISend(output.port, output.destination, list) };
            if status != 0 {
                error!(
                    "MIDI write error on {}: {}",
                    output.device,
                    os_error("MIDISend", status)
                );
                break;
            }
        }
    }
}

impl Drop for MidiHub {
    fn drop(&mut self) {
        // Stop the blocking writer first: joining it guarantees no send is
        // in flight before the output ports are disposed below.
        if let Some(mut writer) = self.blocking_writer.take() {
            writer.shutdown();
        }
        self.close_all();
    }
}

// Hand-written (like the WASAPI hub) rather than `impl_hw_midi_hub_traits!`:
// the macro forwards the optional fd-waiter hooks to inherent methods this
// hub does not implement, which would recurse.
impl crate::hw::traits::HwMidiHub for MidiHub {
    fn open_input(&mut self, device: &str) -> Result<(), String> {
        self.open_input(device)
    }

    fn open_output(&mut self, device: &str) -> Result<(), String> {
        self.open_output(device)
    }

    fn close_all(&mut self) {
        self.close_all();
    }

    fn read_events_into(&mut self, out: &mut Vec<HwMidiEvent>) {
        self.read_events_into(out);
    }

    fn write_events(&mut self, events: &[HwMidiEvent]) {
        self.write_events(events);
    }
}

/// Appends every packet in a `MIDIPacketList` to the shared event queue.
/// Shared by the receive block; `context` outlives the port that invokes it.
///
/// SAFETY: `packet_list` must be a valid `MIDIPacketList` provided by
/// CoreMIDI for the duration of the call.
unsafe fn collect_packets(context: &InputCallbackContext, packet_list: *const MidiPacketList) {
    if packet_list.is_null() {
        return;
    }
    // SAFETY: `packet_list` is a valid MIDIPacketList provided by CoreMIDI.
    let list = unsafe { &*packet_list };
    // SAFETY: unaligned read of the packed `num_packets` field.
    let count = unsafe { ptr::addr_of!(list.num_packets).read_unaligned() } as usize;
    let mut cursor = ptr::addr_of!(list.packet[0]).cast::<u8>();
    for _ in 0..count {
        let packet = cursor.cast::<MidiPacket>();
        // SAFETY: unaligned read of the packed `length` field.
        let length = unsafe { ptr::addr_of!((*packet).length).read_unaligned() } as usize;
        if length > 0 {
            let data_ptr = unsafe { ptr::addr_of!((*packet).data).cast::<u8>() };
            // SAFETY: the packet payload is `length` bytes within the list.
            let data =
                unsafe { std::slice::from_raw_parts(data_ptr, length.min(MIDI_PACKET_DATA_LEN)) };
            if let Ok(mut events) = context.queue.lock() {
                events.push(HwMidiEvent {
                    device: context.device.clone(),
                    event: MidiEvent::new(0, data.to_vec()),
                });
            }
        }
        // MIDIPacketNext: advance past the header plus payload, rounded up
        // to the next 4-byte boundary.
        let advance = offset_of!(MidiPacket, data) + length;
        let pad = (4 - (advance & 3)) & 3;
        cursor = unsafe { cursor.add(advance + pad) };
    }
}

fn parse_prefixed_index(device: &str, prefix: &str) -> Result<usize, String> {
    let rest = device
        .strip_prefix(prefix)
        .ok_or_else(|| format!("Unsupported MIDI device id '{device}'"))?;
    let index_str = rest.split(':').next().unwrap_or("");
    index_str
        .parse::<usize>()
        .map_err(|_| format!("Invalid MIDI device id '{device}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trait_open_input_uses_inherent_impl_not_default() {
        let mut hub = MidiHub::default();
        let err = <MidiHub as crate::hw::traits::HwMidiHub>::open_input(
            &mut hub,
            "coremidi:in:999999:no-such-device",
        )
        .unwrap_err();
        // The trait default would report "not supported by this backend";
        // reaching the inherent impl means the trait forwards correctly.
        assert!(
            !err.contains("not supported by this backend"),
            "trait open_input hit the default impl: {err}"
        );
    }

    #[test]
    fn trait_open_output_uses_inherent_impl_not_default() {
        let mut hub = MidiHub::default();
        let err = <MidiHub as crate::hw::traits::HwMidiHub>::open_output(
            &mut hub,
            "coremidi:out:999999:no-such-device",
        )
        .unwrap_err();
        assert!(
            !err.contains("not supported by this backend"),
            "trait open_output hit the default impl: {err}"
        );
    }

    #[test]
    fn packet_layout_matches_coremidi_packing() {
        // CoreMIDI.h packs its packet structures to 4 bytes: an unaligned
        // u64 timestamp, u16 length at offset 8, data at offset 10, and the
        // first packet of a list immediately after numPackets (offset 4).
        // Rust's natural 8-byte alignment would insert padding and corrupt
        // every packet walked or sent.
        assert_eq!(offset_of!(MidiPacketList, packet), 4);
        assert_eq!(offset_of!(MidiPacket, time_stamp), 0);
        assert_eq!(offset_of!(MidiPacket, length), 8);
        assert_eq!(offset_of!(MidiPacket, data), 10);
        assert_eq!(midi_packet_stride(), 268);
    }
}
