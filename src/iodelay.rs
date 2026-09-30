//! Live loopback latency measurement over FreeBSD OSS mmap duplex, using
//! the MTDM algorithm in [`crate::mtdm`].
//!
//! Opens the input and output devices exclusively, starts them in one sync
//! group, pumps the multitone test signal through the playback ring, and
//! demodulates the looped-back capture, reporting the converged round trip
//! continuously. The latency is content-inferred: `CURRENT_IPTR/OPTR` only
//! position the mmap cursors, never enter the latency arithmetic (see
//! `ARCHITECTURE.md` at the repository root for why).
//!
//! The module is signal-agnostic: pass an [`IoDelayOptions::stop`] flag and
//! flip it from a SIGINT handler (or a GUI button) to end the run.

use crate::hw::oss::consts::{AFMT_S32_NE, PCM_CAP_MMAP, PCM_CAP_TRIGGER};
use crate::hw::oss::{ioctl, position::stream_frame};
use crate::mtdm::{self, Mtdm, make_report};

pub use crate::mtdm::{IoDelayReport, IoDelayStatus};
use nix::libc;
use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const REPORT_INTERVAL_NS: i64 = 250_000_000;
const DEFAULT_RATE: usize = 48_000;

/// Everything a run needs. Devices stay open for the lifetime of [`IoDelay`].
pub struct IoDelayOptions {
    pub input_device: String,
    /// 1-based channel index in the device's interleaved frames.
    pub input_channel: usize,
    pub output_device: String,
    /// 1-based channel index in the device's interleaved frames.
    pub output_channel: usize,
    pub sample_rate: usize,
    /// Extra gain applied to the captured signal before demodulation.
    pub gain: f32,
    /// Run duration in seconds; 0 runs until [`Self::stop`] is set.
    pub seconds: f64,
    /// Flip to request a clean stop; polled once per pump wakeup.
    pub stop: Arc<AtomicBool>,
}

impl IoDelayOptions {
    pub fn new(
        input_device: impl Into<String>,
        input_channel: usize,
        output_device: impl Into<String>,
        output_channel: usize,
    ) -> Self {
        Self {
            input_device: input_device.into(),
            input_channel,
            output_device: output_device.into(),
            output_channel,
            sample_rate: DEFAULT_RATE,
            gain: 1.0,
            seconds: 0.0,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct FdGuard(i32);

impl FdGuard {
    fn open(path: &str, flags: libc::c_int) -> Result<Self, String> {
        let c_path = CString::new(path).map_err(|_| format!("Invalid device path {path:?}"))?;
        let fd = unsafe { libc::open(c_path.as_ptr(), flags) };
        if fd < 0 {
            return Err(format!(
                "opening {path}: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Self(fd))
    }

    fn fd(&self) -> i32 {
        self.0
    }
}

impl Drop for FdGuard {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

struct MmapGuard {
    ptr: *mut libc::c_void,
    len: usize,
}

impl MmapGuard {
    fn map(fd: i32, len: usize, prot: libc::c_int) -> Result<Self, String> {
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, libc::MAP_SHARED, fd, 0) };
        if ptr == libc::MAP_FAILED {
            return Err(format!("mmap: {}", std::io::Error::last_os_error()));
        }
        Ok(Self { ptr, len })
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.cast()
    }

    fn as_ptr(&self) -> *const u8 {
        self.ptr.cast()
    }
}

impl Drop for MmapGuard {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr, self.len);
        }
    }
}

/// One mapped OSS stream plus its geometry.
struct MmapStream {
    fd: FdGuard,
    map: MmapGuard,
    buffer_bytes: usize,
    frame_size: usize,
    channels: usize,
    ring_frames: u64,
}

impl MmapStream {
    fn open(path: &str, input: bool, needed_channel: usize, rate: usize) -> Result<Self, String> {
        let flags = if input {
            libc::O_RDONLY | libc::O_EXCL | libc::O_NONBLOCK
        } else {
            libc::O_WRONLY | libc::O_EXCL | libc::O_NONBLOCK
        };
        let fd = FdGuard::open(path, flags)?;
        let caps = ioctl::get_caps(fd.fd()).map_err(|e| format!("{path}: GETCAPS: {e}"))?;
        if caps & PCM_CAP_TRIGGER == 0 {
            return Err(format!("{path}: device does not support triggering"));
        }
        if caps & PCM_CAP_MMAP == 0 {
            return Err(format!("{path}: device does not support mmap"));
        }
        ioctl::set_cooked(fd.fd(), false).map_err(|e| format!("{path}: COOKEDMODE: {e}"))?;
        let format =
            ioctl::set_format(fd.fd(), AFMT_S32_NE).map_err(|e| format!("{path}: SETFMT: {e}"))?;
        if format != AFMT_S32_NE {
            return Err(format!(
                "{path}: native 32-bit samples required, got format {format:#x}"
            ));
        }
        let channels = ioctl::set_channels(fd.fd(), needed_channel as i32)
            .map_err(|e| format!("{path}: CHANNELS: {e}"))? as usize;
        if channels < needed_channel {
            return Err(format!(
                "{path}: channel {needed_channel} requested but device has {channels}"
            ));
        }
        let actual_rate = ioctl::set_speed(fd.fd(), rate as i32)
            .map_err(|e| format!("{path}: SPEED: {e}"))? as usize;
        if actual_rate == 0 {
            return Err(format!("{path}: device reported a zero sample rate"));
        }
        let frame_size = 4 * channels;
        // Same policy as the standalone tool: two fragments sized to one
        // frame each; the driver expands this to its ring geometry.
        let frag_exp = u32::BITS - (frame_size as u32).leading_zeros() - 1;
        let frag_exp = if 1 << frag_exp == frame_size {
            frag_exp
        } else {
            frag_exp + 1
        };
        ioctl::set_fragment(fd.fd(), 2, frag_exp as i32)
            .map_err(|e| format!("{path}: SETFRAGMENT: {e}"))?;
        let mut info = if input {
            ioctl::input_buffer_info(fd.fd())
        } else {
            ioctl::output_buffer_info(fd.fd())
        }
        .map_err(|e| format!("{path}: buffer info: {e}"))?;
        if info.fragments < 1 {
            info.fragments = info.fragstotal;
        }
        if info.bytes < 1 {
            info.bytes = info.fragstotal * info.fragsize;
        }
        if info.bytes <= 0 || !(info.bytes as usize).is_multiple_of(frame_size) {
            return Err(format!("{path}: invalid mmap ring geometry"));
        }
        let buffer_bytes = info.bytes as usize;
        let ring_frames = (buffer_bytes / frame_size) as u64;
        if ring_frames < 4 {
            return Err(format!(
                "{path}: mmap ring too small ({ring_frames} frames)"
            ));
        }
        let prot = if input {
            libc::PROT_READ
        } else {
            libc::PROT_WRITE
        };
        let map = MmapGuard::map(fd.fd(), buffer_bytes, prot)?;
        Ok(Self {
            fd,
            map,
            buffer_bytes,
            frame_size,
            channels,
            ring_frames,
        })
    }

    /// Consumed (playback) or produced (capture) frame position.
    fn stream_frame(&self, input: bool) -> Result<i64, String> {
        let info = if input {
            ioctl::current_iptr(self.fd.fd())
        } else {
            ioctl::current_optr(self.fd.fd())
        }
        .map_err(|e| format!("stream position: {e}"))?;
        stream_frame(&info, input).map_err(|e| e.to_string())
    }
}

/// A prepared measurement: devices open, rings mapped, streams synced.
pub struct IoDelay {
    input: MmapStream,
    output: MmapStream,
    rate: usize,
    options: IoDelayOptions,
}

impl IoDelay {
    /// Opens both devices, negotiates geometry, maps the rings, and starts
    /// the sync group. Nothing is played or recorded yet.
    pub fn open(options: IoDelayOptions) -> Result<Self, String> {
        let needed_channels = options.input_channel.max(options.output_channel);
        let input = MmapStream::open(
            &options.input_device,
            true,
            needed_channels,
            options.sample_rate,
        )?;
        let output = MmapStream::open(
            &options.output_device,
            false,
            needed_channels,
            options.sample_rate,
        )?;
        if input.channels != output.channels
            || input.buffer_bytes != output.buffer_bytes
            || input.ring_frames != output.ring_frames
        {
            return Err(
                "input and output must have matching geometry (channels, ring, rate)".to_string(),
            );
        }
        let rate = options.sample_rate;
        let group = ioctl::add_to_sync_group(input.fd.fd(), 0, true);
        let group = ioctl::add_to_sync_group(output.fd.fd(), group, false);
        ioctl::start_sync_group(input.fd.fd(), group).map_err(|e| format!("SYNCSTART: {e}"))?;
        Ok(Self {
            input,
            output,
            rate,
            options,
        })
    }

    pub fn ring_frames(&self) -> u64 {
        self.input.ring_frames
    }

    pub fn sample_rate(&self) -> usize {
        self.rate
    }

    /// Run the pump loop until stopped or until `options.seconds` elapses.
    /// `on_report` fires about every 250 ms and once more with
    /// `final_report` set before returning; the return value is the last
    /// report.
    pub fn run(&mut self, mut on_report: impl FnMut(IoDelayReport)) -> IoDelayReport {
        let ctx = PumpContext {
            ring: self.input.ring_frames,
            frame_size: self.input.frame_size,
            input_channel: (self.options.input_channel - 1) * 4,
            output_channel: (self.options.output_channel - 1) * 4,
            gain: self.options.gain,
        };
        let mut mtdm = Mtdm::new(self.rate);
        // Clear the playback ring before writing tones into it.
        unsafe {
            libc::memset(
                self.output.map.as_mut_ptr().cast::<libc::c_void>(),
                0,
                self.output.buffer_bytes,
            );
        }
        let step = (ctx.ring / 4).clamp(1, 16);
        let step_ns = step as i64 * 1_000_000_000 / self.rate as i64;
        let deadline = if self.options.seconds > 0.0 {
            gettime_ns() + (self.options.seconds * 1e9) as i64
        } else {
            0
        };
        let mut next_wakeup = gettime_ns();
        let mut next_report = 0_i64;
        let mut input_prev = 0_i64;
        let mut output_prev = 0_i64;
        let mut scratch: Vec<f32> = Vec::with_capacity(step as usize + 1);
        loop {
            sleep_until_ns(next_wakeup);
            if self.options.stop.load(Ordering::Relaxed) {
                break;
            }
            let now = gettime_ns();
            let report_due = now >= next_report;
            if deadline != 0 && now >= deadline {
                break;
            }
            let wakeup = self.pump_once(&ctx, input_prev, output_prev, &mut scratch, &mut mtdm);
            let Some((in_head, out_head)) = wakeup else {
                break;
            };
            if report_due {
                next_report = now + REPORT_INTERVAL_NS;
                on_report(make_report(&mut mtdm, false));
            }
            input_prev = in_head;
            output_prev = out_head;
            next_wakeup += step_ns;
            if next_wakeup < gettime_ns() {
                next_wakeup = gettime_ns();
            }
        }
        let last = make_report(&mut mtdm, true);
        on_report(last);
        last
    }

    /// One pump wakeup: snapshot the cursors, fill the playback window,
    /// demodulate the capture window. Returns the new cursor positions, or
    /// `None` (after logging) when a device error should end the run.
    fn pump_once(
        &mut self,
        ctx: &PumpContext,
        input_prev: i64,
        output_prev: i64,
        scratch: &mut Vec<f32>,
        mtdm: &mut Mtdm,
    ) -> Option<(i64, i64)> {
        let ring = ctx.ring as i64;
        let in_head = match self.input.stream_frame(true) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("maolan-iodelay: input: {e}");
                return None;
            }
        };
        let out_head = match self.output.stream_frame(false) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("maolan-iodelay: output: {e}");
                return None;
            }
        };
        // Counters are ring-cursor positions only. A regressed or
        // full-ring-jumped counter makes this wakeup's ranges untrusted;
        // skip them. The index-derived MTDM phases need no resync.
        let missed = in_head < input_prev
            || out_head < output_prev
            || in_head - input_prev >= ring
            || out_head - output_prev >= ring;
        if missed {
            return Some((in_head, out_head));
        }
        fill_playback(
            self.output.map.as_mut_ptr(),
            ctx,
            output_prev + ring,
            out_head + ring,
        );
        scratch.clear();
        demod_capture(
            self.input.map.as_ptr(),
            ctx,
            input_prev,
            in_head,
            scratch,
            mtdm,
        );
        let mut overwrite = false;
        if let Ok(after) = self.input.stream_frame(true) {
            overwrite |= after < in_head || after - input_prev >= ring;
        }
        if let Ok(after) = self.output.stream_frame(false) {
            overwrite |= after < out_head || after - output_prev >= ring;
        }
        if overwrite {
            mtdm.reset_filters();
        }
        Some((in_head, out_head))
    }
}

/// Ring geometry and channel selection for one run.
struct PumpContext {
    ring: u64,
    frame_size: usize,
    input_channel: usize,
    output_channel: usize,
    gain: f32,
}

impl PumpContext {
    fn slot(&self, frame: i64) -> usize {
        frame.rem_euclid(self.ring as i64) as usize * self.frame_size
    }
}

/// Write the multitone into [first, end); a pure function of the absolute
/// output index, so rewound or rewritten ranges reproduce identical samples.
///
/// Safety: `buf` must be a live mmap of at least `ring * frame_size` bytes
/// with `frame_size == channels * 4`.
fn fill_playback(buf: *mut u8, ctx: &PumpContext, first: i64, end: i64) {
    for frame in first..end {
        let slot = unsafe { buf.add(ctx.slot(frame)) };
        let sample = (mtdm::synthesize_frame(frame as u64) * 2147483647.0).round() as i32;
        unsafe {
            libc::memset(slot.cast::<libc::c_void>(), 0, ctx.frame_size);
            std::ptr::write_unaligned(slot.add(ctx.output_channel) as *mut i32, sample);
        }
    }
}

/// Demodulate [first, end) of the capture ring into the MTDM accumulator.
///
/// Safety: `buf` must be a live read-only mmap of at least
/// `ring * frame_size` bytes.
fn demod_capture(
    buf: *const u8,
    ctx: &PumpContext,
    first: i64,
    end: i64,
    scratch: &mut Vec<f32>,
    mtdm: &mut Mtdm,
) {
    for frame in first..end {
        let slot = unsafe { buf.add(ctx.slot(frame)) };
        let raw = unsafe { (slot.add(ctx.input_channel) as *const i32).read_unaligned() };
        scratch.push(raw as f32 / 2147483648.0 * ctx.gain);
    }
    mtdm.process(scratch);
}

fn gettime_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec * 1_000_000_000 + ts.tv_nsec
}

fn sleep_until_ns(target: i64) {
    let ts = libc::timespec {
        tv_sec: target / 1_000_000_000,
        tv_nsec: target % 1_000_000_000,
    };
    let req = ts;
    loop {
        let rc = unsafe {
            libc::clock_nanosleep(
                libc::CLOCK_MONOTONIC,
                libc::TIMER_ABSTIME,
                &req,
                std::ptr::null_mut(),
            )
        };
        if rc != libc::EINTR {
            break;
        }
    }
}
