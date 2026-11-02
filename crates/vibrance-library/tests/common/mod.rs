//! Fixtures and scan helpers shared by the integration tests.
#![allow(dead_code)]

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use vibrance_library::{Library, ScanEvent, Track, TrackId, scan};

// ---------------------------------------------------------------------------------------------
// Fixtures

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(name: &str) -> TempDir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "vibrance-library-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        TempDir(dir.canonicalize().unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn db(&self) -> PathBuf {
        self.0.join("db").join("library.db")
    }

    pub fn music(&self) -> PathBuf {
        self.0.join("music")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub fn ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("ffmpeg must be installed to run these tests");
    assert!(
        out.status.success(),
        "ffmpeg {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

pub fn codec_args(path: &Path) -> &'static [&'static str] {
    let name = path
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_ascii_lowercase();
    let ext = name.rsplit('.').next().unwrap();
    match ext {
        "mp3" => &["-c:a", "libmp3lame", "-b:a", "64k"],
        "flac" => &["-c:a", "flac"],
        "ogg" => &["-c:a", "libvorbis"],
        "opus" => &["-ar", "48000", "-c:a", "libopus"],
        "m4a" if name.contains("alac") => &["-c:a", "alac"],
        "m4a" => &["-c:a", "aac"],
        "wav" => &["-c:a", "pcm_s16le"],
        "aiff" => &["-c:a", "pcm_s16be", "-write_id3v2", "1"],
        other => panic!("no codec for .{other}"),
    }
}

/// Half a second of silence, tagged with `tags` (ffmpeg metadata keys).
pub fn audio(path: &Path, tags: &[(&str, &str)]) {
    audio_with_cover(path, tags, None);
}

pub fn audio_with_cover(path: &Path, tags: &[(&str, &str)], cover: Option<(&Path, &str)>) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut args: Vec<String> = ["-f", "lavfi", "-i", "anullsrc=r=44100:cl=stereo:d=0.5"]
        .map(String::from)
        .to_vec();
    if let Some((image, _)) = cover {
        args.extend(["-i".into(), image.to_str().unwrap().into()]);
        args.extend(["-map", "0:a", "-map", "1:v"].map(String::from));
    }
    for (key, value) in tags {
        args.extend(["-metadata".into(), format!("{key}={value}")]);
    }
    args.extend(codec_args(path).iter().map(|s| s.to_string()));
    if let Some((_, picture_type)) = cover {
        args.extend(["-c:v", "png", "-disposition:v", "attached_pic"].map(String::from));
        args.extend(["-metadata:s:v".into(), format!("comment={picture_type}")]);
    }
    args.push(path.to_str().unwrap().into());
    ffmpeg(&args.iter().map(String::as_str).collect::<Vec<_>>());
}

/// A solid-color square image; the format follows the extension.
pub fn image(path: &Path, color: &str, size: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let source = format!("color=c={color}:s={size}x{size}");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        &source,
        "-frames:v",
        "1",
        path.to_str().unwrap(),
    ]);
}

pub const FULL_TAGS: &[(&str, &str)] = &[
    ("title", "  Song One  "),
    ("artist", "Artist A"),
    ("album_artist", "Various Artists"),
    ("album", "Album X"),
    ("track", "3/12"),
    ("disc", "1/2"),
    ("date", "2021-04-02"),
    ("genre", "Rock"),
];

// ---------------------------------------------------------------------------------------------
// Scan helpers

pub fn run_scan(db: &Path, add: Option<&Path>) -> Vec<ScanEvent> {
    let events: Vec<ScanEvent> = scan(db.to_owned(), add.map(Path::to_owned))
        .iter()
        .collect();
    assert!(
        matches!(events.last(), Some(ScanEvent::Done { .. })),
        "scan must end with Done: {events:?}"
    );
    events
}

/// (added, updated, removed)
pub fn counts(events: &[ScanEvent]) -> (usize, usize, usize) {
    match events.last() {
        Some(&ScanEvent::Done {
            added,
            updated,
            removed,
            ..
        }) => (added, updated, removed),
        other => panic!("expected Done, got {other:?}"),
    }
}

pub fn upserted(events: &[ScanEvent]) -> Vec<Track> {
    events
        .iter()
        .filter_map(|e| match e {
            ScanEvent::Upserted(tracks) => Some(tracks.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

pub fn removed(events: &[ScanEvent]) -> Vec<TrackId> {
    events
        .iter()
        .filter_map(|e| match e {
            ScanEvent::Removed(ids) => Some(ids.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

pub fn errors(events: &[ScanEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            ScanEvent::Error(msg) => Some(msg.clone()),
            _ => None,
        })
        .collect()
}

pub fn tracks(db: &Path) -> Vec<Track> {
    let mut tracks = Library::open(db).unwrap().tracks().unwrap();
    tracks.sort_by(|a, b| a.path.cmp(&b.path));
    tracks
}

pub fn by_name<'a>(tracks: &'a [Track], name: &str) -> &'a Track {
    tracks
        .iter()
        .find(|t| t.path.file_name().unwrap() == name)
        .unwrap_or_else(|| panic!("{name} not in library"))
}

pub fn assert_unique_paths(tracks: &[Track]) {
    let paths: HashSet<_> = tracks.iter().map(|t| &t.path).collect();
    assert_eq!(paths.len(), tracks.len(), "duplicate tracks: {tracks:#?}");
}

/// (size, mtime) of every file under `dir`, to prove nothing was modified.
pub fn snapshot(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let mut files: Vec<_> = walk_files(dir)
        .into_iter()
        .map(|p| {
            let md = fs::metadata(&p).unwrap();
            (p, md.len(), md.modified().unwrap())
        })
        .collect();
    files.sort();
    files
}

pub fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let ft = entry.file_type().unwrap();
        if ft.is_dir() {
            out.extend(walk_files(&entry.path()));
        } else if ft.is_file() {
            out.push(entry.path());
        }
    }
    out
}

/// Bumps a file's mtime so the next scan sees it as changed even within one clock tick.
pub fn bump_mtime(path: &Path) {
    let file = fs::File::options().write(true).open(path).unwrap();
    let mtime = file.metadata().unwrap().modified().unwrap();
    file.set_modified(mtime + Duration::from_secs(5)).unwrap();
}
