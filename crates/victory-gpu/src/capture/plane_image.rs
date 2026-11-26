//! Lossless present-plane pixels, without row padding or resampling.
use std::io::{self, Write};
use zlib_rs::{Deflate, DeflateFlush, Status};

pub(super) fn extension(view: i32) -> Option<&'static str> {
    match view {
        28 | 87 | 24 => Some("png"),
        10 => Some("rgba16f-le"), // PNG cannot preserve signed/HDR/infinite half values.
        _ => None,
    }
}

fn chunk(out: &mut impl Write, tag: &[u8; 4], bytes: &[u8]) -> io::Result<()> {
    out.write_all(&(bytes.len() as u32).to_be_bytes())?;
    out.write_all(tag)?;
    out.write_all(bytes)?;
    let crc = zlib_rs::crc32::crc32(zlib_rs::crc32::crc32(0, tag), bytes);
    out.write_all(&crc.to_be_bytes())
}

pub(super) fn write_image(
    mut out: impl Write,
    view: i32,
    width: u32,
    height: u32,
    pitch: usize,
    data: &[u8],
) -> io::Result<()> {
    let error = || io::Error::new(io::ErrorKind::InvalidData, "invalid present-plane readback");
    let raw_half = view == 10;
    let row_len = (width as usize)
        .checked_mul(if raw_half { 8 } else { 4 })
        .ok_or_else(error)?;
    if extension(view).is_none()
        || width == 0
        || height == 0
        || pitch < row_len
        || (height as usize - 1)
            .checked_mul(pitch)
            .and_then(|last| last.checked_add(row_len))
            .is_none_or(|len| len > data.len())
    {
        return Err(error());
    }
    let rows = data
        .chunks(pitch)
        .take(height as usize)
        .map(|row| &row[..row_len]);
    if raw_half {
        for row in rows {
            out.write_all(row)?;
        }
        return Ok(());
    }
    let depth = if view == 24 { 16 } else { 8 };
    out.write_all(b"\x89PNG\r\n\x1a\n")?;
    let mut header = [0; 13];
    header[..4].copy_from_slice(&width.to_be_bytes());
    header[4..8].copy_from_slice(&height.to_be_bytes());
    header[8..10].copy_from_slice(&[depth, 6]); // RGBA
    chunk(&mut out, b"IHDR", &header)?;
    let mut compressor = Deflate::new(1, true, 15);
    let mut compressed = vec![0; 64 << 10];
    let mut encode = |mut input: &[u8], flush| -> io::Result<()> {
        loop {
            let (before_in, before_out) = (compressor.total_in(), compressor.total_out());
            let state = compressor
                .compress(input, &mut compressed, flush)
                .map_err(|_| error())?;
            let used = (compressor.total_in() - before_in) as usize;
            let made = (compressor.total_out() - before_out) as usize;
            if made != 0 {
                chunk(&mut out, b"IDAT", &compressed[..made])?;
            }
            input = &input[used..];
            if state == Status::StreamEnd || (input.is_empty() && flush == DeflateFlush::NoFlush) {
                return Ok(());
            }
            if used == 0 && made == 0 {
                return Err(error());
            }
        }
    };
    let mut scan = Vec::with_capacity(1 + width as usize * 4 * usize::from(depth / 8));
    for row in rows {
        scan.clear();
        scan.push(0); // unfiltered source samples
        match view {
            28 => scan.extend_from_slice(row),
            87 => {
                for p in row.chunks_exact(4) {
                    scan.extend([p[2], p[1], p[0], p[3]]);
                }
            }
            24 => {
                for pixel in row.chunks_exact(4) {
                    let bits = u32::from_le_bytes(pixel.try_into().unwrap());
                    for shift in [0, 10, 20] {
                        let v = ((bits >> shift) & 1023) as u16;
                        scan.extend(((v << 6) | (v >> 4)).to_be_bytes());
                    }
                    scan.extend((((bits >> 30) as u16) * 21845).to_be_bytes());
                }
            }
            _ => unreachable!(),
        }
        encode(&scan, DeflateFlush::NoFlush)?;
    }
    encode(&[], DeflateFlush::Finish)?;
    chunk(&mut out, b"IEND", &[])
}
