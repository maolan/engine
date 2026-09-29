use crate::audio::io::AudioIO;
use crate::hw::convert_policy;
use nix::libc;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use std::{
    fs::File,
    os::{
        fd::{AsRawFd, BorrowedFd},
        unix::fs::OpenOptionsExt,
    },
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub use super::midi_hub::MidiHub;

mod audio_core;
mod channel;
mod consts;
mod convert;
mod driver;
mod io_util;
mod ioctl;
mod sync;

pub use self::channel::OSSChannel;
pub use self::consts::*;
pub use self::driver::HwDriver;
pub use self::ioctl::{AudioInfo, BufferInfo, add_to_sync_group, start_sync_group};
pub use crate::hw::options::HwOptions;

use self::audio_core::DoubleBufferedChannel;
use self::convert::*;
use self::io_util::*;
use self::ioctl::*;
use self::sync::{DuplexSync, FrameClock, get_or_create_duplex_sync};

#[cfg(target_endian = "little")]
const AFMT_S16_FOREIGN: u32 = AFMT_S16_BE;
#[cfg(target_endian = "big")]
const AFMT_S16_FOREIGN: u32 = AFMT_S16_LE;
#[cfg(target_endian = "little")]
const AFMT_S24_FOREIGN: u32 = AFMT_S24_BE;
#[cfg(target_endian = "big")]
const AFMT_S24_FOREIGN: u32 = AFMT_S24_LE;
#[cfg(target_endian = "little")]
const AFMT_S32_FOREIGN: u32 = AFMT_S32_BE;
#[cfg(target_endian = "big")]
const AFMT_S32_FOREIGN: u32 = AFMT_S32_LE;

pub struct Audio {
    dsp: File,
    pub channels: Vec<Arc<AudioIO>>,
    pub input: bool,
    pub output_gain_linear: f32,
    pub output_balance: f32,
    pub rate: i32,
    pub format: u32,
    pub chsamples: usize,
    buffer: Vec<i32>,
    f32_buffer: Vec<f32>,
    pub buffer_info: BufferInfo,
    frame_size_bytes: usize,
    fragment_bytes: usize,
    buffer_frames_cached: i64,
    caps: i32,
    mapped: bool,
    map: *mut libc::c_void,
    zero_block: Vec<u8>,
    map_progress_bytes: usize,
    last_published_balance: i64,
    frame_clock: FrameClock,
    frame_stamp: i64,
    duplex_sync: Arc<std::sync::Mutex<DuplexSync>>,
    channel: DoubleBufferedChannel,
    last_underrun_count: i32,
    last_overrun_count: i32,
    xrun_count: u64,
    playing: Arc<AtomicBool>,
    was_playing_last_cycle: bool,
    stop_fade_remaining_frames: usize,
    stop_fade_total_frames: usize,
    /// Current render plan; when set, the RT cycle reads/writes plan arena
    /// buffers instead of the legacy port buffers.
    plan_slot: Option<Arc<crate::render_plan::PlanSlot>>,
    /// RT-inline render context; when set, the duplex cycle executes the
    /// render plan on the cycle thread between the capture fill and the
    /// playback drain.
    inline_render: Option<Arc<crate::inline_render::InlineRender>>,
    /// Phase 4 back-pressure: the cycle thread's inline render was stale and
    /// this playback drain must emit silence instead of the arena. Set and
    /// consumed on the cycle thread only.
    stale_silence_once: bool,
}

// Manual impl: `basedrop::Owned` (inside `PlanSlot`) has no `Debug` impl.
impl std::fmt::Debug for Audio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Audio")
            .field("channels", &self.channels)
            .field("input", &self.input)
            .field("rate", &self.rate)
            .field("format", &self.format)
            .field("chsamples", &self.chsamples)
            .field("frame_stamp", &self.frame_stamp)
            .finish_non_exhaustive()
    }
}

impl Audio {
    fn sample_format_candidates(bits: i32) -> Vec<u32> {
        fn add_pair(candidates: &mut Vec<u32>, native: u32, foreign: u32) {
            candidates.push(native);
            candidates.push(foreign);
        }

        let mut candidates = Vec::with_capacity(7);
        match bits {
            32 => {
                add_pair(&mut candidates, AFMT_S32_NE, AFMT_S32_FOREIGN);
                add_pair(&mut candidates, AFMT_S24_NE, AFMT_S24_FOREIGN);
                add_pair(&mut candidates, AFMT_S16_NE, AFMT_S16_FOREIGN);
                candidates.push(AFMT_S8);
            }
            24 => {
                add_pair(&mut candidates, AFMT_S24_NE, AFMT_S24_FOREIGN);
                add_pair(&mut candidates, AFMT_S16_NE, AFMT_S16_FOREIGN);
                candidates.push(AFMT_S8);
            }
            16 => {
                add_pair(&mut candidates, AFMT_S16_NE, AFMT_S16_FOREIGN);
                candidates.push(AFMT_S8);
            }
            8 => candidates.push(AFMT_S8),
            _ => {
                add_pair(&mut candidates, AFMT_S16_NE, AFMT_S16_FOREIGN);
                candidates.push(AFMT_S8);
            }
        }
        candidates
    }

    fn negotiate_sample_format(fd: i32, bits: i32) -> Result<u32, std::io::Error> {
        let candidates = Self::sample_format_candidates(bits);
        let mut last_errno = None;
        let mut last_unsupported = None;
        for candidate in candidates {
            let mut negotiated = candidate;
            let setfmt = unsafe { oss_set_format(fd, &mut negotiated) };
            match setfmt {
                Ok(_) => {
                    if supported_sample_format(negotiated) {
                        return Ok(negotiated);
                    }
                    last_unsupported = Some(negotiated);
                }
                Err(_) => {
                    last_errno = Some(std::io::Error::last_os_error());
                }
            }
        }
        if let Some(format) = last_unsupported {
            return Err(std::io::Error::other(format!(
                "Unsupported OSS sample format after setfmt fallback chain: {format:#x}"
            )));
        }
        Err(last_errno
            .unwrap_or_else(|| std::io::Error::other("OSS setfmt failed for all fallback formats")))
    }

    fn min_fragment_bytes(frame_size: usize) -> Result<usize, std::io::Error> {
        const MAX_FRAGMENT_BYTES: usize = 1 << 16;
        if frame_size == 0 {
            return Err(std::io::Error::other("OSS frame size is invalid"));
        }
        if frame_size > MAX_FRAGMENT_BYTES {
            return Err(std::io::Error::other(format!(
                "OSS frame size {frame_size} exceeds maximum fragment size {MAX_FRAGMENT_BYTES}"
            )));
        }
        Ok(frame_size.next_power_of_two().min(MAX_FRAGMENT_BYTES))
    }

    fn request_fragment_layout(
        fd: i32,
        frame_size: usize,
        period_frames: usize,
    ) -> std::io::Result<usize> {
        let fragment_bytes = Self::min_fragment_bytes(frame_size)?;
        let period_bytes =
            Self::period_bytes_for_fragment_grid(frame_size, period_frames, fragment_bytes)?;
        let fragments = period_bytes.div_ceil(fragment_bytes).max(1);
        let exponent = fragment_bytes.trailing_zeros();
        let arg_bits = ((fragments.min(0xffff) as u32) << 16) | exponent;
        let mut arg = arg_bits as i32;
        unsafe { oss_set_fragment(fd, &mut arg) }?;
        Ok(fragment_bytes)
    }

    fn period_bytes_for_fragment_grid(
        frame_size: usize,
        period_frames: usize,
        min_bytes: usize,
    ) -> std::io::Result<usize> {
        let requested_bytes = period_frames
            .max(1)
            .checked_mul(frame_size)
            .ok_or_else(|| std::io::Error::other("OSS period byte size overflow"))?;
        let min_bytes = min_bytes.max(frame_size).max(1);
        requested_bytes
            .max(min_bytes)
            .checked_next_power_of_two()
            .ok_or_else(|| std::io::Error::other("OSS period byte size overflow"))
    }

    fn period_frames_for_fragment_grid(
        frame_size: usize,
        period_frames: usize,
        min_bytes: usize,
    ) -> std::io::Result<usize> {
        let bytes = Self::period_bytes_for_fragment_grid(frame_size, period_frames, min_bytes)?;
        Ok(bytes.div_ceil(frame_size).max(1))
    }

    pub fn fd(&self) -> i32 {
        self.dsp.as_raw_fd()
    }

    pub fn start_trigger(&self) -> std::io::Result<()> {
        let trig: i32 = if self.input {
            PCM_ENABLE_INPUT
        } else {
            PCM_ENABLE_OUTPUT
        };
        self.set_trigger_mask(trig)
    }

    pub fn stop_trigger(&self) -> std::io::Result<()> {
        self.set_trigger_mask(0)
    }

    /// Set the full SNDCTL_DSP_TRIGGER mask for this fd. On a device whose
    /// capture/playback fds are joined in a sync group, clearing only
    /// `PCM_ENABLE_OUTPUT` on the playback fd halts playback DMA while
    /// capture keeps running (verified on FreeBSD OSS); a later mask with
    /// `PCM_ENABLE_OUTPUT` set restarts playback from where it stopped.
    pub fn set_trigger_mask(&self, mask: i32) -> std::io::Result<()> {
        if (self.caps & PCM_CAP_TRIGGER) == 0 {
            return Ok(());
        }
        let trig: i32 = mask;
        unsafe { oss_set_trigger(self.dsp.as_raw_fd(), &trig) }
            .map(|_| ())
            .map_err(|_| std::io::Error::last_os_error())
    }

    pub fn halt(&self) -> std::io::Result<()> {
        unsafe { oss_halt(self.dsp.as_raw_fd()) }
            .map(|_| ())
            .map_err(|_| std::io::Error::last_os_error())
    }

    /// Halt the device and explicitly close the fd so the kernel
    /// cannot drain pending buffers during process exit.
    pub fn close_fd(&mut self) {
        let _ = self.halt();
        if let Ok(devnull) = File::open("/dev/null") {
            drop(std::mem::replace(&mut self.dsp, devnull));
        }
    }

    pub fn new(
        path: &str,
        sync_key: &str,
        rate: i32,
        bits: i32,
        input: bool,
        options: HwOptions,
        playing: Arc<AtomicBool>,
    ) -> Result<Audio, std::io::Error> {
        let mut binding = File::options();

        let mut flags = libc::O_NONBLOCK;
        if input {
            flags |= libc::O_RDONLY;
            if options.exclusive {
                flags |= libc::O_EXCL;
            }
            binding.read(true).write(false).custom_flags(flags);
        } else {
            flags |= libc::O_WRONLY;
            if options.exclusive {
                flags |= libc::O_EXCL;
            }
            binding.read(false).write(true).custom_flags(flags);
        }

        let dsp = binding.open(path)?;

        let cooked = 0_i32;
        unsafe {
            let _ = oss_set_cooked(dsp.as_raw_fd(), &cooked);
        }

        let mut audio_info = AudioInfo::new();
        unsafe {
            oss_get_info(dsp.as_raw_fd(), &mut audio_info)
                .map_err(|_| std::io::Error::last_os_error())?;
        }
        let mut channels = if audio_info.max_channels > 0 {
            audio_info.max_channels
        } else {
            2_i32
        };
        let mut effective_rate = rate;
        let format = Self::negotiate_sample_format(dsp.as_raw_fd(), bits)?;
        unsafe {
            oss_set_channels(dsp.as_raw_fd(), &mut channels)
                .map_err(|_| std::io::Error::last_os_error())?;
            oss_set_speed(dsp.as_raw_fd(), &mut effective_rate)
                .map_err(|_| std::io::Error::last_os_error())?;
        }
        if effective_rate != rate {
            return Err(std::io::Error::other(format!(
                "OSS device forced sample rate {effective_rate} (requested {rate})"
            )));
        }

        let bytes_per_sample = bytes_per_sample(format)
            .ok_or_else(|| std::io::Error::other(format!("Unsupported format: {format:#x}")))?;
        let frame_size = (channels as usize) * bytes_per_sample;

        let _requested_fragment_bytes =
            Self::request_fragment_layout(dsp.as_raw_fd(), frame_size, options.period_frames)?;

        let mut buffer_info = BufferInfo::new();
        unsafe {
            if input {
                oss_input_buffer_info(dsp.as_raw_fd(), &mut buffer_info)
                    .map_err(|_| std::io::Error::last_os_error())?;
            } else {
                oss_output_buffer_info(dsp.as_raw_fd(), &mut buffer_info)
                    .map_err(|_| std::io::Error::last_os_error())?;
            }
        }

        if buffer_info.fragments < 1 {
            buffer_info.fragments = buffer_info.fragstotal;
        }
        if buffer_info.bytes < 1 {
            buffer_info.bytes = buffer_info.fragstotal * buffer_info.fragsize;
        }
        if buffer_info.bytes < 1 {
            return Err(std::io::Error::other("OSS buffer size is invalid"));
        }

        let mut caps = 0_i32;
        unsafe {
            oss_get_caps(dsp.as_raw_fd(), &mut caps)
                .map_err(|_| std::io::Error::last_os_error())?;
        }
        let mut sys = OssSysInfo::default();
        unsafe {
            oss_get_sysinfo(dsp.as_raw_fd(), &mut sys)
                .map_err(|_| std::io::Error::last_os_error())?;
        }
        if (caps & PCM_CAP_MMAP) != 0 {
            let ver = cstr_fixed_prefix(&sys.version);
            if ver.len() >= 7 && ver.as_bytes()[..7].cmp(b"1302000") == std::cmp::Ordering::Less {
                caps &= !PCM_CAP_MMAP;
            }
        }

        let chsamples = Self::period_frames_for_fragment_grid(
            frame_size,
            options.period_frames,
            buffer_info.fragsize.max(1) as usize,
        )?;

        let buffer_bytes = chsamples * frame_size;
        let channel = if input {
            DoubleBufferedChannel::new_read(buffer_bytes, chsamples as i64)
        } else {
            DoubleBufferedChannel::new_write(buffer_bytes, chsamples as i64)
        };

        let mut map = std::ptr::null_mut();
        let mut mapped = false;
        if (caps & PCM_CAP_MMAP) != 0 {
            let prot = if input {
                libc::PROT_READ
            } else {
                libc::PROT_WRITE
            };
            let addr = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    buffer_info.bytes as usize,
                    prot,
                    libc::MAP_SHARED,
                    dsp.as_raw_fd(),
                    0,
                )
            };
            if addr != libc::MAP_FAILED {
                map = addr;
                mapped = true;
            }
        }

        let mut io_channels = Vec::with_capacity(channels as usize);
        for _ in 0..channels {
            io_channels.push(Arc::new(AudioIO::new(chsamples)));
        }

        let duplex_sync = get_or_create_duplex_sync(sync_key, effective_rate, chsamples);
        let mut frame_clock = FrameClock::default();
        frame_clock.set_sample_rate(effective_rate as u32);
        {
            let mut sync = duplex_sync.lock().expect("duplex sync poisoned");
            if let Some(zero) = sync.clock_zero {
                frame_clock.zero = zero;
            } else {
                let _ = frame_clock.init_clock(effective_rate as u32);
                sync.clock_zero = Some(frame_clock.zero);
            }
        }

        let buffer_frames_cached = (buffer_info.bytes as usize / frame_size) as i64;
        let fragment_bytes = buffer_info.fragsize.max(1) as usize;

        let mut initial_audio = Audio {
            dsp,
            channels: io_channels,
            input,
            output_gain_linear: 1.0,
            output_balance: 0.0,
            rate: effective_rate,
            format,
            chsamples,
            buffer: vec![0_i32; chsamples * (channels as usize)],
            f32_buffer: Vec::new(),
            buffer_info,
            frame_size_bytes: frame_size,
            fragment_bytes,
            buffer_frames_cached,
            caps,
            mapped,
            map,
            zero_block: Vec::new(),
            map_progress_bytes: 0,
            last_published_balance: i64::MIN,
            frame_clock,
            frame_stamp: 0,
            duplex_sync,
            channel,
            last_underrun_count: 0,
            last_overrun_count: 0,
            xrun_count: 0,
            playing,
            was_playing_last_cycle: false,
            stop_fade_remaining_frames: 0,
            stop_fade_total_frames: 0,
            plan_slot: None,
            inline_render: None,
            stale_silence_once: false,
        };

        initial_audio.last_underrun_count = initial_audio.get_play_underruns();
        initial_audio.last_overrun_count = initial_audio.get_rec_overruns();

        Ok(initial_audio)
    }

    pub fn is_mapped(&self) -> bool {
        self.mapped
    }

    pub fn set_plan_slot(&mut self, slot: Arc<crate::render_plan::PlanSlot>) {
        self.plan_slot = Some(slot);
    }

    pub fn set_inline_render(&mut self, ctx: Option<Arc<crate::inline_render::InlineRender>>) {
        self.inline_render = ctx;
    }

    /// Phase 4 back-pressure: the next `fill_output_buffer` writes silence
    /// instead of draining the arena (a stale inline render was silenced).
    pub fn write_silence_once(&mut self) {
        self.stale_silence_once = true;
    }

    fn frame_size(&self) -> usize {
        self.frame_size_bytes
    }

    fn buffer_frames(&self) -> i64 {
        self.buffer_frames_cached
    }

    fn io_chunk_bytes(&self) -> usize {
        let aligned = self.fragment_bytes - (self.fragment_bytes % self.frame_size());
        aligned.max(self.frame_size())
    }

    fn stepping(&self) -> i64 {
        self.frame_clock.stepping()
    }

    fn map_pointer(&self) -> usize {
        if self.buffer_info.bytes <= 0 {
            return 0;
        }
        self.map_progress_bytes % (self.buffer_info.bytes as usize)
    }

    fn shared_cycle_end_add(&self, delta: i64) -> i64 {
        let mut sync = self.duplex_sync.lock().expect("duplex sync poisoned");
        sync.cycle_end += delta;
        sync.cycle_end
    }

    fn shared_cycle_end_get(&self) -> i64 {
        self.duplex_sync
            .lock()
            .expect("duplex sync poisoned")
            .cycle_end
    }

    fn publish_balance(&mut self, balance: i64) {
        if self.last_published_balance == balance {
            return;
        }
        self.last_published_balance = balance;
        let mut sync = self.duplex_sync.lock().expect("duplex sync poisoned");
        if self.input {
            sync.capture_balance = Some(balance);
        } else {
            sync.playback_balance = Some(balance);
        }
    }

    fn playback_correction(&self) -> i64 {
        if self.input {
            return 0;
        }
        let mut sync = self.duplex_sync.lock().expect("duplex sync poisoned");
        match (sync.playback_balance, sync.capture_balance) {
            (Some(play), Some(capture)) => sync.correction.correct(play, capture),
            _ => 0,
        }
    }

    fn update_map_progress_from_count(&mut self, info: &CountInfo) -> Option<usize> {
        if self.buffer_info.bytes <= 0
            || self.buffer_info.fragsize <= 0
            || info.ptr < 0
            || info.blocks < 0
            || (info.ptr as usize) >= self.buffer_info.bytes as usize
            || !(info.ptr as usize).is_multiple_of(self.frame_size())
        {
            return None;
        }
        let buf_bytes = self.buffer_info.bytes as usize;
        let frag_bytes = self.buffer_info.fragsize as usize;
        let ptr = info.ptr as usize;
        let mut delta = (ptr + buf_bytes - self.map_pointer()) % buf_bytes;
        let max_bytes = ((info.blocks as usize).saturating_add(1))
            .saturating_mul(frag_bytes)
            .saturating_sub(1);
        if max_bytes >= delta {
            let mut cycles = max_bytes - delta;
            cycles -= cycles % buf_bytes;
            delta += cycles;
        }
        self.map_progress_bytes = self.map_progress_bytes.saturating_add(delta);
        Some(delta)
    }

    fn read_io(&self, dst: &mut [u8], len: usize, count: &mut usize) -> std::io::Result<()> {
        read_nonblock(self.dsp.as_raw_fd(), dst, len, self.io_chunk_bytes(), count)
    }

    fn write_io(&self, src: &mut [u8], len: usize, count: &mut usize) -> std::io::Result<()> {
        write_nonblock(self.dsp.as_raw_fd(), src, len, self.io_chunk_bytes(), count)
    }

    fn read_map(&self, dst: &mut [u8], offset: usize, length: usize) -> usize {
        let total = self.buffer_info.bytes.max(0) as usize;
        map_read(self.map, self.mapped, total, dst, offset, length)
    }

    fn write_map(&self, src: Option<&mut [u8]>, offset: usize, length: usize) -> usize {
        let total = self.buffer_info.bytes.max(0) as usize;
        map_write(self.map, self.mapped, total, src, offset, length)
    }

    fn queued_samples(&self) -> i32 {
        let mut ptr = OssCount::default();
        let req = if self.input {
            unsafe { oss_current_iptr(self.dsp.as_raw_fd(), &mut ptr) }
        } else {
            unsafe { oss_current_optr(self.dsp.as_raw_fd(), &mut ptr) }
        };
        if req.is_ok() { ptr.fifo_samples } else { 0 }
    }

    fn get_play_underruns(&self) -> i32 {
        let mut err = AudioErrInfo::default();
        let rc = unsafe { oss_get_error(self.dsp.as_raw_fd(), &mut err) };
        if rc.is_ok() { err.play_underruns } else { 0 }
    }

    fn get_rec_overruns(&self) -> i32 {
        let mut err = AudioErrInfo::default();
        let rc = unsafe { oss_get_error(self.dsp.as_raw_fd(), &mut err) };
        if rc.is_ok() { err.rec_overruns } else { 0 }
    }

    pub(super) fn detect_xrun_enhanced(&mut self) -> i64 {
        let current_underruns = if self.input {
            0
        } else {
            self.get_play_underruns()
        };
        let current_overruns = if self.input {
            self.get_rec_overruns()
        } else {
            0
        };

        if (self.last_underrun_count >= 0 || self.last_overrun_count >= 0)
            && (current_underruns > self.last_underrun_count
                || current_overruns > self.last_overrun_count)
        {
            self.xrun_count += 1;
            self.last_underrun_count = current_underruns;
            self.last_overrun_count = current_overruns;

            return self.chsamples as i64;
        }

        self.last_underrun_count = current_underruns;
        self.last_overrun_count = current_overruns;

        0
    }

    /// Current capture frame position reported by the OSS driver via
    /// `SNDCTL_DSP_GETIPTR`, expressed in frames since the input stream
    /// started. Returns `None` on ioctl failure or for playback devices.
    pub fn current_capture_frame(&self) -> Option<i64> {
        if !self.input {
            return None;
        }
        let mut info = CountInfo::default();
        let rc = unsafe { oss_get_iptr(self.dsp.as_raw_fd(), &mut info) };
        if rc.is_err() {
            return None;
        }
        let frame_size = self.frame_size().max(1);
        Some((info.bytes.max(0) as i64) / (frame_size as i64))
    }

    /// End of the capture window actually consumed, in the same cumulative
    /// frame base as [`Audio::current_capture_frame`]. Unlike the raw
    /// GETIPTR head this accounts for the ring backlog: it names the data
    /// the current cycle read. Used as the Phase 4 staleness anchor.
    pub fn current_read_frame(&self) -> Option<i64> {
        if !self.input {
            return None;
        }
        self.channel.read_data_end_frame()
    }

    pub fn frame_size_bytes(&self) -> usize {
        self.frame_size_bytes
    }

    pub fn sample_bits(&self) -> i32 {
        bytes_per_sample(self.format)
            .map(|bytes| (bytes * 8) as i32)
            .unwrap_or(0)
    }

    /// Wait until the OSS fd is readable (`writable == false`) or writable
    /// (`writable == true`), bailing out early if a stop is requested. A short
    /// poll timeout is used so `request_stop()` is honored even when the device
    /// never becomes ready.
    fn wait_for_fd(&self, writable: bool, stop_requested: &AtomicBool) -> std::io::Result<()> {
        let flags = if writable {
            PollFlags::POLLOUT
        } else {
            PollFlags::POLLIN
        };
        let fd = unsafe { BorrowedFd::borrow_raw(self.fd()) };
        let mut pollfd = [PollFd::new(fd, flags)];
        loop {
            if stop_requested.load(Ordering::Acquire) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "OSS wait stopped",
                ));
            }
            match poll(&mut pollfd, PollTimeout::from(100u16)) {
                Ok(0) => continue,
                Ok(_) => {
                    let revents = pollfd[0].revents().unwrap_or(PollFlags::empty());
                    if revents.contains(flags)
                        || revents.contains(PollFlags::POLLERR)
                        || revents.contains(PollFlags::POLLHUP)
                    {
                        return Ok(());
                    }
                    continue;
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(nix::errno::Errno::EAGAIN) => continue,
                Err(e) => return Err(std::io::Error::other(format!("poll failed: {e}"))),
            }
        }
    }

    fn read_full_period(&self, buf: &mut [u8], stop_requested: &AtomicBool) -> std::io::Result<()> {
        let mut offset = 0;
        while offset < buf.len() {
            self.wait_for_fd(false, stop_requested)?;
            let n = unsafe {
                libc::read(
                    self.fd(),
                    buf[offset..].as_ptr() as *mut libc::c_void,
                    buf.len() - offset,
                )
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                return Err(e);
            }
            let n = n as usize;
            if n == 0 {
                continue;
            }
            offset += n;
        }
        Ok(())
    }

    fn write_full_period(&self, buf: &[u8], stop_requested: &AtomicBool) -> std::io::Result<()> {
        let mut offset = 0;
        while offset < buf.len() {
            self.wait_for_fd(true, stop_requested)?;
            let n = unsafe {
                libc::write(
                    self.fd(),
                    buf[offset..].as_ptr() as *const libc::c_void,
                    buf.len() - offset,
                )
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                return Err(e);
            }
            let n = n as usize;
            if n == 0 {
                continue;
            }
            offset += n;
        }
        Ok(())
    }

    fn fill_input_ports(&mut self) {
        let num_channels = self.channels.len();
        let norm_factor = convert_policy::F32_FROM_I32_MAX;
        let total_samples = self.chsamples * num_channels;
        self.f32_buffer.resize(total_samples, 0.0);
        crate::simd::convert_i32_to_f32(
            &self.buffer[..total_samples],
            &mut self.f32_buffer,
            norm_factor,
        );
        if let Some(slot) = &self.plan_slot {
            let plan = slot.load();
            crate::hw::ports::fill_arena_from_interleaved(
                &plan,
                self.chsamples,
                &self.f32_buffer,
                num_channels,
            );
            crate::cycle_trace::mark(crate::cycle_trace::TracePoint::CaptureReadDone);
        } else {
            let all_connected = self
                .channels
                .iter()
                .all(crate::hw::ports::has_audio_connections);
            crate::hw::ports::fill_ports_from_interleaved_buffer(
                &self.channels,
                self.chsamples,
                !all_connected,
                &self.f32_buffer,
                num_channels,
            );
        }
    }

    fn fill_output_buffer(&mut self) {
        if self.stale_silence_once {
            // Phase 4 back-pressure: the inline render for this cycle was
            // stale (the device moved a full period past the tagged
            // transport). Emit silence; the transport resyncs in the
            // dispatcher and the ring resyncs via the xrun jump machinery.
            self.stale_silence_once = false;
            self.buffer.as_mut_slice().fill(0);
            return;
        }
        let num_channels = self.channels.len();
        let playing = self.playing.load(Ordering::Relaxed);
        if self.was_playing_last_cycle && !playing {
            let fade_frames = self.chsamples.max(128);
            self.stop_fade_remaining_frames = fade_frames;
            self.stop_fade_total_frames = fade_frames;
        }
        self.was_playing_last_cycle = playing;
        let data_i32 = self.buffer.as_mut_slice();
        if !playing && self.stop_fade_remaining_frames == 0 {
            data_i32.fill(0);
        } else {
            let scale_factor = convert_policy::F32_TO_I32_MAX;
            let output_gain = self.output_gain_linear;
            let all_connected = self
                .channels
                .iter()
                .all(crate::hw::ports::has_audio_connections);
            if !all_connected {
                data_i32.fill(0);
            }
            let fade_remaining = self.stop_fade_remaining_frames;
            let fade_total = self.stop_fade_total_frames.max(1);
            let mut write_sample = |ch_idx: usize, frame: usize, sample: f32| {
                let target_idx = frame * num_channels + ch_idx;
                let fade_gain = if !playing && fade_remaining > 0 {
                    let progressed = self.chsamples.saturating_sub(fade_remaining) + frame;
                    (1.0 - (progressed as f32 / fade_total as f32)).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                data_i32[target_idx] = (sample.clamp(-1.0, 1.0) * fade_gain * scale_factor) as i32;
            };
            if let Some(slot) = &self.plan_slot {
                let plan = slot.load();
                crate::hw::ports::write_interleaved_from_arena(
                    &plan,
                    self.chsamples,
                    output_gain,
                    self.output_balance,
                    &mut write_sample,
                );
                crate::cycle_trace::mark(crate::cycle_trace::TracePoint::PlaybackWriteDone);
            } else {
                crate::hw::ports::write_interleaved_from_ports(
                    &self.channels,
                    self.chsamples,
                    output_gain,
                    self.output_balance,
                    !all_connected,
                    write_sample,
                );
            }
            if !playing && self.stop_fade_remaining_frames > 0 {
                self.stop_fade_remaining_frames = self
                    .stop_fade_remaining_frames
                    .saturating_sub(self.chsamples);
            }
        }
    }

    /// Port/arena fill used by the mmap duplex path, where raw device I/O is
    /// performed by the double-buffered channel machinery instead of
    /// `read_full_period`/`write_full_period`.
    pub fn process_ports(&mut self) {
        if self.input {
            self.fill_input_ports();
        } else {
            self.fill_output_buffer();
        }
    }

    pub fn process(&mut self, stop_requested: &AtomicBool) -> std::io::Result<()> {
        if self.input {
            let period_bytes = self.chsamples * self.frame_size();
            let mut raw = vec![0_u8; period_bytes];
            self.read_full_period(&mut raw, stop_requested)?;
            convert_in_to_i32_connected(
                self.format,
                self.chsamples,
                &raw,
                self.buffer.as_mut_slice(),
                &self.channels,
            );
            self.fill_input_ports();
        } else {
            self.fill_output_buffer();
            let period_bytes = self.chsamples * self.frame_size();
            let mut raw = vec![0_u8; period_bytes];
            convert_out_from_i32_interleaved(
                self.format,
                self.channels.len(),
                self.chsamples,
                self.buffer.as_mut_slice(),
                raw.as_mut_slice(),
            );
            self.write_full_period(&raw, stop_requested)?;
        }

        Ok(())
    }

    /// Write the persistent zero buffer (sized to the mapped ring, filled
    /// once at first use and never written again) into the whole ring, and
    /// zero the userspace interleaved buffer. On stop the zeros drain as
    /// silence; with no cycles running while stopped, nothing overwrites
    /// them. No trigger ioctls are involved — playback DMA keeps running.
    pub fn zero_fill_hw_buffers(&mut self) {
        if self.input {
            return;
        }
        self.buffer.fill(0);
        let total = self.buffer_info.bytes.max(0) as usize;
        if self.mapped && !self.map.is_null() && total > 0 {
            if self.zero_block.len() != total {
                self.zero_block = vec![0_u8; total];
            }
            let block = std::mem::take(&mut self.zero_block);
            let mut written = 0;
            while written < total {
                let len = (64 * 1024).min(total - written);
                let chunk = &mut block[written..written + len].to_vec();
                let n = self.write_map(Some(chunk), written, len);
                if n == 0 {
                    break;
                }
                written += n;
            }
            self.zero_block = block;
        }
        self.channel
            .reset_buffers(self.frame_stamp, self.frame_size().max(1));
    }

    pub fn force_silence_now(&mut self) {
        if self.input {
            return;
        }
        self.buffer.fill(0);
        if let Some(slot) = &self.plan_slot {
            let plan = slot.load();
            crate::hw::ports::clear_hw_arena_buffers(&plan);
        }
        self.stop_fade_remaining_frames = 0;
        self.stop_fade_total_frames = 0;
        self.was_playing_last_cycle = false;
        self.channel
            .reset_buffers(self.frame_stamp, self.frame_size().max(1));
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        // Reset the OSS channel to discard pending buffers. Without this,
        // the kernel's dsp_close() calls chn_flush() on playback channels
        // which drains remaining audio data — sleeping up to CHN_TIMEOUT
        // (5 seconds by default) before actually closing the fd.
        let _ = self.halt();
        if self.mapped && !self.map.is_null() && self.buffer_info.bytes > 0 {
            unsafe {
                let _ = libc::munmap(self.map, self.buffer_info.bytes as usize);
            }
            self.map = std::ptr::null_mut();
        }
    }
}

#[cfg(all(test, target_os = "freebsd"))]
mod tests {

    use super::driver::HwDriver;
    use super::ioctl::{CountInfo, oss_get_iptr, oss_get_optr};
    use crate::hw::traits::HwWorkerDriver;

    fn ptr_counts(driver: &HwDriver) -> (i64, i64) {
        let mut iptr = CountInfo::default();
        let mut optr = CountInfo::default();
        unsafe {
            let _ = oss_get_iptr(driver.input_fd(), &mut iptr);
            let _ = oss_get_optr(driver.output_fd(), &mut optr);
        }
        (iptr.bytes as i64, optr.bytes as i64)
    }

    /// Full transport sequence on a real device (default /dev/dsp5, override
    /// with OSS_TEST_DEVICE; skips when unavailable): play (cycles flow,
    /// capture and playback DMA run) -> stop (NEW INVARIANT: playback DMA
    /// keeps running — no trigger ioctls on stop — and the zero-fill writes
    /// silence into the ring; zeros drain as silence and, with no cycles
    /// requested while stopped, nothing overwrites them) -> play again
    /// (cycles return to block rate immediately — no halt/resync stall) ->
    /// stop again. Readable proxies (the hardware refuses a ring read-back
    /// mapping): DMA advancing during stop, and post-resume cycles at block
    /// rate with no underrun accumulation.
    #[test]
    fn play_stop_play_stop_keeps_dma_healthy() {
        let device = std::env::var("OSS_TEST_DEVICE").unwrap_or_else(|_| "/dev/dsp5".to_string());
        let Ok(mut driver) = HwDriver::new(&device, 48_000, 32) else {
            eprintln!("OSS test device {device} unavailable; skipping");
            return;
        };
        eprintln!("start_duplex_sync={}", driver.start_duplex_sync());
        let nap = std::time::Duration::from_millis(250);

        let run_cycles = |driver: &mut HwDriver, n: usize, what: &str| {
            for i in 0..n {
                driver
                    .run_cycle_for_worker()
                    .unwrap_or_else(|e| panic!("{what} cycle {i}: {e}"));
            }
        };

        // Play.
        driver.set_playing(true);
        run_cycles(&mut driver, 10, "play");
        let (i0, o0) = ptr_counts(&driver);
        std::thread::sleep(nap);
        let (i1, o1) = ptr_counts(&driver);
        assert!(i1 > i0, "play: capture DMA not advancing");
        assert!(o1 > o0, "play: playback DMA not advancing");

        // Stop: no trigger ioctls — both DMA directions keep running, the
        // ring is kept silent by the zero-fill (zeros drain as silence; no
        // cycles run while stopped, so nothing overwrites them).
        driver.set_playing(false);
        driver.zero_fill_hw_buffers();
        let (i2, o2) = ptr_counts(&driver);
        std::thread::sleep(nap);
        let (i3, o3) = ptr_counts(&driver);
        assert!(
            o3 > o2,
            "stop: playback DMA halted (trigger ioctl leaked back in)"
        );
        assert!(
            i3 > i2,
            "stop: capture DMA halted (trigger ioctl leaked back in)"
        );

        // Resume: cycles must return to block rate immediately (no halt ->
        // no resync stall).
        driver.set_playing(true);
        let block_ms = (driver.cycle_samples() as u64 * 1000) / driver.sample_rate() as u64;
        for i in 0..10 {
            let t0 = std::time::Instant::now();
            driver
                .run_cycle_for_worker()
                .unwrap_or_else(|e| panic!("resume cycle {i}: {e}"));
            assert!(
                t0.elapsed() < std::time::Duration::from_millis(block_ms * 3),
                "resume cycle {i} stalled ({}ms, block {block_ms}ms)",
                t0.elapsed().as_millis()
            );
        }
        let (i4, o4) = ptr_counts(&driver);
        std::thread::sleep(nap);
        let (i5, o5) = ptr_counts(&driver);
        assert!(o5 > o4, "resume: playback DMA not advancing");
        assert!(i5 > i4, "resume: capture DMA stalled");

        // Stop again.
        driver.set_playing(false);
        driver.zero_fill_hw_buffers();
        let (_i6, o6) = ptr_counts(&driver);
        std::thread::sleep(nap);
        let (_i7, o7) = ptr_counts(&driver);
        assert!(o7 > o6, "stop-again: playback DMA halted");
    }
}
