use crate::hw::convert_policy::{F32_FROM_F32, F32_TO_F32};

pub fn deinterleave_f32(src: &[f32], channels: usize, frames: usize, dst: &mut [Vec<f32>]) {
    for ch in 0..channels {
        let offset = ch * frames;
        let channel_dst = &mut dst[ch];
        channel_dst.resize(frames, 0.0);
        channel_dst.copy_from_slice(&src[offset..offset + frames]);
    }
}

pub fn interleave_f32(src: &[Vec<f32>], channels: usize, frames: usize, dst: &mut [f32]) {
    for ch in 0..channels {
        let offset = ch * frames;
        let channel_src = &src[ch];
        dst[offset..offset + frames].copy_from_slice(&channel_src[..frames]);
    }
}
