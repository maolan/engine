//! Hand-rolled WinMM MIDI wrapper (replaces the `midir` crate).
//!
//! Same philosophy as the unix `hw::midi_hub`: talk to the OS MIDI API
//! directly and deliver raw MIDI bytes to a caller-supplied closure instead of
//! depending on an external abstraction layer.
//!
//! Ownership invariants (all `unsafe` below hinges on these):
//! - Each open input/output passes one heap allocation as the WinMM `instance`
//!   parameter. WinMM hands it back to the `extern "system"` callback on a
//!   driver thread; it stays valid until the matching `midiInClose` /
//!   `midiOutClose` returns, which blocks until the callback is not running.
//!   `close()` therefore reclaims the allocation only after the close call.
//! - Input sysex buffers are prepared and queued at connect time and remain
//!   owned by the connection (the `MIDIHDR` structs live in the connection;
//!   their `lpData` backing boxes are only freed in `close()`, after
//!   `midiInReset` has returned all queued buffers to the application).
//! - The caller must not invoke `close()` from inside the MIDI callback;
//!   `midiInClose`/`midiOutClose` deadlock if called from the callback thread.

use std::slice;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use windows::Win32::Media::Audio::{
    CALLBACK_FUNCTION, HMIDIIN, HMIDIOUT, MHDR_PREPARED, MIDI_IO_STATUS, MIDIHDR, MIDIINCAPSW,
    MIDIOUTCAPSW, midiInAddBuffer, midiInClose, midiInGetDevCapsW, midiInGetNumDevs, midiInOpen,
    midiInPrepareHeader, midiInReset, midiInStart, midiInUnprepareHeader, midiOutClose,
    midiOutGetDevCapsW, midiOutGetNumDevs, midiOutLongMsg, midiOutOpen, midiOutPrepareHeader,
    midiOutReset, midiOutShortMsg, midiOutUnprepareHeader,
};
use windows::Win32::Media::{
    MM_MIM_DATA, MM_MIM_ERROR, MM_MIM_LONGDATA, MM_MIM_LONGERROR, MM_MIM_MOREDATA, MM_MOM_DONE,
    MMSYSERR_NOERROR,
};
use windows::core::PSTR;

// Driver-callback message IDs. The windows crate only exports the numerically
// identical MM_* window-message constants; the MIM_*/MOM_* callback messages
// share those values.
const MIM_DATA: u32 = MM_MIM_DATA;
const MIM_LONGDATA: u32 = MM_MIM_LONGDATA;
const MIM_ERROR: u32 = MM_MIM_ERROR;
const MIM_LONGERROR: u32 = MM_MIM_LONGERROR;
const MIM_MOREDATA: u32 = MM_MIM_MOREDATA;
const MOM_DONE: u32 = MM_MOM_DONE;

const SYSEX_BUFFER_COUNT: usize = 4;
const SYSEX_BUFFER_LEN: usize = 1024;
const LONG_MSG_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
pub struct MidiInputPort(u32);

#[derive(Clone, Copy)]
pub struct MidiOutputPort(u32);

pub struct MidiInput;

pub struct MidiOutput;

struct InputCallbackState {
    callback: MidiCallback,
}

type MidiCallback = Box<dyn FnMut(&[u8]) + Send>;

struct OutputCallbackState {
    done: Mutex<bool>,
    done_cv: Condvar,
}

impl MidiInput {
    pub fn new(_name: &str) -> Result<Self, String> {
        Ok(Self)
    }

    pub fn ports(&self) -> Vec<MidiInputPort> {
        // SAFETY: takes no parameters; device count is a plain query.
        (0..unsafe { midiInGetNumDevs() })
            .map(MidiInputPort)
            .collect()
    }

    pub fn port_name(&self, port: &MidiInputPort) -> Result<String, String> {
        let mut caps = MIDIINCAPSW::default();
        // SAFETY: `caps` outlives the call and is sized exactly as advertised.
        let rc = unsafe {
            midiInGetDevCapsW(port.0 as usize, &mut caps, size_of::<MIDIINCAPSW>() as u32)
        };
        if rc != MMSYSERR_NOERROR {
            return Err(format!("midiInGetDevCapsW failed with code {rc}"));
        }
        // SAFETY: `caps` is a live packed struct; copy the name array out by
        // value (unaligned read) since packed fields cannot be referenced.
        let pname = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(caps.szPname)) };
        Ok(wchar_to_string(&pname))
    }

    pub fn connect<F: FnMut(&[u8]) + Send + 'static>(
        &self,
        port: &MidiInputPort,
        _name: &str,
        callback: F,
    ) -> Result<MidiInputConnection, String> {
        // The box is handed to WinMM as the instance parameter and reclaimed
        // in `MidiInputConnection::close` after `midiInClose` returns.
        let state = Box::into_raw(Box::new(InputCallbackState {
            callback: Box::new(callback),
        }));
        let mut handle = HMIDIIN::default();
        // SAFETY: `handle` and `state` are valid for the call. The callback
        // only dereferences `state`, which outlives the open handle.
        let rc = unsafe {
            midiInOpen(
                &mut handle,
                port.0,
                Some(input_callback as *const () as usize),
                Some(state as usize),
                CALLBACK_FUNCTION | MIDI_IO_STATUS,
            )
        };
        if rc != MMSYSERR_NOERROR {
            // SAFETY: `state` was never shared with WinMM; reclaim it.
            unsafe {
                drop(Box::from_raw(state));
            }
            return Err(format!("midiInOpen failed with code {rc}"));
        }

        let mut buffers = Vec::with_capacity(SYSEX_BUFFER_COUNT);
        for _ in 0..SYSEX_BUFFER_COUNT {
            // Backing store for the driver to record sysex into. The box is
            // owned by the connection and freed in `close` after
            // `midiInReset`/`midiInUnprepareHeader` return it to us.
            let data = Box::into_raw(Box::new([0u8; SYSEX_BUFFER_LEN]));
            buffers.push(MIDIHDR {
                lpData: PSTR(data.cast::<u8>()),
                dwBufferLength: SYSEX_BUFFER_LEN as u32,
                ..Default::default()
            });
        }
        for buffer in &mut buffers {
            // SAFETY: `buffer` is a live MIDIHDR owned by us and sized
            // correctly; the handle is open.
            let rc = unsafe { midiInPrepareHeader(handle, buffer, size_of::<MIDIHDR>() as u32) };
            if rc != MMSYSERR_NOERROR {
                let _ = unsafe { midiInReset(handle) };
                for prepared in &mut buffers {
                    if prepared.dwFlags & MHDR_PREPARED != 0 {
                        // SAFETY: prepared header, still owned by us.
                        let _ = unsafe {
                            midiInUnprepareHeader(handle, prepared, size_of::<MIDIHDR>() as u32)
                        };
                    }
                }
                let _ = unsafe { midiInClose(handle) };
                for buffer in &buffers {
                    // SAFETY: each lpData came from the same layout box above
                    // and is still owned by us.
                    unsafe {
                        drop(Box::from_raw(
                            buffer.lpData.0.cast::<[u8; SYSEX_BUFFER_LEN]>(),
                        ));
                    }
                }
                unsafe {
                    drop(Box::from_raw(state));
                }
                return Err(format!("midiInPrepareHeader failed with code {rc}"));
            }
            // SAFETY: `buffer` was just prepared and is not yet queued.
            let rc = unsafe { midiInAddBuffer(handle, buffer, size_of::<MIDIHDR>() as u32) };
            if rc != MMSYSERR_NOERROR {
                let _ = unsafe { midiInReset(handle) };
                for buffer in &mut buffers {
                    if buffer.dwFlags & MHDR_PREPARED != 0 {
                        // SAFETY: prepared header, still owned by us.
                        let _ = unsafe {
                            midiInUnprepareHeader(handle, buffer, size_of::<MIDIHDR>() as u32)
                        };
                    }
                }
                let _ = unsafe { midiInClose(handle) };
                for buffer in &buffers {
                    // SAFETY: same layout box as above, still owned by us.
                    unsafe {
                        drop(Box::from_raw(
                            buffer.lpData.0.cast::<[u8; SYSEX_BUFFER_LEN]>(),
                        ));
                    }
                }
                unsafe {
                    drop(Box::from_raw(state));
                }
                return Err(format!("midiInAddBuffer failed with code {rc}"));
            }
        }

        // SAFETY: handle is open, buffers are queued.
        let rc = unsafe { midiInStart(handle) };
        if rc != MMSYSERR_NOERROR {
            let _ = unsafe { midiInReset(handle) };
            for buffer in &mut buffers {
                if buffer.dwFlags & MHDR_PREPARED != 0 {
                    // SAFETY: prepared header, still owned by us.
                    let _ = unsafe {
                        midiInUnprepareHeader(handle, buffer, size_of::<MIDIHDR>() as u32)
                    };
                }
            }
            let _ = unsafe { midiInClose(handle) };
            for buffer in &buffers {
                // SAFETY: same layout box as above, still owned by us.
                unsafe {
                    drop(Box::from_raw(
                        buffer.lpData.0.cast::<[u8; SYSEX_BUFFER_LEN]>(),
                    ));
                }
            }
            unsafe {
                drop(Box::from_raw(state));
            }
            return Err(format!("midiInStart failed with code {rc}"));
        }

        Ok(MidiInputConnection {
            handle,
            state,
            buffers,
        })
    }
}

pub struct MidiInputConnection {
    handle: HMIDIIN,
    state: *mut InputCallbackState,
    buffers: Vec<MIDIHDR>,
}

// Moving the connection between threads is safe: all driver access is
// serialized — the callback may only run while the handle is open, and
// `close()` blocks until it stops. The pointed-to backing stores stay put.
unsafe impl Send for MidiInputConnection {}

impl MidiInputConnection {
    pub fn close(self) -> Result<(), String> {
        // Stop recording and return all queued long buffers to the app.
        // SAFETY: handle is open and owned by us.
        let reset_rc = unsafe { midiInReset(self.handle) };
        let mut last_err = None;
        if reset_rc != MMSYSERR_NOERROR {
            last_err = Some(format!("midiInReset failed with code {reset_rc}"));
        }
        for buffer in &self.buffers {
            if buffer.dwFlags & MHDR_PREPARED != 0 {
                // SAFETY: after midiInReset the driver is done with the
                // buffer; the header is still owned by us.
                let rc = unsafe {
                    midiInUnprepareHeader(
                        self.handle,
                        buffer as *const _ as *mut _,
                        size_of::<MIDIHDR>() as u32,
                    )
                };
                if rc != MMSYSERR_NOERROR {
                    last_err = Some(format!("midiInUnprepareHeader failed with code {rc}"));
                }
            }
        }
        // Blocks until the driver callback is not running; afterwards WinMM
        // will never touch `state` or the buffers again.
        // SAFETY: handle is open and owned by us; not called from the callback.
        let close_rc = unsafe { midiInClose(self.handle) };
        if close_rc != MMSYSERR_NOERROR {
            last_err = Some(format!("midiInClose failed with code {close_rc}"));
        }
        for buffer in &self.buffers {
            // SAFETY: the driver no longer references the buffers (reset
            // returned them, close completed); each lpData was boxed above
            // with exactly this layout and has not been freed yet.
            unsafe {
                drop(Box::from_raw(
                    buffer.lpData.0.cast::<[u8; SYSEX_BUFFER_LEN]>(),
                ));
            }
        }
        // SAFETY: exclusive reclaim of the instance box passed to midiInOpen;
        // WinMM finished using it before midiInClose returned.
        unsafe {
            drop(Box::from_raw(self.state));
        }
        match last_err {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

impl MidiOutput {
    pub fn new(_name: &str) -> Result<Self, String> {
        Ok(Self)
    }

    pub fn ports(&self) -> Vec<MidiOutputPort> {
        // SAFETY: takes no parameters; device count is a plain query.
        (0..unsafe { midiOutGetNumDevs() })
            .map(MidiOutputPort)
            .collect()
    }

    pub fn port_name(&self, port: &MidiOutputPort) -> Result<String, String> {
        let mut caps = MIDIOUTCAPSW::default();
        // SAFETY: `caps` outlives the call and is sized exactly as advertised.
        let rc = unsafe {
            midiOutGetDevCapsW(port.0 as usize, &mut caps, size_of::<MIDIOUTCAPSW>() as u32)
        };
        if rc != MMSYSERR_NOERROR {
            return Err(format!("midiOutGetDevCapsW failed with code {rc}"));
        }
        // SAFETY: `caps` is a live packed struct; copy the name array out by
        // value (unaligned read) since packed fields cannot be referenced.
        let pname = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(caps.szPname)) };
        Ok(wchar_to_string(&pname))
    }

    pub fn connect(
        &self,
        port: &MidiOutputPort,
        _name: &str,
    ) -> Result<MidiOutputConnection, String> {
        // The Arc is handed to WinMM as the instance parameter (one strong
        // ref owned by the driver) and reclaimed in `close` after
        // `midiOutClose` returns.
        let state = Arc::new(OutputCallbackState {
            done: Mutex::new(false),
            done_cv: Condvar::new(),
        });
        let state_ptr = Arc::into_raw(state) as usize;
        let mut handle = HMIDIOUT::default();
        // SAFETY: `handle` is valid for the call; `state_ptr` references the
        // Arc allocation and stays alive until `close`.
        let rc = unsafe {
            midiOutOpen(
                &mut handle,
                port.0,
                Some(output_callback as *const () as usize),
                Some(state_ptr),
                CALLBACK_FUNCTION,
            )
        };
        if rc != MMSYSERR_NOERROR {
            // SAFETY: WinMM never took ownership; reclaim the reference.
            unsafe {
                drop(Arc::from_raw(state_ptr as *const OutputCallbackState));
            }
            return Err(format!("midiOutOpen failed with code {rc}"));
        }
        Ok(MidiOutputConnection {
            handle,
            state: state_ptr as *const OutputCallbackState,
        })
    }
}

pub struct MidiOutputConnection {
    handle: HMIDIOUT,
    state: *const OutputCallbackState,
}

unsafe impl Send for MidiOutputConnection {}

impl MidiOutputConnection {
    pub fn send(&mut self, data: &[u8]) -> Result<(), String> {
        if data.is_empty() {
            return Ok(());
        }
        if data.len() <= 3 {
            // Pack the bytes little-endian exactly like midir did.
            let mut msg = 0u32;
            for (shift, byte) in data.iter().enumerate() {
                msg |= u32::from(*byte) << (shift * 8);
            }
            // SAFETY: `handle` is open and owned by us.
            let rc = unsafe { midiOutShortMsg(self.handle, msg) };
            if rc != MMSYSERR_NOERROR {
                return Err(format!("midiOutShortMsg failed with code {rc}"));
            }
            return Ok(());
        }

        // SAFETY: `state` is valid for the lifetime of the open handle.
        let state = unsafe { &*self.state };
        let mut data_buf = data.to_vec();
        let mut header = MIDIHDR {
            lpData: PSTR(data_buf.as_mut_ptr()),
            dwBufferLength: data.len() as u32,
            ..Default::default()
        };
        *state
            .done
            .lock()
            .map_err(|e| format!("MIDI output lock poisoned: {e}"))? = false;
        let header_size = size_of::<MIDIHDR>() as u32;
        // SAFETY: `header` is a live MIDIHDR describing `data_buf`, which
        // outlives the whole call including the MOM_DONE wait below.
        let rc = unsafe { midiOutPrepareHeader(self.handle, &mut header, header_size) };
        if rc != MMSYSERR_NOERROR {
            return Err(format!("midiOutPrepareHeader failed with code {rc}"));
        }
        // SAFETY: `header` was just prepared and describes `data_buf`.
        let rc = unsafe { midiOutLongMsg(self.handle, &header, header_size) };
        if rc != MMSYSERR_NOERROR {
            // SAFETY: the driver never took ownership of a failed long msg.
            let _ = unsafe { midiOutUnprepareHeader(self.handle, &mut header, header_size) };
            return Err(format!("midiOutLongMsg failed with code {rc}"));
        }
        // Wait for the driver to post MOM_DONE so `data_buf` stays valid until
        // the driver is done with it; the timeout keeps a stuck driver from
        // hanging the audio thread.
        let mut done = state
            .done
            .lock()
            .map_err(|e| format!("MIDI output lock poisoned: {e}"))?;
        let (guard, _timeout_result) = state
            .done_cv
            .wait_timeout(done, LONG_MSG_TIMEOUT)
            .map_err(|e| format!("MIDI output lock poisoned: {e}"))?;
        done = guard;
        let completed = *done;
        drop(done);
        if !completed {
            // The driver is stuck; midiOutReset in `close` will reclaim the
            // buffer. Unprepare may fail with MMSYSERR_STILLPLAYING — the
            // header and buffer are leaked in that case, which is preferable
            // to touching memory the driver still owns.
            let _ = unsafe { midiOutUnprepareHeader(self.handle, &mut header, header_size) };
            return Err(format!(
                "Timed out after {LONG_MSG_TIMEOUT:?} waiting for midiOutLongMsg completion"
            ));
        }
        // SAFETY: MOM_DONE was posted, so the driver is done with `header`.
        let rc = unsafe { midiOutUnprepareHeader(self.handle, &mut header, header_size) };
        if rc != MMSYSERR_NOERROR {
            return Err(format!("midiOutUnprepareHeader failed with code {rc}"));
        }
        Ok(())
    }

    pub fn close(self) -> Result<(), String> {
        // Cancel any pending long message (posts MOM_DONE for it) and stop
        // the driver. SAFETY: handle is open and owned by us.
        let reset_rc = unsafe { midiOutReset(self.handle) };
        // Blocks until the driver callback is not running; afterwards WinMM
        // will never touch `state` again. SAFETY: handle is open, not called
        // from the callback.
        let close_rc = unsafe { midiOutClose(self.handle) };
        // SAFETY: exclusive reclaim of the instance reference handed to
        // midiOutOpen; WinMM finished using it before midiOutClose returned.
        unsafe {
            drop(Arc::from_raw(self.state));
        }
        match (reset_rc, close_rc) {
            (MMSYSERR_NOERROR, MMSYSERR_NOERROR) => Ok(()),
            (rc, MMSYSERR_NOERROR) => Err(format!("midiOutReset failed with code {rc}")),
            (_, rc) => Err(format!("midiOutClose failed with code {rc}")),
        }
    }
}

extern "system" fn input_callback(
    hmidi: HMIDIIN,
    msg: u32,
    instance: usize,
    param1: usize,
    _param2: usize,
) {
    // SAFETY: `instance` is the InputCallbackState box passed to midiInOpen;
    // it is valid from the open until midiInClose returns (close is never
    // called from this callback).
    let state = unsafe { &mut *(instance as *mut InputCallbackState) };
    match msg {
        MIM_DATA | MIM_MOREDATA => {
            // Short message packed little-endian in param1: status byte first.
            let packed = param1 as u32;
            let bytes = [
                (packed & 0xFF) as u8,
                ((packed >> 8) & 0xFF) as u8,
                ((packed >> 16) & 0xFF) as u8,
            ];
            let len = short_message_len(bytes[0]);
            (state.callback)(&bytes[..len]);
        }
        MIM_LONGDATA => {
            // SAFETY: param1 is a queued MIDIHDR owned by the connection; it
            // is lent to the driver until we re-queue it below.
            let header = unsafe { &mut *(param1 as *mut MIDIHDR) };
            if header.dwBytesRecorded > 0 {
                // SAFETY: the driver recorded dwBytesRecorded bytes into the
                // buffer owned by the connection.
                let data = unsafe {
                    slice::from_raw_parts(header.lpData.0, header.dwBytesRecorded as usize)
                };
                (state.callback)(data);
            }
            // Re-queue the buffer for the next sysex message.
            // SAFETY: the driver just returned `header` to us.
            let _ = unsafe { midiInAddBuffer(hmidi, header, size_of::<MIDIHDR>() as u32) };
        }
        MIM_ERROR => {
            // Invalid short message: nothing to re-queue.
        }
        MIM_LONGERROR => {
            // Invalid sysex; return the buffer to the driver queue.
            // SAFETY: param1 is a queued MIDIHDR lent by the connection.
            let header = unsafe { &mut *(param1 as *mut MIDIHDR) };
            let _ = unsafe { midiInAddBuffer(hmidi, header, size_of::<MIDIHDR>() as u32) };
        }
        _ => {}
    }
}

extern "system" fn output_callback(
    _hmidi: HMIDIOUT,
    msg: u32,
    instance: usize,
    _param1: usize,
    _param2: usize,
) {
    if msg != MOM_DONE {
        return;
    }
    // SAFETY: `instance` is the Arc reference passed to midiOutOpen; valid
    // until midiOutClose returns (close is never called from this callback).
    let state = unsafe { &*(instance as *const OutputCallbackState) };
    if let Ok(mut done) = state.done.lock() {
        *done = true;
        state.done_cv.notify_one();
    }
}

fn short_message_len(status: u8) -> usize {
    match status {
        // Program change and channel pressure carry one data byte.
        0xC0..=0xDF => 2,
        // MTC quarter frame and song select.
        0xF1 | 0xF3 => 2,
        // Song position pointer.
        0xF2 => 3,
        // Remaining system common / realtime messages are single-byte.
        0xF0..=0xFF => 1,
        // Everything else: status plus two data bytes.
        _ => 3,
    }
}

fn wchar_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
