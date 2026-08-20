//! The OpenSubtitles file hash.
//!
//! Their matching scheme, and a genuinely well-chosen one: the hash is the file
//! size plus the sum of the first and last 64 KiB read as 64-bit integers. It
//! deliberately does not read the middle, so identifying a two gigabyte film
//! costs 128 kilobytes.
//!
//! That detail is what makes this usable here at all. balerion is streaming
//! from a swarm, and the head of the file is the first thing the picker
//! fetches because the player asks for it, while the tail is kept warm anyway
//! for the index box that plenty of MP4s keep at the end. So the bytes this
//! needs are, more often than not, bytes already on disk.
//!
//! Matching on it is worth far more than matching on a title, because a hash
//! match means subtitles timed against *this exact release*. That resolves both
//! the offset problem and the framerate problem before they arise, rather than
//! correcting them afterwards.

/// How much of each end goes into the hash.
pub const CHUNK: u64 = 64 * 1024;

/// The smallest file the scheme is defined for.
pub const MIN_SIZE: u64 = CHUNK * 2;

/// Compute the hash from a file's size and its two end chunks.
///
/// `head` and `tail` must each be [`CHUNK`] bytes. Returns `None` for a file
/// too small to have two distinct ends, which is every subtitle file and no
/// film.
pub fn compute(size: u64, head: &[u8], tail: &[u8]) -> Option<String> {
    if size < MIN_SIZE || head.len() as u64 != CHUNK || tail.len() as u64 != CHUNK {
        return None;
    }

    let mut hash = size;
    for chunk in [head, tail] {
        for value in chunk.chunks_exact(8) {
            // Wrapping, not saturating: the sum is meant to overflow, and
            // saturating here produces a hash that matches nothing.
            hash = hash.wrapping_add(u64::from_le_bytes(
                value
                    .try_into()
                    .expect("chunks_exact(8) yields eight bytes"),
            ));
        }
    }
    Some(format!("{hash:016x}"))
}

/// Which byte ranges a caller has to produce to compute the hash.
///
/// Returned rather than read here because this crate has no idea where the
/// bytes live. In balerion they come from a torrent that is still downloading.
pub fn ranges(size: u64) -> Option<(std::ops::Range<u64>, std::ops::Range<u64>)> {
    if size < MIN_SIZE {
        return None;
    }
    Some((0..CHUNK, (size - CHUNK)..size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_too_small_has_no_hash() {
        assert_eq!(
            compute(1000, &[0; CHUNK as usize], &[0; CHUNK as usize]),
            None
        );
        assert_eq!(ranges(1000), None);
    }

    #[test]
    fn short_chunks_are_refused_rather_than_padded() {
        // Padding would produce a plausible-looking hash that matches nothing,
        // which is the worst of both outcomes.
        assert_eq!(compute(MIN_SIZE, &[0; 10], &[0; CHUNK as usize]), None);
        assert_eq!(compute(MIN_SIZE, &[0; CHUNK as usize], &[0; 10]), None);
    }

    #[test]
    fn the_hash_is_the_size_when_both_ends_are_zero() {
        let zeros = vec![0u8; CHUNK as usize];
        let size = 1_234_567_890u64;
        assert_eq!(
            compute(size, &zeros, &zeros).as_deref(),
            Some(format!("{size:016x}").as_str())
        );
    }

    #[test]
    fn the_sum_wraps_rather_than_saturating() {
        // Saturating at u64::MAX would collapse every large film to the same
        // hash. Deliberately checked, because it is a one-word mistake.
        let mut head = vec![0u8; CHUNK as usize];
        head[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        let tail = vec![0u8; CHUNK as usize];

        let size = 100u64;
        let expected = size.wrapping_add(u64::MAX);
        assert_eq!(
            compute(MIN_SIZE.max(size), &head, &tail),
            Some(format!("{:016x}", MIN_SIZE.wrapping_add(u64::MAX)))
        );
        assert_ne!(expected, u64::MAX, "the fixture must actually wrap");
    }

    #[test]
    fn different_content_at_either_end_changes_the_hash() {
        let zeros = vec![0u8; CHUNK as usize];
        let mut head = zeros.clone();
        head[0] = 1;
        let mut tail = zeros.clone();
        tail[0] = 1;

        let base = compute(MIN_SIZE, &zeros, &zeros);
        assert_ne!(compute(MIN_SIZE, &head, &zeros), base);
        assert_ne!(compute(MIN_SIZE, &zeros, &tail), base);
        // Both ends contribute to the same sum, so swapping them is invisible.
        // Worth knowing rather than worth fixing: it is their scheme, not ours.
        assert_eq!(
            compute(MIN_SIZE, &head, &zeros),
            compute(MIN_SIZE, &zeros, &head)
        );
    }

    #[test]
    fn the_ranges_cover_both_ends_and_nothing_between() {
        let (head, tail) = ranges(10 * 1024 * 1024).unwrap();
        assert_eq!(head, 0..CHUNK);
        assert_eq!(tail.end, 10 * 1024 * 1024);
        assert_eq!(tail.end - tail.start, CHUNK);
        // The point of the scheme: identifying a ten megabyte file reads 128 KiB.
        assert_eq!((head.end - head.start) + (tail.end - tail.start), 2 * CHUNK);
    }

    #[test]
    fn the_hash_is_sixteen_lowercase_hex_digits() {
        let zeros = vec![0u8; CHUNK as usize];
        let hash = compute(MIN_SIZE, &zeros, &zeros).unwrap();
        assert_eq!(hash.len(), 16);
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase())
        );
    }
}
