//! Where you got to.
//!
//! The difference between a tool and something a household uses without being
//! taught. Balerion could stream a film in seconds and then, if you stopped it
//! and came back after supper, put you back at the beginning with no idea you
//! had ever seen it.
//!
//! One file for the lot, in the data directory rather than beside any one
//! torrent: a position has to outlive the torrent it refers to, because the
//! whole point is to still be there when you come back to something the sweeper
//! removed a week ago.
//!
//! Deliberately not a database. It is a few hundred bytes per programme, it is
//! written by one process, and a JSON file that can be read with `cat` when
//! something looks wrong is worth more here than anything with an index.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const FILE: &str = "watched.json";

/// Within this much of the end, it counts as finished.
///
/// Thirty seconds rather than the actual end, because almost nobody sits
/// through the credits, and a programme that says "1 minute left" for ever is
/// worse than one that says "watched".
const FINISHED_MARGIN: f64 = 30.0;

/// Below this, nothing is remembered.
///
/// Opening something, deciding against it and closing it should not fill the
/// "continue watching" row with things you did not watch.
const MINIMUM_POSITION: f64 = 60.0;

/// One programme, and where you got to in it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    /// Seconds into the file.
    pub seconds: f64,
    /// How long the file is, so a page can draw a progress bar without asking
    /// anything else.
    pub duration: f64,
    /// Unix seconds. What "continue watching" is sorted by.
    pub at: u64,
    /// Watched to the end. Kept rather than deleted so the library can mark it,
    /// and so that starting it again begins at the beginning.
    pub finished: bool,
    /// For showing something recognisable when the torrent is long gone.
    pub name: String,
}

impl Position {
    /// Where to resume, in seconds, or `None` when there is no point.
    pub fn resume_at(&self) -> Option<f64> {
        (!self.finished && self.seconds >= MINIMUM_POSITION).then_some(self.seconds)
    }

    pub fn fraction(&self) -> f64 {
        if self.duration <= 0.0 {
            return 0.0;
        }
        (self.seconds / self.duration).clamp(0.0, 1.0)
    }
}

/// Everything watched on this machine.
#[derive(Debug)]
pub struct History {
    path: PathBuf,
    entries: Mutex<HashMap<String, Position>>,
    /// Set whenever the map changes and cleared by a flush, so the writer can
    /// sleep through the long stretches where nothing is playing.
    dirty: Mutex<bool>,
}

/// The key for one file in one torrent.
fn key(hash: &str, file: usize) -> String {
    format!("{hash}/{file}")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

impl History {
    /// A history that remembers nothing and writes nowhere.
    ///
    /// For fixtures and for the moment before [`History::load`] has run. It
    /// records perfectly happily; the flush simply has nowhere to go.
    pub fn empty() -> Self {
        Self {
            path: PathBuf::new(),
            entries: Mutex::new(HashMap::new()),
            dirty: Mutex::new(false),
        }
    }

    /// Read what is there, or start empty.
    ///
    /// A missing or unreadable file is not an error. Losing your place is a
    /// disappointment; refusing to start the player over it would be worse.
    pub async fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(FILE);
        let entries = match tokio::fs::read(&path).await {
            Ok(raw) => serde_json::from_slice(&raw).unwrap_or_else(|err| {
                tracing::warn!(%err, path = %path.display(), "could not read the watch history");
                HashMap::new()
            }),
            Err(_) => HashMap::new(),
        };
        Self {
            path,
            entries: Mutex::new(entries),
            dirty: Mutex::new(false),
        }
    }

    /// Record where a viewer has got to.
    ///
    /// Ignores anything in the first minute, so opening something and deciding
    /// against it does not put it in the "continue watching" row.
    pub fn record(&self, hash: &str, file: usize, name: &str, seconds: f64, duration: f64) {
        if !seconds.is_finite() || !duration.is_finite() || duration <= 0.0 {
            return;
        }
        let finished = seconds >= duration - FINISHED_MARGIN;
        if seconds < MINIMUM_POSITION && !finished {
            return;
        }

        self.entries.lock().expect("history lock").insert(
            key(hash, file),
            Position {
                seconds,
                duration,
                at: now(),
                finished,
                name: name.to_string(),
            },
        );
        *self.dirty.lock().expect("history dirty lock") = true;
    }

    pub fn get(&self, hash: &str, file: usize) -> Option<Position> {
        self.entries
            .lock()
            .expect("history lock")
            .get(&key(hash, file))
            .cloned()
    }

    /// Forget one file, or every file in one torrent when `file` is `None`.
    pub fn forget(&self, hash: &str, file: Option<usize>) {
        let mut entries = self.entries.lock().expect("history lock");
        match file {
            Some(file) => {
                entries.remove(&key(hash, file));
            }
            None => entries.retain(|held, _| !held.starts_with(&format!("{hash}/"))),
        }
        drop(entries);
        *self.dirty.lock().expect("history dirty lock") = true;
    }

    /// What to offer picking up again, most recent first.
    ///
    /// Finished programmes are left out: the row is for things you are part way
    /// through, and something you watched to the end is not one of them.
    pub fn continuing(&self, limit: usize) -> Vec<(String, Position)> {
        let mut found: Vec<(String, Position)> = self
            .entries
            .lock()
            .expect("history lock")
            .iter()
            .filter(|(_, position)| position.resume_at().is_some())
            .map(|(key, position)| (key.clone(), position.clone()))
            .collect();
        found.sort_by_key(|(_, position)| std::cmp::Reverse(position.at));
        found.truncate(limit);
        found
    }

    /// Write the file, if anything has changed since the last time.
    pub async fn flush(&self) {
        {
            let mut dirty = self.dirty.lock().expect("history dirty lock");
            if !*dirty {
                return;
            }
            *dirty = false;
        }
        if self.path.as_os_str().is_empty() {
            return;
        }
        let snapshot = self.entries.lock().expect("history lock").clone();
        let Ok(encoded) = serde_json::to_vec_pretty(&snapshot) else {
            return;
        };
        // Written to one side and renamed, so an interrupted write leaves the
        // previous history rather than half of a new one.
        let temporary = self.path.with_extension("json.new");
        if tokio::fs::write(&temporary, &encoded).await.is_ok() {
            if let Err(err) = tokio::fs::rename(&temporary, &self.path).await {
                tracing::warn!(%err, "could not replace the watch history");
            }
        }
    }
}

/// Write the history out every so often, for as long as the server runs.
///
/// Every ten seconds rather than on every update: a playing file reports its
/// position constantly, and none of those reports is worth a write of its own.
/// The cost of the gap is losing at most ten seconds of "where you got to",
/// which nobody will ever notice.
pub async fn keep_written(history: std::sync::Arc<History>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(10));
    loop {
        ticker.tick().await;
        history.flush().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn empty() -> History {
        let dir = tempfile::tempdir().unwrap();
        let history = History::load(dir.path()).await;
        // The directory has to outlive the history for the flush tests, so it
        // is deliberately leaked here rather than dropped.
        std::mem::forget(dir);
        history
    }

    #[tokio::test]
    async fn a_position_is_remembered_and_can_be_resumed() {
        let history = empty().await;
        history.record("abc", 0, "A Film", 620.0, 5400.0);

        let position = history.get("abc", 0).expect("remembered");
        assert_eq!(position.resume_at(), Some(620.0));
        assert_eq!(position.name, "A Film");
        assert!(!position.finished);
    }

    #[tokio::test]
    async fn the_first_minute_is_not_worth_remembering() {
        // Otherwise opening something, deciding against it and closing it
        // fills the continue row with things nobody watched.
        let history = empty().await;
        history.record("abc", 0, "A Film", 12.0, 5400.0);
        assert!(history.get("abc", 0).is_none());
    }

    #[tokio::test]
    async fn the_last_thirty_seconds_count_as_finished() {
        // Almost nobody sits through the credits, and a programme stuck at
        // "one minute left" for ever is worse than one marked watched.
        let history = empty().await;
        history.record("abc", 0, "A Film", 5_380.0, 5_400.0);

        let position = history.get("abc", 0).unwrap();
        assert!(position.finished);
        assert_eq!(position.resume_at(), None, "start it again from the top");
        assert!(history.continuing(10).is_empty());
    }

    #[tokio::test]
    async fn a_short_thing_watched_to_the_end_still_counts() {
        // A ten minute Prelinger short never reaches a minute *remaining*
        // before it reaches the end.
        let history = empty().await;
        history.record("abc", 0, "A Short", 40.0, 45.0);
        assert!(history.get("abc", 0).expect("recorded").finished);
    }

    #[tokio::test]
    async fn continuing_is_most_recent_first_and_bounded() {
        let history = empty().await;
        for index in 0..5 {
            history.record("abc", index, &format!("Episode {index}"), 300.0, 3_000.0);
            // The clock has one second resolution, so the order has to be
            // forced rather than raced.
            let mut entries = history.entries.lock().unwrap();
            entries.get_mut(&key("abc", index)).unwrap().at = 1_000 + index as u64;
        }

        let continuing = history.continuing(3);
        assert_eq!(continuing.len(), 3);
        assert_eq!(continuing[0].1.name, "Episode 4");
        assert_eq!(continuing[2].1.name, "Episode 2");
    }

    #[tokio::test]
    async fn nonsense_from_a_player_is_ignored_rather_than_stored() {
        // A `<video>` element reports NaN for duration before it has metadata,
        // and a NaN in the history poisons every comparison that touches it.
        let history = empty().await;
        history.record("abc", 0, "x", f64::NAN, 100.0);
        history.record("abc", 1, "x", 100.0, f64::NAN);
        history.record("abc", 2, "x", 100.0, 0.0);
        history.record("abc", 3, "x", f64::INFINITY, f64::INFINITY);
        assert!(history.continuing(10).is_empty());
    }

    #[tokio::test]
    async fn forgetting_a_torrent_forgets_all_of_its_episodes() {
        let history = empty().await;
        history.record("abc", 0, "One", 300.0, 3_000.0);
        history.record("abc", 1, "Two", 300.0, 3_000.0);
        history.record("def", 0, "Other", 300.0, 3_000.0);

        history.forget("abc", None);
        assert!(history.get("abc", 0).is_none());
        assert!(history.get("abc", 1).is_none());
        assert!(history.get("def", 0).is_some(), "and nothing else");
    }

    #[tokio::test]
    async fn a_similar_looking_infohash_is_not_forgotten_by_accident() {
        // "abc" must not match "abcdef". The separator is what makes the
        // prefix test safe, and it is one character away from being wrong.
        let history = empty().await;
        history.record("abc", 0, "Mine", 300.0, 3_000.0);
        history.record("abcdef", 0, "Theirs", 300.0, 3_000.0);

        history.forget("abc", None);
        assert!(history.get("abcdef", 0).is_some());
    }

    #[tokio::test]
    async fn what_was_written_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let history = History::load(dir.path()).await;
        history.record("abc", 2, "A Film", 900.0, 5_400.0);
        history.flush().await;

        let reopened = History::load(dir.path()).await;
        let position = reopened.get("abc", 2).expect("survived a restart");
        assert_eq!(position.seconds, 900.0);
        assert_eq!(position.name, "A Film");
    }

    #[tokio::test]
    async fn a_corrupt_history_file_costs_the_history_and_not_the_player() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join(FILE), b"{ this is not json")
            .await
            .unwrap();

        let history = History::load(dir.path()).await;
        assert!(history.continuing(10).is_empty());
        // And it still works from here.
        history.record("abc", 0, "A Film", 300.0, 3_000.0);
        assert!(history.get("abc", 0).is_some());
    }

    #[tokio::test]
    async fn flushing_with_nothing_to_say_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let history = History::load(dir.path()).await;
        history.flush().await;
        assert!(
            !dir.path().join(FILE).exists(),
            "an idle server should not be rewriting a file every ten seconds"
        );
    }

    #[test]
    fn the_fraction_is_bounded_even_when_the_numbers_are_not() {
        let position = Position {
            seconds: 9_000.0,
            duration: 3_000.0,
            at: 0,
            finished: false,
            name: "x".into(),
        };
        assert_eq!(position.fraction(), 1.0);
    }
}
