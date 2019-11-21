use crate::event::TimedEvent;

use std::collections::VecDeque;
use std::fmt;
use std::fs::File;
use std::io;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time;

use digest::generic_array::{sequence::*, typenum::consts::*, GenericArray};
use digest::Digest;
use meowhash::MeowHasher;

/// Determines whether frame digests are recorded, verified or ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestMode {
    /// Verify digests.
    Verify,
    /// Record digests.
    Record,
    /// Ignore digest.
    Ignore,
}

#[derive(Debug, Clone)]
pub struct DigestState {
    pub mode: DigestMode,
    pub path: Option<PathBuf>,

    pub recorder: FrameRecorder,
}

impl DigestState {
    pub fn from<P: AsRef<Path>>(mode: DigestMode, path: P) -> io::Result<Self> {
        match mode {
            DigestMode::Verify => Self::verify(path),
            DigestMode::Record => Self::record(path),
            DigestMode::Ignore => Self::ignore(),
        }
    }

    pub fn verify<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let mut frames = Vec::new();
        let path = path.as_ref();

        match File::open(&path) {
            Ok(f) => {
                let r = io::BufReader::new(f);
                for line in r.lines() {
                    let line = line?;
                    let hash = Hash::from_str(line.as_str()).map_err(|e| {
                        io::Error::new(io::ErrorKind::InvalidInput, e)
                    })?;
                    frames.push(hash);
                }
            }
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!("{}: {}", path.display(), e),
                ));
            }
        }

        Ok(Self {
            mode: DigestMode::Verify,
            recorder: FrameRecorder::from(frames),
            path: Some(path.into()),
        })
    }

    pub fn record<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Ok(Self {
            mode: DigestMode::Record,
            recorder: FrameRecorder::new(),
            path: Some(path.as_ref().into()),
        })
    }

    pub fn ignore() -> io::Result<Self> {
        Ok(Self {
            mode: DigestMode::Ignore,
            recorder: FrameRecorder::new(),
            path: None,
        })
    }
}

/// Execution mode. Controls whether the session is playing or recording
/// commands.
// TODO: Make this a `struct` and have `ExecutionMode`.
#[derive(Debug, Clone)]
pub enum Execution {
    /// Normal execution. User inputs are processed normally.
    Normal,
    /// Recording user inputs to log.
    Recording {
        /// Events being recorded.
        events: Vec<TimedEvent>,
        /// Start time of recording.
        start: time::Instant,
        /// Path to save recording to.
        path: PathBuf,
        /// Digest mode.
        digest: DigestState,
    },
    /// Replaying inputs from log.
    Replaying {
        /// Events being replayed.
        events: VecDeque<TimedEvent>,
        /// Start time of the playback.
        start: time::Instant,
        /// Path to read events from.
        path: PathBuf,
        /// Digest mode.
        digest: DigestState,
        /// Replay result.
        result: ReplayResult,
    },
}

impl Execution {
    /// Create a normal execution.
    pub fn normal() -> io::Result<Self> {
        Ok(Self::Normal)
    }

    /// Create a recording.
    pub fn recording<P: AsRef<Path>>(
        path: P,
        mode: DigestMode,
    ) -> io::Result<Self> {
        use io::{Error, ErrorKind};

        let path = path.as_ref();
        let file_name: &Path = path
            .file_name()
            .ok_or(Error::new(
                ErrorKind::InvalidInput,
                format!("invalid path {:?}", path),
            ))?
            .as_ref();

        let digest = DigestState::from(
            mode,
            path.join(file_name).with_extension("digest"),
        )?;

        Ok(Self::Recording {
            events: Vec::new(),
            start: time::Instant::now(),
            path: path.to_path_buf(),
            digest,
        })
    }

    /// Create a replay.
    pub fn replaying<P: AsRef<Path>>(
        path: P,
        mode: DigestMode,
    ) -> io::Result<Self> {
        use io::{Error, ErrorKind};

        let mut events = VecDeque::new();
        let path = path.as_ref();

        let file_name: &Path = path
            .file_name()
            .ok_or(Error::new(
                ErrorKind::InvalidInput,
                format!("invalid path {:?}", path),
            ))?
            .as_ref();

        let digest = DigestState::from(
            mode,
            path.join(file_name).with_extension("digest"),
        )?;

        let events_path = path.join(file_name).with_extension("events");
        match File::open(&events_path) {
            Ok(f) => {
                let r = io::BufReader::new(f);
                for (i, line) in r.lines().enumerate() {
                    let line = line?;
                    let ev = TimedEvent::from_str(&line).map_err(|e| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!(
                                "{}:{}: {}",
                                events_path.display(),
                                i + 1,
                                e
                            ),
                        )
                    })?;
                    events.push_back(ev);
                }
                Ok(Self::Replaying {
                    events,
                    start: time::Instant::now(),
                    path: path.to_path_buf(),
                    digest,
                    result: ReplayResult::new(),
                })
            }
            Err(e) => Err(io::Error::new(
                e.kind(),
                format!("{}: {}", events_path.display(), e),
            )),
        }
    }

    pub fn is_normal(&self) -> bool {
        if let Execution::Normal = self {
            true
        } else {
            false
        }
    }

    pub fn record(&mut self, data: &[u8]) {
        match self {
            // Replaying and verifying digests.
            Self::Replaying {
                digest:
                    DigestState {
                        mode: DigestMode::Verify,
                        recorder,
                        ..
                    },
                result,
                ..
            } => {
                result.record(recorder.verify_frame(data));
            }
            // Replaying/Recording and recording digests.
            Self::Replaying {
                digest:
                    DigestState {
                        mode: DigestMode::Record,
                        recorder,
                        ..
                    },
                ..
            }
            | Self::Recording {
                digest:
                    DigestState {
                        mode: DigestMode::Record,
                        recorder,
                        ..
                    },
                ..
            } => {
                recorder.record_frame(data);
            }
            _ => {}
        }
    }

    ////////////////////////////////////////////////////////////////////////////

    pub fn stop_recording(&mut self) -> io::Result<PathBuf> {
        use io::{Error, ErrorKind};
        use std::io::Write;

        let result = if let Execution::Recording {
            events,
            path,
            digest,
            ..
        } = &self
        {
            std::fs::create_dir_all(path)?;
            let file_name: &Path = path
                .file_name()
                .ok_or(Error::new(
                    ErrorKind::InvalidInput,
                    format!("invalid path {:?}", path),
                ))?
                .as_ref();

            let mut f =
                File::create(path.join(file_name.with_extension("events")))?;
            for ev in events.clone() {
                writeln!(&mut f, "{}", String::from(ev))?;
            }

            if let DigestState {
                mode: DigestMode::Record,
                path: Some(path),
                recorder,
                ..
            } = digest
            {
                Execution::write_digest(recorder, path)?;
            }
            Ok(path.clone())
        } else {
            panic!("record finalizer called outside of recording context")
        };

        if result.is_ok() {
            *self = Execution::Normal;
        }
        result
    }

    pub fn finalize_replaying(&self) -> io::Result<PathBuf> {
        if let Execution::Replaying {
            digest:
                DigestState {
                    mode: DigestMode::Record,
                    recorder,
                    path: Some(path),
                    ..
                },
            ..
        } = &self
        {
            Execution::write_digest(recorder, path)?;
            Ok(path.clone())
        } else {
            panic!("replay finalizer called outside of replay context")
        }
    }

    ////////////////////////////////////////////////////////////////////////////

    fn write_digest<P: AsRef<Path>>(
        recorder: &FrameRecorder,
        path: P,
    ) -> io::Result<()> {
        use std::io::Write;

        let path = path.as_ref();

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = File::create(path)?;

        for frame in &recorder.frames {
            writeln!(&mut f, "{}", frame)?;
        }
        Ok(())
    }
}

impl Default for Execution {
    fn default() -> Self {
        Execution::Normal
    }
}

/// Records and verifies frames being replayed.
#[derive(Debug, Clone)]
pub struct FrameRecorder {
    frames: VecDeque<Hash>,
    last_verified: Option<Hash>,
}

impl FrameRecorder {
    fn new() -> Self {
        Self {
            frames: VecDeque::new(),
            last_verified: None,
        }
    }

    fn from(frames: Vec<Hash>) -> Self {
        Self {
            frames: frames.into(),
            last_verified: None,
        }
    }

    fn record_frame(&mut self, data: &[u8]) {
        let hash = Self::hash(data);

        if self.frames.back().map(|h| h != &hash).unwrap_or(true) {
            info!("frame: {}", hash);
            self.frames.push_back(hash);
        }
    }

    fn verify_frame(&mut self, data: &[u8]) -> VerifyResult {
        let actual = Self::hash(data);

        if self.frames.is_empty() {
            return VerifyResult::EOF;
        }
        if Some(actual.clone()) == self.last_verified {
            return VerifyResult::Stale(actual);
        }
        self.last_verified = Some(actual.clone());

        if let Some(expected) = self.frames.pop_front() {
            if actual == expected {
                VerifyResult::Okay(actual)
            } else {
                VerifyResult::Failed(actual, expected)
            }
        } else {
            VerifyResult::EOF
        }
    }

    fn hash(data: &[u8]) -> Hash {
        let bytes: GenericArray<u8, U64> = MeowHasher::digest(data);
        let (prefix, _): (GenericArray<u8, U4>, _) = bytes.split();

        Hash(prefix.into())
    }
}

#[derive(Debug, Clone)]
pub struct ReplayResult {
    verify_results: Vec<VerifyResult>,
    eof: bool,
    okay_count: u32,
    failed_count: u32,
    stale_count: u32,
}

impl ReplayResult {
    pub fn is_ok(&self) -> bool {
        self.failed_count == 0
    }

    pub fn is_err(&self) -> bool {
        !self.is_ok()
    }

    pub fn is_done(&self) -> bool {
        self.eof
    }

    pub fn summary(&self) -> String {
        let total = (self.okay_count + self.failed_count) as f32;

        format!(
            "{:.1}% OK, {:.1}% FAILED",
            self.okay_count as f32 / total * 100.,
            self.failed_count as f32 / total * 100.
        )
    }

    ////////////////////////////////////////////////////////////////////////////

    fn new() -> Self {
        ReplayResult {
            verify_results: Vec::new(),
            okay_count: 0,
            failed_count: 0,
            stale_count: 0,
            eof: false,
        }
    }

    fn record(&mut self, result: VerifyResult) {
        match &result {
            VerifyResult::Okay(actual) => {
                info!("verify: {} OK", actual);
                self.okay_count += 1;
            }
            VerifyResult::Failed(actual, expected) => {
                error!("verify: {} != {}", actual, expected);
                self.failed_count += 1;
                // TODO: Stop replaying
            }
            VerifyResult::EOF => {
                self.eof = true;
            }
            VerifyResult::Stale { .. } => {
                self.stale_count += 1;
            }
        }
        self.verify_results.push(result);
    }
}

#[derive(Debug, Clone)]
pub enum VerifyResult {
    /// The hash has already been verified.
    Stale(Hash),
    /// The actual and expected hashes match.
    Okay(Hash),
    /// The actual and expected hashes don't match.
    Failed(Hash, Hash),
    /// There are no further expected hashes.
    EOF,
}

#[derive(PartialEq, Eq, Clone, Debug)]
pub struct Hash([u8; 4]);

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for byte in self.0.iter() {
            write!(f, "{:02x}", byte)?;
        }
        Ok(())
    }
}

impl FromStr for Hash {
    type Err = String;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let val = |c: u8| match c {
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'0'..=b'9' => Ok(c - b'0'),
            _ => Err(format!("invalid hex character {:?}", c)),
        };

        let mut hash: Vec<u8> = Vec::new();
        for pair in input.bytes().collect::<Vec<u8>>().chunks(2) {
            match pair {
                [l, r] => {
                    let left = val(*l)? << 4;
                    let right = val(*r)?;

                    hash.push(left | right);
                }
                _ => return Err(format!("invalid hex string: {:?}", input)),
            }
        }

        let mut array = [0; 4];
        array.copy_from_slice(hash.as_slice());

        Ok(Hash(array))
    }
}
