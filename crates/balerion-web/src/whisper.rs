//! Subtitles from the audio itself, when nobody has any.
//!
//! The last resort, and the only one that always works. OpenSubtitles has
//! nothing for a great deal of what the Archive holds, and nothing at all for
//! an obscure release; a transcription needs no account, no allowance and no
//! luck.
//!
//! It also side-steps [`crate::subsync`] entirely, which is the part worth
//! noticing. A transcript is derived from the audio, so it is in step with the
//! audio by construction. There is no offset to find and no framerate to
//! correct, because there was never a second copy of the film involved.
//!
//! And it answers the other half of "English subtitles", which is a film that
//! is not in English. whisper's translate task produces English from any of its
//! languages, which no subtitle index can do for something nobody has bothered
//! to subtitle.
//!
//! Optional, and detected the same way ffmpeg is: absent, the feature does not
//! appear and everything else carries on. It is not cheap. A feature film is
//! minutes of CPU on a fast machine and rather more on a slow one, which is why
//! it runs in the background and why the result is kept.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

/// Long, because this is the point of the exercise: a feature film transcribed
/// on a laptop takes a while. Bounded at all only so a wedged process cannot
/// sit there for the life of the server.
const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(3 * 60 * 60);

/// The binary names whisper.cpp has shipped under.
///
/// Deliberately not `main`, which is what it built to for years. Executing
/// something called `main` because it happens to be on a PATH is not a thing to
/// do without being asked, and anyone with an old build can point
/// `BALERION_WHISPER_BIN` at it.
const BINARIES: &[&str] = &["whisper-cli", "whisper-cpp", "whisper"];

/// Where a model might be, when nobody said.
///
/// Only ever consulted after the environment variable, so a deliberate choice
/// always wins over a guess.
fn default_model_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(dirs) = directories::ProjectDirs::from("", "", "balerion") {
        paths.push(dirs.data_dir().join("models/ggml-base.en.bin"));
        paths.push(dirs.data_dir().join("models/ggml-base.bin"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        paths.push(home.join(".local/share/whisper/ggml-base.en.bin"));
        paths.push(home.join("models/ggml-base.en.bin"));
    }
    paths
}

/// whisper.cpp, if this machine has it and a model to run.
#[derive(Debug, Clone)]
pub struct Whisper {
    pub binary: PathBuf,
    pub model: PathBuf,
}

impl Whisper {
    /// Look for the binary and a model.
    ///
    /// Both are required: a binary with no model can do nothing, and reporting
    /// the feature as available and then failing on every use is worse than not
    /// offering it.
    pub async fn detect() -> Option<Self> {
        let binary = find_binary().await?;
        let model = find_model()?;
        tracing::info!(
            binary = %binary.display(),
            model = %model.display(),
            "transcription is available"
        );
        Some(Self { binary, model })
    }

    /// Transcribe a file's audio into cues.
    ///
    /// `translate` asks for English out regardless of what went in, which is
    /// the whole reason this beats a subtitle index for a foreign film nobody
    /// has subtitled.
    ///
    /// The audio is extracted first rather than handing whisper the URL: it
    /// wants a 16 kHz mono WAV and will not fetch one over HTTP, and ffmpeg
    /// reading through balerion's own range endpoint means the piece picker
    /// steers for it as usual.
    pub async fn transcribe(
        &self,
        tools: &crate::ffmpeg::Tools,
        url: &str,
        translate: bool,
    ) -> Result<Vec<crate::subtitles::Cue>> {
        let work = tempfile::tempdir().context("could not make a working directory")?;
        let audio = work.path().join("audio.wav");

        let extraction = Command::new(&tools.ffmpeg)
            .args([
                "-hide_banner",
                "-v",
                "error",
                "-i",
                url,
                "-map",
                "0:a:0",
                "-ac",
                "1",
                "-ar",
                "16000",
                "-c:a",
                "pcm_s16le",
                "-f",
                "wav",
            ])
            .arg(&audio)
            .stdin(Stdio::null())
            .output()
            .await
            .context("could not run ffmpeg")?;
        if !extraction.status.success() {
            bail!(
                "could not extract the audio: {}",
                String::from_utf8_lossy(&extraction.stderr).trim()
            );
        }

        let stem = work.path().join("out");
        let mut args: Vec<String> = vec![
            "-m".into(),
            self.model.display().to_string(),
            "-f".into(),
            audio.display().to_string(),
            // WebVTT straight out, so there is no third timestamp format to
            // parse and get subtly wrong.
            "-ovtt".into(),
            "-of".into(),
            stem.display().to_string(),
            // Every core it can have. This is the slow part of the program by a
            // wide margin.
            "-t".into(),
            std::thread::available_parallelism()
                .map(|cores| cores.get())
                .unwrap_or(4)
                .to_string(),
        ];
        if translate {
            args.push("-tr".into());
            args.push("-l".into());
            args.push("auto".into());
        }

        let run = tokio::time::timeout(
            TRANSCRIBE_TIMEOUT,
            Command::new(&self.binary)
                .args(&args)
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .context("the transcription took too long")?
        .context("could not run whisper")?;

        if !run.status.success() {
            bail!(
                "whisper failed: {}",
                String::from_utf8_lossy(&run.stderr).trim()
            );
        }

        let written = stem.with_extension("vtt");
        let text = tokio::fs::read_to_string(&written)
            .await
            .with_context(|| format!("whisper wrote nothing to {}", written.display()))?;

        let cues = crate::subtitles::parse_cues(&text);
        if cues.is_empty() {
            bail!("the transcription came back empty");
        }
        Ok(cues)
    }
}

async fn find_binary() -> Option<PathBuf> {
    // A named binary always wins, and is the escape hatch for the old `main`
    // name and for a build that is not on the PATH at all.
    if let Some(chosen) = std::env::var_os("BALERION_WHISPER_BIN") {
        let path = PathBuf::from(chosen);
        if path.is_file() {
            return Some(path);
        }
        tracing::warn!(
            path = %path.display(),
            "BALERION_WHISPER_BIN does not point at a file"
        );
        return None;
    }
    for name in BINARIES {
        let ran = Command::new(name)
            .arg("--help")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        // `--help` exits non-zero in some builds, so the test is whether the
        // process ran at all rather than what it said.
        if ran.is_ok() {
            return Some(PathBuf::from(name));
        }
    }
    None
}

fn find_model() -> Option<PathBuf> {
    if let Some(chosen) = std::env::var_os("BALERION_WHISPER_MODEL") {
        let path = PathBuf::from(chosen);
        if path.is_file() {
            return Some(path);
        }
        // Said out loud: somebody set this deliberately and it does not exist,
        // which is a mistake worth hearing about rather than falling back over.
        tracing::warn!(
            path = %path.display(),
            "BALERION_WHISPER_MODEL does not point at a file"
        );
        return None;
    }
    default_model_paths()
        .into_iter()
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_binary_names_are_specific_enough_to_run_unasked() {
        // `main` is what whisper.cpp built to for years, and is deliberately
        // absent: running whatever is called `main` on somebody's PATH is not a
        // thing to do on their behalf. BALERION_WHISPER_BIN covers that case.
        assert!(BINARIES.contains(&"whisper-cli"));
        assert!(!BINARIES.contains(&"main"), "far too generic to execute");
        assert!(
            BINARIES.iter().all(|name| name.contains("whisper")),
            "every name should say what it is"
        );
    }

    #[test]
    fn a_model_that_was_asked_for_and_is_missing_is_not_papered_over() {
        // Falling back to a guessed model when somebody named one would
        // transcribe with the wrong thing and never say so.
        temp_env(
            "BALERION_WHISPER_MODEL",
            Some("/definitely/not/here.bin"),
            || assert_eq!(find_model(), None),
        );
    }

    #[test]
    fn a_model_that_was_asked_for_and_exists_wins() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        temp_env(
            "BALERION_WHISPER_MODEL",
            Some(&path.display().to_string()),
            || {
                assert_eq!(find_model(), Some(path.clone()));
            },
        );
    }

    #[test]
    fn the_default_locations_are_all_absolute() {
        // A relative path would resolve against whatever directory the server
        // happened to be started in.
        for path in default_model_paths() {
            assert!(path.is_absolute(), "{path:?}");
        }
    }

    /// Set an environment variable for the duration of a closure.
    ///
    /// Serialised on a mutex because the environment is process-wide and the
    /// test harness is threaded.
    fn temp_env(key: &str, value: Option<&str>, body: impl FnOnce()) {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|err| err.into_inner());

        let previous = std::env::var_os(key);
        // Safety: the mutex above makes this the only thread touching the
        // environment for the duration.
        unsafe {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        body();
        unsafe {
            match previous {
                Some(previous) => std::env::set_var(key, previous),
                None => std::env::remove_var(key),
            }
        }
    }
}
