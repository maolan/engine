//! Hand-rolled CoreMIDI hub (replaces the `midir` crate used by the WASAPI
//! backend's MidiHub).
//!
//! Same shape as `wasapi::MidiHub`: prefixed device ids
//! (`coremidi:in:<index>:<name>` / `coremidi:out:<index>:<name>`), an
//! `input_events` queue fed by the `MIDIReadProc`, and `MIDISend` for
//! output. The classic `MIDIPacketList` API
//! (`MIDIInputPortCreate`/`MIDISend`) is formally deprecated since macOS 11
//! in favor of the block-based `MIDIReceiveBlock` API, but remains fully
//! functional and is what the deprecated-free hand-rolled approach binds.
//!
//! Ownership invariants (all `unsafe` below hinges on these):
//! - Each open input boxes one heap allocation and passes it as the
//!   `MIDIReadProc` refcon; CoreMIDI hands it back on its own thread. It
//!   stays valid until `close_all` disconnects and disposes the port and
//!   client (after which no read proc can run), then reclaims the box.
//! - The read proc only locks the shared event queue; it never calls back
//!   into hub code.

use crate::message::HwMidiEvent;
use crate::midi::io::MidiEvent;
use std::ffi::{CStr, CString, c_char, c_void};
use std::mem::{offset_of, size_of};
use std::ptr;
use std::sync::{Arc, Mutex};
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

#[repr(C)]
struct MidiPacket {
    time_stamp: MidiTimeStamp,
    length: u16,
    data: [u8; MIDI_PACKET_DATA_LEN],
}

#[repr(C)]
struct MidiPacketList {
    num_packets: u32,
    packet: [MidiPacket; 1],
}

#[repr(C, align(8))]
struct MidiSendBuffer {
    bytes: [u8; SEND_LIST_CAPACITY],
}

const fn midi_packet_stride() -> usize {
    (offset_of!(MidiPacket, data) + MIDI_PACKET_DATA_LEN + 3) & !3
}

const SEND_LIST_CAPACITY: usize = size_of::<u32>() + midi_packet_stride() * SEND_LIST_PACKETS + 8;

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
    fn MIDIInputPortCreate(
        client: MidiClientRef,
        port_name: *const c_void,
        read_proc: Option<MidiReadProcFn>,
        ref_con: *mut c_void,
        out_port: *mut MidiPortRef,
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
    fn CFRelease(cf: *const c_void);
}

type MidiReadProcFn = unsafe extern "C" fn(*const MidiPacketList, *mut c_void, *mut c_void);

fn os_error(context: &str, status: OsStatus) -> String {
    format!("{context} failed with OSStatus {status}")
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
    context: *mut InputCallbackContext,
}

// Safety: the raw context pointer is owned by this device; CoreMIDI invokes
// the read proc on its own serialized thread and `close` reclaims the box
// only after disconnect/dispose guarantee no further invocations. Moving the
// device between threads does not race with either side.
unsafe impl Send for MidiInputDevice {}

impl MidiInputDevice {
    fn close(&mut self) {
        // SAFETY: the port/client are valid open refs owned by this device;
        // disposing them guarantees no further read-proc invocations, so the
        // boxed context can be reclaimed afterwards.
        unsafe {
            let _ = MIDIPortDisconnectSource(self.port, self.source);
            let _ = MIDIPortDispose(self.port);
            let _ = MIDIClientDispose(self.client);
            if !self.context.is_null() {
                drop(Box::from_raw(self.context));
                self.context = ptr::null_mut();
            }
        }
    }
}

struct MidiOutputDevice {
    device: String,
    client: MidiClientRef,
    port: MidiPortRef,
    destination: MidiEndpointRef,
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
    outputs: Vec<MidiOutputDevice>,
    input_events: Arc<Mutex<Vec<HwMidiEvent>>>,
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

        let client = create_client("maolan-midi-in")?;

        let context = Box::into_raw(Box::new(InputCallbackContext {
            device: device.to_string(),
            queue: self.input_events.clone(),
        }));
        let port_name = create_port_name("maolan-midi-input")?;
        let mut port: MidiPortRef = 0;
        // SAFETY: `client` is valid; `port_name` is valid for the call and
        // released below; `context` outlives the port and is only reclaimed
        // in `close` after the port is disposed.
        let status = unsafe {
            MIDIInputPortCreate(
                client,
                port_name,
                Some(input_read_proc),
                context.cast::<c_void>(),
                &mut port,
            )
        };
        // SAFETY: `port_name` is no longer needed after port creation.
        unsafe {
            CFRelease(port_name);
        }
        if status != 0 || port == 0 {
            // SAFETY: the port was never created; the box is uniquely owned.
            unsafe {
                drop(Box::from_raw(context));
            }
            let _ = unsafe { MIDIClientDispose(client) };
            return Err(os_error("MIDIInputPortCreate", status));
        }

        // SAFETY: `port` and `source` are valid; the connection context is
        // unused (events carry the device string from the refcon box).
        let status = unsafe { MIDIPortConnectSource(port, source, ptr::null_mut()) };
        if status != 0 {
            unsafe {
                let _ = MIDIPortDispose(port);
                let _ = MIDIClientDispose(client);
                drop(Box::from_raw(context));
            }
            return Err(os_error("MIDIPortConnectSource", status));
        }

        self.inputs.push(MidiInputDevice {
            device: device.to_string(),
            client,
            port,
            source,
            context,
        });
        Ok(())
    }

    pub fn open_output(&mut self, device: &str) -> Result<(), String> {
        if self.outputs.iter().any(|d| d.device == device) {
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

        self.outputs.push(MidiOutputDevice {
            device: device.to_string(),
            client,
            port,
            destination,
        });
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
        for output in &mut self.outputs {
            for event in events {
                if event.device != output.device || event.event.data.is_empty() {
                    continue;
                }
                let data_len = event.event.data.len().min(MIDI_PACKET_DATA_LEN);
                let mut buffer = MidiSendBuffer {
                    bytes: [0_u8; SEND_LIST_CAPACITY],
                };
                let list = buffer.bytes.as_mut_ptr().cast::<MidiPacketList>();
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
                        buffer.bytes.len(),
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

    pub fn write_events_blocking(&mut self, events: &[HwMidiEvent], _timeout: std::time::Duration) {
        self.write_events(events);
    }

    pub fn close_all(&mut self) {
        while let Some(mut input) = self.inputs.pop() {
            input.close();
        }
        while let Some(mut output) = self.outputs.pop() {
            output.close();
        }
    }

    pub fn output_devices(&self) -> Vec<String> {
        self.outputs
            .iter()
            .map(|output| output.device.clone())
            .collect()
    }
}

impl Drop for MidiHub {
    fn drop(&mut self) {
        self.close_all();
    }
}

// Hand-written (like the WASAPI hub) rather than `impl_hw_midi_hub_traits!`:
// the macro forwards the optional fd-waiter hooks to inherent methods this
// hub does not implement, which would recurse.
impl crate::hw::traits::HwMidiHub for MidiHub {
    fn read_events_into(&mut self, out: &mut Vec<HwMidiEvent>) {
        self.read_events_into(out);
    }

    fn write_events(&mut self, events: &[HwMidiEvent]) {
        self.write_events(events);
    }
}

unsafe extern "C" fn input_read_proc(
    packet_list: *const MidiPacketList,
    read_proc_ref_con: *mut c_void,
    _src_conn_ref_con: *mut c_void,
) {
    if packet_list.is_null() || read_proc_ref_con.is_null() {
        return;
    }
    // SAFETY: the refcon box lives until the owning input is closed (after
    // the port is disposed, so no read proc can still be running).
    let context = unsafe { &*(read_proc_ref_con as *const InputCallbackContext) };
    // SAFETY: `packet_list` is a valid MIDIPacketList provided by CoreMIDI.
    let list = unsafe { &*packet_list };
    let count = list.num_packets as usize;
    let mut cursor = ptr::addr_of!(list.packet[0]).cast::<u8>();
    for _ in 0..count {
        let packet = cursor.cast::<MidiPacket>();
        // SAFETY: `cursor` walks the packet list using the documented
        // MIDIPacketNext 4-byte-aligned offset arithmetic.
        let (length,) = unsafe { ((*packet).length,) };
        let length = length as usize;
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
