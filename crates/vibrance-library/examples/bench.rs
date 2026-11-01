//! Library benchmarks (SPEC 7.1, "Background" rows).
//!
//! ```text
//! cargo run --release -p vibrance-library --example bench -- import <music-dir> [db-path]
//!     Full import into a fresh database, an unchanged rescan, and a `Library::tracks()` load.
//!     The music folder is only read. The database defaults to a temp dir and is deleted after.
//!
//! cargo run --release -p vibrance-library --example bench -- load <db-path>
//!     Times `Library::tracks()` on an existing database.
//!
//! cargo run --release -p vibrance-library --example bench -- art <music-dir> [max-px]
//!     Times `load_art` for the first audio file of every folder.
//!
//! cargo run --release -p vibrance-library --example bench -- synth <out-dir> [count]
//!     Writes `count` (default 100000) short tagged MP3s spread over 2,000 artists and
//!     8,000 albums, like the SPEC 7.1 reference library. Needs ffmpeg for the audio frames.
//! ```

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use vibrance_library::{Library, ScanEvent, scan};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["import", dir, rest @ ..] => import(Path::new(dir), rest.first().map(PathBuf::from)),
        ["load", db] => print_load(&Library::open(Path::new(db)).expect("open")),
        ["art", dir, rest @ ..] => {
            let max_px = rest.first().map_or(512, |n| n.parse().expect("max_px"));
            art(Path::new(dir), max_px);
        }
        ["synth", dir, rest @ ..] => {
            let count = rest.first().map_or(100_000, |n| n.parse().expect("count"));
            synth(Path::new(dir), count);
        }
        _ => {
            eprintln!(
                "usage: bench import <music-dir> [db-path] | load <db-path> \
                 | art <music-dir> [max-px] | synth <out-dir> [count]"
            );
            std::process::exit(2);
        }
    }
}

struct ScanReport {
    wall: Duration,
    first_tracks: Option<Duration>,
    added: usize,
    updated: usize,
    removed: usize,
    errors: Vec<String>,
}

fn timed_scan(db: &Path, add: Option<&Path>) -> ScanReport {
    let start = Instant::now();
    let mut report = ScanReport {
        wall: Duration::ZERO,
        first_tracks: None,
        added: 0,
        updated: 0,
        removed: 0,
        errors: Vec::new(),
    };
    for event in scan(db.to_owned(), add.map(Path::to_owned)) {
        match event {
            ScanEvent::Upserted(_) if report.first_tracks.is_none() => {
                report.first_tracks = Some(start.elapsed());
            }
            ScanEvent::Done {
                added,
                updated,
                removed,
                ..
            } => {
                report.wall = start.elapsed();
                (report.added, report.updated, report.removed) = (added, updated, removed);
            }
            ScanEvent::Error(e) => report.errors.push(e),
            _ => {}
        }
    }
    report
}

fn import(music: &Path, db: Option<PathBuf>) {
    let temp = std::env::temp_dir().join(format!("vibrance-bench-{}", std::process::id()));
    let db = db.unwrap_or_else(|| temp.join("library.db"));
    assert!(
        !db.exists(),
        "{} already exists; the benchmark needs a fresh database",
        db.display()
    );
    assert!(
        !db.starts_with(music.canonicalize().expect("music dir")),
        "the database must live outside the music folder"
    );

    // The app has its database open long before (and while) a folder is imported.
    let lib = Library::open(&db).expect("create database");
    let full = timed_scan(&db, Some(music));
    let rate = full.added as f64 / full.wall.as_secs_f64();
    println!(
        "import:   {} files in {:.3} s = {rate:.0} files/s (first tracks after {:.1} ms)",
        full.added,
        full.wall.as_secs_f64(),
        full.first_tracks.unwrap_or_default().as_secs_f64() * 1e3,
    );
    for e in &full.errors {
        println!("  error: {e}");
    }

    for run in 1..=3 {
        let again = timed_scan(&db, None);
        println!(
            "rescan {run}: {:.1} ms (added {}, updated {}, removed {})",
            again.wall.as_secs_f64() * 1e3,
            again.added,
            again.updated,
            again.removed,
        );
    }

    print_load(&lib);
    drop(lib);
    let _ = fs::remove_dir_all(&temp);
}

fn print_load(lib: &Library) {
    let mut loads = Vec::new();
    let mut count = 0;
    for _ in 0..10 {
        let start = Instant::now();
        let tracks = lib.tracks().expect("tracks");
        loads.push(start.elapsed());
        count = tracks.len();
    }
    println!(
        "tracks(): {count} rows, first {:.1} ms, best {:.1} ms",
        loads[0].as_secs_f64() * 1e3,
        loads.iter().min().unwrap().as_secs_f64() * 1e3,
    );
}

fn art(music: &Path, max_px: u32) {
    let mut times = Vec::new();
    let mut found = 0;
    let mut stack = vec![music.to_owned()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        entries.sort();
        stack.extend(entries.iter().filter(|p| p.is_dir()).cloned());
        if let Some(track) = entries.iter().find(|p| vibrance_library::is_supported(p)) {
            let start = Instant::now();
            found += usize::from(vibrance_library::load_art(track, max_px).is_some());
            times.push(start.elapsed());
        }
    }
    times.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    let mean = times.iter().sum::<Duration>() / times.len().max(1) as u32;
    println!(
        "load_art({max_px}): {} folders, {found} with art; mean {:.1} ms, p50 {:.1} ms, p95 {:.1} ms, max {:.1} ms",
        times.len(),
        ms(mean),
        ms(times[times.len() / 2]),
        ms(times[times.len() * 95 / 100]),
        ms(*times.last().unwrap()),
    );
}

fn synth(out: &Path, count: usize) {
    fs::create_dir_all(out).expect("create output dir");
    let frames = out.join(".template.mp3");
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", "anullsrc=r=44100:cl=mono:d=0.2"])
        .args(["-c:a", "libmp3lame", "-b:a", "32k", "-write_xing", "0"])
        .args(["-id3v2_version", "0", "-write_id3v1", "0"])
        .arg(&frames)
        .status()
        .expect("ffmpeg");
    assert!(status.success());
    let audio = fs::read(&frames).unwrap();
    fs::remove_file(&frames).unwrap();

    let start = Instant::now();
    for i in 0..count {
        let album = i % 8000;
        let artist = album % 2000;
        let track = i / 8000 + 1;
        let dir = out.join(format!("Artist {artist:04}/Album {album:04}"));
        if i < 8000 {
            fs::create_dir_all(&dir).unwrap();
        }
        let tag = id3v23(&[
            (b"TIT2", format!("Song {i}")),
            (b"TPE1", format!("Artist {artist:04}")),
            (b"TPE2", format!("Artist {artist:04}")),
            (b"TALB", format!("Album {album:04}")),
            (b"TRCK", format!("{track}/13")),
            (b"TYER", format!("{}", 1960 + album % 60)),
            (
                b"TCON",
                ["Rock", "Jazz", "Electronic", "Classical"][album % 4].to_owned(),
            ),
        ]);
        let mut file = fs::File::create(dir.join(format!("{track:02} - Song {i}.mp3"))).unwrap();
        file.write_all(&tag).unwrap();
        file.write_all(&audio).unwrap();
    }
    println!(
        "wrote {count} files to {} in {:.1} s",
        out.display(),
        start.elapsed().as_secs_f64()
    );
}

/// A minimal ID3v2.3 tag with Latin-1 text frames.
fn id3v23(frames: &[(&[u8; 4], String)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (id, text) in frames {
        body.extend_from_slice(*id);
        body.extend_from_slice(&(text.len() as u32 + 1).to_be_bytes());
        body.extend_from_slice(&[0, 0, 0]); // flags, then encoding 0 = ISO-8859-1
        body.extend_from_slice(text.as_bytes());
    }
    let size = body.len() as u32;
    let syncsafe = [
        (size >> 21) & 0x7f,
        (size >> 14) & 0x7f,
        (size >> 7) & 0x7f,
        size & 0x7f,
    ];
    let mut tag = b"ID3\x03\x00\x00".to_vec();
    tag.extend(syncsafe.map(|b| b as u8));
    tag.extend(body);
    tag
}
