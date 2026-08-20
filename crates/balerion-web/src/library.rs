//! What is on this machine, across restarts.
//!
//! The torrents balerion is looking after used to live only in a `HashMap` in
//! memory, and nothing ever looked at the data directory. Two things followed,
//! both bad and neither obvious.
//!
//! A torrent ticked **Keep offline** vanished from the library the moment the
//! process restarted. Its bytes were still on disk and its keep marker was
//! still beside them, but nothing read either until the same magnet happened to
//! be resolved again, so "On this machine" was telling the viewer something
//! untrue.
//!
//! Worse, the sweeper walks the in-memory map, so a directory whose torrent was
//! forgotten by a restart was invisible to it for ever. Browse five films,
//! restart, and twenty gigabytes are stranded with nothing in the process aware
//! that they exist. The README promised they were swept after fifteen minutes.
//! They were not.
//!
//! So each torrent directory now carries a `.torrent` of its own, and this runs
//! at startup: adopt what was kept, and collect what was abandoned.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use balerion_bt::{InfoHash, Metainfo};

use crate::state::{AppState, Clock, IDLE_GRACE, is_marked_kept};
use crate::torrent;

/// The torrent file written beside a download.
///
/// A real `.torrent`, not a private format: any client can read it, which is
/// both a useful property and the honest test of whether we wrote one.
const SIDECAR: &str = "torrent.torrent";

pub fn sidecar_path(root: &Path) -> std::path::PathBuf {
    root.join(SIDECAR)
}

/// Write the torrent beside its data, so the directory can say what it is.
///
/// Failure is logged and never fatal. The cost of not having it is that this
/// download cannot be adopted after a restart, which is exactly where we were
/// before, and that is not a reason to refuse to play something.
pub async fn remember(root: &Path, meta: &Metainfo) {
    if let Err(err) = tokio::fs::write(sidecar_path(root), meta.to_torrent_bytes()).await {
        tracing::warn!(%err, path = %root.display(), "could not write the torrent file");
    }
}

/// Read back what a directory says it holds.
pub async fn recall(root: &Path) -> Option<Metainfo> {
    let raw = tokio::fs::read(sidecar_path(root)).await.ok()?;
    match Metainfo::parse(&raw) {
        Ok(meta) => Some(meta),
        Err(err) => {
            tracing::warn!(%err, path = %root.display(), "the torrent file beside this data is unreadable");
            None
        }
    }
}

/// Is everything this torrent describes already verified on disk?
///
/// Read from the resume file rather than by re-hashing, which is the whole
/// reason the resume file exists. An unclean one means the last run was killed,
/// so it is not trusted and the answer is no: the session will re-hash and find
/// out for itself.
pub async fn is_complete_on_disk(root: &Path, meta: &Metainfo) -> bool {
    balerion_bt::ResumeState::load(root, meta)
        .await
        .is_some_and(|state| state.clean && state.have.is_complete())
}

/// Everything found in the data directory at startup.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Adopted {
    /// Kept torrents put back in the library.
    pub kept: usize,
    /// Abandoned directories removed.
    pub swept: usize,
    /// Directories left alone: too recent to collect, or unreadable.
    pub left: usize,
}

/// Walk the data directory: adopt what was kept, collect what was abandoned.
pub async fn adopt(state: &Arc<AppState>) -> Adopted {
    let mut counts = Adopted::default();
    let Ok(mut entries) = tokio::fs::read_dir(&state.config.data_dir).await else {
        return counts;
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let root = entry.path();
        if !entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        // Directories are named by infohash. Anything else in here was not put
        // there by us and is none of our business.
        let Some(hash) = entry
            .file_name()
            .to_str()
            .and_then(|name| InfoHash::parse(name).ok())
        else {
            continue;
        };

        if is_marked_kept(&root) {
            match adopt_one(state, &root, hash).await {
                Ok(()) => counts.kept += 1,
                Err(err) => {
                    // Never delete a kept torrent we failed to read. Somebody
                    // asked for those bytes to stay, and being unable to
                    // describe them is not permission to remove them.
                    tracing::warn!(%err, path = %root.display(), "could not adopt a kept torrent");
                    counts.left += 1;
                }
            }
            continue;
        }

        // Not kept, and nothing can be watching it: the process has only just
        // started. Collect it if it has been sitting there longer than a
        // torrent gets to idle, and leave anything more recent, so restarting
        // balerion while watching something does not throw the download away.
        if idle_for(&root).await > IDLE_GRACE {
            match tokio::fs::remove_dir_all(&root).await {
                Ok(()) => {
                    tracing::info!(path = %root.display(), "collected an abandoned download");
                    counts.swept += 1;
                }
                Err(err) => {
                    tracing::warn!(%err, path = %root.display(), "could not collect it");
                    counts.left += 1;
                }
            }
        } else {
            counts.left += 1;
        }
    }

    counts
}

/// Put one kept torrent back in the library.
///
/// Started with no peers on purpose. A complete torrent needs none: the session
/// short-circuits when everything is already on disk, so this costs one read of
/// the resume file and no network at all. An incomplete one waits to be fed by
/// the same re-announce loop that feeds every other torrent here, which is the
/// behaviour "keep offline" implies.
#[allow(clippy::single_range_in_vec_init)]
async fn adopt_one(state: &Arc<AppState>, root: &Path, hash: InfoHash) -> anyhow::Result<()> {
    let meta = recall(root)
        .await
        .ok_or_else(|| anyhow::anyhow!("no readable torrent file beside the data"))?;
    if meta.info_hash != hash {
        // The directory name is the infohash. If the file inside disagrees,
        // one of them is lying and neither is safe to act on.
        anyhow::bail!(
            "the torrent file says {} but the directory says {hash}",
            meta.info_hash
        );
    }

    // Measured from now, which for an adoption is the moment the server
    // started rather than the moment anybody asked.
    let clock = Arc::new(Clock::started(std::time::Instant::now()));
    let torrent = torrent::start(
        meta,
        Vec::new(),
        root,
        &state.config,
        state.inbound.clone(),
        clock,
    )
    .await?;
    torrent.set_kept(true);
    // Kept means all of it, not merely what a playhead needs. One span covering
    // every piece, not a range to be collected.
    torrent
        .handle
        .prioritise(vec![0..torrent.meta.piece_count()])
        .await;
    tracing::info!(name = torrent.meta.name, "adopted a kept torrent");
    state.insert(hash, torrent);
    Ok(())
}

/// How long since anything in this directory was touched.
///
/// Shallow on purpose: the resume file and the keep marker both live at the top
/// level and both move whenever the torrent is active, so there is no reason to
/// walk a season pack to find that out.
async fn idle_for(root: &Path) -> Duration {
    let mut newest = None;
    if let Ok(mut entries) = tokio::fs::read_dir(root).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Ok(modified) = entry.metadata().await.and_then(|meta| meta.modified()) {
                newest = Some(newest.map_or(modified, |best: SystemTime| best.max(modified)));
            }
        }
    }
    // An empty or unreadable directory has nothing worth keeping, so treat it
    // as ancient and let it be collected.
    newest
        .and_then(|at| at.elapsed().ok())
        .unwrap_or(Duration::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bstr(s: &[u8]) -> Vec<u8> {
        let mut out = format!("{}:", s.len()).into_bytes();
        out.extend_from_slice(s);
        out
    }

    fn fixture() -> Metainfo {
        let mut info = Vec::new();
        info.extend(b"d");
        info.extend(bstr(b"length"));
        info.extend(b"i2000e");
        info.extend(bstr(b"name"));
        info.extend(bstr(b"a-film.mp4"));
        info.extend(bstr(b"piece length"));
        info.extend(b"i1024e");
        info.extend(bstr(b"pieces"));
        info.extend(bstr(&[0xAAu8; 40]));
        info.extend(b"e");
        Metainfo::from_info_dict(&info).unwrap()
    }

    #[tokio::test]
    async fn a_directory_can_say_what_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let meta = fixture();
        remember(dir.path(), &meta).await;

        let recalled = recall(dir.path()).await.expect("written and read back");
        assert_eq!(recalled.info_hash, meta.info_hash);
        assert_eq!(recalled.name, "a-film.mp4");
    }

    #[tokio::test]
    async fn a_directory_with_no_torrent_file_says_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(recall(dir.path()).await.is_none());
    }

    #[tokio::test]
    async fn rubbish_in_the_torrent_file_is_reported_rather_than_trusted() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(sidecar_path(dir.path()), b"not a torrent")
            .await
            .unwrap();
        assert!(recall(dir.path()).await.is_none());
    }

    /// Write the resume file a finished (or half-finished) download leaves.
    async fn leave_resume(root: &Path, meta: &Metainfo, have: usize, clean: bool) {
        let mut bits = balerion_bt::wire::Bitfield::empty(meta.piece_count());
        for index in 0..have {
            bits.set(index);
        }
        balerion_bt::ResumeState {
            info_hash: meta.info_hash,
            piece_length: meta.piece_length,
            total_length: meta.total_length,
            have: bits,
            clean,
        }
        .save(root)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn a_finished_download_is_recognised_without_re_hashing() {
        // This is what lets replaying something skip discovery entirely.
        let dir = tempfile::tempdir().unwrap();
        let meta = fixture();
        leave_resume(dir.path(), &meta, meta.piece_count(), true).await;
        assert!(is_complete_on_disk(dir.path(), &meta).await);
    }

    #[tokio::test]
    async fn a_half_finished_download_is_not_mistaken_for_a_whole_one() {
        let dir = tempfile::tempdir().unwrap();
        let meta = fixture();
        leave_resume(dir.path(), &meta, 1, true).await;
        assert!(!is_complete_on_disk(dir.path(), &meta).await);
    }

    #[tokio::test]
    async fn an_unclean_resume_file_is_not_trusted() {
        // Written by a run that was killed. The bitfield may claim pieces whose
        // bytes never reached the disk, and serving those would be handing out
        // zeros with a straight face.
        let dir = tempfile::tempdir().unwrap();
        let meta = fixture();
        leave_resume(dir.path(), &meta, meta.piece_count(), false).await;
        assert!(!is_complete_on_disk(dir.path(), &meta).await);
    }

    #[tokio::test]
    async fn a_directory_with_no_resume_file_promises_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_complete_on_disk(dir.path(), &fixture()).await);
    }

    #[tokio::test]
    async fn a_fresh_directory_is_not_idle() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("something"), b"x")
            .await
            .unwrap();
        assert!(idle_for(dir.path()).await < IDLE_GRACE);
    }

    #[tokio::test]
    async fn an_empty_directory_counts_as_abandoned() {
        // Nothing in it to lose, and leaving it would mean it is never
        // collected at all.
        let dir = tempfile::tempdir().unwrap();
        assert!(idle_for(dir.path()).await > IDLE_GRACE);
    }
}
