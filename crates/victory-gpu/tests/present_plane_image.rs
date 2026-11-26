#[path = "../src/capture/plane_image.rs"]
mod image;

fn scanlines(encoded: &[u8], depth: u8, size: usize) -> Vec<u8> {
    assert_eq!(&encoded[..8], b"\x89PNG\r\n\x1a\n");
    let mut chunks = &encoded[8..];
    let mut compressed = Vec::new();
    while !chunks.is_empty() {
        let len = u32::from_be_bytes(chunks[..4].try_into().unwrap()) as usize;
        let tag = &chunks[4..8];
        let data = &chunks[8..8 + len];
        assert_eq!(
            zlib_rs::crc32::crc32(0, &chunks[4..8 + len]).to_be_bytes(),
            chunks[8 + len..12 + len],
        );
        match tag {
            b"IHDR" => assert_eq!(data, &[0, 0, 0, 2, 0, 0, 0, 2, depth, 6, 0, 0, 0]),
            b"IDAT" => compressed.extend_from_slice(data),
            b"IEND" => assert_eq!(chunks.len(), 12),
            _ => panic!("unexpected PNG chunk"),
        }
        chunks = &chunks[len + 12..];
    }
    let mut bytes = vec![0; size];
    let (decoded, status) = zlib_rs::decompress_slice(&mut bytes, &compressed, Default::default());
    assert_eq!(status, zlib_rs::ReturnCode::Ok);
    assert_eq!(decoded.len(), size);
    bytes
}

#[test]
fn rgba_bgra_and_alpha_survive_padded_rows() {
    let input = [
        1, 2, 3, 0, 4, 5, 6, 127, 99, 99, 99, 99, 7, 8, 9, 255, 10, 11, 12, 198,
    ];
    for (format, rgb) in [
        (
            28,
            [
                0, 1, 2, 3, 0, 4, 5, 6, 127, 0, 7, 8, 9, 255, 10, 11, 12, 198,
            ],
        ),
        (
            87,
            [
                0, 3, 2, 1, 0, 6, 5, 4, 127, 0, 9, 8, 7, 255, 12, 11, 10, 198,
            ],
        ),
    ] {
        let mut png = Vec::new();
        image::write_image(&mut png, format, 2, 2, 12, &input).unwrap();
        assert_eq!(scanlines(&png, 8, rgb.len()), rgb);
        assert!(image::write_image(Vec::new(), format, 2, 2, 12, &input[..19]).is_err());
    }
}

#[test]
fn rgb10a2_round_trips_all_alpha_levels() {
    let pixels: [u32; 4] = [1, 0x7fff_a123, 0x8f62_3456, 0xffff_ffff];
    let packed: Vec<_> = pixels.iter().flat_map(|p| p.to_le_bytes()).collect();
    let mut png = Vec::new();
    image::write_image(&mut png, 24, 2, 2, 8, &packed).unwrap();
    let decoded = scanlines(&png, 16, 34);
    let mut restored = Vec::new();
    for row in decoded.chunks_exact(17) {
        assert_eq!(row[0], 0);
        for rgb in row[1..].chunks_exact(8) {
            let mut channels = rgb
                .chunks_exact(2)
                .map(|n| u16::from_be_bytes([n[0], n[1]]) as u32);
            let r = channels.next().unwrap() >> 6;
            let g = channels.next().unwrap() >> 6;
            let b = channels.next().unwrap() >> 6;
            let a = channels.next().unwrap();
            assert_eq!(a % 21845, 0);
            restored.push(r | (g << 10) | (b << 20) | ((a / 21845) << 30));
        }
    }
    assert_eq!(restored, pixels);
}

#[test]
fn float_planes_preserve_signed_hdr_and_non_finite_bit_patterns() {
    let input = [
        0, 60, 0, 192, 0, 124, 1, 126, 88, 99, 22, 44, 0, 128, 0, 0, 0, 56, 0, 60,
    ];
    let mut raw = Vec::new();
    image::write_image(&mut raw, 10, 1, 2, 12, &input).unwrap();
    assert_eq!(
        raw,
        [0, 60, 0, 192, 0, 124, 1, 126, 0, 128, 0, 0, 0, 56, 0, 60]
    );
}
