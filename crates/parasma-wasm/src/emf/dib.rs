//! Device-Independent Bitmap decode — BITMAPINFOHEADER + BI_RGB 24/32bpp only
//! (the Word-common cases). Everything else (RLE / BITFIELDS / JPEG / PNG-in-DIB
//! / palettized 1/4/8bpp) returns `None` so the blit is skipped (the vector
//! layer still paints). Output is TOP-DOWN RGBA, alpha forced to 255 (BI_RGB's
//! 32bpp X byte is undefined).

const BI_RGB: u32 = 0;

/// A decoded blit: top-down RGBA, ready for `createImageBitmap(ImageData)`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Decoded {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// Decode a DIB given its BITMAPINFOHEADER bytes (`bmi`) and pixel bytes
/// (`bits`). The two are passed separately because BITBLT / STRETCHDIBITS supply
/// independent `offBmiSrc` / `offBitsSrc` offsets — they are NOT necessarily
/// contiguous.
pub(crate) fn decode(bmi: &[u8], bits: &[u8]) -> Option<Decoded> {
    if bmi.len() < 40 {
        return None;
    }
    let rd_u32 = |o: usize| u32::from_le_bytes([bmi[o], bmi[o + 1], bmi[o + 2], bmi[o + 3]]);
    let rd_i32 = |o: usize| rd_u32(o) as i32;
    let rd_u16 = |o: usize| u16::from_le_bytes([bmi[o], bmi[o + 1]]);

    let width = rd_i32(4);
    let height = rd_i32(8);
    let bit_count = rd_u16(14) as u32;
    let compression = rd_u32(16);

    if compression != BI_RGB || width <= 0 {
        return None;
    }
    if bit_count != 24 && bit_count != 32 {
        return None;
    }

    let top_down = height < 0;
    if height == 0 {
        return None;
    }
    // Width and height come from the file. Size the pixel data in u64 and
    // require the bits to hold it before anything is allocated: in usize
    // arithmetic on wasm32, `width * bit_count` wraps for widths past ~134M,
    // the stride comes out small, the check passes, and the row slice below
    // panics. Once the bits are known to cover `stride * h`, every product
    // below is bounded by `bits.len()`.
    let (w, h) = (width as u64, height.unsigned_abs() as u64);
    // 4-byte-aligned row stride (padding must NOT be copied into RGBA).
    let stride = (w * bit_count as u64).div_ceil(32) * 4;
    if (bits.len() as u64) < stride.checked_mul(h)? {
        return None;
    }
    let (w, h, stride) = (w as usize, h as usize, stride as usize);

    let bpp = (bit_count / 8) as usize;
    let mut rgba = vec![0u8; w.checked_mul(h)?.checked_mul(4)?];
    for row in 0..h {
        // Bottom-up (height>0): source row (h-1-row) → dest row `row`.
        let src_row = if top_down { row } else { h - 1 - row };
        let src = &bits[src_row * stride..src_row * stride + w * bpp];
        let dst = &mut rgba[row * w * 4..row * w * 4 + w * 4];
        for x in 0..w {
            let sp = x * bpp;
            let dp = x * 4;
            // DIB byte order is B,G,R (,X).
            dst[dp] = src[sp + 2]; // R
            dst[dp + 1] = src[sp + 1]; // G
            dst[dp + 2] = src[sp]; // B
            dst[dp + 3] = 255; // A (X byte undefined for BI_RGB)
        }
    }

    Some(Decoded { w: w as u32, h: h as u32, rgba })
}
