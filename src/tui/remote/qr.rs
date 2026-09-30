//! Minimal QR code encoder for sharing the remote link: byte mode, error
//! correction level M, versions 1–6 (up to 106 bytes).

/// Error-correction codewords per block, blocks, and total codewords for
/// level M, indexed by version − 1.
const EC_PER_BLOCK: [usize; 6] = [10, 16, 26, 18, 24, 16];
const BLOCKS: [usize; 6] = [1, 1, 1, 2, 2, 4];
const TOTAL_CODEWORDS: [usize; 6] = [26, 44, 70, 100, 134, 172];
/// Second alignment-pattern coordinate; version 1 has none.
const ALIGNMENT: [usize; 6] = [0, 18, 22, 26, 30, 34];
/// Light border, in modules, around the code.
const QUIET_ZONE: usize = 2;

pub(super) struct QrCode {
    size: usize,
    /// Row-major; true is a dark module.
    modules: Vec<bool>,
    function: Vec<bool>,
}

impl QrCode {
    /// Encodes `data`, or returns `None` when it exceeds version 6.
    pub(super) fn encode(data: &[u8]) -> Option<Self> {
        let version = (1..=6).find(|&version| capacity(version) >= data.len())?;
        let codewords = add_error_correction(version, &data_codewords(version, data));
        let size = version * 4 + 17;
        let mut code = Self {
            size,
            modules: vec![false; size * size],
            function: vec![false; size * size],
        };
        code.draw_function_patterns(version);
        code.draw_codewords(&codewords);
        let mask = (0..8)
            .min_by_key(|&mask| {
                code.apply_mask(mask);
                code.draw_format_bits(mask);
                let penalty = code.penalty();
                code.apply_mask(mask);
                penalty
            })
            .unwrap_or(0);
        code.apply_mask(mask);
        code.draw_format_bits(mask);
        Some(code)
    }

    #[cfg(test)]
    fn size(&self) -> usize {
        self.size
    }

    pub(super) fn dark(&self, x: usize, y: usize) -> bool {
        self.modules[y * self.size + x]
    }

    /// Renders two module rows per text row with half blocks. Light modules
    /// and the quiet zone are drawn, so the code reads correctly on a dark
    /// background.
    pub(super) fn to_text(&self) -> String {
        let span = self.size + 2 * QUIET_ZONE;
        let light = |x: usize, y: usize| {
            if y >= span {
                return false;
            }
            let inside = |value: usize| (QUIET_ZONE..QUIET_ZONE + self.size).contains(&value);
            !(inside(x) && inside(y) && self.dark(x - QUIET_ZONE, y - QUIET_ZONE))
        };
        let mut text = String::new();
        for y in (0..span).step_by(2) {
            for x in 0..span {
                text.push(match (light(x, y), light(x, y + 1)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                });
            }
            text.push('\n');
        }
        text
    }

    fn set_function(&mut self, x: usize, y: usize, dark: bool) {
        let index = y * self.size + x;
        self.modules[index] = dark;
        self.function[index] = true;
    }

    fn draw_function_patterns(&mut self, version: usize) {
        for i in 0..self.size {
            self.set_function(6, i, i % 2 == 0);
            self.set_function(i, 6, i % 2 == 0);
        }
        let far = self.size - 4;
        for (x, y) in [(3, 3), (far, 3), (3, far)] {
            self.draw_square(x, y, 4, |distance| distance != 2 && distance != 4);
        }
        if version > 1 {
            let at = ALIGNMENT[version - 1];
            self.draw_square(at, at, 2, |distance| distance != 1);
        }
        // Reserve the format areas; the real bits are drawn after masking.
        self.draw_format_bits(0);
    }

    /// Draws the concentric square pattern centered on (`cx`, `cy`).
    fn draw_square(&mut self, cx: usize, cy: usize, radius: isize, dark: impl Fn(isize) -> bool) {
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                let x = cx as isize + dx;
                let y = cy as isize + dy;
                if (0..self.size as isize).contains(&x) && (0..self.size as isize).contains(&y) {
                    self.set_function(x as usize, y as usize, dark(dx.abs().max(dy.abs())));
                }
            }
        }
    }

    fn draw_format_bits(&mut self, mask: u8) {
        // Level M encodes as 0b00 in the two high bits.
        let data = u32::from(mask);
        let mut remainder = data;
        for _ in 0..10 {
            remainder = (remainder << 1) ^ ((remainder >> 9) * 0x537);
        }
        let bits = ((data << 10) | remainder) ^ 0x5412;
        let bit = |i: usize| (bits >> i) & 1 != 0;
        let size = self.size;
        for i in 0..=5 {
            self.set_function(8, i, bit(i));
        }
        self.set_function(8, 7, bit(6));
        self.set_function(8, 8, bit(7));
        self.set_function(7, 8, bit(8));
        for i in 9..15 {
            self.set_function(14 - i, 8, bit(i));
        }
        for i in 0..8 {
            self.set_function(size - 1 - i, 8, bit(i));
        }
        for i in 8..15 {
            self.set_function(8, size - 15 + i, bit(i));
        }
        self.set_function(8, size - 8, true);
    }

    /// Places codewords in the two-column zigzag from the bottom right.
    fn draw_codewords(&mut self, codewords: &[u8]) {
        let bits = codewords.len() * 8;
        let mut index = 0;
        let mut right = self.size - 1;
        loop {
            if right == 6 {
                right = 5;
            }
            let upward = (right + 1) & 2 == 0;
            for vertical in 0..self.size {
                let y = if upward {
                    self.size - 1 - vertical
                } else {
                    vertical
                };
                for x in [right, right - 1] {
                    let module = y * self.size + x;
                    if !self.function[module] && index < bits {
                        self.modules[module] = (codewords[index / 8] >> (7 - index % 8)) & 1 != 0;
                        index += 1;
                    }
                }
            }
            if right < 3 {
                break;
            }
            right -= 2;
        }
    }

    /// XORs the data modules with a mask pattern; applying it twice undoes it.
    fn apply_mask(&mut self, mask: u8) {
        for y in 0..self.size {
            for x in 0..self.size {
                let invert = match mask {
                    0 => (x + y) % 2 == 0,
                    1 => y % 2 == 0,
                    2 => x % 3 == 0,
                    3 => (x + y) % 3 == 0,
                    4 => (x / 3 + y / 2) % 2 == 0,
                    5 => x * y % 2 + x * y % 3 == 0,
                    6 => (x * y % 2 + x * y % 3) % 2 == 0,
                    _ => ((x + y) % 2 + x * y % 3) % 2 == 0,
                };
                let module = y * self.size + x;
                if invert && !self.function[module] {
                    self.modules[module] = !self.modules[module];
                }
            }
        }
    }

    /// Standard mask penalty: long runs, 2×2 blocks, finder look-alikes, and
    /// dark/light imbalance.
    fn penalty(&self) -> usize {
        const FINDER_LIKE: [bool; 11] = [
            true, false, true, true, true, false, true, false, false, false, false,
        ];
        let size = self.size;
        let mut score = 0;
        for line in 0..size {
            for horizontal in [true, false] {
                let module = |i: usize| {
                    if horizontal {
                        self.dark(i, line)
                    } else {
                        self.dark(line, i)
                    }
                };
                let mut run = 1;
                for i in 1..=size {
                    if i < size && module(i) == module(i - 1) {
                        run += 1;
                        continue;
                    }
                    if run >= 5 {
                        score += run - 2;
                    }
                    run = 1;
                }
                for start in 0..=size - FINDER_LIKE.len() {
                    let forward =
                        (0..FINDER_LIKE.len()).all(|i| module(start + i) == FINDER_LIKE[i]);
                    let backward = (0..FINDER_LIKE.len())
                        .all(|i| module(start + i) == FINDER_LIKE[FINDER_LIKE.len() - 1 - i]);
                    score += 40 * (usize::from(forward) + usize::from(backward));
                }
            }
        }
        for y in 0..size - 1 {
            for x in 0..size - 1 {
                let color = self.dark(x, y);
                if self.dark(x + 1, y) == color
                    && self.dark(x, y + 1) == color
                    && self.dark(x + 1, y + 1) == color
                {
                    score += 3;
                }
            }
        }
        let total = size * size;
        let dark = self.modules.iter().filter(|&&module| module).count();
        let deviation = (dark * 20).abs_diff(total * 10);
        score + (deviation.div_ceil(total)).saturating_sub(1) * 10
    }
}

fn data_codeword_count(version: usize) -> usize {
    TOTAL_CODEWORDS[version - 1] - EC_PER_BLOCK[version - 1] * BLOCKS[version - 1]
}

/// Bytes that fit after the 4-bit mode and 8-bit length headers.
fn capacity(version: usize) -> usize {
    (data_codeword_count(version) * 8 - 12) / 8
}

/// Byte-mode segment, terminator, and pad codewords.
fn data_codewords(version: usize, data: &[u8]) -> Vec<u8> {
    let capacity_bits = data_codeword_count(version) * 8;
    let mut bits = Vec::with_capacity(capacity_bits);
    let mut push = |value: usize, length: usize| {
        for shift in (0..length).rev() {
            bits.push((value >> shift) & 1 != 0);
        }
    };
    push(0b0100, 4);
    push(data.len(), 8);
    for &byte in data {
        push(usize::from(byte), 8);
    }
    let terminator = (capacity_bits - bits.len()).min(4);
    bits.extend(std::iter::repeat_n(false, terminator));
    while bits.len() % 8 != 0 {
        bits.push(false);
    }
    let mut codewords = bits
        .chunks(8)
        .map(|chunk| {
            chunk
                .iter()
                .fold(0u8, |byte, &bit| (byte << 1) | u8::from(bit))
        })
        .collect::<Vec<_>>();
    for pad in [0xEC, 0x11].into_iter().cycle() {
        if codewords.len() >= data_codeword_count(version) {
            break;
        }
        codewords.push(pad);
    }
    codewords
}

/// Splits data into blocks, appends Reed–Solomon codewords, and interleaves.
fn add_error_correction(version: usize, data: &[u8]) -> Vec<u8> {
    let blocks = BLOCKS[version - 1];
    let ec_length = EC_PER_BLOCK[version - 1];
    let total = TOTAL_CODEWORDS[version - 1];
    let short_blocks = blocks - total % blocks;
    let short_length = total / blocks;
    let divisor = reed_solomon_divisor(ec_length);
    let mut split = Vec::with_capacity(blocks);
    let mut offset = 0;
    for block in 0..blocks {
        let length = short_length - ec_length + usize::from(block >= short_blocks);
        let mut codewords = data[offset..offset + length].to_vec();
        offset += length;
        let ec = reed_solomon_remainder(&codewords, &divisor);
        if block < short_blocks {
            codewords.push(0);
        }
        codewords.extend(ec);
        split.push(codewords);
    }
    let mut result = Vec::with_capacity(total);
    for i in 0..split[0].len() {
        for (block, codewords) in split.iter().enumerate() {
            // Skip the placeholder that pads short blocks.
            if i != short_length - ec_length || block >= short_blocks {
                result.push(codewords[i]);
            }
        }
    }
    result
}

fn reed_solomon_divisor(degree: usize) -> Vec<u8> {
    let mut result = vec![0u8; degree];
    result[degree - 1] = 1;
    let mut root = 1u8;
    for _ in 0..degree {
        for j in 0..degree {
            result[j] = gf_multiply(result[j], root);
            if j + 1 < degree {
                result[j] ^= result[j + 1];
            }
        }
        root = gf_multiply(root, 0x02);
    }
    result
}

fn reed_solomon_remainder(data: &[u8], divisor: &[u8]) -> Vec<u8> {
    let mut result = vec![0u8; divisor.len()];
    for &byte in data {
        let factor = byte ^ result.remove(0);
        result.push(0);
        for (value, &coefficient) in result.iter_mut().zip(divisor) {
            *value ^= gf_multiply(coefficient, factor);
        }
    }
    result
}

/// Multiplication in GF(2⁸) modulo x⁸ + x⁴ + x³ + x² + 1.
fn gf_multiply(x: u8, y: u8) -> u8 {
    let mut z = 0u16;
    for i in (0..8).rev() {
        z = (z << 1) ^ ((z >> 7) * 0x11D);
        z ^= u16::from((y >> i) & 1) * u16::from(x);
    }
    z as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_smallest_version_that_fits() {
        assert_eq!(
            QrCode::encode(&[b'a'; 14]).map(|code| code.size()),
            Some(21)
        );
        assert_eq!(
            QrCode::encode(&[b'a'; 15]).map(|code| code.size()),
            Some(25)
        );
        assert_eq!(
            QrCode::encode(&[b'a'; 106]).map(|code| code.size()),
            Some(41)
        );
        assert!(QrCode::encode(&[b'a'; 107]).is_none());
    }

    #[test]
    fn reed_solomon_matches_the_specification_example() {
        // ISO/IEC 18004 Annex I: "01234567" at version 1-M.
        let data = [
            0x10, 0x20, 0x0C, 0x56, 0x61, 0x80, 0xEC, 0x11, 0xEC, 0x11, 0xEC, 0x11, 0xEC, 0x11,
            0xEC, 0x11,
        ];
        let ec = reed_solomon_remainder(&data, &reed_solomon_divisor(10));
        assert_eq!(
            ec,
            [0xA5, 0x24, 0xD4, 0xC1, 0xED, 0x36, 0xC7, 0x87, 0x2C, 0x55]
        );
    }

    #[test]
    fn draws_finder_patterns_and_a_light_quiet_zone() {
        let code = QrCode::encode(b"http://100.64.0.1:7474/#123456").expect("fits");
        for (x, y) in [(0, 0), (code.size() - 7, 0), (0, code.size() - 7)] {
            assert!(code.dark(x, y) && code.dark(x + 6, y + 6) && code.dark(x + 3, y + 3));
            assert!(!code.dark(x + 1, y + 1));
        }
        let text = code.to_text();
        let first = text.lines().next().expect("row");
        assert!(first.chars().all(|cell| cell == '█'));
        assert_eq!(first.chars().count(), code.size() + 2 * QUIET_ZONE);
    }
}
