//! Shared raster-image validation and encoding.

use base64::Engine as _;

use crate::provider::ImageContent;

pub(crate) const MAX_IMAGE_BYTES: usize = 5_000_000;

pub(crate) fn media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub(crate) fn encode(media_type: &str, bytes: &[u8]) -> ImageContent {
    ImageContent {
        media_type: media_type.to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    }
}

pub(crate) fn extension(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

/// Returns the encoded image dimensions without decoding the raster payload.
pub(crate) fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    match media_type(bytes)? {
        "image/png" if bytes.len() >= 24 => Some((
            u32::from_be_bytes(bytes[16..20].try_into().ok()?),
            u32::from_be_bytes(bytes[20..24].try_into().ok()?),
        )),
        "image/gif" if bytes.len() >= 10 => Some((
            u32::from(u16::from_le_bytes(bytes[6..8].try_into().ok()?)),
            u32::from(u16::from_le_bytes(bytes[8..10].try_into().ok()?)),
        )),
        "image/jpeg" => jpeg_dimensions(bytes),
        "image/webp" => webp_dimensions(bytes),
        _ => None,
    }
    .filter(|(width, height)| *width > 0 && *height > 0)
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut offset = 2;
    while offset + 4 <= bytes.len() {
        if bytes[offset] != 0xff {
            offset += 1;
            continue;
        }
        while offset < bytes.len() && bytes[offset] == 0xff {
            offset += 1;
        }
        let marker = *bytes.get(offset)?;
        offset += 1;
        if matches!(marker, 0x01 | 0xd0..=0xd9) {
            continue;
        }
        let segment_length = usize::from(u16::from_be_bytes(
            bytes.get(offset..offset + 2)?.try_into().ok()?,
        ));
        if segment_length < 2 || offset.checked_add(segment_length)? > bytes.len() {
            return None;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            let height = u32::from(u16::from_be_bytes(
                bytes.get(offset + 3..offset + 5)?.try_into().ok()?,
            ));
            let width = u32::from(u16::from_be_bytes(
                bytes.get(offset + 5..offset + 7)?.try_into().ok()?,
            ));
            return Some((width, height));
        }
        offset += segment_length;
    }
    None
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    match bytes.get(12..16)? {
        b"VP8X" if bytes.len() >= 30 => Some((
            1 + little_endian_u24(&bytes[24..27]),
            1 + little_endian_u24(&bytes[27..30]),
        )),
        b"VP8L" if bytes.len() >= 25 && bytes[20] == 0x2f => {
            let packed = u32::from_le_bytes(bytes[21..25].try_into().ok()?);
            Some((1 + (packed & 0x3fff), 1 + ((packed >> 14) & 0x3fff)))
        }
        b"VP8 " => {
            let start = bytes
                .windows(3)
                .position(|window| window == [0x9d, 0x01, 0x2a])?;
            let width = u16::from_le_bytes(bytes.get(start + 3..start + 5)?.try_into().ok()?);
            let height = u16::from_le_bytes(bytes.get(start + 5..start + 7)?.try_into().ok()?);
            Some((u32::from(width & 0x3fff), u32::from(height & 0x3fff)))
        }
        _ => None,
    }
}

fn little_endian_u24(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_supported_magic_bytes() {
        assert_eq!(media_type(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(media_type(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(media_type(b"GIF89arest"), Some("image/gif"));
        assert_eq!(media_type(b"RIFF\0\0\0\0WEBPrest"), Some("image/webp"));
        assert_eq!(media_type(b"not an image"), None);
    }

    #[test]
    fn reads_dimensions_from_headers() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0; 8]);
        png.extend_from_slice(&640_u32.to_be_bytes());
        png.extend_from_slice(&480_u32.to_be_bytes());
        assert_eq!(dimensions(&png), Some((640, 480)));

        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&320_u16.to_le_bytes());
        gif.extend_from_slice(&200_u16.to_le_bytes());
        assert_eq!(dimensions(&gif), Some((320, 200)));
    }
}
