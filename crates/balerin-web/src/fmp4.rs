//! Splitting ffmpeg's fragmented MP4 output into the two things MSE wants.
//!
//! Media Source Extensions expects one initialisation segment, appended once,
//! followed by media segments. ffmpeg with `+empty_moov` emits both stuck
//! together:
//!
//! ```text
//! ftyp | moov | moof | mdat | moof | mdat | ...
//! \___________/ \_______________________________/
//!     init                  media
//! ```
//!
//! So the split is at the first `moof`. MP4 boxes are a big-endian 32-bit
//! length followed by a four byte type, which makes walking them trivial, and
//! the whole point of doing it here rather than in the browser is that a
//! malformed box should be a 500 from a tested Rust function rather than a
//! silent stall in a `SourceBuffer`.

/// A box header: total size including the eight header bytes, and the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BoxHeader {
    size: u64,
    kind: [u8; 4],
    /// Bytes of header consumed, which is 8 normally and 16 for a 64-bit size.
    header_len: u64,
}

/// Read the box header at `offset`, if there is a whole one there.
fn read_header(data: &[u8], offset: usize) -> Option<BoxHeader> {
    let bytes = data.get(offset..offset + 8)?;
    let short = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let kind = [bytes[4], bytes[5], bytes[6], bytes[7]];

    match short {
        // 1 means the real size is in a 64-bit field after the type. Rare, but
        // an `mdat` over 4 GB is exactly where it shows up.
        1 => {
            let wide = data.get(offset + 8..offset + 16)?;
            let size = u64::from_be_bytes(wide.try_into().ok()?);
            (size >= 16).then_some(BoxHeader {
                size,
                kind,
                header_len: 16,
            })
        }
        // 0 means "to the end of the file", legal only for the last box.
        0 => Some(BoxHeader {
            size: (data.len() - offset) as u64,
            kind,
            header_len: 8,
        }),
        // Anything smaller than the header it just claimed is nonsense.
        size if size >= 8 => Some(BoxHeader {
            size: u64::from(size),
            kind,
            header_len: 8,
        }),
        _ => None,
    }
}

/// Byte offset of the first `moof` box, which is where init ends and media
/// begins.
///
/// Returns `None` if the data is malformed or contains no fragment, which for
/// our purposes are the same problem: there is nothing playable here.
pub fn first_moof(data: &[u8]) -> Option<usize> {
    let mut offset = 0usize;
    while offset < data.len() {
        let header = read_header(data, offset)?;
        if &header.kind == b"moof" {
            return Some(offset);
        }
        // A box that claims to run past the buffer means we were handed a
        // truncated file; walking on would be reading someone else's bytes.
        let next = offset.checked_add(header.size as usize)?;
        if next > data.len() || next == offset {
            return None;
        }
        offset = next;
    }
    None
}

/// Everything before the first fragment: `ftyp` and `moov`.
pub fn init_segment(data: &[u8]) -> Option<&[u8]> {
    let split = first_moof(data)?;
    (split > 0).then(|| &data[..split])
}

/// Everything from the first fragment on.
pub fn media_segment(data: &[u8]) -> Option<&[u8]> {
    let split = first_moof(data)?;
    Some(&data[split..])
}

/// Does this look like an initialisation segment rather than a fragment?
///
/// Used by the tests and by the play endpoint to refuse to serve something
/// that would wedge a `SourceBuffer`.
pub fn starts_with_box(data: &[u8], kind: &[u8; 4]) -> bool {
    read_header(data, 0).is_some_and(|header| &header.kind == kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a box with the given type and payload length.
    fn boxed(kind: &[u8; 4], payload: usize) -> Vec<u8> {
        let mut out = ((payload + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend(std::iter::repeat_n(0u8, payload));
        out
    }

    /// What ffmpeg actually hands us.
    fn ffmpeg_output() -> Vec<u8> {
        let mut out = boxed(b"ftyp", 24);
        out.extend(boxed(b"moov", 600));
        out.extend(boxed(b"moof", 200));
        out.extend(boxed(b"mdat", 5000));
        out.extend(boxed(b"moof", 180));
        out.extend(boxed(b"mdat", 4800));
        out
    }

    #[test]
    fn splits_ffmpeg_output_at_the_first_fragment() {
        let data = ffmpeg_output();
        let split = first_moof(&data).expect("there is a moof in there");
        // ftyp is 32 bytes, moov is 608, so the first moof starts at 640.
        assert_eq!(split, 640);

        let init = init_segment(&data).unwrap();
        assert_eq!(init.len(), 640);
        assert!(starts_with_box(init, b"ftyp"));

        let media = media_segment(&data).unwrap();
        assert!(starts_with_box(media, b"moof"));
        assert_eq!(init.len() + media.len(), data.len(), "nothing lost");
    }

    #[test]
    fn handles_a_64_bit_box_size() {
        // A large mdat uses size 1 and a 64-bit length after the type.
        let mut data = boxed(b"ftyp", 16);
        data.extend(1u32.to_be_bytes());
        data.extend(b"moov");
        data.extend(80u64.to_be_bytes()); // 16 header + 64 payload
        data.extend(std::iter::repeat_n(0u8, 64));
        data.extend(boxed(b"moof", 40));

        assert_eq!(first_moof(&data), Some(24 + 80));
    }

    #[test]
    fn a_zero_size_box_runs_to_the_end() {
        // Size 0 is legal for the last box only, and must not loop forever.
        let mut data = boxed(b"ftyp", 8);
        data.extend(0u32.to_be_bytes());
        data.extend(b"mdat");
        data.extend(std::iter::repeat_n(0u8, 100));
        assert_eq!(first_moof(&data), None, "no fragment, but also no hang");
    }

    #[test]
    fn truncated_input_is_refused_rather_than_read_past() {
        let full = ffmpeg_output();
        // Cut in the middle of the moov, so its size runs past the buffer.
        let truncated = &full[..300];
        assert_eq!(first_moof(truncated), None);
        assert!(init_segment(truncated).is_none());
        assert!(media_segment(truncated).is_none());
    }

    #[test]
    fn a_box_claiming_an_impossible_size_is_refused() {
        // Size 3 cannot even hold its own header.
        let mut data = 3u32.to_be_bytes().to_vec();
        data.extend(b"ftyp");
        data.extend(std::iter::repeat_n(0u8, 40));
        assert_eq!(first_moof(&data), None);
    }

    #[test]
    fn empty_and_stub_input_are_refused() {
        assert_eq!(first_moof(&[]), None);
        assert_eq!(first_moof(&[0, 0, 0]), None);
        assert!(!starts_with_box(&[], b"ftyp"));
    }

    #[test]
    fn output_with_no_fragment_yields_nothing() {
        // ffmpeg producing only a header means the encode gave us no frames,
        // which must not be served as a segment.
        let mut data = boxed(b"ftyp", 24);
        data.extend(boxed(b"moov", 600));
        assert_eq!(first_moof(&data), None);
        assert!(media_segment(&data).is_none());
    }

    #[test]
    fn a_fragment_with_no_header_has_no_init() {
        // If the very first box is a moof there is nothing to initialise with.
        let data = boxed(b"moof", 100);
        assert_eq!(first_moof(&data), Some(0));
        assert!(init_segment(&data).is_none(), "an empty init is not valid");
    }
}
