//! Which piece to ask for next.
//!
//! Rarest-first, with a random first piece and an endgame at the tail. The
//! strategy matters less than people think for throughput (pipelining depth
//! dominates that) but a great deal for swarm health: everyone downloading the
//! same prefix is how a swarm strands its last piece.
//!
//! A player wants the opposite of all that. [`Strategy::Streaming`] lets a
//! reader nominate spans of pieces it needs imminently, which are served in
//! order before anything else. Rarest-first still runs underneath, so peers
//! holding none of the hot span keep working instead of going idle.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::wire::Bitfield;

/// How to choose among the pieces we still need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Strategy {
    /// Rarest-first with a random opener. Good for the swarm, hopeless for a
    /// player: the bytes you need next are exactly the ones it deprioritises.
    #[default]
    Rarest,
    /// Honour [`Picker::set_priority`] spans first, in order, lowest index
    /// first within a span. Also drops the random opening piece, which fights
    /// a reader waiting on the head of a file.
    Streaming,
}

/// Piece assignment state shared by every peer task.
#[derive(Debug)]
pub struct Picker {
    count: usize,
    have: Bitfield,
    /// How many connected peers hold each piece.
    availability: Vec<u32>,
    /// Piece → how many peers are currently fetching it.
    in_flight: HashMap<usize, usize>,
    /// Once this few pieces remain, start duplicating requests.
    endgame_threshold: usize,
    got_first: bool,
    strategy: Strategy,
    /// Spans a reader is waiting on, most urgent first.
    priority: Vec<Range<usize>>,
}

impl Picker {
    pub fn new(count: usize) -> Self {
        Self {
            count,
            have: Bitfield::empty(count),
            availability: vec![0; count],
            in_flight: HashMap::new(),
            endgame_threshold: endgame_threshold(count),
            got_first: false,
            strategy: Strategy::default(),
            priority: Vec::new(),
        }
    }

    /// Start from an existing bitfield, as after a resume.
    pub fn with_have(have: Bitfield) -> Self {
        let count = have.len();
        let got_first = have.count_set() > 0;
        Self {
            count,
            have,
            availability: vec![0; count],
            in_flight: HashMap::new(),
            endgame_threshold: endgame_threshold(count),
            got_first,
            strategy: Strategy::default(),
            priority: Vec::new(),
        }
    }

    pub fn set_strategy(&mut self, strategy: Strategy) {
        self.strategy = strategy;
    }

    /// Nominate the piece spans a reader needs soonest, most urgent first.
    ///
    /// Only consulted under [`Strategy::Streaming`]. Replaces any previous
    /// priority wholesale: a seek invalidates the old window entirely, and
    /// carrying it forward would have peers fetching for a playhead that
    /// moved on.
    pub fn set_priority(&mut self, spans: Vec<Range<usize>>) {
        self.priority = spans;
    }

    pub fn priority(&self) -> &[Range<usize>] {
        &self.priority
    }

    pub fn have(&self) -> &Bitfield {
        &self.have
    }

    pub fn piece_count(&self) -> usize {
        self.count
    }

    pub fn completed(&self) -> usize {
        self.have.count_set()
    }

    pub fn remaining(&self) -> usize {
        self.count - self.completed()
    }

    pub fn is_complete(&self) -> bool {
        self.have.is_complete()
    }

    /// Fold a peer's bitfield into the availability counts.
    pub fn add_peer(&mut self, peer: &Bitfield) {
        for index in 0..self.count.min(peer.len()) {
            if peer.has(index) {
                self.availability[index] += 1;
            }
        }
    }

    /// And take it back out when the peer goes away, so a departed seed does
    /// not keep making its pieces look common.
    pub fn remove_peer(&mut self, peer: &Bitfield) {
        for index in 0..self.count.min(peer.len()) {
            if peer.has(index) {
                self.availability[index] = self.availability[index].saturating_sub(1);
            }
        }
    }

    pub fn peer_has(&mut self, index: usize) {
        if index < self.count {
            self.availability[index] += 1;
        }
    }

    /// True when we are close enough to the end to duplicate requests.
    pub fn in_endgame(&self) -> bool {
        self.remaining() <= self.endgame_threshold
    }

    /// Choose a piece for a peer to fetch, marking it in flight.
    ///
    /// Returns `None` when the peer has nothing we still need.
    pub fn next_for(&mut self, peer: &Bitfield, seed: u64) -> Option<usize> {
        let endgame = self.in_endgame();

        // A reader is waiting on these, so they beat any notion of swarm
        // health. Spans are tried in order and taken lowest index first: a
        // player consumes bytes forwards, so piece 300 is useless until 299
        // has landed.
        if self.strategy == Strategy::Streaming
            && let Some(index) = self.next_priority(peer, endgame)
        {
            *self.in_flight.entry(index).or_insert(0) += 1;
            return Some(index);
        }

        let mut candidates: Vec<usize> = (0..self.count)
            .filter(|index| !self.have.has(*index))
            .filter(|index| peer.has(*index))
            // Outside the endgame, one fetcher per piece is enough.
            .filter(|index| endgame || !self.in_flight.contains_key(index))
            .collect();

        if candidates.is_empty() {
            return None;
        }

        // Under Streaming the random opener works against the reader: it wants
        // the head of the file, not a lucky dip.
        let chosen = if !self.got_first && self.strategy != Strategy::Streaming {
            // Random first piece: get *something* to trade with, fast. The
            // rarest piece is by definition the one the fewest peers can send.
            candidates[(seed as usize) % candidates.len()]
        } else {
            // Rarest first, ties broken by the caller's seed so peers do not
            // all march in lockstep down the same list.
            let rarest = candidates
                .iter()
                .map(|index| self.availability[*index])
                .min()
                .unwrap_or(0);
            candidates.retain(|index| self.availability[*index] == rarest);
            candidates[(seed as usize) % candidates.len()]
        };

        *self.in_flight.entry(chosen).or_insert(0) += 1;
        Some(chosen)
    }

    /// The lowest-indexed piece this peer can serve from the most urgent span
    /// that yields anything at all.
    fn next_priority(&self, peer: &Bitfield, endgame: bool) -> Option<usize> {
        for span in &self.priority {
            let found = (span.start..span.end.min(self.count))
                .filter(|index| !self.have.has(*index))
                .filter(|index| peer.has(*index))
                .find(|index| endgame || !self.in_flight.contains_key(index));
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// A peer gave up on a piece; let someone else have it.
    pub fn release(&mut self, index: usize) {
        if let Some(count) = self.in_flight.get_mut(&index) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.in_flight.remove(&index);
            }
        }
    }

    /// A piece arrived and verified.
    pub fn complete(&mut self, index: usize) {
        self.have.set(index);
        self.got_first = true;
        self.in_flight.remove(&index);
    }

    /// Pieces currently being fetched, for endgame cancellation.
    pub fn in_flight_pieces(&self) -> HashSet<usize> {
        self.in_flight.keys().copied().collect()
    }
}

/// The endgame is the last few percent, not the last eight pieces: on a
/// four-piece torrent a fixed threshold would mean duplicating every request
/// from the start.
fn endgame_threshold(count: usize) -> usize {
    (count / 20).clamp(1, 8)
}

#[cfg(test)]
// `set_priority` takes a list of spans, so `vec![40..45]` really is a one
// element vector of ranges rather than a fumbled attempt at `(40..45).collect()`.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    fn full(count: usize) -> Bitfield {
        let mut field = Bitfield::empty(count);
        for index in 0..count {
            field.set(index);
        }
        field
    }

    fn only(count: usize, indices: &[usize]) -> Bitfield {
        let mut field = Bitfield::empty(count);
        for index in indices {
            field.set(*index);
        }
        field
    }

    #[test]
    fn hands_out_a_piece_the_peer_actually_has() {
        let mut picker = Picker::new(4);
        let peer = only(4, &[2]);
        assert_eq!(picker.next_for(&peer, 0), Some(2));
        // Now in flight, so a second peer gets nothing.
        assert_eq!(picker.next_for(&peer, 0), None);
    }

    #[test]
    fn prefers_the_rarest_piece_once_we_are_in_the_economy() {
        let mut picker = Picker::new(4);
        picker.complete(0); // we already have one, so no random-first
        // Pieces 1 and 2 are common, 3 is held by one peer only.
        picker.add_peer(&only(4, &[1, 2, 3]));
        picker.add_peer(&only(4, &[1, 2]));
        picker.add_peer(&only(4, &[1, 2]));

        assert_eq!(
            picker.next_for(&full(4), 0),
            Some(3),
            "the rarest piece wins"
        );
    }

    #[test]
    fn departing_peers_stop_making_their_pieces_look_common() {
        let mut picker = Picker::new(3);
        picker.complete(0);
        let seed = only(3, &[1, 2]);
        picker.add_peer(&seed);
        picker.add_peer(&only(3, &[1]));
        // 2 is rarer than 1 while the seed is present.
        assert_eq!(picker.next_for(&full(3), 0), Some(2));
        picker.release(2);

        picker.remove_peer(&seed);
        // With the seed gone, nobody has 2 and 1 is the only real candidate.
        assert_eq!(picker.availability[2], 0);
        assert_eq!(picker.availability[1], 1);
    }

    #[test]
    fn one_fetcher_per_piece_until_the_endgame() {
        let mut picker = Picker::new(100);
        let peer = full(100);
        let first = picker.next_for(&peer, 0).unwrap();
        let second = picker.next_for(&peer, 0).unwrap();
        assert_ne!(first, second, "no duplicate assignment mid-download");
    }

    #[test]
    fn the_endgame_duplicates_requests() {
        let mut picker = Picker::new(4);
        for index in 0..3 {
            picker.complete(index);
        }
        assert!(picker.in_endgame(), "one piece left is well into the tail");

        let peer = full(4);
        assert_eq!(picker.next_for(&peer, 0), Some(3));
        assert_eq!(
            picker.next_for(&peer, 0),
            Some(3),
            "the last piece goes to everyone who can serve it"
        );
        assert_eq!(picker.in_flight_pieces().len(), 1);
    }

    #[test]
    fn released_pieces_come_back_up_for_grabs() {
        let mut picker = Picker::new(50);
        let peer = only(50, &[7]);
        assert_eq!(picker.next_for(&peer, 0), Some(7));
        assert_eq!(picker.next_for(&peer, 0), None);
        picker.release(7);
        assert_eq!(picker.next_for(&peer, 0), Some(7));
    }

    #[test]
    fn completion_is_tracked() {
        let mut picker = Picker::new(3);
        assert_eq!(picker.remaining(), 3);
        picker.complete(0);
        picker.complete(1);
        assert_eq!(picker.completed(), 2);
        assert!(!picker.is_complete());
        picker.complete(2);
        assert!(picker.is_complete());
        assert_eq!(picker.next_for(&full(3), 0), None);
    }

    #[test]
    fn resuming_starts_from_what_is_already_on_disk() {
        let mut picker = Picker::with_have(only(4, &[0, 1]));
        assert_eq!(picker.completed(), 2);
        let chosen = picker.next_for(&full(4), 0).unwrap();
        assert!(chosen == 2 || chosen == 3);
    }

    /// A picker in streaming mode with nothing downloaded yet.
    fn streaming(count: usize) -> Picker {
        let mut picker = Picker::new(count);
        picker.set_strategy(Strategy::Streaming);
        picker
    }

    #[test]
    fn priority_spans_are_served_in_order_lowest_index_first() {
        let mut picker = streaming(100);
        picker.set_priority(vec![40..45]);

        // Not the random opener, and not the rarest: the head of the span.
        assert_eq!(picker.next_for(&full(100), 7), Some(40));
        assert_eq!(picker.next_for(&full(100), 7), Some(41));
        assert_eq!(picker.next_for(&full(100), 7), Some(42));
    }

    #[test]
    fn the_most_urgent_span_is_exhausted_before_the_next_one() {
        let mut picker = streaming(100);
        // The player needs 10..12 now and the file tail eventually, which is
        // the shape that makes a non-faststart MP4 play.
        picker.set_priority(vec![10..12, 98..100]);

        assert_eq!(picker.next_for(&full(100), 0), Some(10));
        assert_eq!(picker.next_for(&full(100), 0), Some(11));
        assert_eq!(picker.next_for(&full(100), 0), Some(98));
        assert_eq!(picker.next_for(&full(100), 0), Some(99));
    }

    #[test]
    fn a_peer_missing_the_hot_span_still_gets_work() {
        let mut picker = streaming(100);
        picker.set_priority(vec![40..45]);

        // This peer holds nothing the player is waiting on. Handing it `None`
        // would have `run_peer` give up after four idle rounds, so a peer that
        // could be filling in the rest of the file goes to waste.
        let elsewhere = only(100, &[80, 81]);
        let chosen = picker.next_for(&elsewhere, 0).expect("fall back, not idle");
        assert!(chosen == 80 || chosen == 81);
    }

    #[test]
    fn a_satisfied_span_falls_through_to_rarest_first() {
        let mut picker = streaming(6);
        picker.set_priority(vec![0..2]);
        picker.complete(0);
        picker.complete(1);

        // Piece 5 is held by one peer, 2 to 4 by three. With the span already
        // on disk we are back to ordinary rarest-first economics.
        picker.add_peer(&only(6, &[2, 3, 4, 5]));
        picker.add_peer(&only(6, &[2, 3, 4]));
        picker.add_peer(&only(6, &[2, 3, 4]));
        assert_eq!(picker.next_for(&full(6), 0), Some(5));
    }

    #[test]
    fn streaming_skips_the_random_opening_piece() {
        // Every seed must give piece 0 when it is the head of the span: a
        // reader waiting on the start of a file cannot use a lucky dip.
        for seed in 0..8 {
            let mut picker = streaming(8);
            picker.set_priority(vec![0..8]);
            assert_eq!(picker.next_for(&full(8), seed), Some(0));
        }
    }

    #[test]
    fn setting_a_new_priority_replaces_the_old_one() {
        let mut picker = streaming(100);
        picker.set_priority(vec![10..20]);
        assert_eq!(picker.next_for(&full(100), 0), Some(10));

        // The viewer seeked. The old window is stale, not merely lower ranked.
        picker.set_priority(vec![70..80]);
        assert_eq!(picker.next_for(&full(100), 0), Some(70));
        assert_eq!(picker.priority(), &[70..80]);
    }

    #[test]
    fn spans_running_past_the_end_are_harmless() {
        let mut picker = streaming(5);
        picker.set_priority(vec![3..900]);
        assert_eq!(picker.next_for(&full(5), 0), Some(3));
        assert_eq!(picker.next_for(&full(5), 0), Some(4));
        // Nothing left in the span, nothing left at all.
        picker.complete(0);
        picker.complete(1);
        picker.complete(2);
        assert_eq!(picker.next_for(&full(5), 0), None);
    }

    #[test]
    fn priority_is_ignored_under_the_default_strategy() {
        let mut picker = Picker::new(3);
        picker.complete(0); // clear the random-first path
        picker.set_priority(vec![2..3]);
        picker.add_peer(&only(3, &[1, 2]));
        picker.add_peer(&only(3, &[2]));
        picker.add_peer(&only(3, &[2]));

        // Piece 1 is the rarest, piece 2 is what a streaming picker would take.
        assert_eq!(picker.next_for(&full(3), 0), Some(1));
    }

    #[test]
    fn one_fetcher_per_priority_piece_until_the_endgame() {
        let mut picker = streaming(100);
        picker.set_priority(vec![0..10]);
        assert_eq!(picker.next_for(&full(100), 0), Some(0));
        assert_eq!(
            picker.next_for(&full(100), 0),
            Some(1),
            "the second peer must not duplicate the first"
        );
    }

    #[test]
    fn different_seeds_spread_peers_across_equally_rare_pieces() {
        let picks: HashSet<usize> = (0..8)
            .map(|seed| {
                let mut picker = Picker::new(8);
                picker.complete(0);
                picker.next_for(&full(8), seed).unwrap()
            })
            .collect();
        assert!(
            picks.len() > 1,
            "every peer starting on the same piece defeats the point"
        );
    }
}
