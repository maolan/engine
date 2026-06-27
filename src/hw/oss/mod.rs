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

mod channel;
mod consts;
mod convert;
mod driver;
mod ioctl;
mod sync;

pub use self::channel::OSSChannel;
pub use self::consts::*;
pub use self::driver::HwDriver;
pub use self::ioctl::{AudioInfo, add_to_sync_group, start_sync_group};
pub use crate::hw::options::HwOptions;

use self::convert::*;
use self::ioctl::*;
use self::sync::FrameClock;

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

#[derive(Debug)]
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
    frame_size_bytes: usize,
    caps: i32,
    frame_clock: FrameClock,
    frame_stamp: i64,
    playing: Arc<AtomicBool>,
    was_playing_last_cycle: bool,
    stop_fade_remaining_frames: usize,
    stop_fade_total_frames: usize,
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

    pub fn fd(&self) -> i32 {
        self.dsp.as_raw_fd()
    }

    pub fn start_trigger(&self) -> std::io::Result<()> {
        if (self.caps & PCM_CAP_TRIGGER) == 0 {
            return Ok(());
        }
        let trig: i32 = if self.input {
            PCM_ENABLE_INPUT
        } else {
            PCM_ENABLE_OUTPUT
        };
        unsafe { oss_set_trigger(self.dsp.as_raw_fd(), &trig) }
            .map(|_| ())
            .map_err(|_| std::io::Error::last_os_error())
    }

    pub fn stop_trigger(&self) -> std::io::Result<()> {
        if (self.caps & PCM_CAP_TRIGGER) == 0 {
            return Ok(());
        }
        let trig: i32 = 0;
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
        _sync_key: &str,
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

        let mut caps = 0_i32;
        unsafe {
            oss_get_caps(dsp.as_raw_fd(), &mut caps)
                .map_err(|_| std::io::Error::last_os_error())?;
        }

        let chsamples = options.period_frames.max(1);

        let mut io_channels = Vec::with_capacity(channels as usize);
        for _ in 0..channels {
            io_channels.push(Arc::new(AudioIO::new(chsamples)));
        }

        let mut frame_clock = FrameClock::default();
        frame_clock.set_sample_rate(effective_rate as u32);
        let _ = frame_clock.init_clock(effective_rate as u32);

        Ok(Audio {
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
            frame_size_bytes: frame_size,
            caps,
            frame_clock,
            frame_stamp: 0,
            playing,
            was_playing_last_cycle: false,
            stop_fade_remaining_frames: 0,
            stop_fade_total_frames: 0,
        })
    }

    fn frame_size(&self) -> usize {
        self.frame_size_bytes
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

    pub fn process(&mut self, stop_requested: &AtomicBool) -> std::io::Result<()> {
        let num_channels = self.channels.len();
        let all_connected = self
            .channels
            .iter()
            .all(crate::hw::ports::has_audio_connections);

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

            let norm_factor = convert_policy::F32_FROM_I32_MAX;
            let total_samples = self.chsamples * num_channels;
            self.f32_buffer.resize(total_samples, 0.0);
            crate::simd::convert_i32_to_f32(
                &self.buffer[..total_samples],
                &mut self.f32_buffer,
                norm_factor,
            );
            crate::hw::ports::fill_ports_from_interleaved_buffer(
                &self.channels,
                self.chsamples,
                !all_connected,
                &self.f32_buffer,
                num_channels,
            );
        } else {
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
                if !all_connected {
                    data_i32.fill(0);
                }
                let fade_remaining = self.stop_fade_remaining_frames;
                let fade_total = self.stop_fade_total_frames.max(1);
                crate::hw::ports::write_interleaved_from_ports(
                    &self.channels,
                    self.chsamples,
                    output_gain,
                    self.output_balance,
                    !all_connected,
                    |ch_idx, frame, sample| {
                        let target_idx = frame * num_channels + ch_idx;
                        let fade_gain = if !playing && fade_remaining > 0 {
                            let progressed = self.chsamples.saturating_sub(fade_remaining) + frame;
                            (1.0 - (progressed as f32 / fade_total as f32)).clamp(0.0, 1.0)
                        } else {
                            1.0
                        };
                        data_i32[target_idx] =
                            (sample.clamp(-1.0, 1.0) * fade_gain * scale_factor) as i32;
                    },
                );
                if !playing && self.stop_fade_remaining_frames > 0 {
                    self.stop_fade_remaining_frames = self
                        .stop_fade_remaining_frames
                        .saturating_sub(self.chsamples);
                }
            }

            let period_bytes = self.chsamples * self.frame_size();
            let mut raw = vec![0_u8; period_bytes];
            convert_out_from_i32_interleaved(
                self.format,
                num_channels,
                self.chsamples,
                self.buffer.as_mut_slice(),
                raw.as_mut_slice(),
            );
            self.write_full_period(&raw, stop_requested)?;
        }

        Ok(())
    }

    pub fn force_silence_now(&mut self) {
        if self.input {
            return;
        }
        self.buffer.fill(0);
        for ch in &self.channels {
            ch.buffer.lock().fill(0.0);
        }
        self.stop_fade_remaining_frames = 0;
        self.stop_fade_total_frames = 0;
        self.was_playing_last_cycle = false;
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        // Reset the OSS channel to discard pending buffers. Without this,
        // the kernel's dsp_close() calls chn_flush() on playback channels
        // which drains remaining audio data — sleeping up to CHN_TIMEOUT
        // (5 seconds by default) before actually closing the fd.
        let _ = self.halt();
    }
}
