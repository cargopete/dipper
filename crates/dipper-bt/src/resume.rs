//! Fast resume: remember which pieces verified, so restarting does not mean
//! re-hashing the whole download.
//!
//! The file is written atomically (temp file, then rename) and carries a
//! trailing SHA-1 of its own contents, because a resume file that is subtly
//! wrong is worse than no resume file at all: it makes us claim to have data
//! we do not. A `clean` flag distinguishes a tidy shutdown from a kill -9; an
//! unclean state is ignored and we fall back to re-hashing.

use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use crate::error::{Error, Result};
use crate::infohash::InfoHash;
use crate::metainfo::Metainfo;
use crate::wire::Bitfield;

const MAGIC: &[u8; 7] = b"DIPPER\x00";
const VERSION: u8 = 1;

/// Header bytes before the bitfield: magic, version, infohash, piece length,
/// total length, piece count, flags.
const HEADER_LEN: usize = 7 + 1 + 20 + 8 + 8 + 4 + 1;

const FLAG_CLEAN: u8 = 0b0000_0001;

/// What we knew about a download when we last wrote it down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeState {
    pub info_hash: InfoHash,
    pub piece_length: u64,
    pub total_length: u64,
    pub have: Bitfield,
    /// False if we were still running when the file was written. An unclean
    /// state is not trusted.
    pub clean: bool,
}

impl ResumeState {
    pub fn new(meta: &Metainfo, have: Bitfield, clean: bool) -> Self {
        Self {
            info_hash: meta.info_hash,
            piece_length: meta.piece_length,
            total_length: meta.total_length,
            have,
            clean,
        }
    }

    /// Where the sidecar for this torrent lives, given a download root.
    pub fn path(root: impl AsRef<Path>, info_hash: InfoHash) -> PathBuf {
        root.as_ref()
            .join(format!(".dipper-{}.resume", info_hash.to_hex()))
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.have.as_bytes().len() + 20);
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.extend_from_slice(self.info_hash.as_bytes());
        out.extend_from_slice(&self.piece_length.to_le_bytes());
        out.extend_from_slice(&self.total_length.to_le_bytes());
        out.extend_from_slice(&(self.have.len() as u32).to_le_bytes());
        out.push(if self.clean { FLAG_CLEAN } else { 0 });
        out.extend_from_slice(self.have.as_bytes());
        let digest = Sha1::digest(&out);
        out.extend_from_slice(&digest);
        out
    }

    pub fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() < HEADER_LEN + 20 {
            return Err(Error::Metainfo("resume file is too short".into()));
        }
        let (body, digest) = raw.split_at(raw.len() - 20);
        if Sha1::digest(body).as_slice() != digest {
            return Err(Error::Metainfo("resume file failed its checksum".into()));
        }
        if &body[..7] != MAGIC {
            return Err(Error::Metainfo("not a dipper resume file".into()));
        }
        if body[7] != VERSION {
            return Err(Error::Metainfo(format!(
                "resume file is version {}, we speak {VERSION}",
                body[7]
            )));
        }

        let mut info_hash = [0u8; 20];
        info_hash.copy_from_slice(&body[8..28]);
        let piece_length = u64::from_le_bytes(body[28..36].try_into().unwrap());
        let total_length = u64::from_le_bytes(body[36..44].try_into().unwrap());
        let piece_count = u32::from_le_bytes(body[44..48].try_into().unwrap()) as usize;
        let clean = body[48] & FLAG_CLEAN != 0;

        let bits = &body[HEADER_LEN..];
        let have = Bitfield::from_bytes(bits, piece_count)?;

        Ok(Self {
            info_hash: InfoHash::new(info_hash),
            piece_length,
            total_length,
            have,
            clean,
        })
    }

    /// Does this state describe the torrent we are actually downloading?
    pub fn matches(&self, meta: &Metainfo) -> bool {
        self.info_hash == meta.info_hash
            && self.piece_length == meta.piece_length
            && self.total_length == meta.total_length
            && self.have.len() == meta.piece_count()
    }

    /// Load the sidecar for `meta` under `root`, if there is a usable one.
    ///
    /// Returns `None` for every kind of "we cannot trust this", which is the
    /// only safe way to fail: the fallback is simply to re-hash.
    pub async fn load(root: impl AsRef<Path>, meta: &Metainfo) -> Option<Self> {
        let path = Self::path(root, meta.info_hash);
        let raw = tokio::fs::read(&path).await.ok()?;
        match Self::decode(&raw) {
            Ok(state) if state.matches(meta) => Some(state),
            Ok(_) => {
                tracing::debug!(path = %path.display(), "resume file is for a different torrent");
                None
            }
            Err(err) => {
                tracing::debug!(path = %path.display(), %err, "ignoring resume file");
                None
            }
        }
    }

    /// Write the sidecar atomically, so a crash mid-write cannot leave a
    /// half-file that decodes to something plausible.
    pub async fn save(&self, root: impl AsRef<Path>) -> Result<()> {
        let path = Self::path(root, self.info_hash);
        let temp = path.with_extension("resume.tmp");
        tokio::fs::write(&temp, self.encode()).await?;
        tokio::fs::rename(&temp, &path).await?;
        Ok(())
    }

    pub async fn remove(root: impl AsRef<Path>, info_hash: InfoHash) {
        let _ = tokio::fs::remove_file(Self::path(root, info_hash)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha1::Sha1;

    fn bstr(s: &[u8]) -> Vec<u8> {
        let mut out = format!("{}:", s.len()).into_bytes();
        out.extend_from_slice(s);
        out
    }

    fn meta(pieces: usize) -> Metainfo {
        let mut info = Vec::new();
        info.extend(b"d");
        info.extend(format!("6:lengthi{}e", pieces * 1024).into_bytes());
        info.extend(bstr(b"name"));
        info.extend(bstr(b"payload.bin"));
        info.extend(bstr(b"piece length"));
        info.extend(b"i1024e");
        info.extend(bstr(b"pieces"));
        info.extend(bstr(&vec![0xAAu8; pieces * 20]));
        info.extend(b"e");
        Metainfo::from_info_dict(&info).unwrap()
    }

    fn have(count: usize, set: &[usize]) -> Bitfield {
        let mut field = Bitfield::empty(count);
        for index in set {
            field.set(*index);
        }
        field
    }

    #[test]
    fn round_trips() {
        let meta = meta(10);
        let state = ResumeState::new(&meta, have(10, &[0, 3, 9]), true);
        let decoded = ResumeState::decode(&state.encode()).unwrap();

        assert_eq!(decoded, state);
        assert!(decoded.have.has(0));
        assert!(decoded.have.has(9));
        assert!(!decoded.have.has(1));
        assert!(decoded.clean);
        assert!(decoded.matches(&meta));
    }

    #[test]
    fn an_unclean_state_says_so() {
        let meta = meta(4);
        let state = ResumeState::new(&meta, have(4, &[1]), false);
        assert!(!ResumeState::decode(&state.encode()).unwrap().clean);
    }

    #[test]
    fn tampering_is_caught_by_the_checksum() {
        let meta = meta(16);
        let mut raw = ResumeState::new(&meta, have(16, &[0, 1, 2]), true).encode();
        let last_bit = HEADER_LEN;
        raw[last_bit] ^= 0b0000_1000;
        let err = ResumeState::decode(&raw).unwrap_err();
        assert!(format!("{err}").contains("checksum"), "{err}");
    }

    #[test]
    fn truncation_is_caught() {
        let meta = meta(16);
        let raw = ResumeState::new(&meta, have(16, &[0]), true).encode();
        assert!(ResumeState::decode(&raw[..raw.len() - 5]).is_err());
        assert!(ResumeState::decode(b"").is_err());
        assert!(ResumeState::decode(b"not a resume file at all, honestly").is_err());
    }

    #[test]
    fn a_state_for_another_torrent_does_not_match() {
        let mine = meta(10);
        let theirs = meta(20);
        let state = ResumeState::new(&theirs, have(20, &[0]), true);
        assert!(!state.matches(&mine));
    }

    #[test]
    fn a_resized_torrent_does_not_match() {
        let meta_a = meta(10);
        let mut state = ResumeState::new(&meta_a, have(10, &[0]), true);
        // Same infohash, different length: the file must have changed under us.
        state.total_length += 1;
        assert!(!state.matches(&meta_a));
    }

    #[tokio::test]
    async fn saves_and_loads_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(8);
        let state = ResumeState::new(&meta, have(8, &[2, 5]), true);
        state.save(dir.path()).await.unwrap();

        let loaded = ResumeState::load(dir.path(), &meta).await.expect("loads");
        assert_eq!(loaded, state);
        // And no stray temp file left behind.
        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].ends_with(".resume"));
    }

    #[tokio::test]
    async fn a_missing_or_corrupt_file_just_means_no_resume() {
        let dir = tempfile::tempdir().unwrap();
        let meta = meta(8);
        assert!(ResumeState::load(dir.path(), &meta).await.is_none());

        tokio::fs::write(
            ResumeState::path(dir.path(), meta.info_hash),
            b"absolute rubbish",
        )
        .await
        .unwrap();
        assert!(
            ResumeState::load(dir.path(), &meta).await.is_none(),
            "a bad file must fall back to re-hashing, not explode"
        );
    }

    #[tokio::test]
    async fn a_state_for_a_different_torrent_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mine = meta(8);
        let theirs = meta(9);
        // Write theirs under *our* path, as a stale download directory would.
        let state = ResumeState::new(&theirs, have(9, &[0]), true);
        tokio::fs::write(
            ResumeState::path(dir.path(), mine.info_hash),
            state.encode(),
        )
        .await
        .unwrap();
        assert!(ResumeState::load(dir.path(), &mine).await.is_none());
    }

    #[test]
    fn the_digest_covers_the_whole_body() {
        let meta = meta(10);
        let raw = ResumeState::new(&meta, have(10, &[4]), true).encode();
        let (body, digest) = raw.split_at(raw.len() - 20);
        assert_eq!(Sha1::digest(body).as_slice(), digest);
    }
}
