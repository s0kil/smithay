//! Checked decoding of the kernel's `drm_format_modifier_blob` layout.

use std::io;

use drm_fourcc::{DrmFormat, DrmFourcc, DrmModifier};
use indexmap::IndexSet;

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid DRM format modifier blob")
}

fn read<const N: usize>(data: &[u8], offset: usize) -> io::Result<[u8; N]> {
    data.get(offset..offset.checked_add(N).ok_or_else(invalid)?)
        .ok_or_else(invalid)?
        .try_into()
        .map_err(|_| invalid())
}

pub(super) fn parse(data: &[u8]) -> io::Result<IndexSet<DrmFormat>> {
    let word = |offset| read(data, offset).map(u32::from_ne_bytes);
    if word(0)? != 1 || word(4)? != 0 {
        return Err(invalid());
    }
    let count_formats = word(8)? as usize;
    let formats_offset = word(12)? as usize;
    let count_modifiers = word(16)? as usize;
    let modifiers_offset = word(20)? as usize;
    // Validate both arrays before iterating. No assumptions about alignment or
    // host pointer width are needed, and a malformed count cannot create a loop.
    for (offset, count, size) in [
        (formats_offset, count_formats, 4),
        (modifiers_offset, count_modifiers, 24),
    ] {
        let end = offset
            .checked_add(count.checked_mul(size).ok_or_else(invalid)?)
            .ok_or_else(invalid)?;
        if (count != 0 && offset < 24) || end > data.len() {
            return Err(invalid());
        }
    }
    let mut formats = IndexSet::new();
    for index in 0..count_modifiers {
        let offset = modifiers_offset + index * 24;
        let bitmap = u64::from_ne_bytes(read(data, offset)?);
        let first = word(offset + 8)? as usize;
        let modifier = DrmModifier::from(u64::from_ne_bytes(read(data, offset + 16)?));
        for bit in 0..64 {
            if bitmap & (1 << bit) == 0 {
                continue;
            }
            let index = first.checked_add(bit).ok_or_else(invalid)?;
            if index >= count_formats {
                return Err(invalid());
            }
            if let Ok(code) = DrmFourcc::try_from(word(formats_offset + index * 4)?) {
                formats.insert(DrmFormat { code, modifier });
            }
        }
    }
    Ok(formats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob() -> Vec<u8> {
        let mut data = Vec::new();
        for word in [
            1,
            0,
            2,
            24,
            1,
            32,
            DrmFourcc::Argb8888 as u32,
            DrmFourcc::Xrgb8888 as u32,
        ] {
            data.extend_from_slice(&word.to_ne_bytes());
        }
        data.extend_from_slice(&3_u64.to_ne_bytes());
        data.extend_from_slice(&0_u32.to_ne_bytes());
        data.extend_from_slice(&0_u32.to_ne_bytes());
        data.extend_from_slice(&0_u64.to_ne_bytes());
        data
    }

    #[test]
    fn decodes_unaligned_blob() {
        let mut data = vec![0];
        data.extend(blob());
        let formats = parse(&data[1..]).unwrap();
        assert_eq!(formats.len(), 2);
        assert!(formats.contains(&DrmFormat {
            code: DrmFourcc::Argb8888,
            modifier: DrmModifier::Linear
        }));
    }

    #[test]
    fn rejects_every_truncated_blob() {
        let data = blob();
        for length in 0..data.len() {
            assert!(parse(&data[..length]).is_err(), "length {length}");
        }
    }

    #[test]
    fn rejects_invalid_offsets_counts_and_bitmaps() {
        for (offset, word) in [
            (0, 2),
            (4, 1),
            (8, u32::MAX),
            (12, u32::MAX),
            (16, u32::MAX),
            (20, 4),
            (40, u32::MAX),
        ] {
            let mut data = blob();
            data[offset..offset + 4].copy_from_slice(&word.to_ne_bytes());
            assert!(parse(&data).is_err(), "offset {offset}");
        }
        let mut data = blob();
        data[32..40].copy_from_slice(&(1_u64 << 63).to_ne_bytes());
        assert!(parse(&data).is_err());
    }

    #[test]
    fn skips_unknown_fourcc_without_losing_known_formats() {
        let mut data = blob();
        data[24..28].copy_from_slice(&0_u32.to_ne_bytes());
        assert_eq!(parse(&data).unwrap().len(), 1);
    }
}
