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

/// Drop a trailing `mfra`, if there is one.
///
/// Written and tested, and deliberately not wired in: see the note in
/// [`crate::play`] about the fragment timeline. It belongs with that work.
///
/// ffmpeg finishes every run by writing a Movie Fragment Random Access box: a
/// small index saying where each fragment is and what time it starts at. In a
/// whole file that is useful. In one segment of an HLS presentation it is
/// noise, because the playlist is the index, and it is worse than noise once
/// [`set_start_time`] has moved the fragment: the `tfra` inside it still says
/// the fragment begins at zero, and a demuxer handed two indexes that disagree
/// about the same fragment has every right to give up. Chrome does, silently,
/// with a spinner and no error.
///
/// Rewriting it to agree would be the other answer. Removing it is better: the
/// box is not wanted here, and one index is easier to keep honest than two.
pub fn without_trailer(data: &[u8]) -> &[u8] {
    let mut offset = 0usize;
    let mut last = None;
    while offset < data.len() {
        let Some(header) = read_header(data, offset) else {
            return data;
        };
        let Some(next) = offset.checked_add(header.size as usize) else {
            return data;
        };
        if next > data.len() || next == offset {
            return data;
        }
        last = Some((offset, header.kind));
        offset = next;
    }
    match last {
        Some((at, kind)) if &kind == b"mfra" => &data[..at],
        _ => data,
    }
}

/// Walk the boxes directly inside `data[start..end]`.
fn children(data: &[u8], start: usize, end: usize) -> Vec<(usize, BoxHeader)> {
    let mut found = Vec::new();
    let mut offset = start;
    while offset < end {
        let Some(header) = read_header(data, offset) else {
            break;
        };
        let Some(next) = offset.checked_add(header.size as usize) else {
            break;
        };
        if next > end || next == offset {
            break;
        }
        found.push((offset, header));
        offset = next;
    }
    found
}

/// Read a big-endian `u32` at `offset`.
fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Each track's timescale, by track id, read from the `moov`.
///
/// Needed because a fragment's start time is written in the track's own units,
/// and they differ: video at 24000 ticks a second and audio at 48000 in the
/// files this serves.
fn timescales(data: &[u8]) -> std::collections::HashMap<u32, u32> {
    let mut found = std::collections::HashMap::new();
    for (moov_at, moov) in children(data, 0, data.len()) {
        if &moov.kind != b"moov" {
            continue;
        }
        let moov_end = moov_at + moov.size as usize;
        for (trak_at, trak) in children(data, moov_at + moov.header_len as usize, moov_end) {
            if &trak.kind != b"trak" {
                continue;
            }
            let trak_end = trak_at + trak.size as usize;
            let mut id = None;
            let mut scale = None;
            for (child_at, child) in children(data, trak_at + trak.header_len as usize, trak_end) {
                let body = child_at + child.header_len as usize;
                match &child.kind {
                    // A full box: version, three flag bytes, then two times
                    // whose width the version decides, then the id.
                    b"tkhd" => {
                        let wide = data.get(body).is_some_and(|version| *version == 1);
                        id = u32_at(data, body + 4 + if wide { 16 } else { 8 });
                    }
                    b"mdia" => {
                        let mdia_end = child_at + child.size as usize;
                        for (mdhd_at, mdhd) in children(data, body, mdia_end) {
                            if &mdhd.kind != b"mdhd" {
                                continue;
                            }
                            let mdhd_body = mdhd_at + mdhd.header_len as usize;
                            let wide = data.get(mdhd_body).is_some_and(|version| *version == 1);
                            scale = u32_at(data, mdhd_body + 4 + if wide { 16 } else { 8 });
                        }
                    }
                    _ => {}
                }
            }
            if let (Some(id), Some(scale)) = (id, scale)
                && scale > 0
            {
                found.insert(id, scale);
            }
        }
    }
    found
}

/// Tell every fragment in `data` where in the film it belongs.
///
/// ffmpeg is run once per segment with `-ss`, and a run that seeks first
/// produces output numbered from zero. Every segment therefore arrived saying
/// `baseMediaDecodeTime = 0`, which is a fragment claiming to be the opening of
/// the film. Playing them in order survives that, because a player appending
/// one after another does not have to believe them. Seeking does not: asked for
/// twenty minutes in, the player works out which segment that is, fetches it,
/// reads a fragment that says it is the beginning, and has no timeline left.
///
/// Which is exactly what "starting from a certain point doesn't work" looks
/// like, and it is worse on a phone, because iOS plays HLS natively and trusts
/// the media rather than the playlist's arithmetic.
///
/// `-output_ts_offset` does not do this; it was tried, and the box came out
/// zero regardless. `-copyts` produces a fragment with no `tfdt` at all. So it
/// is written here, where it is exact and can be tested: the box is a version 1
/// full box holding a 64-bit value, so the number is replaced in place and not
/// one byte moves.
///
/// Returns false when nothing could be written, which the caller should treat
/// as a segment not worth serving.
pub fn set_start_time(data: &mut [u8], seconds: f64) -> bool {
    if !seconds.is_finite() || seconds < 0.0 {
        return false;
    }
    let scales = timescales(data);
    if scales.is_empty() {
        return false;
    }

    let mut written = 0usize;
    for (moof_at, moof) in children(data, 0, data.len()) {
        if &moof.kind != b"moof" {
            continue;
        }
        let moof_end = moof_at + moof.size as usize;
        for (traf_at, traf) in children(data, moof_at + moof.header_len as usize, moof_end) {
            if &traf.kind != b"traf" {
                continue;
            }
            let traf_end = traf_at + traf.size as usize;
            let kids = children(data, traf_at + traf.header_len as usize, traf_end);

            // Which track this fragment is for, so the right timescale is used.
            let track = kids.iter().find(|(_, kid)| &kid.kind == b"tfhd").and_then(
                |(at, header)| u32_at(data, at + header.header_len as usize + 4),
            );
            let Some(scale) = track.and_then(|id| scales.get(&id).copied()) else {
                continue;
            };
            let ticks = (seconds * f64::from(scale)).round() as u64;

            for (at, header) in &kids {
                if &header.kind != b"tfdt" {
                    continue;
                }
                let body = at + header.header_len as usize;
                match data.get(body) {
                    Some(1) => {
                        let Some(slot) = data.get_mut(body + 4..body + 12) else {
                            continue;
                        };
                        slot.copy_from_slice(&ticks.to_be_bytes());
                        written += 1;
                    }
                    // A 32-bit box cannot hold a long film's later fragments,
                    // and widening it would move every byte after it. ffmpeg
                    // writes version 1 here; this is the door being shut
                    // rather than a case anybody has seen.
                    Some(0) => {
                        if let Ok(narrow) = u32::try_from(ticks)
                            && let Some(slot) = data.get_mut(body + 4..body + 8)
                        {
                            slot.copy_from_slice(&narrow.to_be_bytes());
                            written += 1;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    written > 0
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

    /// A `moov` describing one track, and one `moof` fragment for it.
    ///
    /// Hand-built rather than checked in as a file, because the point is to be
    /// able to say exactly what is at every offset.
    fn film(track_id: u32, timescale: u32, tfdt_version: u8) -> Vec<u8> {
        fn full(kind: &[u8; 4], version: u8, body: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            let size = 12 + body.len() as u32;
            out.extend_from_slice(&size.to_be_bytes());
            out.extend_from_slice(kind);
            out.push(version);
            out.extend_from_slice(&[0, 0, 0]);
            out.extend_from_slice(body);
            out
        }
        fn container(kind: &[u8; 4], parts: &[Vec<u8>]) -> Vec<u8> {
            let inner: usize = parts.iter().map(Vec::len).sum();
            let mut out = Vec::new();
            out.extend_from_slice(&((8 + inner) as u32).to_be_bytes());
            out.extend_from_slice(kind);
            for part in parts {
                out.extend_from_slice(part);
            }
            out
        }

        // tkhd v0: creation(4) modification(4) track_id(4) ...
        let mut tkhd_body = vec![0u8; 8];
        tkhd_body.extend_from_slice(&track_id.to_be_bytes());
        tkhd_body.extend_from_slice(&[0u8; 4]);
        let tkhd = full(b"tkhd", 0, &tkhd_body);

        // mdhd v0: creation(4) modification(4) timescale(4) duration(4)
        let mut mdhd_body = vec![0u8; 8];
        mdhd_body.extend_from_slice(&timescale.to_be_bytes());
        mdhd_body.extend_from_slice(&[0u8; 4]);
        let mdhd = full(b"mdhd", 0, &mdhd_body);

        let mdia = container(b"mdia", &[mdhd]);
        let trak = container(b"trak", &[tkhd, mdia]);
        let moov = container(b"moov", &[trak]);

        // tfhd: the flags word then the track id.
        let mut tfhd_body = Vec::new();
        tfhd_body.extend_from_slice(&track_id.to_be_bytes());
        let tfhd = full(b"tfhd", 0, &tfhd_body);

        let tfdt = if tfdt_version == 1 {
            full(b"tfdt", 1, &0u64.to_be_bytes())
        } else {
            full(b"tfdt", 0, &0u32.to_be_bytes())
        };

        let traf = container(b"traf", &[tfhd, tfdt]);
        let moof = container(b"moof", &[traf]);
        let mdat = container(b"mdat", &[vec![0u8; 16]]);

        let mut out = Vec::new();
        out.extend_from_slice(&moov);
        out.extend_from_slice(&moof);
        out.extend_from_slice(&mdat);
        out
    }

    /// Read every `tfdt` value back out.
    fn start_times(data: &[u8]) -> Vec<u64> {
        let mut found = Vec::new();
        for (moof_at, moof) in children(data, 0, data.len()) {
            if &moof.kind != b"moof" {
                continue;
            }
            let moof_end = moof_at + moof.size as usize;
            for (traf_at, traf) in children(data, moof_at + moof.header_len as usize, moof_end) {
                let traf_end = traf_at + traf.size as usize;
                for (at, header) in children(data, traf_at + traf.header_len as usize, traf_end) {
                    if &header.kind != b"tfdt" {
                        continue;
                    }
                    let body = at + header.header_len as usize;
                    found.push(match data[body] {
                        1 => u64::from_be_bytes(data[body + 4..body + 12].try_into().unwrap()),
                        _ => u64::from(u32::from_be_bytes(
                            data[body + 4..body + 8].try_into().unwrap(),
                        )),
                    });
                }
            }
        }
        found
    }

    #[test]
    fn the_trailing_index_can_be_dropped() {
        let mut data = film(1, 24_000, 1);
        let mfra = boxed(b"mfra", 100);
        data.extend_from_slice(&mfra);
        let trimmed = without_trailer(&data);
        assert_eq!(trimmed.len(), data.len() - mfra.len());
        assert!(starts_with_box(trimmed, b"moov"));
    }

    #[test]
    fn something_with_no_trailer_is_left_alone() {
        let data = film(1, 24_000, 1);
        assert_eq!(without_trailer(&data).len(), data.len());
        assert_eq!(without_trailer(&[]).len(), 0);
        // Rubbish is returned untouched rather than half-eaten.
        let rubbish = vec![9u8; 5];
        assert_eq!(without_trailer(&rubbish).len(), rubbish.len());
    }

    #[test]
    fn a_fragment_is_told_where_in_the_film_it_belongs() {
        // Twenty minutes in, at 24000 ticks a second.
        let mut data = film(1, 24_000, 1);
        assert_eq!(start_times(&data), vec![0], "starts life claiming to be the opening");
        assert!(set_start_time(&mut data, 1200.0));
        assert_eq!(start_times(&data), vec![1200 * 24_000]);
    }

    #[test]
    fn each_track_is_counted_in_its_own_units() {
        // The bug this guards: audio at 48000 stamped with video's 24000 puts
        // the sound half an hour adrift by the end of a film.
        let mut video = film(1, 24_000, 1);
        let mut audio = film(2, 48_000, 1);
        assert!(set_start_time(&mut video, 300.0));
        assert!(set_start_time(&mut audio, 300.0));
        assert_eq!(start_times(&video), vec![300 * 24_000]);
        assert_eq!(start_times(&audio), vec![300 * 48_000]);
    }

    #[test]
    fn the_size_of_the_data_never_changes() {
        // In place, or every byte offset after it would be wrong.
        let mut data = film(1, 24_000, 1);
        let before = data.len();
        assert!(set_start_time(&mut data, 3600.0));
        assert_eq!(data.len(), before);
    }

    #[test]
    fn a_narrow_box_is_written_when_the_number_still_fits() {
        let mut data = film(1, 1_000, 0);
        assert!(set_start_time(&mut data, 60.0));
        assert_eq!(start_times(&data), vec![60_000]);
    }

    #[test]
    fn a_narrow_box_is_refused_rather_than_wrapped() {
        // Two and a half hours at 48000 does not fit in 32 bits. Silently
        // truncating would put the fragment somewhere else entirely.
        let mut data = film(1, 48_000, 0);
        assert!(!set_start_time(&mut data, 100_000.0));
        assert_eq!(start_times(&data), vec![0], "left alone rather than wrong");
    }

    #[test]
    fn nonsense_is_refused() {
        let mut data = film(1, 24_000, 1);
        assert!(!set_start_time(&mut data, f64::NAN));
        assert!(!set_start_time(&mut data, -1.0));
        assert!(!set_start_time(&mut Vec::new(), 10.0), "nothing to write to");
        // A fragment with no `moov` has no timescale to count in.
        let mut orphan = film(1, 24_000, 1);
        let split = first_moof(&orphan).unwrap();
        let mut media = orphan.split_off(split);
        assert!(!set_start_time(&mut media, 10.0));
    }

    #[test]
    fn the_start_time_is_rounded_rather_than_floored() {
        // 6.006 seconds at 24000 is 144144 exactly; the interesting case is a
        // segment boundary that does not land on a tick.
        let mut data = film(1, 24_000, 1);
        assert!(set_start_time(&mut data, 6.0001));
        assert_eq!(start_times(&data), vec![144_002]);
    }

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
