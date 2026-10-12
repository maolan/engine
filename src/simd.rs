use fearless_simd::prelude::*;
use fearless_simd::{Level, dispatch};
use fearless_simd_macros::simd;

pub fn add_inplace(dst: &mut [f32], src: &[f32]) {
    dispatch!(Level::new(), simd => add_inplace_kernel(simd, dst, src));
}

pub fn mul_inplace(dst: &mut [f32], gain: f32) {
    dispatch!(Level::new(), simd => mul_inplace_kernel(simd, dst, gain));
}

pub fn add_scaled_inplace(dst: &mut [f32], src: &[f32], gain: f32) {
    dispatch!(Level::new(), simd => add_scaled_inplace_kernel(simd, dst, src, gain));
}

pub fn copy_scaled_inplace(dst: &mut [f32], src: &[f32], gain: f32) {
    dispatch!(Level::new(), simd => copy_scaled_inplace_kernel(simd, dst, src, gain));
}

pub fn add_sanitized_inplace(dst: &mut [f32], src: &[f32]) {
    dispatch!(Level::new(), simd => add_sanitized_inplace_kernel(simd, dst, src));
}

pub fn copy_sanitized_inplace(dst: &mut [f32], src: &[f32]) {
    dispatch!(Level::new(), simd => copy_sanitized_inplace_kernel(simd, dst, src));
}

pub fn sanitize_finite_inplace(buf: &mut [f32]) {
    dispatch!(Level::new(), simd => sanitize_finite_inplace_kernel(simd, buf));
}

pub fn peak_abs(buf: &[f32]) -> f32 {
    dispatch!(Level::new(), simd => peak_abs_kernel(simd, buf))
}

pub fn clamp_inplace(buf: &mut [f32], min: f32, max: f32) {
    dispatch!(Level::new(), simd => clamp_inplace_kernel(simd, buf, min, max));
}

pub fn convert_i32_to_f32(src: &[i32], dst: &mut [f32], gain: f32) {
    dispatch!(Level::new(), simd => convert_i32_to_f32_kernel(simd, src, dst, gain));
}

pub fn convert_i16_to_f32(src: &[i16], dst: &mut [f32], gain: f32) {
    dispatch!(Level::new(), simd => convert_i16_to_f32_kernel(simd, src, dst, gain));
}

pub fn convert_i8_to_f32(src: &[i8], dst: &mut [f32], gain: f32) {
    dispatch!(Level::new(), simd => convert_i8_to_f32_kernel(simd, src, dst, gain));
}

pub fn convert_i24_to_f32(src: &[i32], dst: &mut [f32], gain: f32) {
    dispatch!(Level::new(), simd => convert_i24_to_f32_kernel(simd, src, dst, gain));
}

pub fn convert_f32_to_i24(src: &[f32], dst: &mut [i32], gain: f32) {
    dispatch!(Level::new(), simd => convert_f32_to_i24_kernel(simd, src, dst, gain));
}

pub fn convert_f32_to_i32(src: &[f32], dst: &mut [i32], gain: f32) {
    dispatch!(Level::new(), simd => convert_f32_to_i32_kernel(simd, src, dst, gain));
}

pub fn convert_f32_to_i16(src: &[f32], dst: &mut [i16], gain: f32) {
    dispatch!(Level::new(), simd => convert_f32_to_i16_kernel(simd, src, dst, gain));
}

pub fn convert_f32_to_i8(src: &[f32], dst: &mut [i8], gain: f32) {
    dispatch!(Level::new(), simd => convert_f32_to_i8_kernel(simd, src, dst, gain));
}

pub fn apply_fade_in_inplace(buf: &mut [f32], start_t: f32, dt: f32) {
    dispatch!(Level::new(), simd => apply_fade_in_kernel(simd, buf, start_t, dt));
}

pub fn apply_fade_out_inplace(buf: &mut [f32], start_t: f32, dt: f32) {
    dispatch!(Level::new(), simd => apply_fade_out_kernel(simd, buf, start_t, dt));
}

pub fn interleave_stereo(l: &[f32], r: &[f32], dst: &mut [f32]) {
    dispatch!(Level::new(), simd => interleave_stereo_kernel(simd, l, r, dst));
}

#[simd]
fn add_inplace_kernel<S: Simd>(simd: S, dst: &mut [f32], src: &[f32]) {
    let n = dst.len().min(src.len());
    let dst = &mut dst[..n];
    let src = &src[..n];
    let mut dchunks = dst.chunks_exact_mut(S::f32s::LEN);
    let mut schunks = src.chunks_exact(S::f32s::LEN);
    for (d, s) in dchunks.by_ref().zip(schunks.by_ref()) {
        let dv = S::f32s::from_slice(simd, d);
        let sv = S::f32s::from_slice(simd, s);
        (dv + sv).store_slice(d);
    }
    for (d, s) in dchunks.into_remainder().iter_mut().zip(schunks.remainder()) {
        *d += *s;
    }
}

#[simd]
fn mul_inplace_kernel<S: Simd>(simd: S, dst: &mut [f32], gain: f32) {
    let mut chunks = dst.chunks_exact_mut(S::f32s::LEN);
    for d in chunks.by_ref() {
        (S::f32s::from_slice(simd, d) * gain).store_slice(d);
    }
    for d in chunks.into_remainder() {
        *d *= gain;
    }
}

#[simd]
fn add_scaled_inplace_kernel<S: Simd>(simd: S, dst: &mut [f32], src: &[f32], gain: f32) {
    let n = dst.len().min(src.len());
    let dst = &mut dst[..n];
    let src = &src[..n];
    let mut dchunks = dst.chunks_exact_mut(S::f32s::LEN);
    let mut schunks = src.chunks_exact(S::f32s::LEN);
    for (d, s) in dchunks.by_ref().zip(schunks.by_ref()) {
        let dv = S::f32s::from_slice(simd, d);
        let sv = S::f32s::from_slice(simd, s);
        (dv + sv * gain).store_slice(d);
    }
    for (d, s) in dchunks.into_remainder().iter_mut().zip(schunks.remainder()) {
        *d += *s * gain;
    }
}

#[simd]
fn copy_scaled_inplace_kernel<S: Simd>(simd: S, dst: &mut [f32], src: &[f32], gain: f32) {
    let n = dst.len().min(src.len());
    let dst = &mut dst[..n];
    let src = &src[..n];
    let mut dchunks = dst.chunks_exact_mut(S::f32s::LEN);
    let mut schunks = src.chunks_exact(S::f32s::LEN);
    for (d, s) in dchunks.by_ref().zip(schunks.by_ref()) {
        let sv = S::f32s::from_slice(simd, s);
        (sv * gain).store_slice(d);
    }
    for (d, s) in dchunks.into_remainder().iter_mut().zip(schunks.remainder()) {
        *d = *s * gain;
    }
}

#[simd]
fn add_sanitized_inplace_kernel<S: Simd>(simd: S, dst: &mut [f32], src: &[f32]) {
    let n = dst.len().min(src.len());
    let dst = &mut dst[..n];
    let src = &src[..n];
    let zero = S::f32s::splat(simd, 0.0);
    let mut dchunks = dst.chunks_exact_mut(S::f32s::LEN);
    let mut schunks = src.chunks_exact(S::f32s::LEN);
    for (d, s) in dchunks.by_ref().zip(schunks.by_ref()) {
        let dv = S::f32s::from_slice(simd, d);
        let sv = S::f32s::from_slice(simd, s);
        (dv + sv.is_finite().select(sv, zero)).store_slice(d);
    }
    for (d, s) in dchunks.into_remainder().iter_mut().zip(schunks.remainder()) {
        *d += if s.is_finite() { *s } else { 0.0 };
    }
}

#[simd]
fn copy_sanitized_inplace_kernel<S: Simd>(simd: S, dst: &mut [f32], src: &[f32]) {
    let n = dst.len().min(src.len());
    let dst = &mut dst[..n];
    let src = &src[..n];
    let zero = S::f32s::splat(simd, 0.0);
    let mut dchunks = dst.chunks_exact_mut(S::f32s::LEN);
    let mut schunks = src.chunks_exact(S::f32s::LEN);
    for (d, s) in dchunks.by_ref().zip(schunks.by_ref()) {
        let sv = S::f32s::from_slice(simd, s);
        sv.is_finite().select(sv, zero).store_slice(d);
    }
    for (d, s) in dchunks.into_remainder().iter_mut().zip(schunks.remainder()) {
        *d = if s.is_finite() { *s } else { 0.0 };
    }
}

#[simd]
fn sanitize_finite_inplace_kernel<S: Simd>(simd: S, buf: &mut [f32]) {
    let zero = S::f32s::splat(simd, 0.0);
    let mut chunks = buf.chunks_exact_mut(S::f32s::LEN);
    for b in chunks.by_ref() {
        let v = S::f32s::from_slice(simd, b);
        v.is_finite().select(v, zero).store_slice(b);
    }
    for s in chunks.into_remainder() {
        if !s.is_finite() {
            *s = 0.0;
        }
    }
}

#[simd]
fn peak_abs_kernel<S: Simd>(simd: S, buf: &[f32]) -> f32 {
    let mut acc = S::f32s::splat(simd, 0.0);
    let mut chunks = buf.chunks_exact(S::f32s::LEN);
    for c in chunks.by_ref() {
        acc = acc.max(S::f32s::from_slice(simd, c).abs());
    }
    let mut m = acc.reduce_max();
    for v in chunks.remainder() {
        m = m.max(v.abs());
    }
    m
}

#[simd]
fn clamp_inplace_kernel<S: Simd>(simd: S, buf: &mut [f32], min: f32, max: f32) {
    let vmin = S::f32s::splat(simd, min);
    let vmax = S::f32s::splat(simd, max);
    let mut chunks = buf.chunks_exact_mut(S::f32s::LEN);
    for b in chunks.by_ref() {
        let v = S::f32s::from_slice(simd, b);
        v.min(vmax).max(vmin).store_slice(b);
    }
    for s in chunks.into_remainder() {
        *s = s.clamp(min, max);
    }
}

#[simd]
fn convert_i32_to_f32_kernel<S: Simd>(simd: S, src: &[i32], dst: &mut [f32], gain: f32) {
    let n = dst.len().min(src.len());
    let dst = &mut dst[..n];
    let src = &src[..n];
    let mut dchunks = dst.chunks_exact_mut(S::f32s::LEN);
    let mut schunks = src.chunks_exact(S::i32s::LEN);
    for (d, s) in dchunks.by_ref().zip(schunks.by_ref()) {
        let v = S::i32s::from_slice(simd, s);
        (v.to_float::<S::f32s>() * gain).store_slice(d);
    }
    for (d, s) in dchunks.into_remainder().iter_mut().zip(schunks.remainder()) {
        *d = *s as f32 * gain;
    }
}

#[simd]
fn convert_i16_to_f32_kernel<S: Simd>(simd: S, src: &[i16], dst: &mut [f32], gain: f32) {
    let n = dst.len().min(src.len());
    let dst = &mut dst[..n];
    let src = &src[..n];
    let half = S::i32s::LEN;
    let mut dchunks = dst.chunks_exact_mut(2 * half);
    let mut schunks = src.chunks_exact(S::i16s::LEN);
    for (d, s) in dchunks.by_ref().zip(schunks.by_ref()) {
        let (lo, hi) = S::i16s::from_slice(simd, s).widen();
        (lo.to_float::<S::f32s>() * gain).store_slice(&mut d[..half]);
        (hi.to_float::<S::f32s>() * gain).store_slice(&mut d[half..]);
    }
    for (d, s) in dchunks.into_remainder().iter_mut().zip(schunks.remainder()) {
        *d = *s as f32 * gain;
    }
}

#[simd]
fn convert_i8_to_f32_kernel<S: Simd>(simd: S, src: &[i8], dst: &mut [f32], gain: f32) {
    let n = dst.len().min(src.len());
    let dst = &mut dst[..n];
    let src = &src[..n];
    let quarter = S::i32s::LEN;
    let mut dchunks = dst.chunks_exact_mut(4 * quarter);
    let mut schunks = src.chunks_exact(S::i8s::LEN);
    for (d, s) in dchunks.by_ref().zip(schunks.by_ref()) {
        let (lo, hi) = S::i8s::from_slice(simd, s).widen();
        let (l0, l1) = lo.widen();
        let (h0, h1) = hi.widen();
        (l0.to_float::<S::f32s>() * gain).store_slice(&mut d[..quarter]);
        (l1.to_float::<S::f32s>() * gain).store_slice(&mut d[quarter..2 * quarter]);
        (h0.to_float::<S::f32s>() * gain).store_slice(&mut d[2 * quarter..3 * quarter]);
        (h1.to_float::<S::f32s>() * gain).store_slice(&mut d[3 * quarter..]);
    }
    for (d, s) in dchunks.into_remainder().iter_mut().zip(schunks.remainder()) {
        *d = *s as f32 * gain;
    }
}

#[simd]
fn convert_i24_to_f32_kernel<S: Simd>(simd: S, src: &[i32], dst: &mut [f32], gain: f32) {
    let n = dst.len().min(src.len());
    let dst = &mut dst[..n];
    let src = &src[..n];
    let mut dchunks = dst.chunks_exact_mut(S::f32s::LEN);
    let mut schunks = src.chunks_exact(S::i32s::LEN);
    for (d, s) in dchunks.by_ref().zip(schunks.by_ref()) {
        let v = S::i32s::from_slice(simd, s);
        // Sign-extend the low 24 bits: shift the sign bit into place, then back arithmetically.
        let ext = (v << 8u32) >> 8u32;
        (ext.to_float::<S::f32s>() * gain).store_slice(d);
    }
    for (d, s) in dchunks.into_remainder().iter_mut().zip(schunks.remainder()) {
        let mut v = *s & 0x00FF_FFFF;
        if (v & 0x0080_0000) != 0 {
            v |= 0xFF00_0000u32 as i32;
        }
        *d = v as f32 * gain;
    }
}

#[simd]
fn convert_f32_to_i32_kernel<S: Simd>(simd: S, src: &[f32], dst: &mut [i32], gain: f32) {
    let n = dst.len().min(src.len());
    let dst = &mut dst[..n];
    let src = &src[..n];
    let mut dchunks = dst.chunks_exact_mut(S::i32s::LEN);
    let mut schunks = src.chunks_exact(S::f32s::LEN);
    for (d, s) in dchunks.by_ref().zip(schunks.by_ref()) {
        let v = S::f32s::from_slice(simd, s) * gain;
        let ints: S::i32s = v.to_int_precise();
        ints.store_slice(d);
    }
    for (d, s) in dchunks.into_remainder().iter_mut().zip(schunks.remainder()) {
        *d = (*s * gain) as i32;
    }
}

#[simd]
fn convert_f32_to_i16_kernel<S: Simd>(simd: S, src: &[f32], dst: &mut [i16], gain: f32) {
    let n = dst.len().min(src.len());
    let dst = &mut dst[..n];
    let src = &src[..n];
    let l = S::f32s::LEN;
    let mut dchunks = dst.chunks_exact_mut(2 * l);
    let mut schunks = src.chunks_exact(2 * l);
    for (d, s) in dchunks.by_ref().zip(schunks.by_ref()) {
        let v0 = S::f32s::from_slice(simd, &s[..l]) * gain;
        let v1 = S::f32s::from_slice(simd, &s[l..]) * gain;
        let i0: S::i32s = v0.to_int_precise();
        let i1: S::i32s = v1.to_int_precise();
        // Saturating narrowing matches the old `packs` behavior and Rust's `as` casts.
        i0.saturating_narrow(i1).store_slice(d);
    }
    for (d, s) in dchunks.into_remainder().iter_mut().zip(schunks.remainder()) {
        *d = (*s * gain) as i16;
    }
}

#[simd]
fn convert_f32_to_i8_kernel<S: Simd>(simd: S, src: &[f32], dst: &mut [i8], gain: f32) {
    let n = dst.len().min(src.len());
    let dst = &mut dst[..n];
    let src = &src[..n];
    let l = S::f32s::LEN;
    let mut dchunks = dst.chunks_exact_mut(4 * l);
    let mut schunks = src.chunks_exact(4 * l);
    for (d, s) in dchunks.by_ref().zip(schunks.by_ref()) {
        let i0: S::i32s = (S::f32s::from_slice(simd, &s[..l]) * gain).to_int_precise();
        let i1: S::i32s = (S::f32s::from_slice(simd, &s[l..2 * l]) * gain).to_int_precise();
        let i2: S::i32s = (S::f32s::from_slice(simd, &s[2 * l..3 * l]) * gain).to_int_precise();
        let i3: S::i32s = (S::f32s::from_slice(simd, &s[3 * l..]) * gain).to_int_precise();
        let n0 = i0.saturating_narrow(i1);
        let n1 = i2.saturating_narrow(i3);
        n0.saturating_narrow(n1).store_slice(d);
    }
    for (d, s) in dchunks.into_remainder().iter_mut().zip(schunks.remainder()) {
        *d = (*s * gain) as i8;
    }
}

#[simd]
fn convert_f32_to_i24_kernel<S: Simd>(simd: S, src: &[f32], dst: &mut [i32], gain: f32) {
    let n = dst.len().min(src.len());
    let dst = &mut dst[..n];
    let src = &src[..n];
    let mask = S::i32s::splat(simd, 0x00FF_FFFF);
    let mut dchunks = dst.chunks_exact_mut(S::i32s::LEN);
    let mut schunks = src.chunks_exact(S::f32s::LEN);
    for (d, s) in dchunks.by_ref().zip(schunks.by_ref()) {
        let v = S::f32s::from_slice(simd, s) * gain;
        let ints: S::i32s = v.to_int_precise();
        (ints & mask).store_slice(d);
    }
    for (d, s) in dchunks.into_remainder().iter_mut().zip(schunks.remainder()) {
        *d = (*s * gain) as i32 & 0x00FF_FFFF;
    }
}

#[simd]
fn interleave_stereo_kernel<S: Simd>(simd: S, l: &[f32], r: &[f32], dst: &mut [f32]) {
    let n = l.len().min(r.len()).min(dst.len() / 2);
    let l = &l[..n];
    let r = &r[..n];
    let dst = &mut dst[..2 * n];
    let len = S::f32s::LEN;
    let mut lchunks = l.chunks_exact(len);
    let mut rchunks = r.chunks_exact(len);
    let mut dchunks = dst.chunks_exact_mut(2 * len);
    for ((lc, rc), d) in lchunks.by_ref().zip(rchunks.by_ref()).zip(dchunks.by_ref()) {
        let lv = S::f32s::from_slice(simd, lc);
        let rv = S::f32s::from_slice(simd, rc);
        let (i0, i1) = lv.interleave(rv);
        i0.store_slice(&mut d[..len]);
        i1.store_slice(&mut d[len..]);
    }
    let lrem = lchunks.remainder();
    let rrem = rchunks.remainder();
    let drem = dchunks.into_remainder();
    for i in 0..lrem.len() {
        drem[2 * i] = lrem[i];
        drem[2 * i + 1] = rrem[i];
    }
}

#[simd]
fn apply_fade_in_kernel<S: Simd>(simd: S, buf: &mut [f32], start_t: f32, dt: f32) {
    let len = S::f32s::LEN;
    let mut chunks = buf.chunks_exact_mut(len);
    let mut base = 0usize;
    for chunk in chunks.by_ref() {
        let mut gain = [0.0f32; 16];
        for (lane, g) in gain.iter_mut().enumerate() {
            let t = (start_t + (base + lane) as f32 * dt).clamp(0.0, 1.0);
            *g = (t * std::f32::consts::FRAC_PI_2).sin();
        }
        let gv = S::f32s::from_slice(simd, &gain[..len]);
        (S::f32s::from_slice(simd, chunk) * gv).store_slice(chunk);
        base += len;
    }
    for (j, v) in chunks.into_remainder().iter_mut().enumerate() {
        let t = (start_t + (base + j) as f32 * dt).clamp(0.0, 1.0);
        *v *= (t * std::f32::consts::FRAC_PI_2).sin();
    }
}

#[simd]
fn apply_fade_out_kernel<S: Simd>(simd: S, buf: &mut [f32], start_t: f32, dt: f32) {
    let len = S::f32s::LEN;
    let mut chunks = buf.chunks_exact_mut(len);
    let mut base = 0usize;
    for chunk in chunks.by_ref() {
        let mut gain = [0.0f32; 16];
        for (lane, g) in gain.iter_mut().enumerate() {
            let t = (start_t + (base + lane) as f32 * dt).clamp(0.0, 1.0);
            *g = (t * std::f32::consts::FRAC_PI_2).cos();
        }
        let gv = S::f32s::from_slice(simd, &gain[..len]);
        (S::f32s::from_slice(simd, chunk) * gv).store_slice(chunk);
        base += len;
    }
    for (j, v) in chunks.into_remainder().iter_mut().enumerate() {
        let t = (start_t + (base + j) as f32 * dt).clamp(0.0, 1.0);
        *v *= (t * std::f32::consts::FRAC_PI_2).cos();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_inplace_basic() {
        let mut a = [1.0f32, 2.0, 3.0, 4.0, 5.0];
        let b = [10.0f32, 20.0, 30.0, 40.0, 50.0];
        add_inplace(&mut a, &b);
        assert_eq!(a, [11.0, 22.0, 33.0, 44.0, 55.0]);
    }

    #[test]
    fn mul_inplace_basic() {
        let mut a = [1.0f32, 2.0, 3.0, 4.0, 5.0];
        mul_inplace(&mut a, 2.0);
        assert_eq!(a, [2.0, 4.0, 6.0, 8.0, 10.0]);
    }

    #[test]
    fn add_scaled_inplace_basic() {
        let mut a = [1.0f32, 2.0, 3.0, 4.0, 5.0];
        let b = [10.0f32, 20.0, 30.0, 40.0, 50.0];
        add_scaled_inplace(&mut a, &b, 0.5);
        assert_eq!(a, [6.0, 12.0, 18.0, 24.0, 30.0]);
    }

    #[test]
    fn copy_scaled_inplace_basic() {
        let mut a = [0.0f32; 5];
        let b = [10.0f32, 20.0, 30.0, 40.0, 50.0];
        copy_scaled_inplace(&mut a, &b, 0.5);
        assert_eq!(a, [5.0, 10.0, 15.0, 20.0, 25.0]);
    }

    #[test]
    fn add_sanitized_inplace_basic() {
        let mut a = [1.0f32, 2.0, 3.0, 4.0];
        let b = [0.5f32, f32::NAN, f32::INFINITY, 1.0];
        add_sanitized_inplace(&mut a, &b);
        assert!(a[0].is_finite() && a[0] == 1.5);
        assert!(a[1].is_finite() && a[1] == 2.0);
        assert!(a[2].is_finite() && a[2] == 3.0);
        assert!(a[3].is_finite() && a[3] == 5.0);
    }

    #[test]
    fn copy_sanitized_inplace_basic() {
        let mut a = [0.0f32; 4];
        let b = [0.5f32, f32::NAN, f32::INFINITY, 1.0];
        copy_sanitized_inplace(&mut a, &b);
        assert!(a[0].is_finite() && a[0] == 0.5);
        assert!(a[1].is_finite() && a[1] == 0.0);
        assert!(a[2].is_finite() && a[2] == 0.0);
        assert!(a[3].is_finite() && a[3] == 1.0);
    }

    #[test]
    fn sanitize_finite_inplace_basic() {
        let mut a = [1.0f32, f32::NAN, f32::INFINITY, 4.0, f32::NEG_INFINITY];
        sanitize_finite_inplace(&mut a);
        assert!(a[0].is_finite() && a[0] == 1.0);
        assert_eq!(a[1], 0.0);
        assert_eq!(a[2], 0.0);
        assert!(a[3].is_finite() && a[3] == 4.0);
        assert_eq!(a[4], 0.0);
    }

    #[test]
    fn peak_abs_basic() {
        let a = [1.0f32, -3.0, 2.0, 0.5];
        assert_eq!(peak_abs(&a), 3.0);
    }

    #[test]
    fn clamp_inplace_basic() {
        let mut a = [-2.0f32, -0.5, 0.0, 0.5, 2.0];
        clamp_inplace(&mut a, -1.0, 1.0);
        assert_eq!(a, [-1.0, -0.5, 0.0, 0.5, 1.0]);
    }

    #[test]
    fn convert_i32_to_f32_matches_scalar() {
        let src: Vec<i32> = (0..17).map(|i| i * 123_456_789 - 1_000_000_000).collect();
        let mut dst = vec![0.0f32; src.len()];
        convert_i32_to_f32(&src, &mut dst, 0.5);
        for (s, d) in src.iter().zip(dst.iter()) {
            assert_eq!(*d, *s as f32 * 0.5);
        }
    }

    #[test]
    fn convert_i16_to_f32_matches_scalar() {
        let src: Vec<i16> = (0..19).map(|i| (i * 7000 - 60_000) as i16).collect();
        let mut dst = vec![0.0f32; src.len()];
        convert_i16_to_f32(&src, &mut dst, 1.0);
        for (s, d) in src.iter().zip(dst.iter()) {
            assert_eq!(*d, *s as f32);
        }
    }

    #[test]
    fn convert_i8_to_f32_matches_scalar() {
        let src: Vec<i8> = (0..21).map(|i| (i * 17 - 180) as i8).collect();
        let mut dst = vec![0.0f32; src.len()];
        convert_i8_to_f32(&src, &mut dst, 2.0);
        for (s, d) in src.iter().zip(dst.iter()) {
            assert_eq!(*d, *s as f32 * 2.0);
        }
    }

    #[test]
    fn convert_i24_to_f32_sign_extends() {
        // i24 samples packed in the low 24 bits of each i32 lane.
        let src = [
            0i32,
            1,
            -1,          // 0xFFFFFFFF -> -1
            0x007F_FFFF, // max positive i24
            0x0080_0000, // min negative i24 (sign bit set)
            0x00FF_FFFF, // all low bits set -> -1
            0x1234_5678, // high byte ignored
            42,
            -42,
            0x00AB_CDEF, // negative when sign-extended
        ];
        let mut dst = vec![0.0f32; src.len()];
        convert_i24_to_f32(&src, &mut dst, 1.0);
        let expected = [
            0.0f32,
            1.0,
            -1.0,
            8_388_607.0,
            -8_388_608.0,
            -1.0,
            0x34_5678 as f32,
            42.0,
            -42.0,
            -5_517_841.0,
        ];
        assert_eq!(dst, expected);
    }

    #[test]
    fn convert_f32_to_i32_truncates() {
        let src = [
            0.0f32,
            0.9,
            -0.9,
            1.5,
            -1.5,
            2.5,
            -2.5,
            123_456.79,
            -123_456.79,
            7.0,
            -3.0,
        ];
        let mut dst = vec![0i32; src.len()];
        convert_f32_to_i32(&src, &mut dst, 1.0);
        let expected: Vec<i32> = src.iter().map(|s| *s as i32).collect();
        assert_eq!(dst, expected);
        assert_eq!(dst[3], 1);
        assert_eq!(dst[4], -1);
    }

    #[test]
    fn convert_f32_to_i16_saturates() {
        // `cvttps` + `packs` saturates like Rust's `as` for values in i32 range.
        let src = [
            0.0f32, 0.9, -0.9, 1.5, -1.5, 32767.0, -32768.0, 100_000.0, -100_000.0, 5.0, -7.0,
            123.456,
        ];
        let mut dst = vec![0i16; src.len()];
        convert_f32_to_i16(&src, &mut dst, 1.0);
        let expected: Vec<i16> = src.iter().map(|s| *s as i16).collect();
        assert_eq!(dst, expected);
        assert_eq!(dst[7], i16::MAX);
        assert_eq!(dst[8], i16::MIN);
    }

    #[test]
    fn convert_f32_to_i8_saturates() {
        let src = [
            0.0f32, 0.9, -0.9, 1.5, -1.5, 127.0, -128.0, 200.0, -200.0, 5.0, -7.0, 63.0, -64.0,
            12.0,
        ];
        let mut dst = vec![0i8; src.len()];
        convert_f32_to_i8(&src, &mut dst, 1.0);
        let expected: Vec<i8> = src.iter().map(|s| *s as i8).collect();
        assert_eq!(dst, expected);
        assert_eq!(dst[7], i8::MAX);
        assert_eq!(dst[8], i8::MIN);
    }

    #[test]
    fn convert_f32_to_i24_masks_low_bits() {
        let src = [
            0.0f32,
            0.9,
            -0.9,
            5.9,
            -5.9,
            300.0,
            -300.0,
            8_388_607.0,
            -8_388_608.0,
            1.0,
            -1.0,
            16_777_215.0,
            1e7,
        ];
        let mut dst = vec![0i32; src.len()];
        convert_f32_to_i24(&src, &mut dst, 1.0);
        let expected: Vec<i32> = src.iter().map(|s| (*s as i32) & 0x00FF_FFFF).collect();
        assert_eq!(dst, expected);
        assert_eq!(dst[7], 0x007F_FFFF);
        // -8388608 truncates to i32 -8388608, mask keeps all 24 bits.
        assert_eq!(dst[8], 0x0080_0000);
        // -1 truncates to -1, mask keeps the low 24 bits (0x00FF_FFFF).
        assert_eq!(dst[10], 0x00FF_FFFF);
    }

    #[test]
    fn apply_fade_in_matches_scalar() {
        let mut buf: Vec<f32> = (0..37).map(|i| (i % 7) as f32 * 0.25 - 0.5).collect();
        let mut expected = buf.clone();
        let (start_t, dt) = (0.1f32, 0.023);
        apply_fade_in_inplace(&mut buf, start_t, dt);
        for (i, v) in expected.iter_mut().enumerate() {
            let t = (start_t + i as f32 * dt).clamp(0.0, 1.0);
            *v *= (t * std::f32::consts::FRAC_PI_2).sin();
        }
        assert_eq!(buf, expected);
    }

    #[test]
    fn apply_fade_out_matches_scalar() {
        let mut buf: Vec<f32> = (0..29).map(|i| (i % 5) as f32 * 0.5 - 1.0).collect();
        let mut expected = buf.clone();
        let (start_t, dt) = (0.0f32, 0.041);
        apply_fade_out_inplace(&mut buf, start_t, dt);
        for (i, v) in expected.iter_mut().enumerate() {
            let t = (start_t + i as f32 * dt).clamp(0.0, 1.0);
            *v *= (t * std::f32::consts::FRAC_PI_2).cos();
        }
        assert_eq!(buf, expected);
    }

    #[test]
    fn interleave_stereo_matches_scalar() {
        let l: Vec<f32> = (0..23).map(|i| i as f32 * 0.5 - 5.0).collect();
        let r: Vec<f32> = (0..23).map(|i| 5.0 - i as f32 * 0.25).collect();
        let mut dst = vec![0.0f32; 2 * l.len()];
        interleave_stereo(&l, &r, &mut dst);
        for i in 0..l.len() {
            assert_eq!(dst[2 * i], l[i]);
            assert_eq!(dst[2 * i + 1], r[i]);
        }
    }
}
