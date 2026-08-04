//! Which piece to ask for next.
//!
//! Rarest-first, with a random first piece and an endgame at the tail. The
//! strategy matters less than people think for throughput (pipelining depth
//! dominates that) but a great deal for swarm health: everyone downloading the
//! same prefix is how a swarm strands its last piece.

use std::collections::{HashMap, HashSet};

use crate::wire::Bitfield;

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
        }
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
        let mut candidates: Vec<usize> = (0..self.count)
            .filter(|index| !self.have.has(*index))
            .filter(|index| peer.has(*index))
            // Outside the endgame, one fetcher per piece is enough.
            .filter(|index| endgame || !self.in_flight.contains_key(index))
            .collect();

        if candidates.is_empty() {
            return None;
        }

        let chosen = if !self.got_first {
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
