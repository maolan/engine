use super::consts::*;
use crate::audio::io::AudioIO;
use fearless_simd::prelude::*;
use fearless_simd::{Level, dispatch, i32x4, u8x16};
use fearless_simd_macros::simd;
use nix::libc;
use std::sync::Arc;

pub(super) fn bytes_per_sample(format: u32) -> Option<usize> {
    match format {
        AFMT_S16_LE | AFMT_S16_BE => Some(2),
        AFMT_S24_LE | AFMT_S24_BE => Some(3),
        AFMT_S32_LE | AFMT_S32_BE => Some(4),
        AFMT_S8 => Some(1),
        _ => None,
    }
}

pub(super) fn supported_sample_format(format: u32) -> bool {
    matches!(
        format,
        AFMT_S16_LE | AFMT_S16_BE | AFMT_S24_LE | AFMT_S24_BE | AFMT_S32_LE | AFMT_S32_BE | AFMT_S8
    )
}

pub(super) fn cstr_fixed_prefix<const N: usize>(buf: &[libc::c_char; N]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(N);
    let bytes: Vec<u8> = buf[..len].iter().map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub(super) fn convert_in_to_i32_interleaved(
    format: u32,
    channels: usize,
    frames: usize,
    src: &[u8],
    dst: &mut [i32],
) {
    if format == AFMT_S32_NE {
        let samples = frames
            .saturating_mul(channels)
            .min(dst.len())
            .min(src.len() / 4);
        let bytes = samples * 4;
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), dst.as_mut_ptr().cast::<u8>(), bytes);
        }
        return;
    }

    if format == AFMT_S16_LE {
        let n = frames.saturating_mul(channels).min(dst.len());
        dispatch!(Level::new(), simd => s16le_in_kernel(simd, src, &mut dst[..n]));
        return;
    }

    if format == AFMT_S24_LE {
        let n = frames.saturating_mul(channels).min(dst.len());
        dispatch!(Level::new(), simd => s24le_in_kernel(simd, src, &mut dst[..n]));
        return;
    }

    let bps = bytes_per_sample(format).unwrap_or(4);
    let n = frames.saturating_mul(channels);
    for i in 0..n.min(dst.len()) {
        let o = i * bps;
        dst[i] = match format {
            AFMT_S16_LE => i16::from_le_bytes([src[o], src[o + 1]]) as i32 * 65536,
            AFMT_S16_BE => i16::from_be_bytes([src[o], src[o + 1]]) as i32 * 65536,
            AFMT_S24_LE => {
                let v = ((src[o + 2] as i32) << 24)
                    | ((src[o + 1] as i32) << 16)
                    | ((src[o] as i32) << 8);
                v >> 8
            }
            AFMT_S24_BE => {
                let v = ((src[o] as i32) << 24)
                    | ((src[o + 1] as i32) << 16)
                    | ((src[o + 2] as i32) << 8);
                v >> 8
            }
            AFMT_S32_LE => i32::from_le_bytes([src[o], src[o + 1], src[o + 2], src[o + 3]]),
            AFMT_S32_BE => i32::from_be_bytes([src[o], src[o + 1], src[o + 2], src[o + 3]]),
            AFMT_S8 => (src[o] as i8 as i32) << 24,
            _ => 0,
        };
    }
}

pub(super) fn convert_in_to_i32_connected(
    format: u32,
    frames: usize,
    src: &[u8],
    dst: &mut [i32],
    channels: &[Arc<AudioIO>],
) {
    if channels.iter().all(crate::hw::ports::has_audio_connections) {
        convert_in_to_i32_interleaved(format, channels.len(), frames, src, dst);
        return;
    }
    let bps = bytes_per_sample(format).unwrap_or(4);
    let channel_count = channels.len();
    for (ch, port) in channels.iter().enumerate() {
        if !crate::hw::ports::has_audio_connections(port) {
            continue;
        }
        for frame in 0..frames {
            let i = frame * channel_count + ch;
            if i >= dst.len() {
                continue;
            }
            let o = i * bps;
            dst[i] = match format {
                AFMT_S16_LE => i16::from_le_bytes([src[o], src[o + 1]]) as i32 * 65536,
                AFMT_S16_BE => i16::from_be_bytes([src[o], src[o + 1]]) as i32 * 65536,
                AFMT_S24_LE => {
                    let v = ((src[o + 2] as i32) << 24)
                        | ((src[o + 1] as i32) << 16)
                        | ((src[o] as i32) << 8);
                    v >> 8
                }
                AFMT_S24_BE => {
                    let v = ((src[o] as i32) << 24)
                        | ((src[o + 1] as i32) << 16)
                        | ((src[o + 2] as i32) << 8);
                    v >> 8
                }
                AFMT_S32_LE => i32::from_le_bytes([src[o], src[o + 1], src[o + 2], src[o + 3]]),
                AFMT_S32_BE => i32::from_be_bytes([src[o], src[o + 1], src[o + 2], src[o + 3]]),
                AFMT_S8 => (src[o] as i8 as i32) << 24,
                _ => 0,
            };
        }
    }
}

pub(super) fn convert_out_from_i32_interleaved(
    format: u32,
    channels: usize,
    frames: usize,
    src: &mut [i32],
    dst: &mut [u8],
) {
    if format == AFMT_S32_NE {
        let samples = frames
            .saturating_mul(channels)
            .min(src.len())
            .min(dst.len() / 4);
        let bytes = samples * 4;
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr().cast::<u8>(), dst.as_mut_ptr(), bytes);
        }
        return;
    }

    if format == AFMT_S16_LE {
        let n = frames.saturating_mul(channels).min(src.len());
        dispatch!(Level::new(), simd => s16le_out_kernel(simd, &src[..n], dst));
        return;
    }

    if format == AFMT_S24_LE {
        let n = frames.saturating_mul(channels).min(src.len());
        dispatch!(Level::new(), simd => s24le_out_kernel(simd, &src[..n], dst));
        return;
    }

    let bps = bytes_per_sample(format).unwrap_or(4);
    let n = frames.saturating_mul(channels);
    for (i, _item) in src.iter().enumerate().take(n.min(src.len())) {
        let o = i * bps;
        let s = src[i];
        match format {
            AFMT_S16_LE => {
                let v = (s >> 16) as i16;
                dst[o..o + 2].copy_from_slice(&v.to_le_bytes());
            }
            AFMT_S16_BE => {
                let v = (s >> 16) as i16;
                dst[o..o + 2].copy_from_slice(&v.to_be_bytes());
            }
            AFMT_S24_LE => {
                let v = s >> 8;
                dst[o] = v as u8;
                dst[o + 1] = (v >> 8) as u8;
                dst[o + 2] = (v >> 16) as u8;
            }
            AFMT_S24_BE => {
                let v = s >> 8;
                dst[o] = (v >> 16) as u8;
                dst[o + 1] = (v >> 8) as u8;
                dst[o + 2] = v as u8;
            }
            AFMT_S32_LE => {
                dst[o..o + 4].copy_from_slice(&s.to_le_bytes());
            }
            AFMT_S32_BE => {
                dst[o..o + 4].copy_from_slice(&s.to_be_bytes());
            }
            AFMT_S8 => {
                dst[o] = (s >> 24) as i8 as u8;
            }
            _ => {}
        }
    }
}

/// Spread 16-bit LE samples into the high half of each 32-bit lane (`sample << 16`).
#[simd]
fn s16le_in_kernel<S: Simd>(simd: S, src: &[u8], dst: &mut [i32]) {
    let l = S::i32s::LEN;
    let n = dst.len().min(src.len() / 2);
    let mut i = 0;
    while i + 2 * l <= n && i * 2 + 4 * l <= src.len() {
        let bytes = S::u8s::from_slice(simd, &src[i * 2..i * 2 + 4 * l]);
        let v16: S::i16s = bytes.bitcast();
        let (lo, hi) = v16.widen();
        (lo << 16u32).store_slice(&mut dst[i..i + l]);
        (hi << 16u32).store_slice(&mut dst[i + l..i + 2 * l]);
        i += 2 * l;
    }
    for (j, d) in dst[i..n].iter_mut().enumerate() {
        let o = (i + j) * 2;
        *d = i16::from_le_bytes([src[o], src[o + 1]]) as i32 * 65536;
    }
}

/// Unpack 24-bit LE samples (3 bytes each) into sign-extended 32-bit lanes.
#[simd]
fn s24le_in_kernel<S: Simd>(simd: S, src: &[u8], dst: &mut [i32]) {
    // 3 bytes -> 4 bytes, zeroing the top byte per lane; the shifts then sign-extend bit 23.
    let shuffle = u8x16::load_array(
        simd,
        [0, 1, 2, 128, 3, 4, 5, 128, 6, 7, 8, 128, 9, 10, 11, 128],
    );
    let n = dst.len().min(src.len() / 3);
    let mut i = 0;
    while i + 4 <= n && i * 3 + 16 <= src.len() {
        let bytes = u8x16::from_slice(simd, &src[i * 3..i * 3 + 16]);
        let v: i32x4<S> = bytes.swizzle_dyn_precise(shuffle).bitcast();
        let ext = (v << 8u32) >> 8u32;
        ext.store_slice(&mut dst[i..i + 4]);
        i += 4;
    }
    for (j, d) in dst[i..n].iter_mut().enumerate() {
        let o = (i + j) * 3;
        let v = ((src[o + 2] as i32) << 24) | ((src[o + 1] as i32) << 16) | ((src[o] as i32) << 8);
        *d = v >> 8;
    }
}

/// Pack the high 16 bits of each 32-bit lane into 16-bit LE samples.
#[simd]
fn s16le_out_kernel<S: Simd>(simd: S, src: &[i32], dst: &mut [u8]) {
    let l = S::i32s::LEN;
    let n = src.len().min(dst.len() / 2);
    let mut i = 0;
    while i + 2 * l <= n {
        let lo = S::i32s::from_slice(simd, &src[i..i + l]);
        let hi = S::i32s::from_slice(simd, &src[i + l..i + 2 * l]);
        let packed: S::i16s = (lo >> 16u32).saturating_narrow(hi >> 16u32);
        let bytes: S::u8s = packed.bitcast();
        bytes.store_slice(&mut dst[i * 2..i * 2 + 4 * l]);
        i += 2 * l;
    }
    for (j, s) in src[i..n].iter().enumerate() {
        let o = (i + j) * 2;
        let v = (*s >> 16) as i16;
        dst[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }
}

/// Truncate each 32-bit lane to 24 bits and pack into 3-byte LE samples.
#[simd]
fn s24le_out_kernel<S: Simd>(simd: S, src: &[i32], dst: &mut [u8]) {
    // Keep the low 3 bytes of each lane; the tail of the 16-byte scratch store is discarded.
    let shuffle = u8x16::load_array(
        simd,
        [0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 13, 14, 128, 128, 128, 128],
    );
    let n = src.len().min(dst.len() / 3);
    let mut i = 0;
    while i + 4 <= n && i * 3 + 16 <= dst.len() {
        let v = i32x4::from_slice(simd, &src[i..i + 4]);
        let shifted = v >> 8u32;
        let bytes: u8x16<S> = shifted.bitcast();
        let packed = bytes.swizzle_dyn_precise(shuffle);
        let mut tmp = [0u8; 16];
        packed.store_slice(&mut tmp);
        dst[i * 3..i * 3 + 12].copy_from_slice(&tmp[..12]);
        i += 4;
    }
    for (j, s) in src[i..n].iter().enumerate() {
        let o = (i + j) * 3;
        let v = s >> 8;
        dst[o] = v as u8;
        dst[o + 1] = (v >> 8) as u8;
        dst[o + 2] = (v >> 16) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s16le_in_expected(src: &[u8], n: usize) -> Vec<i32> {
        (0..n)
            .map(|i| i16::from_le_bytes([src[i * 2], src[i * 2 + 1]]) as i32 * 65536)
            .collect()
    }

    fn s24le_in_expected(src: &[u8], n: usize) -> Vec<i32> {
        (0..n)
            .map(|i| {
                let o = i * 3;
                let v = ((src[o + 2] as i32) << 24)
                    | ((src[o + 1] as i32) << 16)
                    | ((src[o] as i32) << 8);
                v >> 8
            })
            .collect()
    }

    #[test]
    fn s16le_in_matches_scalar() {
        let samples: Vec<i16> = (0..37).map(|i| (i * 3000 - 50_000) as i16).collect();
        let mut src = Vec::new();
        for s in &samples {
            src.extend_from_slice(&s.to_le_bytes());
        }
        for n in [samples.len(), 13, 5, 0] {
            let mut dst = vec![0i32; n];
            convert_in_to_i32_interleaved(AFMT_S16_LE, 1, n, &src, &mut dst);
            assert_eq!(dst, s16le_in_expected(&src, n));
        }
    }

    #[test]
    fn s24le_in_matches_scalar() {
        let raw: Vec<i32> = (0..29).map(|i| i * 1_234_567 - 16_000_000).collect();
        let mut src = Vec::new();
        for s in &raw {
            src.extend_from_slice(&s.to_le_bytes()[..3]);
        }
        for n in [raw.len(), 14, 6, 0] {
            let mut dst = vec![0i32; n];
            convert_in_to_i32_interleaved(AFMT_S24_LE, 1, n, &src, &mut dst);
            assert_eq!(dst, s24le_in_expected(&src, n));
        }
    }

    #[test]
    fn s16le_out_matches_scalar() {
        let src: Vec<i32> = (0..37).map(|i| i * 50_000_000 - 900_000_000).collect();
        for n in [src.len(), 11, 3, 0] {
            let mut dst = vec![0u8; n * 2];
            convert_out_from_i32_interleaved(AFMT_S16_LE, 1, n, &mut src.clone(), &mut dst);
            let expected: Vec<u8> = src[..n]
                .iter()
                .flat_map(|s| ((*s >> 16) as i16).to_le_bytes())
                .collect();
            assert_eq!(dst, expected);
        }
    }

    #[test]
    fn s24le_out_matches_scalar() {
        let src: Vec<i32> = (0..29).map(|i| i * 56_789 - 800_000_000).collect();
        for n in [src.len(), 9, 2, 0] {
            let mut dst = vec![0u8; n * 3];
            convert_out_from_i32_interleaved(AFMT_S24_LE, 1, n, &mut src.clone(), &mut dst);
            let expected: Vec<u8> = src[..n]
                .iter()
                .flat_map(|s| {
                    let v = s >> 8;
                    [v as u8, (v >> 8) as u8, (v >> 16) as u8]
                })
                .collect();
            assert_eq!(dst, expected);
        }
    }
}
