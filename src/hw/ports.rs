#[cfg(unix)]
use crate::audio::io::AudioIO;
#[cfg(unix)]
use std::sync::Arc;

#[cfg(unix)]
pub fn has_audio_connections(port: &Arc<AudioIO>) -> bool {
    port.connection_count
        .load(std::sync::atomic::Ordering::Relaxed)
        > 0
}

/// Fast path when the interleaved buffer is already in f32.
#[cfg(unix)]
pub fn fill_ports_from_interleaved_buffer(
    ports: &[Arc<AudioIO>],
    frames: usize,
    connected_only: bool,
    buffer: &[f32],
    channels: usize,
) {
    let total = frames.saturating_mul(channels).min(buffer.len());

    if channels == 2 && ports.len() >= 2 && total >= 8 {
        let left_connected = !connected_only || has_audio_connections(&ports[0]);
        let right_connected = !connected_only || has_audio_connections(&ports[1]);
        #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
        if left_connected || right_connected {
            let left_len = frames.min(ports[0].buffer.lock().len());
            let right_len = frames.min(ports[1].buffer.lock().len());
            let copy_frames = left_len.min(right_len).min(total / 2);
            unsafe {
                if std::arch::is_x86_feature_detected!("sse") {
                    let left_lock = ports[0].buffer.lock();
                    let right_lock = ports[1].buffer.lock();
                    let left_dst = &mut left_lock[..left_len];
                    let right_dst = &mut right_lock[..right_len];
                    let n = copy_frames / 4;
                    for i in 0..n {
                        let src = std::arch::x86_64::_mm_loadu_ps(buffer.as_ptr().add(i * 8));
                        let src2 = std::arch::x86_64::_mm_loadu_ps(buffer.as_ptr().add(i * 8 + 4));
                        let left = std::arch::x86_64::_mm_shuffle_ps(src, src2, 0b10_00_10_00);
                        let right = std::arch::x86_64::_mm_shuffle_ps(src, src2, 0b11_01_11_01);
                        std::arch::x86_64::_mm_storeu_ps(left_dst.as_mut_ptr().add(i * 4), left);
                        std::arch::x86_64::_mm_storeu_ps(right_dst.as_mut_ptr().add(i * 4), right);
                    }
                    for i in n * 4..copy_frames {
                        left_dst[i] = buffer[i * 2];
                        right_dst[i] = buffer[i * 2 + 1];
                    }

                    if left_connected {
                        *ports[0].finished.lock() = true;
                    }
                    if right_connected {
                        *ports[1].finished.lock() = true;
                    }

                    for (ch_idx, io_port) in ports.iter().enumerate().skip(2) {
                        if connected_only && !has_audio_connections(io_port) {
                            *io_port.finished.lock() = true;
                            continue;
                        }
                        let channel_buf_lock = io_port.buffer.lock();
                        let channel_samples: &mut [f32] = &mut *channel_buf_lock;
                        let end = frames.min(channel_samples.len());
                        let dst = &mut channel_samples[..end];
                        let mut i = ch_idx;
                        for d in dst.iter_mut() {
                            *d = buffer.get(i).copied().unwrap_or(0.0);
                            i = i.saturating_add(channels);
                            if i >= total {
                                break;
                            }
                        }
                        *io_port.finished.lock() = true;
                    }
                    return;
                }
            }
        }
    }

    for (ch_idx, io_port) in ports.iter().enumerate() {
        if connected_only && !has_audio_connections(io_port) {
            *io_port.finished.lock() = true;
            continue;
        }
        let channel_buf_lock = io_port.buffer.lock();
        let channel_samples: &mut [f32] = &mut *channel_buf_lock;
        let end = frames.min(channel_samples.len());
        let dst = &mut channel_samples[..end];
        let mut i = ch_idx;
        for d in dst.iter_mut() {
            *d = buffer.get(i).copied().unwrap_or(0.0);
            i = i.saturating_add(channels);
            if i >= total {
                break;
            }
        }
        *io_port.finished.lock() = true;
    }
}

#[cfg(unix)]
pub fn write_interleaved_from_ports(
    ports: &[Arc<AudioIO>],
    frames: usize,
    gain: f32,
    balance: f32,
    connected_only: bool,
    mut write_sample: impl FnMut(usize, usize, f32),
) {
    let ch_count = ports.len();
    for (ch_idx, io_port) in ports.iter().enumerate() {
        if connected_only && !has_audio_connections(io_port) {
            continue;
        }
        io_port.process();
        let channel_buf_lock = io_port.buffer.lock();
        let channel_samples: &[f32] = &*channel_buf_lock;
        let balance_gain = crate::hw::common::channel_balance_gain(ch_count, ch_idx, balance);
        let total_gain = gain * balance_gain;
        let mut offset = 0;
        while offset + 64 <= frames {
            let mut chunk = [0.0f32; 64];
            chunk.copy_from_slice(&channel_samples[offset..offset + 64]);
            crate::simd::mul_inplace(&mut chunk, total_gain);
            for (i, v) in chunk.iter().enumerate() {
                write_sample(ch_idx, offset + i, *v);
            }
            offset += 64;
        }
        for (frame, v) in channel_samples[offset..frames].iter().enumerate() {
            write_sample(ch_idx, offset + frame, *v * total_gain);
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::audio::io::AudioIO;
    use std::sync::Arc;

    #[cfg(unix)]
    pub fn fill_ports_from_interleaved(
        ports: &[Arc<AudioIO>],
        frames: usize,
        connected_only: bool,
        mut sample_at: impl FnMut(usize, usize) -> f32,
    ) {
        for (ch_idx, io_port) in ports.iter().enumerate() {
            if connected_only && !has_audio_connections(io_port) {
                *io_port.finished.lock() = true;
                continue;
            }
            let channel_buf_lock = io_port.buffer.lock();
            let channel_samples: &mut [f32] = &mut *channel_buf_lock;
            for (frame, sample) in channel_samples.iter_mut().enumerate().take(frames) {
                *sample = sample_at(ch_idx, frame);
            }
            *io_port.finished.lock() = true;
        }
    }

    #[test]
    fn fill_ports_from_interleaved_skips_unconnected_ports_when_requested() {
        let connected = Arc::new(AudioIO::new(4));
        let disconnected = Arc::new(AudioIO::new(4));
        connected
            .connection_count
            .store(1, std::sync::atomic::Ordering::Relaxed);

        fill_ports_from_interleaved(
            &[connected.clone(), disconnected.clone()],
            3,
            true,
            |ch, frame| (ch * 10 + frame) as f32,
        );

        assert_eq!(connected.buffer.lock().as_slice()[..3], [0.0, 1.0, 2.0]);
        assert_eq!(disconnected.buffer.lock().as_slice()[..3], [0.0, 0.0, 0.0]);
        assert!(*connected.finished.lock());
        assert!(*disconnected.finished.lock());
    }

    #[test]
    fn write_interleaved_from_ports_applies_gain_and_stereo_balance() {
        let left_src = Arc::new(AudioIO::new(3));
        let right_src = Arc::new(AudioIO::new(3));
        let left = Arc::new(AudioIO::new(3));
        let right = Arc::new(AudioIO::new(3));
        AudioIO::connect(&left_src, &left);
        AudioIO::connect(&right_src, &right);
        left_src.buffer.lock().as_mut_slice()[..3].copy_from_slice(&[1.0, 0.5, -1.0]);
        right_src.buffer.lock().as_mut_slice()[..3].copy_from_slice(&[0.25, -0.25, 0.75]);

        let mut written = vec![vec![0.0_f32; 3]; 2];
        write_interleaved_from_ports(&[left, right], 3, 2.0, 0.5, true, |ch, frame, sample| {
            written[ch][frame] = sample;
        });

        assert_eq!(written[0], vec![1.0, 0.5, -1.0]);
        assert_eq!(written[1], vec![0.5, -0.5, 1.5]);
    }
}
