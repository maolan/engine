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

#[cfg(unix)]
pub fn fill_ports_from_interleaved_buffer(
    ports: &[Arc<AudioIO>],
    _frames: usize,
    connected_only: bool,
    _buffer: &[f32],
    _channels: usize,
) {
    for io_port in ports {
        if connected_only && !has_audio_connections(io_port) {
            io_port
                .finished
                .store(true, std::sync::atomic::Ordering::Release);
            continue;
        }
        io_port
            .finished
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Deinterleave a hardware input buffer straight into the render-plan arena
/// buffers (`plan.hw_in_map`). Used in plan mode instead of
/// `fill_ports_from_interleaved_buffer`: the dispatcher copies arena → port
/// at cycle start, so the RT thread never touches port buffers.
pub fn fill_arena_from_interleaved(
    plan: &crate::render_plan::RenderPlan,
    frames: usize,
    buffer: &[f32],
    channels: usize,
) {
    let total = frames.saturating_mul(channels).min(buffer.len());
    for &(ch_idx, buf) in &plan.hw_in_map {
        // Safety: the driver is the sole producer of HwInput arena buffers.
        // The engine only dispatches a cycle after the driver reports new
        // input, and this write happens before that report, so no node is
        // reading these buffers concurrently.
        let arena = unsafe { &mut *plan.buffer_ptr(buf) };
        let end = frames.min(arena.len());
        let dst = &mut arena[..end];
        let mut i = ch_idx;
        for d in dst.iter_mut() {
            *d = buffer.get(i).copied().unwrap_or(0.0);
            i = i.saturating_add(channels);
            if i >= total {
                break;
            }
        }
    }
}

/// Interleave the render-plan arena output buffers (`plan.hw_out_map`) into
/// the hardware output, applying gain and stereo balance. Used in plan mode
/// instead of `write_interleaved_from_ports`: the plan's Sum nodes already
/// produced the final samples, so no `process()` call is needed, and
/// unconnected ports were handled by Zero nodes.
pub fn write_interleaved_from_arena(
    plan: &crate::render_plan::RenderPlan,
    frames: usize,
    gain: f32,
    balance: f32,
    mut write_sample: impl FnMut(usize, usize, f32),
) {
    let ch_count = plan.hw_out_map.len();
    for &(buf, ch_idx) in &plan.hw_out_map {
        // Safety: runs at the cycle boundary — the engine calls back into the
        // driver only after the cycle completed, so every producer of these
        // buffers has finished and no worker touches the arena now.
        let arena = unsafe { plan.buffer(buf) };
        let balance_gain = crate::hw::common::channel_balance_gain(ch_count, ch_idx, balance);
        let total_gain = gain * balance_gain;
        let available = frames.min(arena.len());
        let mut offset = 0;
        while offset + 64 <= available {
            let mut chunk = [0.0f32; 64];
            chunk.copy_from_slice(&arena[offset..offset + 64]);
            crate::simd::mul_inplace(&mut chunk, total_gain);
            for (i, v) in chunk.iter().enumerate() {
                write_sample(ch_idx, offset + i, *v);
            }
            offset += 64;
        }
        for (frame, v) in arena[offset..available].iter().enumerate() {
            write_sample(ch_idx, offset + frame, *v * total_gain);
        }
    }
}

pub fn clear_hw_arena_buffers(plan: &crate::render_plan::RenderPlan) {
    for &(_, buf) in &plan.hw_in_map {
        // Safety: stop/silence runs at a hardware cycle boundary; the driver
        // is the sole producer of hardware input arena buffers.
        unsafe { &mut *plan.buffer_ptr(buf) }.fill(0.0);
    }
    for &(buf, _) in &plan.hw_out_map {
        // Safety: stop/silence runs when no worker is producing this cycle's
        // hardware output buffers.
        unsafe { &mut *plan.buffer_ptr(buf) }.fill(0.0);
    }
}

#[cfg(unix)]
pub fn write_interleaved_from_ports(
    ports: &[Arc<AudioIO>],
    frames: usize,
    _gain: f32,
    _balance: f32,
    connected_only: bool,
    mut write_sample: impl FnMut(usize, usize, f32),
) {
    for (ch_idx, io_port) in ports.iter().enumerate() {
        if connected_only && !has_audio_connections(io_port) {
            continue;
        }
        for frame in 0..frames {
            write_sample(ch_idx, frame, 0.0);
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::audio::io::AudioIO;
    use std::sync::Arc;

    #[test]
    fn fill_ports_from_interleaved_marks_ports_finished_without_legacy_writes() {
        let connected = Arc::new(AudioIO::new(4));
        let disconnected = Arc::new(AudioIO::new(4));
        connected
            .connection_count
            .store(1, std::sync::atomic::Ordering::Relaxed);

        fill_ports_from_interleaved_buffer(
            &[connected.clone(), disconnected.clone()],
            3,
            true,
            &[1.0, 2.0, 3.0],
            1,
        );

        assert!(
            connected
                .finished
                .load(std::sync::atomic::Ordering::Relaxed)
        );
        assert!(
            disconnected
                .finished
                .load(std::sync::atomic::Ordering::Relaxed)
        );
    }

    #[test]
    fn fill_arena_from_interleaved_writes_hw_in_map_buffers() {
        let hw_in = Arc::new(AudioIO::new(4));
        let plan = crate::render_plan::RenderPlan::compile(
            &crate::state::State::default().snapshot(),
            &[hw_in],
            &[],
            4,
        );
        // Stereo interleaved input: left = 0,1,2,3; right = 10,11,12,13.
        let interleaved = [0.0, 10.0, 1.0, 11.0, 2.0, 12.0, 3.0, 13.0];
        fill_arena_from_interleaved(&plan, 4, &interleaved, 2);

        assert_eq!(plan.hw_in_map.len(), 1);
        let (_, buf) = plan.hw_in_map[0];
        // Safety: test is single-threaded; no node runs concurrently.
        let arena = unsafe { plan.buffer(buf) };
        assert_eq!(arena, [0.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn write_interleaved_from_arena_applies_gain_and_balance() {
        let hw_out = Arc::new(AudioIO::new(3));
        let plan = crate::render_plan::RenderPlan::compile(
            &crate::state::State::default().snapshot(),
            &[],
            &[hw_out],
            3,
        );
        let (buf, ch) = plan.hw_out_map[0];
        assert_eq!(ch, 0);
        // Safety: test is single-threaded; no node runs concurrently.
        let arena = unsafe { &mut *plan.buffer_ptr(buf) };
        arena[..3].copy_from_slice(&[1.0, 0.5, -1.0]);

        let mut written = vec![vec![0.0_f32; 3]; 1];
        write_interleaved_from_arena(&plan, 3, 2.0, 0.0, |ch, frame, sample| {
            written[ch][frame] = sample;
        });

        assert_eq!(written[0], vec![2.0, 1.0, -2.0]);
    }

    #[test]
    fn write_interleaved_from_ports_writes_silence_without_plan() {
        let left = Arc::new(AudioIO::new(3));
        let right = Arc::new(AudioIO::new(3));
        left.connection_count
            .store(1, std::sync::atomic::Ordering::Relaxed);
        right
            .connection_count
            .store(1, std::sync::atomic::Ordering::Relaxed);

        let mut written = vec![vec![1.0_f32; 3]; 2];
        write_interleaved_from_ports(&[left, right], 3, 2.0, 0.5, true, |ch, frame, sample| {
            written[ch][frame] = sample;
        });

        assert_eq!(written[0], vec![0.0, 0.0, 0.0]);
        assert_eq!(written[1], vec![0.0, 0.0, 0.0]);
    }
}
