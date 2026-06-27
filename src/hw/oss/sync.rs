use nix::libc;

#[derive(Debug, Clone, Copy)]
pub(super) struct FrameClock {
    pub(super) zero: libc::timespec,
    pub(super) sample_rate: u32,
}

impl Default for FrameClock {
    fn default() -> Self {
        Self {
            zero: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            sample_rate: 48_000,
        }
    }
}

impl FrameClock {
    pub(super) fn set_sample_rate(&mut self, sample_rate: u32) -> bool {
        if sample_rate == 0 {
            return false;
        }
        self.sample_rate = sample_rate;
        true
    }

    pub(super) fn init_clock(&mut self, sample_rate: u32) -> bool {
        if !self.set_sample_rate(sample_rate) {
            return false;
        }
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut self.zero) == 0 }
    }

    pub(super) fn now(&self) -> Option<i64> {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        let ok = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) == 0 };
        if !ok {
            return None;
        }
        let ns = (now.tv_sec - self.zero.tv_sec) as i128 * 1_000_000_000_i128
            + (now.tv_nsec - self.zero.tv_nsec) as i128;
        Some(((ns * self.sample_rate as i128) / 1_000_000_000_i128) as i64)
    }
}
