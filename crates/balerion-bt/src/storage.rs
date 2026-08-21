//! Where the bytes land.
//!
//! Files are pre-created at full length and written sparsely, since pieces
//! arrive out of order. A piece can straddle any number of files, so every
//! write fans out over [`Metainfo::piece_slices`].

use std::io::SeekFrom;
use std::path::{Component, Path, PathBuf};

use sha1::{Digest, Sha1};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::error::{Error, Result};
use crate::metainfo::Metainfo;
use crate::wire::Bitfield;

/// On-disk storage for one torrent.
#[derive(Debug)]
pub struct Storage {
    root: PathBuf,
    paths: Vec<PathBuf>,
    meta: Metainfo,
}

impl Storage {
    /// Create (or reopen) the torrent's files under `root`.
    ///
    /// Every path is checked to stay inside `root`. Metainfo parsing already
    /// rejects `..` segments, but a second check here is cheap and this is the
    /// place where being wrong means writing to someone's ssh config.
    pub async fn create(root: impl AsRef<Path>, meta: &Metainfo) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&root).await?;

        let mut paths = Vec::with_capacity(meta.files.len());
        for file in &meta.files {
            let path = safe_join(&root, &file.path)?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let handle = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .await?;
            // Set the final length up front so writes at any offset land, and
            // so the filesystem can allocate sensibly. Sparse until written.
            if handle.metadata().await?.len() != file.length {
                handle.set_len(file.length).await?;
            }
            paths.push(path);
        }

        Ok(Self {
            root,
            paths,
            meta: meta.clone(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Write a verified piece, fanning out across whichever files it covers.
    pub async fn write_piece(&self, index: usize, data: &[u8]) -> Result<()> {
        let expected = self
            .meta
            .piece_size(index)
            .ok_or_else(|| Error::Metainfo(format!("piece {index} is out of range")))?;
        if data.len() as u64 != expected {
            return Err(Error::Metainfo(format!(
                "piece {index} is {} bytes, expected {expected}",
                data.len()
            )));
        }

        for slice in self.meta.piece_slices(index) {
            let mut file = self.open(slice.file_index).await?;
            file.seek(SeekFrom::Start(slice.file_offset)).await?;
            let from = slice.offset_in_span as usize;
            let to = from + slice.length as usize;
            file.write_all(&data[from..to]).await?;
            file.flush().await?;
        }
        Ok(())
    }

    /// Read a piece back off disk, for verification or resume.
    pub async fn read_piece(&self, index: usize) -> Result<Vec<u8>> {
        let size = self
            .meta
            .piece_size(index)
            .ok_or_else(|| Error::Metainfo(format!("piece {index} is out of range")))?;
        let mut out = vec![0u8; size as usize];

        for slice in self.meta.piece_slices(index) {
            let mut file = self.open(slice.file_index).await?;
            file.seek(SeekFrom::Start(slice.file_offset)).await?;
            let from = slice.offset_in_span as usize;
            let to = from + slice.length as usize;
            file.read_exact(&mut out[from..to]).await?;
        }
        Ok(out)
    }

    /// Read an arbitrary byte span of the concatenated piece space.
    ///
    /// The span is clamped to the torrent, so the returned buffer may be
    /// shorter than `len` at the tail. Callers serving HTTP ranges should use
    /// its actual length rather than assuming they got what they asked for.
    ///
    /// This reads whatever is on disk: files are created at full length and
    /// filled sparsely, so asking for a span whose pieces have not arrived
    /// yields zeros rather than an error. Only ask for verified pieces.
    pub async fn read_range(&self, start: u64, len: u64) -> Result<Vec<u8>> {
        let slices = self.meta.byte_slices(start, len);
        let total: u64 = slices.iter().map(|slice| slice.length).sum();
        let mut out = vec![0u8; total as usize];

        for slice in slices {
            let mut file = self.open(slice.file_index).await?;
            file.seek(SeekFrom::Start(slice.file_offset)).await?;
            let from = slice.offset_in_span as usize;
            let to = from + slice.length as usize;
            file.read_exact(&mut out[from..to]).await?;
        }
        Ok(out)
    }

    /// Does this data hash to what the info dict promised for this piece?
    pub fn verify(&self, index: usize, data: &[u8]) -> bool {
        match self.meta.piece_hash(index) {
            Some(expected) => Sha1::digest(data).as_slice() == expected,
            None => false,
        }
    }

    /// Re-hash everything on disk to work out what we already have.
    ///
    /// Used on resume, and after an unclean shutdown when a persisted bitfield
    /// cannot be trusted. `on_progress` is called with (checked, total).
    pub async fn verify_all<F>(&self, mut on_progress: F) -> Result<Bitfield>
    where
        F: FnMut(usize, usize),
    {
        let count = self.meta.piece_count();
        let mut have = Bitfield::empty(count);
        for index in 0..count {
            if let Ok(data) = self.read_piece(index).await
                && self.verify(index, &data)
            {
                have.set(index);
            }
            on_progress(index + 1, count);
        }
        Ok(have)
    }

    async fn open(&self, file_index: usize) -> Result<File> {
        let path = self
            .paths
            .get(file_index)
            .ok_or_else(|| Error::Metainfo(format!("file {file_index} is out of range")))?;
        Ok(OpenOptions::new().read(true).write(true).open(path).await?)
    }
}

/// Join a torrent-supplied relative path onto a root, refusing anything that
/// would escape it.
///
/// Public because the file's real path is worth knowing outside this module:
/// once every piece is on disk there is no reason to read it back through an
/// HTTP endpoint, and the caller needs the same joining rule rather than its
/// own approximation of it.
pub fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    for segment in relative.split('/') {
        let component = Path::new(segment);
        let mut components = component.components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(part)), None) => path.push(part),
            _ => {
                return Err(Error::Metainfo(format!(
                    "refusing unsafe path segment {segment:?} in {relative:?}"
                )));
            }
        }
    }
    if !path.starts_with(root) {
        return Err(Error::Metainfo(format!(
            "path {relative:?} escapes the download directory"
        )));
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bstr(s: &[u8]) -> Vec<u8> {
        let mut out = format!("{}:", s.len()).into_bytes();
        out.extend_from_slice(s);
        out
    }

    /// A two-file torrent whose second piece straddles the boundary.
    fn multi_file(content: &[u8]) -> Metainfo {
        assert_eq!(content.len(), 1000);
        let piece_length = 512usize;
        let mut hashes = Vec::new();
        for chunk in content.chunks(piece_length) {
            hashes.extend_from_slice(&Sha1::digest(chunk));
        }

        let mut info = Vec::new();
        info.extend(b"d");
        info.extend(bstr(b"files"));
        info.extend(b"l");
        info.extend(b"d");
        info.extend(bstr(b"length"));
        info.extend(b"i600e");
        info.extend(bstr(b"path"));
        info.extend(b"l");
        info.extend(bstr(b"sub"));
        info.extend(bstr(b"one.bin"));
        info.extend(b"e");
        info.extend(b"e");
        info.extend(b"d");
        info.extend(bstr(b"length"));
        info.extend(b"i400e");
        info.extend(bstr(b"path"));
        info.extend(b"l");
        info.extend(bstr(b"two.bin"));
        info.extend(b"e");
        info.extend(b"e");
        info.extend(b"e");
        info.extend(bstr(b"name"));
        info.extend(bstr(b"a-torrent"));
        info.extend(bstr(b"piece length"));
        info.extend(b"i512e");
        info.extend(bstr(b"pieces"));
        info.extend(bstr(&hashes));
        info.extend(b"e");

        Metainfo::from_info_dict(&info).unwrap()
    }

    fn content() -> Vec<u8> {
        (0..1000u32).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn creates_files_at_full_length() {
        let dir = tempfile::tempdir().unwrap();
        let meta = multi_file(&content());
        let storage = Storage::create(dir.path(), &meta).await.unwrap();

        assert_eq!(storage.paths().len(), 2);
        assert!(storage.paths()[0].ends_with("a-torrent/sub/one.bin"));
        assert_eq!(
            tokio::fs::metadata(&storage.paths()[0])
                .await
                .unwrap()
                .len(),
            600
        );
        assert_eq!(
            tokio::fs::metadata(&storage.paths()[1])
                .await
                .unwrap()
                .len(),
            400
        );
    }

    #[tokio::test]
    async fn a_piece_spanning_two_files_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let content = content();
        let meta = multi_file(&content);
        let storage = Storage::create(dir.path(), &meta).await.unwrap();

        // Piece 1 covers bytes 512..1000, crossing the boundary at 600.
        let piece = &content[512..1000];
        assert!(storage.verify(1, piece), "fixture hashes must line up");
        storage.write_piece(1, piece).await.unwrap();

        assert_eq!(storage.read_piece(1).await.unwrap(), piece);
        // And the bytes landed in the right halves of the right files.
        let one = tokio::fs::read(&storage.paths()[0]).await.unwrap();
        let two = tokio::fs::read(&storage.paths()[1]).await.unwrap();
        assert_eq!(&one[512..600], &content[512..600]);
        assert_eq!(&two[..400], &content[600..1000]);
    }

    #[tokio::test]
    async fn writing_every_piece_reproduces_the_content_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let content = content();
        let meta = multi_file(&content);
        let storage = Storage::create(dir.path(), &meta).await.unwrap();

        for index in 0..meta.piece_count() {
            let start = index * meta.piece_length as usize;
            let end = (start + meta.piece_length as usize).min(content.len());
            storage
                .write_piece(index, &content[start..end])
                .await
                .unwrap();
        }

        let mut rebuilt = tokio::fs::read(&storage.paths()[0]).await.unwrap();
        rebuilt.extend(tokio::fs::read(&storage.paths()[1]).await.unwrap());
        assert_eq!(rebuilt, content);
    }

    #[tokio::test]
    async fn verify_all_finds_exactly_what_was_written() {
        let dir = tempfile::tempdir().unwrap();
        let content = content();
        let meta = multi_file(&content);
        let storage = Storage::create(dir.path(), &meta).await.unwrap();

        storage.write_piece(0, &content[..512]).await.unwrap();

        let mut progress = 0;
        let have = storage.verify_all(|done, _| progress = done).await.unwrap();
        assert!(have.has(0));
        assert!(
            !have.has(1),
            "unwritten pieces are zeros, which will not hash"
        );
        assert_eq!(progress, meta.piece_count());
    }

    #[tokio::test]
    async fn read_range_stitches_across_a_file_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let content = content();
        let meta = multi_file(&content);
        let storage = Storage::create(dir.path(), &meta).await.unwrap();

        for index in 0..meta.piece_count() {
            let start = index * meta.piece_length as usize;
            let end = (start + meta.piece_length as usize).min(content.len());
            storage
                .write_piece(index, &content[start..end])
                .await
                .unwrap();
        }

        // Wholly inside the second file, which begins at 600.
        assert_eq!(
            storage.read_range(700, 50).await.unwrap(),
            &content[700..750]
        );
        // Across the boundary, which is the case worth having a test for.
        assert_eq!(
            storage.read_range(580, 40).await.unwrap(),
            &content[580..620]
        );
        // And the whole thing, spanning both files and every piece.
        assert_eq!(storage.read_range(0, 1000).await.unwrap(), content);
    }

    #[tokio::test]
    async fn read_range_clamps_at_the_end_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let content = content();
        let meta = multi_file(&content);
        let storage = Storage::create(dir.path(), &meta).await.unwrap();
        storage.write_piece(1, &content[512..1000]).await.unwrap();

        // A player probing with `Range: bytes=950-` asks for more than exists.
        let tail = storage.read_range(950, 10_000).await.unwrap();
        assert_eq!(tail, &content[950..1000], "short read, not an error");
        assert!(storage.read_range(1000, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_corrupted_piece_fails_verification() {
        let dir = tempfile::tempdir().unwrap();
        let content = content();
        let meta = multi_file(&content);
        let storage = Storage::create(dir.path(), &meta).await.unwrap();

        let mut tampered = content[..512].to_vec();
        tampered[0] ^= 0xff;
        assert!(!storage.verify(0, &tampered));
    }

    #[tokio::test]
    async fn wrong_sized_pieces_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let meta = multi_file(&content());
        let storage = Storage::create(dir.path(), &meta).await.unwrap();

        assert!(storage.write_piece(0, b"too short").await.is_err());
        assert!(storage.write_piece(99, &[0; 512]).await.is_err());
    }

    #[test]
    fn path_joining_refuses_to_escape() {
        let root = Path::new("/tmp/dl");
        assert_eq!(
            safe_join(root, "torrent/file.bin").unwrap(),
            Path::new("/tmp/dl/torrent/file.bin")
        );
        assert!(safe_join(root, "../etc/passwd").is_err());
        assert!(safe_join(root, "a/../../b").is_err());
        assert!(safe_join(root, "/absolute").is_err());
        assert!(safe_join(root, "a/./b").is_err());
    }
}
