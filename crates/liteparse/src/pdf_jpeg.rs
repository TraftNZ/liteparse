// Copyright 2011, 2025 The Go Authors. All rights reserved.
// Rust adaptation of image/jpeg and image/color from Go 1.27.1.
// Distributed under the BSD license in ../licenses/Go-JPEG-LICENSE.
//! Deterministic baseline JPEG encoding for the PDF render contract.

use image::RgbImage;
use std::io;

const BLOCK_EDGE: usize = 8;
const BLOCK_SIZE: usize = BLOCK_EDGE * BLOCK_EDGE;
const MCU_EDGE: usize = BLOCK_EDGE * 2;
const COMPONENTS: usize = 3;
const SAMPLE_CENTER: i64 = 128;
const COLOR_PRECISION: u32 = 16;
const COLOR_ROUNDING: i64 = 1 << (COLOR_PRECISION - 1);
const LUMA_WEIGHTS: [i64; 3] = [19595, 38470, 7471];
const CB_WEIGHTS: [i64; 3] = [-11056, -21712, 32768];
const CR_WEIGHTS: [i64; 3] = [32768, -27440, -5328];
const COLUMN_PRECISION: u32 = 18;
const ROW_PRECISION: u32 = 14;
const COLUMN_SQRT_PRECISION: u32 = 12;
const COS_ONE: u64 = 1130768441178740757;
const SIN_ONE: u64 = 224923827593068887;
const COS_THREE: u64 = 958619196450722178;
const SIN_THREE: u64 = 640528868967736374;
const SQRT_TWO: u64 = 1630477228166597777;
const SQRT_TWO_COS_SIX: u64 = 623956622067911264;
const SQRT_TWO_SIN_SIX: u64 = 1506364539328854985;
const CONSTANT_PRECISION: u32 = 60;
const MARKER_PREFIX: u8 = 0xff;
const START_IMAGE: u8 = 0xd8;
const END_IMAGE: u8 = 0xd9;
const QUANTIZATION_MARKER: u8 = 0xdb;
const FRAME_MARKER: u8 = 0xc0;
const HUFFMAN_MARKER: u8 = 0xc4;
const SCAN_MARKER: u8 = 0xda;
const END_BLOCK: usize = 0x00;
const ZERO_RUN: usize = 0xf0;
const MAX_ZERO_RUN: usize = 16;
const HUFFMAN_SYMBOLS: usize = 256;
const HUFFMAN_LENGTHS: usize = 16;
const QUALITY_MIDPOINT: u32 = 50;
const MAX_QUALITY: u32 = 100;
const LOW_QUALITY_SCALE: u32 = 5000;
const HUFFMAN_IDS: [u8; 4] = [0x00, 0x10, 0x01, 0x11];
type Block = [i64; BLOCK_SIZE];

const ZIGZAG: [usize; BLOCK_SIZE] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
const BASE_QUANT: [[u8; BLOCK_SIZE]; 2] = [
    [
        16, 11, 12, 14, 12, 10, 16, 14, 13, 14, 18, 17, 16, 19, 24, 40, 26, 24, 22, 22, 24, 49, 35,
        37, 29, 40, 58, 51, 61, 60, 57, 51, 56, 55, 64, 72, 92, 78, 64, 68, 87, 69, 55, 56, 80,
        109, 81, 87, 95, 98, 103, 104, 103, 62, 77, 113, 121, 112, 100, 120, 92, 101, 103, 99,
    ],
    [
        17, 18, 18, 24, 21, 24, 47, 26, 26, 47, 99, 66, 56, 66, 99, 99, 99, 99, 99, 99, 99, 99, 99,
        99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
        99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    ],
];
const HUFFMAN_COUNTS_0: [u8; HUFFMAN_LENGTHS] = [0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0];
const HUFFMAN_VALUES_0: &[u8] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const HUFFMAN_COUNTS_1: [u8; HUFFMAN_LENGTHS] = [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 125];
const HUFFMAN_VALUES_1: &[u8] = &[
    1, 2, 3, 0, 4, 17, 5, 18, 33, 49, 65, 6, 19, 81, 97, 7, 34, 113, 20, 50, 129, 145, 161, 8, 35,
    66, 177, 193, 21, 82, 209, 240, 36, 51, 98, 114, 130, 9, 10, 22, 23, 24, 25, 26, 37, 38, 39,
    40, 41, 42, 52, 53, 54, 55, 56, 57, 58, 67, 68, 69, 70, 71, 72, 73, 74, 83, 84, 85, 86, 87, 88,
    89, 90, 99, 100, 101, 102, 103, 104, 105, 106, 115, 116, 117, 118, 119, 120, 121, 122, 131,
    132, 133, 134, 135, 136, 137, 138, 146, 147, 148, 149, 150, 151, 152, 153, 154, 162, 163, 164,
    165, 166, 167, 168, 169, 170, 178, 179, 180, 181, 182, 183, 184, 185, 186, 194, 195, 196, 197,
    198, 199, 200, 201, 202, 210, 211, 212, 213, 214, 215, 216, 217, 218, 225, 226, 227, 228, 229,
    230, 231, 232, 233, 234, 241, 242, 243, 244, 245, 246, 247, 248, 249, 250,
];
const HUFFMAN_COUNTS_2: [u8; HUFFMAN_LENGTHS] = [0, 3, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0];
const HUFFMAN_VALUES_2: &[u8] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
const HUFFMAN_COUNTS_3: [u8; HUFFMAN_LENGTHS] = [0, 2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 119];
const HUFFMAN_VALUES_3: &[u8] = &[
    0, 1, 2, 3, 17, 4, 5, 33, 49, 6, 18, 65, 81, 7, 97, 113, 19, 34, 50, 129, 8, 20, 66, 145, 161,
    177, 193, 9, 35, 51, 82, 240, 21, 98, 114, 209, 10, 22, 36, 52, 225, 37, 241, 23, 24, 25, 26,
    38, 39, 40, 41, 42, 53, 54, 55, 56, 57, 58, 67, 68, 69, 70, 71, 72, 73, 74, 83, 84, 85, 86, 87,
    88, 89, 90, 99, 100, 101, 102, 103, 104, 105, 106, 115, 116, 117, 118, 119, 120, 121, 122, 130,
    131, 132, 133, 134, 135, 136, 137, 138, 146, 147, 148, 149, 150, 151, 152, 153, 154, 162, 163,
    164, 165, 166, 167, 168, 169, 170, 178, 179, 180, 181, 182, 183, 184, 185, 186, 194, 195, 196,
    197, 198, 199, 200, 201, 202, 210, 211, 212, 213, 214, 215, 216, 217, 218, 226, 227, 228, 229,
    230, 231, 232, 233, 234, 242, 243, 244, 245, 246, 247, 248, 249, 250,
];
const HUFFMAN_SPECS: [(&[u8; HUFFMAN_LENGTHS], &[u8]); 4] = [
    (&HUFFMAN_COUNTS_0, HUFFMAN_VALUES_0),
    (&HUFFMAN_COUNTS_1, HUFFMAN_VALUES_1),
    (&HUFFMAN_COUNTS_2, HUFFMAN_VALUES_2),
    (&HUFFMAN_COUNTS_3, HUFFMAN_VALUES_3),
];

const fn fixed(value: u64, precision: u32) -> i64 {
    ((value + (1 << (CONSTANT_PRECISION - precision - 1))) >> (CONSTANT_PRECISION - precision))
        as i64
}

fn rotate(a: i64, b: i64, cosine: i64, sine: i64) -> (i64, i64) {
    let sum = cosine * (a + b);
    (sum + (sine - cosine) * b, sum - (cosine + sine) * a)
}

fn transform_line(mut x: [i64; BLOCK_EDGE], column: bool) -> [i64; BLOCK_EDGE] {
    (x[0], x[7]) = (x[0] + x[7], x[0] - x[7]);
    (x[1], x[6]) = (x[1] + x[6], x[1] - x[6]);
    (x[2], x[5]) = (x[2] + x[5], x[2] - x[5]);
    (x[3], x[4]) = (x[3] + x[4], x[3] - x[4]);
    let precision = if column {
        COLUMN_PRECISION
    } else {
        ROW_PRECISION
    };
    let shift = if column { 0 } else { ROW_PRECISION };
    (x[4], x[7]) = rotate(
        x[4] >> shift,
        x[7] >> shift,
        fixed(COS_THREE, precision),
        fixed(SIN_THREE, precision),
    );
    (x[5], x[6]) = rotate(
        x[5] >> shift,
        x[6] >> shift,
        fixed(COS_ONE, precision),
        fixed(SIN_ONE, precision),
    );
    (x[0], x[3]) = (x[0] + x[3], x[0] - x[3]);
    (x[1], x[2]) = (x[1] + x[2], x[1] - x[2]);
    (x[2], x[3]) = rotate(
        x[2] >> shift,
        x[3] >> shift,
        fixed(SQRT_TWO_COS_SIX, precision),
        fixed(SQRT_TWO_SIN_SIX, precision),
    );
    (x[0], x[1]) = (x[0] + x[1], x[0] - x[1]);
    if column {
        x[0] = (x[0] - SAMPLE_CENTER * BLOCK_EDGE as i64) << COLUMN_PRECISION;
        x[1] <<= COLUMN_PRECISION;
    }
    (x[4], x[6]) = (x[4] + x[6], x[4] - x[6]);
    (x[7], x[5]) = (x[7] + x[5], x[7] - x[5]);
    let sqrt_precision = if column {
        COLUMN_SQRT_PRECISION
    } else {
        ROW_PRECISION
    };
    x[5] = (x[5] >> sqrt_precision) * fixed(SQRT_TWO, sqrt_precision);
    x[6] = (x[6] >> sqrt_precision) * fixed(SQRT_TWO, sqrt_precision);
    (x[7], x[4]) = (x[7] + x[4], x[7] - x[4]);
    if !column {
        for value in &mut x {
            *value = (*value + (1 << (COLUMN_PRECISION - 1))) >> COLUMN_PRECISION;
        }
    }
    [x[0], x[7], x[2], x[5], x[1], x[6], x[3], x[4]]
}

fn transform(block: &mut Block) {
    for column in 0..BLOCK_EDGE {
        let result = transform_line(
            std::array::from_fn(|row| block[row * BLOCK_EDGE + column]),
            true,
        );
        for (row, value) in result.into_iter().enumerate() {
            block[row * BLOCK_EDGE + column] = value;
        }
    }
    for row in block.as_chunks_mut::<BLOCK_EDGE>().0 {
        *row = transform_line(*row, false);
    }
}

fn color(pixel: &[u8]) -> [i64; COMPONENTS] {
    let rgb = [
        i64::from(pixel[0]),
        i64::from(pixel[1]),
        i64::from(pixel[2]),
    ];
    let dot = |weights: [i64; COMPONENTS]| {
        weights
            .into_iter()
            .zip(rgb)
            .map(|(weight, value)| weight * value)
            .sum::<i64>()
    };
    let chroma = |weights| {
        ((dot(weights) + (SAMPLE_CENTER << COLOR_PRECISION) + COLOR_ROUNDING) >> COLOR_PRECISION)
            .clamp(0, i64::from(u8::MAX))
    };
    [
        (dot(LUMA_WEIGHTS) + COLOR_ROUNDING) >> COLOR_PRECISION,
        chroma(CB_WEIGHTS),
        chroma(CR_WEIGHTS),
    ]
}

fn rounded_division(value: i64, divisor: i64) -> i64 {
    if value < 0 {
        -((-value + divisor / 2) / divisor)
    } else {
        (value + divisor / 2) / divisor
    }
}

struct BitWriter {
    bytes: Vec<u8>,
    bits: u64,
    count: u32,
}

impl BitWriter {
    fn marker(&mut self, marker: u8, payload: &[u8]) {
        self.bytes.extend_from_slice(&[MARKER_PREFIX, marker]);
        self.bytes
            .extend_from_slice(&((payload.len() + size_of::<u16>()) as u16).to_be_bytes());
        self.bytes.extend_from_slice(payload);
    }

    fn emit(&mut self, bits: u32, count: u32) {
        self.bits = (self.bits << count) | u64::from(bits);
        self.count += count;
        while self.count >= u8::BITS {
            self.count -= u8::BITS;
            let byte = (self.bits >> self.count) as u8;
            self.bytes.push(byte);
            if byte == MARKER_PREFIX {
                self.bytes.push(0);
            }
        }
        self.bits &= (1_u64 << self.count) - 1;
    }

    fn symbol(&mut self, table: &[(u32, u32); HUFFMAN_SYMBOLS], symbol: usize) {
        let (bits, count) = table[symbol];
        self.emit(bits, count);
    }

    fn coefficient(&mut self, table: &[(u32, u32); HUFFMAN_SYMBOLS], run: usize, value: i64) {
        let magnitude = value.unsigned_abs();
        let count = u64::BITS - magnitude.leading_zeros();
        self.symbol(table, run * MAX_ZERO_RUN + count as usize);
        if count > 0 {
            let bits = if value < 0 { value - 1 } else { value };
            self.emit((bits as u64 & ((1_u64 << count) - 1)) as u32, count);
        }
    }

    fn block(
        &mut self,
        mut block: Block,
        quant: &[u8; BLOCK_SIZE],
        tables: &[[(u32, u32); HUFFMAN_SYMBOLS]],
        previous: i64,
    ) -> i64 {
        transform(&mut block);
        let dc = rounded_division(block[0], BLOCK_EDGE as i64 * i64::from(quant[0]));
        self.coefficient(&tables[0], 0, dc - previous);
        let mut run = 0;
        for index in 1..BLOCK_SIZE {
            let value = rounded_division(
                block[ZIGZAG[index]],
                BLOCK_EDGE as i64 * i64::from(quant[index]),
            );
            if value == 0 {
                run += 1;
            } else {
                while run >= MAX_ZERO_RUN {
                    self.symbol(&tables[1], ZERO_RUN);
                    run -= MAX_ZERO_RUN;
                }
                self.coefficient(&tables[1], run, value);
                run = 0;
            }
        }
        if run > 0 {
            self.symbol(&tables[1], END_BLOCK);
        }
        dc
    }
}

fn huffman_tables() -> [[(u32, u32); HUFFMAN_SYMBOLS]; 4] {
    std::array::from_fn(|index| {
        let (counts, values) = HUFFMAN_SPECS[index];
        let mut table = [(0, 0); HUFFMAN_SYMBOLS];
        let mut code = 0;
        let mut offset = 0;
        for (length, &count) in counts.iter().enumerate() {
            for &symbol in &values[offset..offset + usize::from(count)] {
                table[usize::from(symbol)] = (code, length as u32 + 1);
                code += 1;
            }
            offset += usize::from(count);
            code <<= 1;
        }
        table
    })
}

pub(crate) fn encode(image: &RgbImage, quality: u8) -> Result<Vec<u8>, io::Error> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "JPEG dimensions must be in 1..=65535",
        )
    };
    let width = u16::try_from(image.width()).map_err(|_| invalid())?;
    let height = u16::try_from(image.height()).map_err(|_| invalid())?;
    if width == 0 || height == 0 {
        return Err(invalid());
    }
    let quality = u32::from(quality).clamp(1, MAX_QUALITY);
    let scale = if quality < QUALITY_MIDPOINT {
        LOW_QUALITY_SCALE / quality
    } else {
        (MAX_QUALITY - quality) * 2
    };
    let quant: [[u8; BLOCK_SIZE]; 2] = BASE_QUANT.map(|table| {
        table.map(|value| {
            ((u32::from(value) * scale + QUALITY_MIDPOINT) / MAX_QUALITY)
                .clamp(1, u32::from(u8::MAX)) as u8
        })
    });
    let mut writer = BitWriter {
        bytes: vec![MARKER_PREFIX, START_IMAGE],
        bits: 0,
        count: 0,
    };
    let mut payload = Vec::new();
    for (index, table) in quant.iter().enumerate() {
        payload.push(index as u8);
        payload.extend_from_slice(table);
    }
    writer.marker(QUANTIZATION_MARKER, &payload);
    payload.clear();
    payload.push(u8::BITS as u8);
    payload.extend_from_slice(&height.to_be_bytes());
    payload.extend_from_slice(&width.to_be_bytes());
    payload.extend_from_slice(&[COMPONENTS as u8, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
    writer.marker(FRAME_MARKER, &payload);
    payload.clear();
    for (index, (counts, values)) in HUFFMAN_SPECS.iter().enumerate() {
        payload.push(HUFFMAN_IDS[index]);
        payload.extend_from_slice(*counts);
        payload.extend_from_slice(values);
    }
    writer.marker(HUFFMAN_MARKER, &payload);
    writer.marker(
        SCAN_MARKER,
        &[
            COMPONENTS as u8,
            1,
            0,
            2,
            0x11,
            3,
            0x11,
            0,
            (BLOCK_SIZE - 1) as u8,
            0,
        ],
    );
    let tables = huffman_tables();
    let mut previous = [0; COMPONENTS];
    let width = usize::from(width);
    let height = usize::from(height);
    let raw = image.as_raw();
    for top in (0..height).step_by(MCU_EDGE) {
        for left in (0..width).step_by(MCU_EDGE) {
            let mut chroma = [[0; BLOCK_SIZE]; 2];
            let mut sums = [[[0; MCU_EDGE]; MCU_EDGE]; 2];
            for quadrant in 0..4 {
                let x_offset = (quadrant % 2) * BLOCK_EDGE;
                let y_offset = (quadrant / 2) * BLOCK_EDGE;
                let mut luma = [0; BLOCK_SIZE];
                for y in 0..BLOCK_EDGE {
                    for x in 0..BLOCK_EDGE {
                        let source_x = (left + x_offset + x).min(width - 1);
                        let source_y = (top + y_offset + y).min(height - 1);
                        let offset = (source_y * width + source_x) * COMPONENTS;
                        let values = color(&raw[offset..offset + COMPONENTS]);
                        luma[y * BLOCK_EDGE + x] = values[0];
                        for component in 0..2 {
                            sums[component][y_offset + y][x_offset + x] = values[component + 1];
                        }
                    }
                }
                previous[0] = writer.block(luma, &quant[0], &tables[..2], previous[0]);
            }
            for component in 0..2 {
                for y in 0..BLOCK_EDGE {
                    for x in 0..BLOCK_EDGE {
                        let sum = sums[component][y * 2][x * 2]
                            + sums[component][y * 2][x * 2 + 1]
                            + sums[component][y * 2 + 1][x * 2]
                            + sums[component][y * 2 + 1][x * 2 + 1];
                        chroma[component][y * BLOCK_EDGE + x] = (sum + 2) / 4;
                    }
                }
                previous[component + 1] = writer.block(
                    chroma[component],
                    &quant[1],
                    &tables[2..],
                    previous[component + 1],
                );
            }
        }
    }
    writer.emit((1 << (u8::BITS - 1)) - 1, u8::BITS - 1);
    writer.bytes.extend_from_slice(&[MARKER_PREFIX, END_IMAGE]);
    Ok(writer.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PDF_JPEG_QUALITY: u8 = 80;

    #[test]
    fn actual_pdf_rgb_matches_reference_jpeg_bytes() {
        let rgb = include_bytes!("../tests/fixtures/jpeg_reference.rgb").to_vec();
        let image = RgbImage::from_raw(200, 200, rgb).unwrap();
        assert_eq!(
            encode(&image, PDF_JPEG_QUALITY).unwrap(),
            include_bytes!("../tests/fixtures/jpeg_reference.jpg")
        );
    }

    #[test]
    fn nonuniform_odd_dimensions_match_reference_edge_padding() {
        let rgb = include_bytes!("../tests/fixtures/jpeg_odd_edge.rgb").to_vec();
        let image = RgbImage::from_raw(17, 19, rgb).unwrap();
        assert_eq!(
            encode(&image, PDF_JPEG_QUALITY).unwrap(),
            include_bytes!("../tests/fixtures/jpeg_odd_edge.jpg")
        );
    }

    #[test]
    fn rejects_empty_and_oversized_jpeg_dimensions() {
        for image in [
            RgbImage::new(0, 1),
            RgbImage::new(u32::from(u16::MAX) + 1, 1),
        ] {
            assert_eq!(
                encode(&image, PDF_JPEG_QUALITY).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
}
