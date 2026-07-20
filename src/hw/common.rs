pub fn channel_balance_gain(ch_count: usize, channel_idx: usize, balance: f32) -> f32 {
    if ch_count == 2 {
        let b = balance.clamp(-1.0, 1.0);
        if channel_idx == 0 {
            (1.0 - b).clamp(0.0, 1.0)
        } else {
            (1.0 + b).clamp(0.0, 1.0)
        }
    } else {
        1.0
    }
}

pub fn output_meter_linear(port_count: usize, gain: f32, balance: f32) -> Vec<f32> {
    (0..port_count)
        .map(|channel_idx| 0.0 * gain * channel_balance_gain(port_count, channel_idx, balance))
        .collect()
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub fn output_meter_db(port_count: usize, gain: f32, balance: f32) -> Vec<f32> {
    output_meter_linear(port_count, gain, balance)
        .into_iter()
        .map(|peak| {
            if peak <= 1.0e-6 {
                -90.0
            } else {
                (20.0 * peak.log10()).clamp(-90.0, 20.0)
            }
        })
        .collect()
}

/// Meter the plan's hardware-output arena buffers (the driver drain source)
/// instead of the legacy port buffers.
pub fn output_meter_linear_from_plan(
    plan: &crate::render_plan::RenderPlan,
    gain: f32,
    balance: f32,
) -> Vec<f32> {
    let ch_count = plan.hw_out_map.len();
    let mut out = Vec::with_capacity(ch_count);
    for (channel_idx, &(buf, _channel)) in plan.hw_out_map.iter().enumerate() {
        let balance_gain = channel_balance_gain(ch_count, channel_idx, balance);
        // Safety: called after the cycle completed (HWFinished handler /
        // driver drain point) — no node writes these buffers now.
        let buf = unsafe { plan.buffer(buf) };
        out.push(crate::simd::peak_abs(buf) * gain * balance_gain);
    }
    out
}

/// Read the plan's hardware-output arena buffers and return them as an
/// interleaved `f32` vector suitable for feeding into an EBU R128 loudness
/// meter.
pub fn interleaved_hw_out_samples(plan: &crate::render_plan::RenderPlan) -> Vec<f32> {
    let ch_count = plan.hw_out_map.len();
    if ch_count == 0 {
        return Vec::new();
    }

    let channel_bufs: Vec<&[f32]> = plan
        .hw_out_map
        .iter()
        .map(|&(buf, _channel)| {
            // Safety: called after the cycle completed — no node writes these
            // buffers now.
            unsafe { plan.buffer(buf) }
        })
        .collect();

    let frame_count = channel_bufs.first().map(|b| b.len()).unwrap_or(0);
    (0..frame_count)
        .flat_map(|frame| channel_bufs.iter().map(move |buf| buf[frame]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{channel_balance_gain, output_meter_linear};

    #[test]
    fn channel_balance_gain_is_neutral_for_non_stereo() {
        assert_eq!(channel_balance_gain(1, 0, -1.0), 1.0);
        assert_eq!(channel_balance_gain(4, 2, 0.75), 1.0);
    }

    #[test]
    fn channel_balance_gain_clamps_full_left_and_right_for_stereo() {
        assert_eq!(channel_balance_gain(2, 0, -1.0), 1.0);
        assert_eq!(channel_balance_gain(2, 1, -1.0), 0.0);
        assert_eq!(channel_balance_gain(2, 0, 1.0), 0.0);
        assert_eq!(channel_balance_gain(2, 1, 1.0), 1.0);
    }

    #[test]
    fn output_meter_linear_returns_silent_fallback_meter() {
        let meter = output_meter_linear(2, 2.0, 0.5);

        assert_eq!(meter, vec![0.0, 0.0]);
    }

    #[test]
    fn output_meter_linear_handles_empty_outputs_and_zero_gain() {
        assert!(output_meter_linear(0, 1.0, 0.0).is_empty());
        assert_eq!(output_meter_linear(1, 0.0, 9.0), vec![0.0]);
    }

    #[test]
    fn channel_balance_gain_clamps_out_of_range_balance() {
        assert_eq!(channel_balance_gain(2, 0, 2.0), 0.0);
        assert_eq!(channel_balance_gain(2, 1, 2.0), 1.0);
        assert_eq!(channel_balance_gain(2, 0, -2.0), 1.0);
        assert_eq!(channel_balance_gain(2, 1, -2.0), 0.0);
    }
}
