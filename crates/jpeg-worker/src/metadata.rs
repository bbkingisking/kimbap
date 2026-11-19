use kimbap_worker::invalid;
use std::io;

#[derive(Default)]
pub struct Metadata {
    pub orientation: u32,
    pub icc: Option<Vec<u8>>,
}

pub fn read(bytes: &[u8]) -> io::Result<Metadata> {
    if !bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Err(invalid("This file is not a JPEG"));
    }
    let mut orientation = 1;
    let mut offset = 2;
    let mut profile: Vec<Option<Vec<u8>>> = Vec::new();
    let mut profile_bytes = 0;
    while offset < bytes.len() {
        if bytes[offset] != 0xff {
            return Err(invalid("Invalid JPEG marker"));
        }
        while bytes.get(offset) == Some(&0xff) {
            offset += 1;
        }
        let marker = *bytes
            .get(offset)
            .ok_or_else(|| invalid("Truncated JPEG marker"))?;
        offset += 1;
        if marker == 0xda {
            break;
        }
        if marker == 0xd9 {
            return Err(invalid("JPEG has no image data"));
        }
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            continue;
        }
        let size = bytes
            .get(offset..offset + 2)
            .ok_or_else(|| invalid("Truncated JPEG metadata"))?;
        let len = u16::from_be_bytes(size.try_into().unwrap()) as usize;
        if len < 2 {
            return Err(invalid("Invalid JPEG segment length"));
        }
        let data = bytes
            .get(offset + 2..offset + len)
            .ok_or_else(|| invalid("Truncated JPEG segment"))?;
        if marker == 0xe1 && data.starts_with(b"Exif\0\0") {
            let exif = exif::Reader::new()
                .read_raw(data[6..].to_vec())
                .map_err(|e| invalid(format!("Invalid EXIF metadata: {e}")))?;
            if let Some(field) = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY) {
                orientation = field
                    .value
                    .get_uint(0)
                    .filter(|v| (1..=8).contains(v))
                    .ok_or_else(|| invalid("Invalid EXIF orientation"))?;
            }
        }
        if marker == 0xe2 && data.starts_with(b"ICC_PROFILE\0") {
            if data.len() < 14 {
                return Err(invalid("Truncated ICC profile header"));
            }
            let sequence = data[12] as usize;
            let count = data[13] as usize;
            if count == 0 || sequence == 0 || sequence > count {
                return Err(invalid("Invalid ICC profile sequence"));
            }
            if profile.is_empty() {
                profile.resize(count, None);
            }
            if profile.len() != count || profile[sequence - 1].is_some() {
                return Err(invalid("Inconsistent ICC profile segments"));
            }
            profile_bytes += data.len() - 14;
            if profile_bytes > 4 * 1024 * 1024 {
                return Err(invalid("ICC profile exceeds the 4-MiB limit"));
            }
            profile[sequence - 1] = Some(data[14..].to_vec());
        }
        offset += len;
    }
    let icc = if profile.is_empty() {
        None
    } else {
        let mut joined = Vec::with_capacity(profile_bytes);
        for part in profile {
            joined.extend(part.ok_or_else(|| invalid("Incomplete ICC profile"))?);
        }
        Some(joined)
    };
    Ok(Metadata { orientation, icc })
}

pub fn oriented_position(
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    orientation: u32,
) -> (usize, usize) {
    match orientation {
        1 => (x, y),
        2 => (w - 1 - x, y),
        3 => (w - 1 - x, h - 1 - y),
        4 => (x, h - 1 - y),
        5 => (y, x),
        6 => (h - 1 - y, x),
        7 => (h - 1 - y, w - 1 - x),
        8 => (y, w - 1 - x),
        _ => unreachable!("orientation was checked"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_exif_orientations() {
        let expected = [
            vec![0, 1, 2, 3, 4, 5],
            vec![2, 1, 0, 5, 4, 3],
            vec![5, 4, 3, 2, 1, 0],
            vec![3, 4, 5, 0, 1, 2],
            vec![0, 3, 1, 4, 2, 5],
            vec![3, 0, 4, 1, 5, 2],
            vec![5, 2, 4, 1, 3, 0],
            vec![2, 5, 1, 4, 0, 3],
        ];
        for orientation in 1..=8 {
            let width = if orientation >= 5 { 2 } else { 3 };
            let mut out = vec![0; 6];
            for y in 0..2 {
                for x in 0..3 {
                    let (dx, dy) = oriented_position(x, y, 3, 2, orientation);
                    out[dy * width + dx] = y * 3 + x;
                }
            }
            assert_eq!(out, expected[orientation as usize - 1]);
        }
    }
    #[test]
    fn rejects_broken_metadata() {
        assert!(read(b"not a jpeg").is_err());
        assert!(read(&[255, 216, 255, 225, 255, 255]).is_err());
    }
}
