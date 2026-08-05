//! Parsing the `Range` header.
//!
//! Small, fiddly, and the single most load-bearing thing in the server: get
//! `Content-Range` wrong and seeking silently stops working, which presents as
//! "the video is broken" rather than as anything you can grep for.

/// What the client asked for, resolved against the real file length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Requested {
    /// No usable `Range` header. Serve the whole thing with a 200.
    Whole,
    /// A byte range, resolved and clamped. `end` is exclusive, unlike the
    /// header, because half-open ranges are far harder to get wrong.
    Partial { start: u64, end: u64 },
    /// The client asked to start past the end of the file, which earns a 416
    /// rather than an empty 206.
    Unsatisfiable,
}

/// Resolve a `Range` header value against a known file length.
///
/// A malformed header is ignored rather than rejected, as RFC 9110 requires:
/// the client gets the whole file and works it out.
pub fn parse(header: Option<&str>, length: u64) -> Requested {
    let Some(raw) = header else {
        return Requested::Whole;
    };
    let Some(spec) = raw.trim().strip_prefix("bytes=") else {
        return Requested::Whole;
    };

    // Multi-range requests are legal and would need a multipart body. Players
    // do not use them, so ignore the header and serve the lot rather than
    // quietly answering with only the first range.
    if spec.contains(',') {
        return Requested::Whole;
    }

    let Some((from, to)) = spec.split_once('-') else {
        return Requested::Whole;
    };
    let (from, to) = (from.trim(), to.trim());

    // An empty file cannot satisfy any range at all.
    if length == 0 {
        return Requested::Unsatisfiable;
    }

    match (from.is_empty(), to.is_empty()) {
        // `bytes=-500`: the final 500 bytes, which is how a player hunts for
        // an index box parked at the end of the file.
        (true, false) => {
            let Ok(suffix) = to.parse::<u64>() else {
                return Requested::Whole;
            };
            if suffix == 0 {
                return Requested::Unsatisfiable;
            }
            Requested::Partial {
                start: length.saturating_sub(suffix),
                end: length,
            }
        }
        // `bytes=100-`: everything from 100 on.
        (false, true) => {
            let Ok(start) = from.parse::<u64>() else {
                return Requested::Whole;
            };
            if start >= length {
                return Requested::Unsatisfiable;
            }
            Requested::Partial { start, end: length }
        }
        // `bytes=100-199`: inclusive at both ends in the header, so the
        // exclusive end is one past.
        (false, false) => {
            let (Ok(start), Ok(last)) = (from.parse::<u64>(), to.parse::<u64>()) else {
                return Requested::Whole;
            };
            if start >= length || last < start {
                return Requested::Unsatisfiable;
            }
            Requested::Partial {
                start,
                end: last.saturating_add(1).min(length),
            }
        }
        (true, true) => Requested::Whole,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(header: &str, length: u64) -> Requested {
        parse(Some(header), length)
    }

    #[test]
    fn no_header_means_the_whole_file() {
        assert_eq!(parse(None, 1000), Requested::Whole);
    }

    #[test]
    fn an_open_ended_range_runs_to_the_end() {
        // What a `<video>` element opens with, near enough universally.
        assert_eq!(
            parse_str("bytes=0-", 1000),
            Requested::Partial { start: 0, end: 1000 }
        );
        assert_eq!(
            parse_str("bytes=600-", 1000),
            Requested::Partial { start: 600, end: 1000 }
        );
    }

    #[test]
    fn a_closed_range_is_inclusive_in_the_header_and_exclusive_here() {
        assert_eq!(
            parse_str("bytes=0-99", 1000),
            Requested::Partial { start: 0, end: 100 }
        );
        assert_eq!(
            parse_str("bytes=100-199", 1000),
            Requested::Partial { start: 100, end: 200 }
        );
        // The last byte of the file, asked for precisely.
        assert_eq!(
            parse_str("bytes=999-999", 1000),
            Requested::Partial { start: 999, end: 1000 }
        );
    }

    #[test]
    fn a_suffix_range_takes_the_tail() {
        // This is the request that makes a non-faststart MP4 work: the player
        // goes looking for the index box at the end of the file.
        assert_eq!(
            parse_str("bytes=-500", 1000),
            Requested::Partial { start: 500, end: 1000 }
        );
        // A suffix longer than the file is the whole file, not an error.
        assert_eq!(
            parse_str("bytes=-9999", 1000),
            Requested::Partial { start: 0, end: 1000 }
        );
    }

    #[test]
    fn an_end_past_the_file_is_clamped_rather_than_refused() {
        assert_eq!(
            parse_str("bytes=900-99999", 1000),
            Requested::Partial { start: 900, end: 1000 }
        );
    }

    #[test]
    fn starting_past_the_end_is_unsatisfiable() {
        assert_eq!(parse_str("bytes=1000-", 1000), Requested::Unsatisfiable);
        assert_eq!(parse_str("bytes=5000-6000", 1000), Requested::Unsatisfiable);
        // A backwards range is nonsense rather than merely empty.
        assert_eq!(parse_str("bytes=500-400", 1000), Requested::Unsatisfiable);
        assert_eq!(parse_str("bytes=-0", 1000), Requested::Unsatisfiable);
    }

    #[test]
    fn an_empty_file_satisfies_nothing() {
        assert_eq!(parse_str("bytes=0-", 0), Requested::Unsatisfiable);
    }

    #[test]
    fn rubbish_headers_are_ignored_rather_than_rejected() {
        // RFC 9110: an unparseable Range is treated as absent.
        assert_eq!(parse_str("bytes=abc-def", 1000), Requested::Whole);
        assert_eq!(parse_str("items=0-99", 1000), Requested::Whole);
        assert_eq!(parse_str("bytes=", 1000), Requested::Whole);
        assert_eq!(parse_str("bytes=-", 1000), Requested::Whole);
        assert_eq!(parse_str("nonsense", 1000), Requested::Whole);
    }

    #[test]
    fn multi_range_requests_fall_back_to_the_whole_file() {
        // Answering with only the first range would be a quiet lie about what
        // the body contains.
        assert_eq!(parse_str("bytes=0-99,200-299", 1000), Requested::Whole);
    }

    #[test]
    fn whitespace_is_tolerated() {
        assert_eq!(
            parse_str("  bytes=0-99  ", 1000),
            Requested::Partial { start: 0, end: 100 }
        );
    }
}
