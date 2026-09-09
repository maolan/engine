//! Hand-rolled CoreAudio backend (HALOutput AudioUnit).
//!
//! Callback-driven like WASAPI: the engine's cycle is paced by the HAL
//! render callback consuming interleaved f32 periods from a bounded
//! mutex+condvar reservoir (backpressure mirrors `wasapi::HwDriver`).
//! All CoreAudio/CoreFoundation interaction is raw FFI — no cpal,
//! coreaudio-rs, or other device-access crates.
//!
//! Ownership invariants (all `unsafe` below hinges on these):
//! - Each render/input callback receives one heap allocation via its
//!   refcon; it stays valid until `AudioOutputUnitStop` returns (stop is
//!   synchronous with respect to the IO thread) and the box is reclaimed
//!   in `close_fds`.
//! - The callback only locks the shared reservoir mutex; it never calls
//!   into driver code that could re-enter the HAL IO thread.

use crate::audio::io::AudioIO;
use crate::hw::{common, latency, options::HwOptions, traits};
use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_void};
use std::mem::size_of;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tracing::{debug, warn};

const COREAUDIO_PREFIX: &str = "coreaudio:";
const K_AUDIO_OBJECT_SYSTEM_OBJECT: u32 = 1;
const K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL: u32 = 0x676C_6F62; // 'glob'
const K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN: u32 = 0;
const K_AUDIO_HARDWARE_PROPERTY_DEVICES: u32 = 0x6465_7623; // 'dev#'
const K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE: u32 = 0x644F_7574; // 'dOut'
const K_AUDIO_HARDWARE_PROPERTY_DEFAULT_INPUT_DEVICE: u32 = 0x6449_6E20; // 'dIn '
const K_AUDIO_OBJECT_PROPERTY_NAME: u32 = 0x6C6E_616D; // 'lnam'
const K_AUDIO_DEVICE_PROPERTY_DEVICE_UID: u32 = 0x7569_6420; // 'uid '
const K_AUDIO_DEVICE_PROPERTY_STREAMS: u32 = 0x7374_6D23; // 'stm#'
const K_AUDIO_STREAM_PROPERTY_DIRECTION: u32 = 0x7364_6972; // 'sdir'
const K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT: u32 = 0x7066_7420; // 'pft '
const K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE: u32 = 0x6E73_7274; // 'nsrt'
const K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE: u32 = 0x6673_697A; // 'fsiz'
const K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE_RANGE: u32 = 0x6673_7A23; // 'fsz#'
const K_AUDIO_DEVICE_PROPERTY_LATENCY: u32 = 0x6C74_6E63; // 'ltnc'
const K_AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET: u32 = 0x7361_6674; // 'saft'
const K_AUDIO_UNIT_TYPE_OUTPUT: u32 = 0x6175_6F75; // 'auou'
const K_AUDIO_UNIT_SUBTYPE_HAL_OUTPUT: u32 = 0x6168_616C; // 'ahal'
const K_AUDIO_UNIT_MANUFACTURER_APPLE: u32 = 0x6170_706C; // 'appl'
const K_AUDIO_UNIT_SCOPE_GLOBAL: u32 = 0;
const K_AUDIO_UNIT_SCOPE_INPUT: u32 = 1;
const K_AUDIO_UNIT_SCOPE_OUTPUT: u32 = 2;
const K_AUDIO_UNIT_PROPERTY_SAMPLE_RATE: u32 = 2;
const K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = 8;
const K_AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE: u32 = 14;
const K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: u32 = 23;
const K_AUDIO_OUTPUT_UNIT_PROPERTY_ENABLE_IO: u32 = 2003;
const K_AUDIO_OUTPUT_UNIT_PROPERTY_CURRENT_DEVICE: u32 = 2000;
const K_AUDIO_OUTPUT_UNIT_PROPERTY_SET_INPUT_CALLBACK: u32 = 2005;
const K_AUDIO_FORMAT_LINEAR_PCM: u32 = 0x6C70_636D; // 'lpcm'
const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 0x1;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 0x8;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

type AudioObjectId = u32;
type AudioDeviceId = u32;
type AudioUnitRef = *mut c_void;
type OsStatus = i32;

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioObjectPropertyAddress {
    m_selector: u32,
    m_scope: u32,
    m_element: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioValueRange {
    m_minimum: f64,
    m_maximum: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioStreamBasicDescription {
    m_sample_rate: f64,
    m_format_id: u32,
    m_format_flags: u32,
    m_bytes_per_packet: u32,
    m_frames_per_packet: u32,
    m_bytes_per_frame: u32,
    m_channels_per_frame: u32,
    m_bits_per_channel: u32,
    m_reserved: u32,
}

impl AudioStreamBasicDescription {
    fn float_interleaved(sample_rate: f64, channels: u32) -> Self {
        let bytes_per_frame = channels.saturating_mul(4);
        Self {
            m_sample_rate: sample_rate,
            m_format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            m_format_flags: K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED,
            m_bytes_per_packet: bytes_per_frame,
            m_frames_per_packet: 1,
            m_bytes_per_frame: bytes_per_frame,
            m_channels_per_frame: channels,
            m_bits_per_channel: 32,
            m_reserved: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AudioBuffer {
    m_number_channels: u32,
    m_data_byte_size: u32,
    m_data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    m_number_buffers: u32,
    m_buffers: [AudioBuffer; 1],
}

#[repr(C)]
struct AuRenderCallbackStruct {
    input_proc: Option<RenderCallbackFn>,
    input_proc_ref_con: *mut c_void,
}

type RenderCallbackFn = unsafe extern "C" fn(
    *mut c_void,
    *mut u32,
    *const c_void,
    u32,
    u32,
    *mut AudioBufferList,
) -> i32;

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(
        in_object_id: AudioObjectId,
        in_address: *const AudioObjectPropertyAddress,
        in_qualifier_data_size: u32,
        in_qualifier_data: *const c_void,
        out_data_size: *mut u32,
    ) -> OsStatus;
    fn AudioObjectGetPropertyData(
        in_object_id: AudioObjectId,
        in_address: *const AudioObjectPropertyAddress,
        in_qualifier_data_size: u32,
        in_qualifier_data: *const c_void,
        io_data_size: *mut u32,
        out_data: *mut c_void,
    ) -> OsStatus;
    fn AudioObjectSetPropertyData(
        in_object_id: AudioObjectId,
        in_address: *const AudioObjectPropertyAddress,
        in_qualifier_data_size: u32,
        in_qualifier_data: *const c_void,
        in_data_size: u32,
        in_data: *const c_void,
    ) -> OsStatus;
}

#[link(name = "AudioUnit", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(
        in_component: *mut c_void,
        in_desc: *const AudioComponentDescription,
    ) -> *mut c_void;
    fn AudioComponentInstanceNew(
        in_component: *mut c_void,
        out_instance: *mut AudioUnitRef,
    ) -> OsStatus;
    fn AudioComponentInstanceDispose(instance: AudioUnitRef) -> OsStatus;
    fn AudioUnitInitialize(unit: AudioUnitRef) -> OsStatus;
    fn AudioUnitUninitialize(unit: AudioUnitRef) -> OsStatus;
    fn AudioUnitSetProperty(
        unit: AudioUnitRef,
        in_id: u32,
        in_scope: u32,
        in_element: u32,
        in_data: *const c_void,
        in_data_size: u32,
    ) -> OsStatus;
    fn AudioUnitGetProperty(
        unit: AudioUnitRef,
        in_id: u32,
        in_scope: u32,
        in_element: u32,
        out_data: *mut c_void,
        io_data_size: *mut u32,
    ) -> OsStatus;
    fn AudioOutputUnitStart(unit: AudioUnitRef) -> OsStatus;
    fn AudioOutputUnitStop(unit: AudioUnitRef) -> OsStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
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

impl Default for HwOptions {
    fn default() -> Self {
        Self {
            exclusive: false,
            period_frames: 1024,
            nperiods: 2,
            ignore_hwbuf: false,
            sync_mode: false,
            input_latency_frames: 0,
            output_latency_frames: 0,
        }
    }
}

fn os_error(context: &str, status: OsStatus) -> String {
    format!("{context} failed with OSStatus {status}")
}

fn property_address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        m_selector: selector,
        m_scope: scope,
        m_element: K_AUDIO_OBJECT_PROPERTY_ELEMENT_MAIN,
    }
}

fn get_property_data<T>(
    object: AudioObjectId,
    selector: u32,
    scope: u32,
    context: &str,
) -> Result<T, String> {
    let address = property_address(selector, scope);
    let mut size = size_of::<T>() as u32;
    // SAFETY: `T` is only instantiated for plain-old-data property payloads
    // (ints, f64s, structs of them, pointers) below, all valid when zeroed.
    let mut value = unsafe { std::mem::zeroed::<T>() };
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address,
            0,
            ptr::null(),
            &mut size,
            (&mut value as *mut T).cast::<c_void>(),
        )
    };
    if status != 0 {
        return Err(os_error(context, status));
    }
    Ok(value)
}

fn get_property_string(
    object: AudioObjectId,
    selector: u32,
    scope: u32,
    context: &str,
) -> Result<String, String> {
    let raw = get_property_data::<*const c_void>(object, selector, scope, context)?;
    if raw.is_null() {
        return Err(format!("{context} returned a null string"));
    }
    let length = unsafe { CFStringGetLength(raw) };
    let capacity =
        unsafe { CFStringGetMaximumSizeForEncoding(length, K_CF_STRING_ENCODING_UTF8) } + 1;
    let mut buffer = vec![0_u8; capacity.max(1) as usize];
    let converted = unsafe {
        CFStringGetCString(
            raw,
            buffer.as_mut_ptr().cast::<c_char>(),
            buffer.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
        )
    };
    let text = if converted {
        let cstr = unsafe { CStr::from_ptr(buffer.as_ptr().cast::<c_char>()) };
        cstr.to_string_lossy().into_owned()
    } else {
        String::new()
    };
    unsafe {
        CFRelease(raw);
    }
    Ok(text)
}

fn audio_device_ids() -> Vec<AudioDeviceId> {
    let address = property_address(
        K_AUDIO_HARDWARE_PROPERTY_DEVICES,
        K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
    );
    let mut size = 0_u32;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &address,
            0,
            ptr::null(),
            &mut size,
        )
    };
    if status != 0 || size == 0 {
        return Vec::new();
    }
    let count = size as usize / size_of::<AudioDeviceId>();
    let mut ids = vec![0_u32; count];
    let mut used = size;
    let status = unsafe {
        AudioObjectGetPropertyData(
            K_AUDIO_OBJECT_SYSTEM_OBJECT,
            &address,
            0,
            ptr::null(),
            &mut used,
            ids.as_mut_ptr().cast::<c_void>(),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    ids.truncate(used as usize / size_of::<AudioDeviceId>());
    ids
}

fn device_uid(device: AudioDeviceId) -> Option<String> {
    get_property_string(
        device,
        K_AUDIO_DEVICE_PROPERTY_DEVICE_UID,
        K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        "kAudioDevicePropertyDeviceUID",
    )
    .ok()
    .filter(|uid| !uid.is_empty())
}

fn device_name(device: AudioDeviceId) -> Option<String> {
    get_property_string(
        device,
        K_AUDIO_OBJECT_PROPERTY_NAME,
        K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        "kAudioObjectPropertyName",
    )
    .ok()
}

fn device_stream_ids(device: AudioDeviceId) -> Vec<AudioObjectId> {
    let address = property_address(
        K_AUDIO_DEVICE_PROPERTY_STREAMS,
        K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
    );
    let mut size = 0_u32;
    // SAFETY: pure size query on a device id returned by the HAL.
    let status =
        unsafe { AudioObjectGetPropertyDataSize(device, &address, 0, ptr::null(), &mut size) };
    if status != 0 || size == 0 {
        return Vec::new();
    }
    let count = size as usize / size_of::<AudioObjectId>();
    let mut ids = vec![0_u32; count];
    let mut used = size;
    // SAFETY: `ids` holds `count` u32s, matching the queried property size.
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            &address,
            0,
            ptr::null(),
            &mut used,
            ids.as_mut_ptr().cast::<c_void>(),
        )
    };
    if status != 0 {
        return Vec::new();
    }
    ids.truncate(used as usize / size_of::<AudioObjectId>());
    ids
}

/// Channel count for one stream direction (0 = output, 1 = input per
/// kAudioStreamPropertyDirection).
fn device_channel_count(device: AudioDeviceId, input: bool) -> usize {
    let wanted_direction = u32::from(input);
    device_stream_ids(device)
        .into_iter()
        .filter_map(|stream| {
            let direction = get_property_data::<u32>(
                stream,
                K_AUDIO_STREAM_PROPERTY_DIRECTION,
                K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
                "kAudioStreamPropertyDirection",
            )
            .ok()?;
            if direction != wanted_direction {
                return None;
            }
            let format = get_property_data::<AudioStreamBasicDescription>(
                stream,
                K_AUDIO_STREAM_PROPERTY_PHYSICAL_FORMAT,
                K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
                "kAudioStreamPropertyPhysicalFormat",
            )
            .ok()?;
            Some(format.m_channels_per_frame as usize)
        })
        .sum()
}

fn device_u32_property(device: AudioDeviceId, selector: u32, context: &str) -> Option<u32> {
    get_property_data::<u32>(
        device,
        selector,
        K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        context,
    )
    .ok()
}

fn device_nominal_sample_rate(device: AudioDeviceId) -> Option<f64> {
    get_property_data::<f64>(
        device,
        K_AUDIO_DEVICE_PROPERTY_NOMINAL_SAMPLE_RATE,
        K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        "kAudioDevicePropertyNominalSampleRate",
    )
    .ok()
}

/// Device latency contribution: device latency + safety offset (frames).
fn device_latency_frames(device: AudioDeviceId) -> usize {
    let latency = device_u32_property(
        device,
        K_AUDIO_DEVICE_PROPERTY_LATENCY,
        "kAudioDevicePropertyLatency",
    )
    .unwrap_or(0) as usize;
    let safety = device_u32_property(
        device,
        K_AUDIO_DEVICE_PROPERTY_SAFETY_OFFSET,
        "kAudioDevicePropertySafetyOffset",
    )
    .unwrap_or(0) as usize;
    latency + safety
}

fn device_buffer_frame_size_range(device: AudioDeviceId) -> Option<AudioValueRange> {
    get_property_data::<AudioValueRange>(
        device,
        K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE_RANGE,
        K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        "kAudioDevicePropertyBufferFrameSizeRange",
    )
    .ok()
}

fn set_device_buffer_frame_size(device: AudioDeviceId, frames: u32) -> bool {
    let address = property_address(
        K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE,
        K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
    );
    let status = unsafe {
        AudioObjectSetPropertyData(
            device,
            &address,
            0,
            ptr::null(),
            size_of::<u32>() as u32,
            (&frames as *const u32).cast::<c_void>(),
        )
    };
    status == 0
}

fn actual_buffer_frame_size(device: AudioDeviceId) -> Option<u32> {
    device_u32_property(
        device,
        K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE,
        "kAudioDevicePropertyBufferFrameSize",
    )
}

fn default_device_id(selector: u32, context: &str) -> Result<AudioDeviceId, String> {
    let id = get_property_data::<AudioDeviceId>(
        K_AUDIO_OBJECT_SYSTEM_OBJECT,
        selector,
        K_AUDIO_OBJECT_PROPERTY_SCOPE_GLOBAL,
        context,
    )?;
    if id == 0 {
        return Err(format!("{context} returned no device"));
    }
    Ok(id)
}

/// Public audio device descriptor (mirrors the freebsd/oss discovery shape).
pub struct AudioDeviceDescriptor {
    pub id: String,
    pub label: String,
    pub supports_input: bool,
    pub supports_output: bool,
    pub sample_rates: Vec<i32>,
}

pub fn discover_coreaudio_audio_devices() -> Vec<AudioDeviceDescriptor> {
    audio_device_ids()
        .into_iter()
        .filter_map(|id| {
            let uid = device_uid(id)?;
            let label = device_name(id).unwrap_or_else(|| uid.clone());
            let supports_input = device_channel_count(id, true) > 0;
            let supports_output = device_channel_count(id, false) > 0;
            let mut sample_rates: Vec<i32> = device_nominal_sample_rate(id)
                .map(|rate| rate.round() as i32)
                .into_iter()
                .filter(|rate| *rate > 0)
                .collect();
            sample_rates.sort_unstable();
            sample_rates.dedup();
            Some(AudioDeviceDescriptor {
                id: uid,
                label,
                supports_input,
                supports_output,
                sample_rates,
            })
        })
        .collect()
}

pub fn default_output_device_id() -> Option<String> {
    default_device_id(
        K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE,
        "kAudioHardwarePropertyDefaultOutputDevice",
    )
    .ok()
    .and_then(device_uid)
}

pub fn default_input_device_id() -> Option<String> {
    default_device_id(
        K_AUDIO_HARDWARE_PROPERTY_DEFAULT_INPUT_DEVICE,
        "kAudioHardwarePropertyDefaultInputDevice",
    )
    .ok()
    .and_then(device_uid)
}

fn strip_coreaudio_prefix(device: &str) -> &str {
    device
        .strip_prefix(COREAUDIO_PREFIX)
        .unwrap_or(device)
        .trim()
}

fn find_device_id_by_uid(uid: &str) -> Option<AudioDeviceId> {
    let requested = uid.trim();
    if requested.is_empty() {
        return None;
    }
    let requested_lc = requested.to_lowercase();
    let mut fuzzy = None;
    for id in audio_device_ids() {
        let Some(device_uid) = device_uid(id) else {
            continue;
        };
        if device_uid.eq_ignore_ascii_case(requested) {
            return Some(id);
        }
        if fuzzy.is_none() && device_uid.to_lowercase().contains(&requested_lc) {
            fuzzy = Some(id);
        }
    }
    fuzzy
}

fn resolve_device_id(
    requested: &str,
    default_selector: u32,
    context: &str,
) -> Result<AudioDeviceId, String> {
    let requested = strip_coreaudio_prefix(requested);
    if requested.is_empty() || requested.eq_ignore_ascii_case("default") {
        return default_device_id(default_selector, context);
    }
    find_device_id_by_uid(requested)
        .ok_or_else(|| format!("No matching CoreAudio device for '{requested}'"))
}

fn create_hal_output_unit() -> Result<AudioUnitRef, String> {
    let desc = AudioComponentDescription {
        component_type: K_AUDIO_UNIT_TYPE_OUTPUT,
        component_sub_type: K_AUDIO_UNIT_SUBTYPE_HAL_OUTPUT,
        component_manufacturer: K_AUDIO_UNIT_MANUFACTURER_APPLE,
        component_flags: 0,
        component_flags_mask: 0,
    };
    // SAFETY: `desc` is a plain value struct; a null in-component starts the
    // search. The returned component (if any) is borrowed from the system.
    let component = unsafe { AudioComponentFindNext(ptr::null_mut(), &desc) };
    if component.is_null() {
        return Err("HALOutput AudioComponent not found".to_string());
    }
    let mut unit: AudioUnitRef = ptr::null_mut();
    // SAFETY: `unit` outlives the call and is written only by the callee.
    let status = unsafe { AudioComponentInstanceNew(component, &mut unit) };
    if status != 0 || unit.is_null() {
        return Err(os_error("AudioComponentInstanceNew", status));
    }
    Ok(unit)
}

/// Disposes the unit on drop unless disarmed; guarantees cleanup when a
/// multi-step open fails partway.
struct UnitGuard {
    unit: AudioUnitRef,
    initialized: bool,
}

impl UnitGuard {
    fn new(unit: AudioUnitRef) -> Self {
        Self {
            unit,
            initialized: false,
        }
    }

    fn mark_initialized(&mut self) {
        self.initialized = true;
    }

    fn disarm(mut self) -> AudioUnitRef {
        let unit = self.unit;
        self.unit = ptr::null_mut();
        unit
    }
}

impl Drop for UnitGuard {
    fn drop(&mut self) {
        if self.unit.is_null() {
            return;
        }
        if self.initialized {
            // SAFETY: the unit is still owned by this guard and was
            // successfully initialized; Stop is a no-op if never started.
            unsafe {
                let _ = AudioOutputUnitStop(self.unit);
                let _ = AudioUnitUninitialize(self.unit);
            }
        }
        // SAFETY: the unit handle is unique to this guard.
        unsafe {
            let _ = AudioComponentInstanceDispose(self.unit);
        }
    }
}

fn set_unit_property<T>(
    unit: AudioUnitRef,
    property: u32,
    scope: u32,
    element: u32,
    value: &T,
    context: &str,
) -> Result<(), String> {
    // SAFETY: `value` outlives the call; size matches the pointed-to value.
    let status = unsafe {
        AudioUnitSetProperty(
            unit,
            property,
            scope,
            element,
            (value as *const T).cast::<c_void>(),
            size_of::<T>() as u32,
        )
    };
    if status != 0 {
        return Err(os_error(context, status));
    }
    Ok(())
}

fn get_unit_property<T>(
    unit: AudioUnitRef,
    property: u32,
    scope: u32,
    element: u32,
    context: &str,
) -> Result<T, String> {
    let mut size = size_of::<T>() as u32;
    // SAFETY: `T` is only instantiated for plain-old-data property payloads
    // (ints, f64s, structs of them) below, all valid when zeroed.
    let mut value = unsafe { std::mem::zeroed::<T>() };
    // SAFETY: `value` outlives the call and is sized per `size`.
    let status = unsafe {
        AudioUnitGetProperty(
            unit,
            property,
            scope,
            element,
            (&mut value as *mut T).cast::<c_void>(),
            &mut size,
        )
    };
    if status != 0 {
        return Err(os_error(context, status));
    }
    Ok(value)
}

fn set_enable_io(unit: AudioUnitRef, scope: u32, element: u32, enable: bool) -> Result<(), String> {
    let flag: u32 = u32::from(enable);
    set_unit_property(
        unit,
        K_AUDIO_OUTPUT_UNIT_PROPERTY_ENABLE_IO,
        scope,
        element,
        &flag,
        "kAudioOutputUnitPropertyEnableIO",
    )
}

fn set_current_device(unit: AudioUnitRef, device: AudioDeviceId) -> Result<(), String> {
    set_unit_property(
        unit,
        K_AUDIO_OUTPUT_UNIT_PROPERTY_CURRENT_DEVICE,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
        &device,
        "kAudioOutputUnitPropertyCurrentDevice",
    )
}

fn get_stream_format(
    unit: AudioUnitRef,
    scope: u32,
    element: u32,
    context: &str,
) -> Result<AudioStreamBasicDescription, String> {
    get_unit_property(
        unit,
        K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
        scope,
        element,
        context,
    )
}

fn set_stream_format(
    unit: AudioUnitRef,
    scope: u32,
    element: u32,
    sample_rate: f64,
    channels: u32,
) -> Result<(), String> {
    let format = AudioStreamBasicDescription::float_interleaved(sample_rate, channels);
    set_unit_property(
        unit,
        K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
        scope,
        element,
        &format,
        "kAudioUnitProperty_StreamFormat",
    )
}

fn set_maximum_frames_per_slice(unit: AudioUnitRef, frames: u32) -> Result<(), String> {
    set_unit_property(
        unit,
        K_AUDIO_UNIT_PROPERTY_MAXIMUM_FRAMES_PER_SLICE,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
        &frames,
        "kAudioUnitProperty_MaximumFramesPerSlice",
    )
}

fn set_sample_rate(unit: AudioUnitRef, sample_rate: f64) -> Result<(), String> {
    set_unit_property(
        unit,
        K_AUDIO_UNIT_PROPERTY_SAMPLE_RATE,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
        &sample_rate,
        "kAudioUnitProperty_SampleRate",
    )
}

fn set_render_callback(unit: AudioUnitRef, context: *mut c_void) -> Result<(), String> {
    let callback = AuRenderCallbackStruct {
        input_proc: Some(render_callback),
        input_proc_ref_con: context,
    };
    set_unit_property(
        unit,
        K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK,
        K_AUDIO_UNIT_SCOPE_OUTPUT,
        0,
        &callback,
        "kAudioUnitProperty_SetRenderCallback",
    )
}

fn set_input_callback(unit: AudioUnitRef, context: *mut c_void) -> Result<(), String> {
    let callback = AuRenderCallbackStruct {
        input_proc: Some(input_callback),
        input_proc_ref_con: context,
    };
    set_unit_property(
        unit,
        K_AUDIO_OUTPUT_UNIT_PROPERTY_SET_INPUT_CALLBACK,
        K_AUDIO_UNIT_SCOPE_GLOBAL,
        0,
        &callback,
        "kAudioOutputUnitPropertySetInputCallback",
    )
}

fn start_unit(unit: AudioUnitRef, context: &str) -> Result<(), String> {
    // SAFETY: `unit` is a valid, initialized HALOutput instance.
    let status = unsafe { AudioOutputUnitStart(unit) };
    if status != 0 {
        return Err(os_error(context, status));
    }
    Ok(())
}

/// Try to pin the device buffer to `requested` frames when the device
/// advertises a supported range; returns the effective frame count.
fn tune_device_buffer(device: AudioDeviceId, requested: u32) -> u32 {
    if let Some(range) = device_buffer_frame_size_range(device)
        && requested >= range.m_minimum.max(1.0) as u32
        && requested <= range.m_maximum as u32
        && set_device_buffer_frame_size(device, requested)
        && let Some(actual) = actual_buffer_frame_size(device)
    {
        return actual.max(1);
    }
    requested.max(1)
}

struct OutputState {
    samples: VecDeque<f32>,
    capacity: usize,
    wait_timeout: Duration,
    /// Short grace used once the transport stops: the callback must never
    /// block the HAL IO thread past the buffer deadline waiting for engine
    /// data that will never arrive, or the HAL emits repeated/clicking
    /// buffers (the stop-crackle bug).
    idle_timeout: Duration,
    stopped: bool,
}

struct OutputShared {
    state: Mutex<OutputState>,
    condvar: Condvar,
    playing: AtomicBool,
    xruns: AtomicUsize,
}

struct OutputCallbackContext {
    shared: Arc<OutputShared>,
}

struct InputState {
    samples: VecDeque<f32>,
    capacity: usize,
    channels: usize,
}

struct InputShared {
    state: Mutex<InputState>,
}

struct InputCallbackContext {
    shared: Arc<InputShared>,
}

/// Resolve an `AudioBufferList` into per-channel (base, stride, offset)
/// triples, covering both the single interleaved buffer and the
/// per-channel (non-interleaved) layouts.
fn channel_layouts(io_data: *mut AudioBufferList) -> Vec<(*mut f32, usize, usize)> {
    if io_data.is_null() {
        return Vec::new();
    }
    // SAFETY: `io_data` is a valid AudioBufferList provided by the HAL with
    // m_number_buffers buffers. We index past the nominal 1-element array
    // using the documented variable-length layout.
    unsafe {
        let list = &*io_data;
        let count = list.m_number_buffers as usize;
        let first = ptr::addr_of!(list.m_buffers[0]).cast::<AudioBuffer>();
        let buffers = std::slice::from_raw_parts(first, count);
        let mut layouts = Vec::new();
        for buffer in buffers {
            if buffer.m_data.is_null() {
                continue;
            }
            let stride = buffer.m_number_channels.max(1) as usize;
            for channel in 0..stride {
                layouts.push((buffer.m_data.cast::<f32>(), stride, channel));
            }
        }
        layouts
    }
}

/// Pull `frames * channels` samples from the reservoir into the HAL output
/// buffers. While the transport is playing, block up to two periods for the
/// engine to catch up (device-paced backpressure); once stopped (or at
/// startup), only wait a short jitter grace and then emit silence — the HAL
/// render callback must return within the buffer period or the device
/// glitches.
fn fill_output_buffers(shared: &OutputShared, frames: usize, io_data: *mut AudioBufferList) {
    let layouts = channel_layouts(io_data);
    let needed = frames.saturating_mul(layouts.len().max(1));
    let mut state = match shared.state.lock() {
        Ok(state) => state,
        Err(_) => return,
    };
    while state.samples.len() < needed && !state.stopped {
        let playing = shared.playing.load(Ordering::Acquire);
        let wait = if playing {
            state.wait_timeout
        } else {
            state.idle_timeout
        };
        if wait.is_zero() {
            break;
        }
        let (guard, timeout) = match shared.condvar.wait_timeout(state, wait) {
            Ok(pair) => pair,
            Err(poisoned) => poisoned.into_inner(),
        };
        state = guard;
        if timeout.timed_out() {
            break;
        }
    }
    let available = state.samples.len().min(needed);
    let mut copied = 0_usize;
    if !layouts.is_empty() {
        for frame in 0..frames {
            for (base, stride, offset) in &layouts {
                let sample = if copied < available {
                    state.samples.pop_front().unwrap_or(0.0)
                } else {
                    0.0
                };
                // SAFETY: `base` points at a live f32 buffer of
                // frames * stride samples owned by the HAL for this callback.
                unsafe {
                    *base.add(frame.saturating_mul(*stride) + offset) = sample;
                }
                copied += 1;
            }
        }
    }
    if copied < needed {
        shared.xruns.fetch_add(1, Ordering::Relaxed);
    }
    drop(state);
    // Wake a producer blocked on a full reservoir.
    shared.condvar.notify_all();
}

unsafe extern "C" fn render_callback(
    in_ref_con: *mut c_void,
    _io_action_flags: *mut u32,
    _in_time_stamp: *const c_void,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> i32 {
    if in_ref_con.is_null() {
        return 0;
    }
    // SAFETY: the refcon box lives until `close_fds` reclaims it after the
    // unit is stopped; the HAL guarantees io_data validity for the call.
    let context = unsafe { &*(in_ref_con as *const OutputCallbackContext) };
    fill_output_buffers(&context.shared, in_number_frames as usize, io_data);
    0
}

/// Interleave an incoming `AudioBufferList` and push it into the input
/// reservoir, dropping the oldest frames when full.
fn push_input_buffers(shared: &InputShared, frames: usize, io_data: *mut AudioBufferList) {
    let layouts = channel_layouts(io_data);
    if layouts.is_empty() {
        return;
    }
    let channels = layouts.len();
    let mut chunk = Vec::with_capacity(frames.saturating_mul(channels));
    for frame in 0..frames {
        for (base, stride, offset) in &layouts {
            // SAFETY: `base` points at a live f32 buffer of
            // frames * stride samples owned by the HAL for this callback.
            let sample = unsafe { *base.add(frame.saturating_mul(*stride) + offset) };
            chunk.push(sample);
        }
    }
    let Ok(mut state) = shared.state.lock() else {
        return;
    };
    while state.samples.len() + chunk.len() > state.capacity {
        for _ in 0..state.channels.max(1) {
            state.samples.pop_front();
        }
    }
    state.samples.extend(chunk);
}

unsafe extern "C" fn input_callback(
    in_ref_con: *mut c_void,
    _io_action_flags: *mut u32,
    _in_time_stamp: *const c_void,
    _in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> i32 {
    if in_ref_con.is_null() {
        return 0;
    }
    // SAFETY: same ownership invariant as `render_callback`.
    let context = unsafe { &*(in_ref_con as *const InputCallbackContext) };
    push_input_buffers(&context.shared, in_number_frames as usize, io_data);
    0
}

pub struct HwDriver {
    output_unit: AudioUnitRef,
    input_unit: Option<AudioUnitRef>,
    output_context: Option<*mut OutputCallbackContext>,
    input_context: Option<*mut InputCallbackContext>,
    output_shared: Arc<OutputShared>,
    input_shared: Option<Arc<InputShared>>,
    audio_ins: Vec<Arc<AudioIO>>,
    audio_outs: Vec<Arc<AudioIO>>,
    input_queue: Vec<f32>,
    output_gain_linear: f32,
    output_balance: f32,
    sample_rate: usize,
    period_frames: usize,
    nperiods: usize,
    input_channels: usize,
    output_channels: usize,
    input_latency_frames: usize,
    output_latency_frames: usize,
    playing: bool,
    closed: bool,
    stop_requested: Arc<AtomicBool>,
    plan_slot: Option<Arc<crate::render_plan::PlanSlot>>,
}

impl HwDriver {
    pub fn new_with_options(
        device: &str,
        input_device: Option<&str>,
        rate: i32,
        _bits: i32,
        options: HwOptions,
    ) -> Result<Self, String> {
        let requested_rate = f64::from(rate.max(1));
        let requested_period = (options.period_frames.max(1)) as u32;
        let nperiods = options.nperiods.max(2);
        let stop_requested = Arc::new(AtomicBool::new(false));

        let output_id = resolve_device_id(
            device,
            K_AUDIO_HARDWARE_PROPERTY_DEFAULT_OUTPUT_DEVICE,
            "kAudioHardwarePropertyDefaultOutputDevice",
        )?;
        let input_id = if input_device.is_some() {
            Some(resolve_device_id(
                input_device.unwrap_or("default"),
                K_AUDIO_HARDWARE_PROPERTY_DEFAULT_INPUT_DEVICE,
                "kAudioHardwarePropertyDefaultInputDevice",
            )?)
        } else {
            None
        };
        let duplex_same_device = input_id == Some(output_id);

        // ---- output unit -------------------------------------------------
        let mut unit_guard = UnitGuard::new(create_hal_output_unit()?);
        let unit = unit_guard.unit;
        let input_on_primary = input_id.is_some() && duplex_same_device;
        set_enable_io(unit, K_AUDIO_UNIT_SCOPE_INPUT, 1, input_on_primary)?;
        set_enable_io(unit, K_AUDIO_UNIT_SCOPE_OUTPUT, 0, true)?;
        set_current_device(unit, output_id)?;
        let output_format = get_stream_format(
            unit,
            K_AUDIO_UNIT_SCOPE_OUTPUT,
            0,
            "kAudioUnitProperty_StreamFormat (output)",
        )?;
        let output_channels = output_format.m_channels_per_frame as usize;
        if output_channels == 0 {
            return Err("CoreAudio output device reports zero channels".to_string());
        }
        set_stream_format(
            unit,
            K_AUDIO_UNIT_SCOPE_OUTPUT,
            0,
            requested_rate,
            output_channels as u32,
        )?;
        set_sample_rate(unit, requested_rate)?;
        let input_channels = if input_on_primary {
            let input_format = get_stream_format(
                unit,
                K_AUDIO_UNIT_SCOPE_INPUT,
                1,
                "kAudioUnitProperty_StreamFormat (input)",
            )?;
            let channels = input_format.m_channels_per_frame as usize;
            if channels > 0 {
                set_stream_format(
                    unit,
                    K_AUDIO_UNIT_SCOPE_INPUT,
                    1,
                    requested_rate,
                    channels as u32,
                )?;
            }
            channels
        } else {
            0
        };
        set_maximum_frames_per_slice(unit, requested_period)?;

        let period_frames = tune_device_buffer(output_id, requested_period) as usize;
        let sample_rate = device_nominal_sample_rate(output_id)
            .filter(|r| *r > 0.0)
            .unwrap_or(requested_rate) as usize;
        let period_samples = period_frames.saturating_mul(output_channels);
        let output_capacity = nperiods.saturating_mul(period_samples).max(period_samples);
        let wait_timeout = Duration::from_millis(
            ((period_frames as u64) * 2_000 / sample_rate.max(1) as u64).max(2),
        );
        let output_shared = Arc::new(OutputShared {
            state: Mutex::new(OutputState {
                samples: VecDeque::with_capacity(period_samples.saturating_mul(2)),
                capacity: output_capacity,
                wait_timeout,
                idle_timeout: Duration::from_millis(2),
                stopped: false,
            }),
            condvar: Condvar::new(),
            playing: AtomicBool::new(false),
            xruns: AtomicUsize::new(0),
        });
        let output_context = Box::into_raw(Box::new(OutputCallbackContext {
            shared: output_shared.clone(),
        }));
        let render_result = set_render_callback(unit, output_context.cast::<c_void>());
        if let Err(err) = render_result {
            // SAFETY: the unit was never started; the box is uniquely owned.
            unsafe {
                drop(Box::from_raw(output_context));
            }
            return Err(err);
        }

        // ---- input -------------------------------------------------------
        let (input_shared, input_context) = if input_on_primary {
            let shared = Arc::new(InputShared {
                state: Mutex::new(InputState {
                    samples: VecDeque::with_capacity(period_samples),
                    capacity: period_samples.saturating_mul(4).max(period_samples),
                    channels: input_channels.max(1),
                }),
            });
            let context = Box::into_raw(Box::new(InputCallbackContext {
                shared: shared.clone(),
            }));
            if let Err(err) = set_input_callback(unit, context.cast::<c_void>()) {
                // SAFETY: same uniqueness invariant as the render context.
                unsafe {
                    drop(Box::from_raw(context));
                }
                return Err(err);
            }
            (Some(shared), Some(context))
        } else {
            (None, None)
        };

        // Everything device-dependent is configured; initialize and start.
        // SAFETY: `unit` is valid and all pre-initialize properties are set.
        let init_status = unsafe { AudioUnitInitialize(unit) };
        if init_status != 0 {
            return Err(os_error("AudioUnitInitialize (output)", init_status));
        }
        unit_guard.mark_initialized();
        start_unit(unit, "AudioOutputUnitStart (output)")?;

        // ---- optional second input-only unit ------------------------------
        let mut input_unit = None;
        let mut final_input_channels = input_channels;
        let mut final_input_shared = input_shared;
        let mut final_input_context = input_context;
        if let (Some(in_id), false) = (input_id, duplex_same_device) {
            let in_channels = device_channel_count(in_id, true);
            if in_channels == 0 {
                warn!("CoreAudio input device has no input streams; input disabled");
            } else {
                let mut in_guard = UnitGuard::new(create_hal_output_unit()?);
                let in_unit = in_guard.unit;
                set_enable_io(in_unit, K_AUDIO_UNIT_SCOPE_OUTPUT, 0, false)?;
                set_enable_io(in_unit, K_AUDIO_UNIT_SCOPE_INPUT, 1, true)?;
                set_current_device(in_unit, in_id)?;
                set_stream_format(
                    in_unit,
                    K_AUDIO_UNIT_SCOPE_INPUT,
                    1,
                    requested_rate,
                    in_channels as u32,
                )?;
                set_maximum_frames_per_slice(in_unit, requested_period)?;
                let in_period = tune_device_buffer(in_id, requested_period) as usize;
                let in_capacity = in_period
                    .saturating_mul(in_channels)
                    .saturating_mul(4)
                    .max(1);
                let shared = Arc::new(InputShared {
                    state: Mutex::new(InputState {
                        samples: VecDeque::with_capacity(in_capacity),
                        capacity: in_capacity,
                        channels: in_channels.max(1),
                    }),
                });
                let context = Box::into_raw(Box::new(InputCallbackContext {
                    shared: shared.clone(),
                }));
                if let Err(err) = set_input_callback(in_unit, context.cast::<c_void>()) {
                    // SAFETY: the unit was never started; the box is uniquely
                    // owned here.
                    unsafe {
                        drop(Box::from_raw(context));
                    }
                    return Err(err);
                }
                // SAFETY: `in_unit` is valid and configured.
                let init_status = unsafe { AudioUnitInitialize(in_unit) };
                if init_status != 0 {
                    return Err(os_error("AudioUnitInitialize (input)", init_status));
                }
                in_guard.mark_initialized();
                start_unit(in_unit, "AudioOutputUnitStart (input)")?;
                input_unit = Some(in_guard.disarm());
                final_input_channels = in_channels;
                final_input_shared = Some(shared);
                final_input_context = Some(context);
            }
        }

        let audio_outs = (0..output_channels)
            .map(|_| Arc::new(AudioIO::new(period_frames)))
            .collect();
        let audio_ins = (0..final_input_channels)
            .map(|_| Arc::new(AudioIO::new(period_frames)))
            .collect();

        let input_hw_latency = match input_id {
            Some(in_id) if final_input_channels > 0 => device_latency_frames(in_id),
            _ => 0,
        };
        let output_hw_latency = device_latency_frames(output_id);
        let input_latency_frames = input_hw_latency.saturating_add(options.input_latency_frames);
        let output_latency_frames = output_hw_latency.saturating_add(options.output_latency_frames);

        debug!(
            output_device = device,
            input_channels = final_input_channels,
            output_channels,
            sample_rate,
            period_frames,
            "CoreAudio backend opened"
        );

        Ok(Self {
            output_unit: unit_guard.disarm(),
            input_unit,
            output_context: Some(output_context),
            input_context: final_input_context,
            output_shared,
            input_shared: final_input_shared,
            audio_ins,
            audio_outs,
            input_queue: Vec::new(),
            output_gain_linear: 1.0,
            output_balance: 0.0,
            sample_rate,
            period_frames,
            nperiods,
            input_channels: final_input_channels,
            output_channels,
            input_latency_frames,
            output_latency_frames,
            playing: false,
            closed: false,
            stop_requested,
            plan_slot: None,
        })
    }

    pub fn input_channels(&self) -> usize {
        self.input_channels
    }

    pub fn output_channels(&self) -> usize {
        self.output_channels
    }

    pub fn sample_rate(&self) -> i32 {
        self.sample_rate as i32
    }

    pub fn cycle_samples(&self) -> usize {
        self.period_frames
    }

    pub fn sample_bits(&self) -> i32 {
        32
    }

    pub fn frame_size_bytes(&self) -> usize {
        self.output_channels * 4
    }

    pub fn input_port(&self, idx: usize) -> Option<Arc<AudioIO>> {
        self.audio_ins.get(idx).cloned()
    }

    pub fn output_port(&self, idx: usize) -> Option<Arc<AudioIO>> {
        self.audio_outs.get(idx).cloned()
    }

    pub fn set_output_gain_balance(&mut self, gain: f32, balance: f32) {
        self.output_gain_linear = gain.max(0.0);
        self.output_balance = balance.clamp(-1.0, 1.0);
    }

    pub fn set_plan_slot(&mut self, slot: Arc<crate::render_plan::PlanSlot>) {
        self.plan_slot = Some(slot);
    }

    pub fn output_meter_db(&self, gain: f32, balance: f32) -> Vec<f32> {
        common::output_meter_db(self.audio_outs.len(), gain, balance)
    }

    pub fn output_meter_linear(&self, gain: f32, balance: f32) -> Vec<f32> {
        if let Some(slot) = &self.plan_slot {
            let plan = slot.load();
            common::output_meter_linear_from_plan(&plan, gain, balance)
        } else {
            common::output_meter_linear(self.audio_outs.len(), gain, balance)
        }
    }

    pub fn run_cycle(&mut self) -> Result<(), String> {
        let input_frames = self.period_frames;
        let input_channels = self.input_channels.max(1);
        if let Some(shared) = &self.input_shared
            && let Ok(mut state) = shared.state.lock()
        {
            self.input_queue.extend(state.samples.drain(..));
        }

        let have_samples = self.input_queue.len();
        let consume_frames = (have_samples / input_channels).min(input_frames);
        let consume_samples = consume_frames.saturating_mul(input_channels);

        if let Some(slot) = &self.plan_slot {
            let plan = slot.load();
            crate::hw::ports::fill_arena_from_interleaved(
                &plan,
                input_frames,
                &self.input_queue[..consume_samples],
                input_channels,
            );
        } else {
            for io_port in &self.audio_ins {
                io_port.finished.store(true, Ordering::Release);
            }
        }

        if consume_samples > 0 {
            self.input_queue.drain(..consume_samples);
        }

        let frames = self.period_frames;
        let channels = self.output_channels;
        let gain = self.output_gain_linear;
        let balance = self.output_balance;
        let mut interleaved = vec![0.0_f32; frames.saturating_mul(channels)];
        if self.playing
            && let Some(slot) = &self.plan_slot
        {
            let plan = slot.load();
            crate::hw::ports::write_interleaved_from_arena(
                &plan,
                frames,
                gain,
                balance,
                |ch, frame, sample| {
                    let idx = frame * channels + ch;
                    if let Some(dst) = interleaved.get_mut(idx) {
                        *dst = sample;
                    }
                },
            );
        }

        self.queue_output_period(interleaved)
    }

    fn queue_output_period(&mut self, interleaved: Vec<f32>) -> Result<(), String> {
        let shared = &self.output_shared;
        let mut state = shared
            .state
            .lock()
            .map_err(|_| "CoreAudio output state poisoned".to_string())?;
        loop {
            if self.stop_requested.load(Ordering::Acquire) || state.stopped {
                return Ok(());
            }
            if state.samples.len() + interleaved.len() <= state.capacity {
                state.samples.extend(interleaved.iter().copied());
                shared.condvar.notify_one();
                return Ok(());
            }
            // Reservoir full: the HAL render callback paces the engine cycle.
            let (guard, timeout) = shared
                .condvar
                .wait_timeout(state, Duration::from_millis(500))
                .map_err(|_| "CoreAudio output state poisoned".to_string())?;
            state = guard;
            if timeout.timed_out() {
                return Err("Timed out waiting for CoreAudio render callback".to_string());
            }
        }
    }

    pub fn run_assist_step(&mut self) -> Result<bool, String> {
        Ok(false)
    }

    pub fn channel(&mut self) -> &mut Self {
        self
    }

    pub fn set_playing(&mut self, playing: bool) {
        self.playing = playing;
        // The render callback uses this flag to decide whether it may block
        // waiting for engine data (playing) or must fall back to silence
        // immediately (stopped), so the HAL IO thread never overruns the
        // buffer deadline on a starved queue.
        self.output_shared.playing.store(playing, Ordering::Release);
    }

    pub fn close_fds(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.stop_requested.store(true, Ordering::Release);
        if let Ok(mut state) = self.output_shared.state.lock() {
            state.stopped = true;
        }
        self.output_shared.condvar.notify_all();
        // SAFETY: stopping a HALOutput unit is synchronous with respect to
        // its IO thread, so no callback is running once Stop returns.
        unsafe {
            if !self.output_unit.is_null() {
                let _ = AudioOutputUnitStop(self.output_unit);
                let _ = AudioUnitUninitialize(self.output_unit);
                let _ = AudioComponentInstanceDispose(self.output_unit);
                self.output_unit = ptr::null_mut();
            }
            if let Some(unit) = &mut self.input_unit
                && !unit.is_null()
            {
                let _ = AudioOutputUnitStop(*unit);
                let _ = AudioUnitUninitialize(*unit);
                let _ = AudioComponentInstanceDispose(*unit);
                *unit = ptr::null_mut();
            }
        }
        if let Some(context) = self.output_context.take()
            // SAFETY: the unit is stopped and disposed, so no callback can
            // run; the box is uniquely owned here.
            && !context.is_null()
        {
            unsafe {
                drop(Box::from_raw(context));
            }
        }
        if let Some(context) = self.input_context.take()
            && !context.is_null()
        {
            unsafe {
                drop(Box::from_raw(context));
            }
        }
    }

    pub fn latency_ranges(&self) -> ((usize, usize), (usize, usize)) {
        latency::latency_ranges(
            self.cycle_samples(),
            self.nperiods,
            true,
            self.input_latency_frames,
            self.output_latency_frames,
        )
    }
}

unsafe impl Send for HwDriver {}

impl Drop for HwDriver {
    fn drop(&mut self) {
        self.close_fds();
    }
}

impl traits::HwWorkerDriver for HwDriver {
    fn cycle_samples(&self) -> usize {
        self.cycle_samples()
    }

    fn sample_rate(&self) -> i32 {
        self.sample_rate()
    }

    fn close_fds(&mut self) {
        self.close_fds();
    }

    fn set_playing(&mut self, playing: bool) {
        self.set_playing(playing)
    }

    fn set_output_gain_balance(&mut self, gain: f32, balance: f32) {
        self.set_output_gain_balance(gain, balance)
    }

    fn run_cycle_for_worker(&mut self) -> Result<(), String> {
        self.channel().run_cycle()
    }

    fn run_assist_step_for_worker(&mut self) -> Result<bool, String> {
        self.run_assist_step()
    }

    fn set_plan_slot(&mut self, slot: Arc<crate::render_plan::PlanSlot>) {
        self.set_plan_slot(slot);
    }

    fn request_stop(&mut self) {
        self.stop_requested.store(true, Ordering::Release);
        self.close_fds();
    }
}

crate::impl_hw_device_for_driver!(HwDriver);

#[cfg(test)]
mod stop_silence_tests {
    use super::*;
    use std::time::Instant;

    fn test_shared(wait_timeout: Duration) -> Arc<OutputShared> {
        Arc::new(OutputShared {
            state: Mutex::new(OutputState {
                samples: VecDeque::new(),
                capacity: 4096,
                wait_timeout,
                idle_timeout: Duration::from_millis(1),
                stopped: false,
            }),
            condvar: Condvar::new(),
            playing: AtomicBool::new(false),
            xruns: AtomicUsize::new(0),
        })
    }

    /// One interleaved HAL buffer filled with a 7.0 sentinel so tests can
    /// detect both unwritten and stale regions.
    fn make_abl(frames: usize, channels: usize) -> (AudioBufferList, *mut f32, usize) {
        let len = frames.saturating_mul(channels);
        let storage = vec![7.0_f32; len].into_boxed_slice();
        let ptr = Box::into_raw(storage).cast::<f32>();
        let list = AudioBufferList {
            m_number_buffers: 1,
            m_buffers: [AudioBuffer {
                m_number_channels: channels as u32,
                m_data_byte_size: (len.saturating_mul(4)) as u32,
                m_data: ptr.cast::<c_void>(),
            }],
        };
        (list, ptr, len)
    }

    unsafe fn read_abl(ptr: *const f32, len: usize) -> Vec<f32> {
        unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
    }

    #[test]
    fn starved_callback_emits_silence_immediately_when_not_playing() {
        let shared = test_shared(Duration::from_secs(5));
        let (mut list, ptr, len) = make_abl(64, 2);
        let start = Instant::now();
        fill_output_buffers(&shared, 64, &mut list);
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "starved callback blocked the HAL thread for {:?}",
            start.elapsed()
        );
        let out = unsafe { read_abl(ptr, len) };
        assert!(out.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn starved_callback_still_backpressures_while_playing() {
        let shared = test_shared(Duration::from_millis(150));
        shared.playing.store(true, Ordering::Release);
        let (mut list, _, _) = make_abl(64, 2);
        let start = Instant::now();
        fill_output_buffers(&shared, 64, &mut list);
        assert!(
            start.elapsed() >= Duration::from_millis(100),
            "playing callback should wait for engine data, returned after {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn drained_reservoir_never_repeats_stale_audio() {
        let shared = test_shared(Duration::from_secs(5));
        let (mut list, ptr, len) = make_abl(64, 2);
        shared
            .state
            .lock()
            .unwrap()
            .samples
            .extend(std::iter::repeat_n(0.5_f32, 128));
        fill_output_buffers(&shared, 64, &mut list);
        // Reservoir now empty: the next callback must output silence, not
        // replay the previous period.
        fill_output_buffers(&shared, 64, &mut list);
        let out = unsafe { read_abl(ptr, len) };
        assert!(
            out.iter().all(|sample| *sample == 0.0),
            "stale audio repeated after drain"
        );
    }
}
