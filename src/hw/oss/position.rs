use super::ioctl::OssCount;

/// FreeBSD's CURRENT_*PTR snapshot counts acquired frames. Playback ready
/// frames have not been consumed yet; capture total is already produced data.
pub(crate) fn stream_frame(info: &OssCount, input: bool) -> std::io::Result<i64> {
    if info.samples < 0 || info.fifo_samples < 0 {
        return Err(std::io::Error::other("Invalid OSS stream counter"));
    }
    let frame = if input {
        info.samples
    } else {
        info.samples - i64::from(info.fifo_samples)
    };
    if frame < 0 {
        return Err(std::io::Error::other(
            "OSS ready frames exceed total frames",
        ));
    }
    Ok(frame)
}

#[derive(Debug, Default)]
pub(super) struct MmapProgress {
    pub(super) frames: i64,
}

impl MmapProgress {
    pub(super) fn update(&mut self, frame: i64) -> std::io::Result<i64> {
        if frame < self.frames {
            return Err(std::io::Error::other("OSS stream counter moved backwards"));
        }
        let delta = frame - self.frames;
        self.frames = frame;
        Ok(delta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(samples: i64, fifo_samples: i32) -> OssCount {
        OssCount {
            samples,
            fifo_samples,
            ..OssCount::default()
        }
    }

    #[test]
    fn playback_acquisition_does_not_invent_a_ring_wrap() {
        let mut progress = MmapProgress::default();
        // mmap acquires 128 frames, then the feeder consumes 48.
        assert_eq!(
            progress
                .update(stream_frame(&count(128, 80), false).unwrap())
                .unwrap(),
            48
        );
        // The next acquisition replenishes those 48 without moving the reader.
        assert_eq!(
            progress
                .update(stream_frame(&count(176, 128), false).unwrap())
                .unwrap(),
            0
        );
        assert_eq!(
            progress
                .update(stream_frame(&count(176, 80), false).unwrap())
                .unwrap(),
            48
        );
    }

    #[test]
    fn whole_wraps_and_long_sessions_preserve_exact_progress() {
        let mut progress = MmapProgress::default();
        let start = (1_i64 << 32) + 96;
        assert_eq!(
            progress
                .update(stream_frame(&count(start, 96), true).unwrap())
                .unwrap(),
            start
        );
        // Same ring offset after three complete wraps is still progress.
        assert_eq!(progress.update(start + 3 * 128).unwrap(), 3 * 128);
        assert_eq!(
            stream_frame(&count(start + 128, 128), false).unwrap(),
            start
        );
    }

    #[test]
    fn invalid_or_reset_counters_do_not_corrupt_progress() {
        assert!(stream_frame(&count(-1, 0), true).is_err());
        assert!(stream_frame(&count(128, -1), false).is_err());
        assert!(stream_frame(&count(0, 128), false).is_err());
        let mut progress = MmapProgress::default();
        assert_eq!(progress.update(128).unwrap(), 128);
        assert!(progress.update(0).is_err());
        assert_eq!(progress.update(256).unwrap(), 128);
    }
}
