//! WebAssembly SIMD128 versions of the per-block encoder stages.
//!
//! Every routine here is bit-exact with its scalar counterpart: the DCT
//! keeps the islow fixed-point arithmetic in i32 lanes, and quantization
//! and color conversion use the same integer formulas. Only the data
//! layout changes.
//!
//! The DCT is a modified form of the IJG islow code in `fdct.rs`, and
//! that file's notice applies to it.

use core::arch::wasm32::*;

use alloc::vec::Vec;

use crate::encoder::Operations;
use crate::quantization::QuantizationTable;
use crate::{ImageBuffer, JpegColorType, rgb_to_ycbcr};

pub(crate) struct WasmSimdOperations;

impl Operations for WasmSimdOperations {
    /// Same result as the default. The block stays in registers from
    /// sampling to quantization. Pass 1 runs on the transposed block so each
    /// lane is one row; the second transpose makes pass 2's lanes columns.
    /// Quantization reads pass 2's i32 lanes directly: the islow output fits
    /// in i16 for 8-bit samples, so skipping the narrow changes no value.
    #[inline(always)]
    fn encode_block(
        data: &[u8],
        start_x: usize,
        start_y: usize,
        col_stride: usize,
        row_stride: usize,
        width: usize,
        table: &QuantizationTable,
        q_block: &mut [i16; 64],
    ) {
        let rows = sample_rows(data, start_x, start_y, col_stride, row_stride, width);
        if let Some(dc) = flat_dc(&rows) {
            quantize_dc_only(dc, q_block, table);
            return;
        }
        let pass1 = transpose(dct_pass(transpose(rows), true));
        let lo = dct_1d(pass1.map(|x| i32x4_extend_low_i16x8(x)), false);
        let hi = dct_1d(pass1.map(|x| i32x4_extend_high_i16x8(x)), false);
        quantize_rows(lo, hi, q_block, table);
    }
}

/// For a block whose 64 samples all equal `v`, the islow DCT outputs
/// exactly `64 * v` at DC and zero elsewhere: every difference term is
/// zero, pass 1 scales the row sum `8v` up by `PASS1_BITS`, and pass 2's
/// column sum `256v` descales back down by `PASS1_BITS`. Page frames are
/// mostly flat blocks (backgrounds, and chroma under gray text), so this
/// skips both passes for them.
#[inline(always)]
fn flat_dc(rows: &[v128; 8]) -> Option<i32> {
    let first = i16x8_splat(i16x8_extract_lane::<0>(rows[0]));
    let mut diff = v128_xor(rows[0], first);
    for row in &rows[1..] {
        diff = v128_or(diff, v128_xor(*row, first));
    }
    if v128_any_true(diff) {
        None
    } else {
        Some(i32::from(i16x8_extract_lane::<0>(rows[0])) * 64)
    }
}

/// Quantizes a DC-only block with the same arithmetic as `quantize_i32`.
#[inline(always)]
fn quantize_dc_only(dc: i32, q_block: &mut [i16; 64], table: &QuantizationTable) {
    let (reciprocals, corrections) = table.reciprocals_and_corrections();
    let q = quantize_i32(i32x4_splat(dc), &reciprocals[0..4], &corrections[0..4]);
    let zero = i16x8_splat(0);
    store(&mut q_block[0..8], i16x8_replace_lane::<0>(zero, i32x4_extract_lane::<0>(q) as i16));
    for i in 1..8 {
        store(&mut q_block[i * 8..i * 8 + 8], zero);
    }
}

#[inline(always)]
fn load(src: &[i16]) -> v128 {
    let src: &[i16; 8] = src.try_into().unwrap();
    // SAFETY: `src` is exactly 16 readable bytes; v128_load permits unaligned reads.
    unsafe { v128_load(src.as_ptr() as *const v128) }
}

#[inline(always)]
fn store(dst: &mut [i16], v: v128) {
    let dst: &mut [i16; 8] = dst.try_into().unwrap();
    // SAFETY: `dst` is exactly 16 writable bytes; v128_store permits unaligned writes.
    unsafe { v128_store(dst.as_mut_ptr() as *mut v128, v) }
}

/// Transposes an 8x8 matrix of i16 held as eight row vectors.
#[inline(always)]
fn transpose(r: [v128; 8]) -> [v128; 8] {
    let t0 = i16x8_shuffle::<0, 8, 1, 9, 2, 10, 3, 11>(r[0], r[1]);
    let t1 = i16x8_shuffle::<4, 12, 5, 13, 6, 14, 7, 15>(r[0], r[1]);
    let t2 = i16x8_shuffle::<0, 8, 1, 9, 2, 10, 3, 11>(r[2], r[3]);
    let t3 = i16x8_shuffle::<4, 12, 5, 13, 6, 14, 7, 15>(r[2], r[3]);
    let t4 = i16x8_shuffle::<0, 8, 1, 9, 2, 10, 3, 11>(r[4], r[5]);
    let t5 = i16x8_shuffle::<4, 12, 5, 13, 6, 14, 7, 15>(r[4], r[5]);
    let t6 = i16x8_shuffle::<0, 8, 1, 9, 2, 10, 3, 11>(r[6], r[7]);
    let t7 = i16x8_shuffle::<4, 12, 5, 13, 6, 14, 7, 15>(r[6], r[7]);

    let u0 = i32x4_shuffle::<0, 4, 1, 5>(t0, t2);
    let u1 = i32x4_shuffle::<2, 6, 3, 7>(t0, t2);
    let u2 = i32x4_shuffle::<0, 4, 1, 5>(t1, t3);
    let u3 = i32x4_shuffle::<2, 6, 3, 7>(t1, t3);
    let u4 = i32x4_shuffle::<0, 4, 1, 5>(t4, t6);
    let u5 = i32x4_shuffle::<2, 6, 3, 7>(t4, t6);
    let u6 = i32x4_shuffle::<0, 4, 1, 5>(t5, t7);
    let u7 = i32x4_shuffle::<2, 6, 3, 7>(t5, t7);

    [
        i64x2_shuffle::<0, 2>(u0, u4),
        i64x2_shuffle::<1, 3>(u0, u4),
        i64x2_shuffle::<0, 2>(u1, u5),
        i64x2_shuffle::<1, 3>(u1, u5),
        i64x2_shuffle::<0, 2>(u2, u6),
        i64x2_shuffle::<1, 3>(u2, u6),
        i64x2_shuffle::<0, 2>(u3, u7),
        i64x2_shuffle::<1, 3>(u3, u7),
    ]
}

const CONST_BITS: u32 = 13;
const PASS1_BITS: u32 = 2;

const FIX_0_298631336: i32 = 2446;
const FIX_0_390180644: i32 = 3196;
const FIX_0_541196100: i32 = 4433;
const FIX_0_765366865: i32 = 6270;
const FIX_0_899976223: i32 = 7373;
const FIX_1_175875602: i32 = 9633;
const FIX_1_501321110: i32 = 12299;
const FIX_1_847759065: i32 = 15137;
const FIX_1_961570560: i32 = 16069;
const FIX_2_053119869: i32 = 16819;
const FIX_2_562915447: i32 = 20995;
const FIX_3_072711026: i32 = 25172;

#[inline(always)]
fn mul(v: v128, c: i32) -> v128 {
    i32x4_mul(v, i32x4_splat(c))
}

#[inline(always)]
fn descale(v: v128, n: u32) -> v128 {
    i32x4_shr(i32x4_add(v, i32x4_splat(1 << (n - 1))), n)
}

/// One 1-D islow DCT over four independent lanes, with pass 1's or
/// pass 2's output scaling.
#[inline(always)]
fn dct_1d(x: [v128; 8], pass1: bool) -> [v128; 8] {
    let odd_bits = if pass1 {
        CONST_BITS - PASS1_BITS
    } else {
        CONST_BITS + PASS1_BITS
    };

    let tmp0 = i32x4_add(x[0], x[7]);
    let tmp7 = i32x4_sub(x[0], x[7]);
    let tmp1 = i32x4_add(x[1], x[6]);
    let tmp6 = i32x4_sub(x[1], x[6]);
    let tmp2 = i32x4_add(x[2], x[5]);
    let tmp5 = i32x4_sub(x[2], x[5]);
    let tmp3 = i32x4_add(x[3], x[4]);
    let tmp4 = i32x4_sub(x[3], x[4]);

    let tmp10 = i32x4_add(tmp0, tmp3);
    let tmp13 = i32x4_sub(tmp0, tmp3);
    let tmp11 = i32x4_add(tmp1, tmp2);
    let tmp12 = i32x4_sub(tmp1, tmp2);

    let (o0, o4) = if pass1 {
        (
            i32x4_shl(i32x4_add(tmp10, tmp11), PASS1_BITS),
            i32x4_shl(i32x4_sub(tmp10, tmp11), PASS1_BITS),
        )
    } else {
        (
            descale(i32x4_add(tmp10, tmp11), PASS1_BITS),
            descale(i32x4_sub(tmp10, tmp11), PASS1_BITS),
        )
    };

    let z1 = mul(i32x4_add(tmp12, tmp13), FIX_0_541196100);
    let o2 = descale(i32x4_add(z1, mul(tmp13, FIX_0_765366865)), odd_bits);
    let o6 = descale(i32x4_add(z1, mul(tmp12, -FIX_1_847759065)), odd_bits);

    let z1 = i32x4_add(tmp4, tmp7);
    let z2 = i32x4_add(tmp5, tmp6);
    let z3 = i32x4_add(tmp4, tmp6);
    let z4 = i32x4_add(tmp5, tmp7);
    let z5 = mul(i32x4_add(z3, z4), FIX_1_175875602);

    let tmp4 = mul(tmp4, FIX_0_298631336);
    let tmp5 = mul(tmp5, FIX_2_053119869);
    let tmp6 = mul(tmp6, FIX_3_072711026);
    let tmp7 = mul(tmp7, FIX_1_501321110);
    let z1 = mul(z1, -FIX_0_899976223);
    let z2 = mul(z2, -FIX_2_562915447);
    let z3 = i32x4_add(mul(z3, -FIX_1_961570560), z5);
    let z4 = i32x4_add(mul(z4, -FIX_0_390180644), z5);

    let o7 = descale(i32x4_add(i32x4_add(tmp4, z1), z3), odd_bits);
    let o5 = descale(i32x4_add(i32x4_add(tmp5, z2), z4), odd_bits);
    let o3 = descale(i32x4_add(i32x4_add(tmp6, z2), z3), odd_bits);
    let o1 = descale(i32x4_add(i32x4_add(tmp7, z1), z4), odd_bits);

    [o0, o1, o2, o3, o4, o5, o6, o7]
}

/// Runs the 1-D DCT on eight i16x8 vectors, widening to i32 so the
/// fixed-point products never overflow, and narrows the result back.
/// Both passes' outputs fit in i16 for 8-bit samples, so the saturating
/// narrow never clamps.
#[inline(always)]
fn dct_pass(v: [v128; 8], pass1: bool) -> [v128; 8] {
    let lo = dct_1d(v.map(|x| i32x4_extend_low_i16x8(x)), pass1);
    let hi = dct_1d(v.map(|x| i32x4_extend_high_i16x8(x)), pass1);
    core::array::from_fn(|i| i16x8_narrow_i32x4(lo[i], hi[i]))
}

#[inline(always)]
fn quantize_i32(v: v128, reciprocals: &[i32], corrections: &[i32]) -> v128 {
    let reciprocals: &[i32; 4] = reciprocals.try_into().unwrap();
    let corrections: &[i32; 4] = corrections.try_into().unwrap();
    // SAFETY: both arrays are exactly 16 readable bytes.
    let (reciprocals, corrections) = unsafe {
        (
            v128_load(reciprocals.as_ptr() as *const v128),
            v128_load(corrections.as_ptr() as *const v128),
        )
    };
    let sign = i32x4_shr(v, 31);
    let product = i32x4_shr(
        i32x4_mul(i32x4_add(i32x4_abs(v), corrections), reciprocals),
        QuantizationTable::SHIFT,
    );
    i32x4_sub(v128_xor(product, sign), sign)
}

/// Quantizes a block given as the low and high i32 halves of its eight
/// rows and writes it to `q_block` in zigzag order.
#[inline(always)]
fn quantize_rows(lo: [v128; 8], hi: [v128; 8], q_block: &mut [i16; 64], table: &QuantizationTable) {
    let (reciprocals, corrections) = table.reciprocals_and_corrections();
    let natural = core::array::from_fn(|i| {
        let r = i * 8;
        let lo = quantize_i32(lo[i], &reciprocals[r..r + 4], &corrections[r..r + 4]);
        let hi = quantize_i32(hi[i], &reciprocals[r + 4..r + 8], &corrections[r + 4..r + 8]);
        i16x8_narrow_i32x4(lo, hi)
    });
    store_zigzag(natural, q_block);
}

/// Writes eight natural-order rows to `q_block` in zigzag order. Each
/// output row gathers its lanes from the source rows with a chain of byte
/// shuffles (generated from [`crate::writer::ZIGZAG`]).
#[inline(always)]
fn store_zigzag(r: [v128; 8], q_block: &mut [i16; 64]) {
    let v = i8x16_shuffle::<0, 1, 2, 3, 16, 17, 6, 7, 18, 19, 4, 5, 6, 7, 20, 21>(r[0], r[1]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 4, 5, 16, 17, 8, 9, 10, 11, 12, 13, 14, 15>(v, r[2]);
    store(&mut q_block[0..8], v);
    let v = i8x16_shuffle::<2, 3, 16, 17, 4, 5, 18, 19, 4, 5, 10, 11, 12, 13, 14, 15>(r[2], r[3]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 16, 17, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15>(v, r[4]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 22, 23, 12, 13, 14, 15>(v, r[1]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 24, 25, 26, 27>(v, r[0]);
    store(&mut q_block[8..16], v);
    let v = i8x16_shuffle::<8, 9, 22, 23, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15>(r[1], r[2]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 20, 21, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15>(v, r[3]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 4, 5, 18, 19, 8, 9, 10, 11, 12, 13, 20, 21>(v, r[4]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 10, 11, 18, 19, 14, 15>(v, r[5]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 16, 17, 12, 13, 14, 15>(v, r[6]);
    store(&mut q_block[16..24], v);
    let v = i8x16_shuffle::<6, 7, 24, 25, 4, 5, 6, 7, 8, 9, 10, 11, 26, 27, 8, 9>(r[3], r[2]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 26, 27, 6, 7, 8, 9, 28, 29, 12, 13, 14, 15>(v, r[1]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 4, 5, 28, 29, 30, 31, 10, 11, 12, 13, 14, 15>(v, r[0]);
    store(&mut q_block[24..32], v);
    let v = i8x16_shuffle::<6, 7, 20, 21, 4, 5, 6, 7, 8, 9, 10, 11, 22, 23, 8, 9>(r[4], r[5]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 18, 19, 6, 7, 8, 9, 20, 21, 12, 13, 14, 15>(v, r[6]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 4, 5, 16, 17, 18, 19, 10, 11, 12, 13, 14, 15>(v, r[7]);
    store(&mut q_block[32..40], v);
    let v = i8x16_shuffle::<10, 11, 28, 29, 4, 5, 30, 31, 12, 13, 10, 11, 12, 13, 14, 15>(r[3], r[2]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 30, 31, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15>(v, r[1]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 26, 27, 12, 13, 14, 15>(v, r[4]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 24, 25, 14, 15>(v, r[5]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 22, 23>(v, r[6]);
    store(&mut q_block[40..48], v);
    let v = i8x16_shuffle::<4, 5, 6, 7, 24, 25, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15>(r[7], r[6]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 4, 5, 26, 27, 8, 9, 10, 11, 12, 13, 28, 29>(v, r[5]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 4, 5, 6, 7, 28, 29, 10, 11, 30, 31, 14, 15>(v, r[4]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 30, 31, 12, 13, 14, 15>(v, r[3]);
    store(&mut q_block[48..56], v);
    let v = i8x16_shuffle::<10, 11, 24, 25, 26, 27, 12, 13, 8, 9, 14, 15, 28, 29, 30, 31>(r[6], r[7]);
    let v = i8x16_shuffle::<0, 1, 2, 3, 4, 5, 6, 7, 30, 31, 10, 11, 12, 13, 14, 15>(v, r[5]);
    store(&mut q_block[56..64], v);
}

/// Bit i is set when `block[i] != 0`.
#[inline(always)]
pub(crate) fn nonzero_mask_simd128(block: &[i16; 64]) -> u64 {
    let zero = i16x8_splat(0);
    let mut mask = 0u64;
    for i in 0..8 {
        let v = load(&block[i * 8..i * 8 + 8]);
        let bits = i16x8_bitmask(i16x8_ne(v, zero));
        mask |= u64::from(bits) << (i * 8);
    }
    mask
}

/// Samples an 8x8 block, level-shifted to i16, as eight row vectors.
/// Same values as the scalar `get_block`, vectorized for the two strides
/// the encoder uses (full resolution and 2x horizontal subsampling).
#[inline(always)]
fn sample_rows(
    data: &[u8],
    start_x: usize,
    start_y: usize,
    col_stride: usize,
    row_stride: usize,
    width: usize,
) -> [v128; 8] {
    let bias = i16x8_splat(128);
    // A plain loop rather than `array::from_fn`, whose closure did not
    // inline and returned the rows through memory.
    let mut rows = [bias; 8];
    for (y, out) in rows.iter_mut().enumerate() {
        let start = (start_y + y * row_stride) * width + start_x;
        let row = match col_stride {
            1 => {
                let src: &[u8; 8] = data[start..start + 8].try_into().unwrap();
                // SAFETY: `src` is exactly 8 readable bytes.
                let v = unsafe { v128_load64_zero(src.as_ptr() as *const u64) };
                u16x8_extend_low_u8x16(v)
            }
            2 if start + 16 <= data.len() => {
                let src: &[u8; 16] = data[start..start + 16].try_into().unwrap();
                // SAFETY: `src` is exactly 16 readable bytes.
                let v = unsafe { v128_load(src.as_ptr() as *const v128) };
                // Keeping the low byte of each u16 lane selects even pixels.
                v128_and(v, u16x8_splat(0xff))
            }
            _ => {
                let row: [i16; 8] =
                    core::array::from_fn(|x| i16::from(data[start + x * col_stride]));
                load(&row)
            }
        };
        *out = i16x8_sub(row, bias);
    }
    rows
}

/// Converts 4 RGBA pixels (one per i32 lane) to one YCbCr channel.
#[inline(always)]
fn ycbcr_channel(r: v128, g: v128, b: v128, cr: i32, cg: i32, cb: i32, offset: i32) -> v128 {
    let sum = i32x4_add(i32x4_add(mul(r, cr), mul(g, cg)), mul(b, cb));
    i32x4_shr(i32x4_add(sum, i32x4_splat(offset + 0x7FFF)), 16)
}

#[inline(always)]
fn narrow_u8(v: [v128; 4]) -> [u8; 16] {
    let v = u8x16_narrow_i16x8(
        i16x8_narrow_i32x4(v[0], v[1]),
        i16x8_narrow_i32x4(v[2], v[3]),
    );
    let mut out = [0u8; 16];
    // SAFETY: `out` is exactly 16 writable bytes.
    unsafe { v128_store(out.as_mut_ptr() as *mut v128, v) };
    out
}

/// RGBA input with SIMD128 color conversion. Same output as `RgbaImage`.
pub(crate) struct RgbaImageSimd128<'a>(pub &'a [u8], pub u16, pub u16);

impl RgbaImageSimd128<'_> {
    /// Converts row `y`. Without `CHROMA` only luma is computed and the
    /// chroma buffers get zeros of the same length, for rows the encoder
    /// never reads chroma from. With `EVEN`, chroma is computed for even
    /// pixels and copied into the odd ones after them, for rows the
    /// encoder reads chroma from only at even columns. The last pixel
    /// still gets its own chroma, because edge padding repeats it: a
    /// 16-pixel chunk ends on an odd pixel, but a width that is a multiple
    /// of 16 needs no padding, and any other width ends in the scalar
    /// remainder below.
    #[inline(always)]
    fn fill_row<const CHROMA: bool, const EVEN: bool>(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        let width = usize::from(self.1);
        let start = usize::from(y) * width * 4;
        let line = &self.0[start..start + width * 4];
        let byte = u32x4_splat(0xff);

        let mut chunks = line.chunks_exact(64);
        for chunk in &mut chunks {
            let mut pxs = [i32x4_splat(0); 4];
            let mut ys = pxs;
            let mut cbs = pxs;
            let mut crs = pxs;
            for i in 0..4 {
                let src: &[u8; 16] = chunk[i * 16..i * 16 + 16].try_into().unwrap();
                // SAFETY: `src` is exactly 16 readable bytes.
                let px = unsafe { v128_load(src.as_ptr() as *const v128) };
                pxs[i] = px;
                let r = v128_and(px, byte);
                let g = v128_and(u32x4_shr(px, 8), byte);
                let b = v128_and(u32x4_shr(px, 16), byte);
                ys[i] = ycbcr_channel(r, g, b, 19595, 38470, 7471, 0);
                if CHROMA && !EVEN {
                    cbs[i] = ycbcr_channel(r, g, b, -11059, -21709, 32768, 128 << 16);
                    crs[i] = ycbcr_channel(r, g, b, 32768, -27439, -5329, 128 << 16);
                }
            }
            buffers[0].extend_from_slice(&narrow_u8(ys));
            if CHROMA && EVEN {
                for i in 0..2 {
                    let px = i32x4_shuffle::<0, 2, 4, 6>(pxs[2 * i], pxs[2 * i + 1]);
                    let r = v128_and(px, byte);
                    let g = v128_and(u32x4_shr(px, 8), byte);
                    let b = v128_and(u32x4_shr(px, 16), byte);
                    cbs[i] = ycbcr_channel(r, g, b, -11059, -21709, 32768, 128 << 16);
                    crs[i] = ycbcr_channel(r, g, b, 32768, -27439, -5329, 128 << 16);
                }
                // Bytes 0..8 are the even pixels' Cb, 8..16 their Cr.
                let both = u8x16_narrow_i16x8(
                    i16x8_narrow_i32x4(cbs[0], cbs[1]),
                    i16x8_narrow_i32x4(crs[0], crs[1]),
                );
                let cb = i8x16_shuffle::<0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7>(both, both);
                let cr = i8x16_shuffle::<8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13, 14, 14, 15, 15>(
                    both, both,
                );
                for (buffer, v) in buffers[1..3].iter_mut().zip([cb, cr]) {
                    let mut out = [0u8; 16];
                    // SAFETY: `out` is exactly 16 writable bytes.
                    unsafe { v128_store(out.as_mut_ptr() as *mut v128, v) };
                    buffer.extend_from_slice(&out);
                }
            } else if CHROMA {
                buffers[1].extend_from_slice(&narrow_u8(cbs));
                buffers[2].extend_from_slice(&narrow_u8(crs));
            }
        }

        for px in chunks.remainder().chunks_exact(4) {
            let (y, cb, cr) = rgb_to_ycbcr(px[0], px[1], px[2]);
            buffers[0].push(y);
            if CHROMA {
                buffers[1].push(cb);
                buffers[2].push(cr);
            }
        }

        if !CHROMA {
            for chroma in &mut buffers[1..3] {
                chroma.resize(chroma.len() + width, 0);
            }
        }
    }
}

impl ImageBuffer for RgbaImageSimd128<'_> {
    fn get_jpeg_color_type(&self) -> JpegColorType {
        JpegColorType::Ycbcr
    }

    fn width(&self) -> u16 {
        self.1
    }

    fn height(&self) -> u16 {
        self.2
    }

    fn fill_buffers(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        self.fill_row::<true, false>(y, buffers);
    }

    fn fill_buffers_first_only(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        self.fill_row::<false, false>(y, buffers);
    }

    fn fill_buffers_even_others(&self, y: u16, buffers: &mut [Vec<u8>; 4]) {
        self.fill_row::<true, true>(y, buffers);
    }
}
