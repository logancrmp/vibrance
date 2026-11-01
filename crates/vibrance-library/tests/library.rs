//! Integration tests against real audio files generated with ffmpeg.

use std::collections::HashSet;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use vibrance_library::{Library, ScanEvent, Track, TrackId, load_art, scan};

// ---------------------------------------------------------------------------------------------
// Fixtures

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
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

    fn path(&self) -> &Path {
        &self.0
    }

    fn db(&self) -> PathBuf {
        self.0.join("db").join("library.db")
    }

    fn music(&self) -> PathBuf {
        self.0.join("music")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn ffmpeg(args: &[&str]) {
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

fn codec_args(path: &Path) -> &'static [&'static str] {
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
fn audio(path: &Path, tags: &[(&str, &str)]) {
    audio_with_cover(path, tags, None);
}

fn audio_with_cover(path: &Path, tags: &[(&str, &str)], cover: Option<(&Path, &str)>) {
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
fn image(path: &Path, color: &str, size: u32) {
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

const FULL_TAGS: &[(&str, &str)] = &[
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

fn run_scan(db: &Path, add: Option<&Path>) -> Vec<ScanEvent> {
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
fn counts(events: &[ScanEvent]) -> (usize, usize, usize) {
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

fn upserted(events: &[ScanEvent]) -> Vec<Track> {
    events
        .iter()
        .filter_map(|e| match e {
            ScanEvent::Upserted(tracks) => Some(tracks.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn removed(events: &[ScanEvent]) -> Vec<TrackId> {
    events
        .iter()
        .filter_map(|e| match e {
            ScanEvent::Removed(ids) => Some(ids.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn errors(events: &[ScanEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            ScanEvent::Error(msg) => Some(msg.clone()),
            _ => None,
        })
        .collect()
}

fn tracks(db: &Path) -> Vec<Track> {
    let mut tracks = Library::open(db).unwrap().tracks().unwrap();
    tracks.sort_by(|a, b| a.path.cmp(&b.path));
    tracks
}

fn by_name<'a>(tracks: &'a [Track], name: &str) -> &'a Track {
    tracks
        .iter()
        .find(|t| t.path.file_name().unwrap() == name)
        .unwrap_or_else(|| panic!("{name} not in library"))
}

fn assert_unique_paths(tracks: &[Track]) {
    let paths: HashSet<_> = tracks.iter().map(|t| &t.path).collect();
    assert_eq!(paths.len(), tracks.len(), "duplicate tracks: {tracks:#?}");
}

/// (size, mtime) of every file under `dir`, to prove nothing was modified.
fn snapshot(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
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

fn walk_files(dir: &Path) -> Vec<PathBuf> {
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
fn bump_mtime(path: &Path) {
    let file = fs::File::options().write(true).open(path).unwrap();
    let mtime = file.metadata().unwrap().modified().unwrap();
    file.set_modified(mtime + Duration::from_secs(5)).unwrap();
}

// ---------------------------------------------------------------------------------------------
// Tests

#[test]
fn scan_reads_tags_formats_and_fallbacks() {
    let tmp = TempDir::new("tags");
    let music = tmp.music();
    for name in [
        "a/01.mp3",
        "a/02.flac",
        "b/03.ogg",
        "b/04.opus",
        "b/05.m4a",
        "b/06 alac.m4a",
    ] {
        audio(&music.join(name), FULL_TAGS);
    }
    audio(
        &music.join("c/07.wav"),
        &[("title", "Wave"), ("artist", "Artist W")],
    );
    audio(&music.join("c/08.aiff"), FULL_TAGS);
    audio(&music.join("c/09 - No Tags.mp3"), &[]);
    audio(
        &music.join("c/10 UPPER.MP3"),
        &[("artist", "Solo"), ("album", "Only Artist")],
    );
    fs::write(music.join("c/garbage.mp3"), [0x5a_u8; 3000]).unwrap();
    fs::write(music.join("c/notes.txt"), "not audio").unwrap();
    audio(&music.join(".hidden/x.mp3"), FULL_TAGS);
    audio(&music.join("c/._resource.mp3"), FULL_TAGS);
    let before = snapshot(&music);

    let events = run_scan(&tmp.db(), Some(&music));
    assert_eq!(counts(&events), (11, 0, 0));
    assert!(errors(&events).is_empty(), "{events:?}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ScanEvent::Progress { .. }))
    );
    let sent = upserted(&events);
    assert_eq!(sent.len(), 11);
    assert!(sent.iter().all(|t| t.id > 0));

    let lib = Library::open(&tmp.db()).unwrap();
    assert_eq!(lib.folders().unwrap(), vec![music.clone()]);
    let all = tracks(&tmp.db());
    assert_eq!(all.len(), 11);
    assert_unique_paths(&all);
    let ids: HashSet<_> = sent.iter().map(|t| t.id).collect();
    assert_eq!(ids, all.iter().map(|t| t.id).collect());

    let mp3 = by_name(&all, "01.mp3");
    assert_eq!(mp3.title, "Song One");
    assert_eq!(mp3.artist, "Artist A");
    assert_eq!(mp3.album_artist, "Various Artists");
    assert_eq!(mp3.album, "Album X");
    assert_eq!(mp3.track_no, Some(3));
    assert_eq!(mp3.disc_no, Some(1));
    assert_eq!(mp3.year, Some(2021));
    assert_eq!(mp3.genre.as_deref(), Some("Rock"));
    assert_eq!(mp3.format, "MP3");
    assert_eq!(mp3.sample_rate, Some(44100));
    assert!(mp3.bitrate_kbps.is_some());
    assert!((450..700).contains(&mp3.duration_ms), "{}", mp3.duration_ms);
    assert_eq!(mp3.folder, music);
    let md = fs::metadata(&mp3.path).unwrap();
    assert_eq!(mp3.size, md.len());
    assert!(mp3.mtime > 0);

    for (name, format) in [
        ("02.flac", "FLAC"),
        ("03.ogg", "Vorbis"),
        ("04.opus", "Opus"),
        ("05.m4a", "AAC"),
        ("06 alac.m4a", "ALAC"),
        ("08.aiff", "AIFF"),
    ] {
        let t = by_name(&all, name);
        assert_eq!(t.format, format, "{name}");
        assert_eq!(
            (
                t.title.as_str(),
                t.artist.as_str(),
                t.album_artist.as_str(),
                t.album.as_str()
            ),
            ("Song One", "Artist A", "Various Artists", "Album X"),
            "{name}"
        );
        assert_eq!(
            (t.track_no, t.disc_no, t.year),
            (Some(3), Some(1), Some(2021)),
            "{name}"
        );
        assert_eq!(t.genre.as_deref(), Some("Rock"), "{name}");
        assert!(
            (450..700).contains(&t.duration_ms),
            "{name}: {}",
            t.duration_ms
        );
    }
    assert_eq!(by_name(&all, "04.opus").sample_rate, Some(48000));

    let wav = by_name(&all, "07.wav");
    assert_eq!((wav.format.as_str(), wav.title.as_str()), ("WAV", "Wave"));
    assert_eq!(
        (wav.artist.as_str(), wav.album_artist.as_str()),
        ("Artist W", "Artist W")
    );
    assert_eq!(wav.album, "Unknown Album");

    let untagged = by_name(&all, "09 - No Tags.mp3");
    assert_eq!(untagged.title, "09 - No Tags");
    assert_eq!(untagged.artist, "Unknown Artist");
    assert_eq!(untagged.album_artist, "Unknown Artist");
    assert_eq!(untagged.album, "Unknown Album");
    assert_eq!(
        (untagged.track_no, untagged.year, untagged.genre.as_deref()),
        (None, None, None)
    );

    let upper = by_name(&all, "10 UPPER.MP3");
    assert_eq!(
        (upper.title.as_str(), upper.album_artist.as_str()),
        ("10 UPPER", "Solo")
    );

    // Unreadable files are still added, with fallbacks.
    let garbage = by_name(&all, "garbage.mp3");
    assert_eq!(garbage.title, "garbage");
    assert_eq!(garbage.artist, "Unknown Artist");
    assert_eq!(garbage.album, "Unknown Album");
    assert_eq!(garbage.format, "MP3");

    assert_eq!(
        snapshot(&music),
        before,
        "scanning must not touch audio files"
    );
}

#[test]
fn rescan_only_rereads_changed_files() {
    let tmp = TempDir::new("rescan");
    let music = tmp.music();
    for name in ["one.mp3", "two.flac", "three.ogg"] {
        audio(&music.join(name), &[("title", name), ("artist", "A")]);
    }
    assert_eq!(counts(&run_scan(&tmp.db(), Some(&music))), (3, 0, 0));
    let original = tracks(&tmp.db());

    // No changes: nothing re-read, nothing sent.
    let events = run_scan(&tmp.db(), None);
    assert_eq!(counts(&events), (0, 0, 0));
    assert!(upserted(&events).is_empty() && removed(&events).is_empty());

    // Re-tag one file in place (the way a tag editor would): same id, new tags.
    let staged = tmp.path().join("staged.mp3");
    audio(&staged, &[("title", "Retitled"), ("artist", "B")]);
    fs::copy(&staged, music.join("one.mp3")).unwrap();
    bump_mtime(&music.join("one.mp3"));
    let events = run_scan(&tmp.db(), None);
    assert_eq!(counts(&events), (0, 1, 0));
    let changed = upserted(&events);
    assert_eq!(changed.len(), 1);
    assert_eq!(
        (changed[0].title.as_str(), changed[0].artist.as_str()),
        ("Retitled", "B")
    );
    let old_id = by_name(&original, "one.mp3").id;
    assert_eq!(changed[0].id, old_id);
    assert_eq!(by_name(&tracks(&tmp.db()), "one.mp3").title, "Retitled");

    // An mtime change alone triggers a re-read.
    bump_mtime(&music.join("two.flac"));
    assert_eq!(counts(&run_scan(&tmp.db(), None)), (0, 1, 0));

    // Non-UTF-8 file names round-trip exactly and are recognized as unchanged.
    let latin1 = music.join(std::ffi::OsStr::from_bytes(b"caf\xe9.flac"));
    fs::copy(music.join("two.flac"), &latin1).unwrap();
    let events = run_scan(&tmp.db(), None);
    assert_eq!(counts(&events), (1, 0, 0));
    assert_eq!(upserted(&events)[0].path, latin1);
    assert_eq!(upserted(&events)[0].title, "two.flac");
    assert_eq!(counts(&run_scan(&tmp.db(), None)), (0, 0, 0));
    fs::remove_file(&latin1).unwrap();
    assert_eq!(counts(&run_scan(&tmp.db(), None)), (0, 0, 1));

    // Deleted files are removed.
    let gone_id = by_name(&original, "three.ogg").id;
    fs::remove_file(music.join("three.ogg")).unwrap();
    let events = run_scan(&tmp.db(), None);
    assert_eq!(counts(&events), (0, 0, 1));
    assert_eq!(removed(&events), vec![gone_id]);

    // New files are added, with fresh ids.
    audio(&music.join("sub/four.wav"), &[]);
    let events = run_scan(&tmp.db(), None);
    assert_eq!(counts(&events), (1, 0, 0));
    assert!(!original.iter().any(|t| t.id == upserted(&events)[0].id));
    assert_eq!(tracks(&tmp.db()).len(), 3);
}

#[test]
fn nested_and_overlapping_folders_never_duplicate() {
    let tmp = TempDir::new("nested");
    let root = tmp.music();
    let sub = root.join("sub");
    audio(&root.join("top.mp3"), &[]);
    audio(&sub.join("inner.mp3"), &[]);
    audio(&sub.join("deeper/deep.flac"), &[]);

    // The nested folder first, then its parent: tracks move to the parent, no duplicates.
    assert_eq!(counts(&run_scan(&tmp.db(), Some(&sub))), (2, 0, 0));
    assert!(tracks(&tmp.db()).iter().all(|t| t.folder == sub));
    let events = run_scan(&tmp.db(), Some(&root));
    let (added, _, removed) = counts(&events);
    assert_eq!((added, removed), (1, 0));
    let all = tracks(&tmp.db());
    assert_eq!(all.len(), 3);
    assert_unique_paths(&all);
    assert!(all.iter().all(|t| t.folder == root), "{all:#?}");
    assert!(upserted(&events).iter().all(|t| t.folder == root));

    // Adding a folder that is already covered (directly or through a symlink) changes nothing.
    std::os::unix::fs::symlink(&sub, tmp.path().join("alias")).unwrap();
    for dir in [sub.clone(), root.clone(), tmp.path().join("alias")] {
        let events = run_scan(&tmp.db(), Some(&dir));
        assert_eq!(counts(&events), (0, 0, 0), "{dir:?}");
        assert!(upserted(&events).is_empty());
    }
    let lib = Library::open(&tmp.db()).unwrap();
    assert_eq!(lib.folders().unwrap(), vec![root.clone(), sub.clone()]);

    // Symlinks inside a tracked folder: a second route to the same files, and a loop.
    std::os::unix::fs::symlink(&sub, root.join("link-to-sub")).unwrap();
    std::os::unix::fs::symlink(&root, sub.join("loop")).unwrap();
    let events = run_scan(&tmp.db(), None);
    assert_eq!(counts(&events), (0, 0, 0));
    let all = tracks(&tmp.db());
    assert_eq!(all.len(), 3);
    assert_unique_paths(&all);
    assert!(
        all.iter()
            .all(|t| !t.path.to_string_lossy().contains("link-to-sub"))
    );

    // Files only reachable through a symlink are followed.
    let outside = tmp.path().join("outside");
    audio(&outside.join("linked.ogg"), &[]);
    std::os::unix::fs::symlink(&outside, root.join("linked-dir")).unwrap();
    let events = run_scan(&tmp.db(), None);
    assert_eq!(counts(&events), (1, 0, 0));
    assert_eq!(
        upserted(&events)[0].path,
        root.join("linked-dir/linked.ogg")
    );
}

#[test]
fn remove_folder_drops_its_tracks() {
    let tmp = TempDir::new("remove");
    let (a, b) = (tmp.music().join("a"), tmp.music().join("b"));
    audio(&a.join("a1.mp3"), &[]);
    audio(&a.join("nested/a2.mp3"), &[]);
    audio(&b.join("b1.flac"), &[]);
    run_scan(&tmp.db(), Some(&a));
    run_scan(&tmp.db(), Some(&a.join("nested")));
    run_scan(&tmp.db(), Some(&b));
    assert_eq!(tracks(&tmp.db()).len(), 3);

    let lib = Library::open(&tmp.db()).unwrap();
    // Removing the outer folder keeps what the nested folder still covers.
    lib.remove_folder(&a).unwrap();
    let all = lib.tracks().unwrap();
    let mut names: Vec<_> = all
        .iter()
        .map(|t| t.path.file_name().unwrap().to_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["a2.mp3", "b1.flac"]);
    assert_eq!(by_name(&all, "a2.mp3").folder, a.join("nested"));
    assert_eq!(lib.folders().unwrap(), vec![a.join("nested"), b.clone()]);

    lib.remove_folder(&a.join("nested")).unwrap();
    lib.remove_folder(&tmp.path().join("never-tracked"))
        .unwrap();
    let all = lib.tracks().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].folder, b);
    assert!(a.join("a1.mp3").exists() && a.join("nested/a2.mp3").exists());

    // A rescan doesn't bring removed folders back.
    assert_eq!(counts(&run_scan(&tmp.db(), None)), (0, 0, 0));
    assert_eq!(lib.tracks().unwrap().len(), 1);
}

#[test]
fn unavailable_folder_keeps_its_tracks() {
    let tmp = TempDir::new("unmounted");
    let music = tmp.music();
    audio(&music.join("x.mp3"), &[]);
    audio(&music.join("y.flac"), &[]);
    run_scan(&tmp.db(), Some(&music));

    let away = tmp.path().join("away");
    fs::rename(&music, &away).unwrap();
    let events = run_scan(&tmp.db(), None);
    assert_eq!(counts(&events), (0, 0, 0));
    assert_eq!(errors(&events).len(), 1, "{events:?}");
    assert_eq!(tracks(&tmp.db()).len(), 2);

    fs::rename(&away, &music).unwrap();
    let events = run_scan(&tmp.db(), None);
    assert_eq!(counts(&events), (0, 0, 0));
    assert!(errors(&events).is_empty());
}

#[test]
fn large_imports_arrive_in_batches() {
    let tmp = TempDir::new("batches");
    let music = tmp.music();
    let template = tmp.path().join("template.mp3");
    audio(&template, &[("artist", "Batch")]);
    for i in 0..600 {
        let dir = music.join(format!("album{}", i / 50));
        fs::create_dir_all(&dir).unwrap();
        fs::copy(&template, dir.join(format!("{i:03}.mp3"))).unwrap();
    }
    let events = run_scan(&tmp.db(), Some(&music));
    assert_eq!(counts(&events), (600, 0, 0));
    let batches: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            ScanEvent::Upserted(t) => Some(t.len()),
            _ => None,
        })
        .collect();
    assert!(batches.len() >= 3, "{batches:?}");
    assert!(batches.iter().all(|&n| n <= 250), "{batches:?}");
    match events[events.len() - 2] {
        ScanEvent::Progress { scanned, total } => assert_eq!((scanned, total), (600, 600)),
        ref other => panic!("expected final Progress, got {other:?}"),
    }
}

#[test]
fn api_types_can_cross_threads() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Library>();
    send_sync::<ScanEvent>();
    send_sync::<Track>();
}

#[test]
fn state_round_trips() {
    let tmp = TempDir::new("state");
    {
        let lib = Library::open(&tmp.db()).unwrap();
        assert_eq!(lib.get_state("queue").unwrap(), None);
        lib.set_state("queue", "[1,2,3]").unwrap();
        lib.set_state("layout", "{\"panes\":\"ünïcødé\"}").unwrap();
        lib.set_state("queue", "[4]").unwrap();
        assert_eq!(lib.get_state("queue").unwrap().as_deref(), Some("[4]"));
    }
    let lib = Library::open(&tmp.db()).unwrap();
    assert_eq!(lib.get_state("queue").unwrap().as_deref(), Some("[4]"));
    assert_eq!(
        lib.get_state("layout").unwrap().as_deref(),
        Some("{\"panes\":\"ünïcødé\"}")
    );
    assert_eq!(lib.get_state("missing").unwrap(), None);
    assert!(lib.tracks().unwrap().is_empty());
    assert!(lib.folders().unwrap().is_empty());
}

fn assert_color(img: &image::RgbaImage, rgb: [u8; 3], what: &str) {
    let px = img.get_pixel(img.width() / 2, img.height() / 2).0;
    for (got, want) in px.iter().zip(rgb) {
        assert!(got.abs_diff(want) < 40, "{what}: got {px:?}, want {rgb:?}");
    }
}

#[test]
fn art_from_embedded_pictures_and_folder_images() {
    let tmp = TempDir::new("art");
    let music = tmp.music();
    let red = tmp.path().join("red.png");
    image(&red, "red", 64);

    // Embedded front covers win over folder images.
    let album = music.join("embedded");
    image(&album.join("cover.jpg"), "blue", 64);
    for name in ["e.mp3", "e.flac", "e.m4a"] {
        audio_with_cover(&album.join(name), &[], Some((&red, "Cover (front)")));
        let art = load_art(&album.join(name), 32).unwrap_or_else(|| panic!("no art for {name}"));
        assert_eq!(art.dimensions(), (32, 32), "{name}");
        assert_color(&art, [255, 0, 0], name);
    }
    // Any embedded picture beats a folder image; small art is never upscaled.
    audio_with_cover(&album.join("back.mp3"), &[], Some((&red, "Cover (back)")));
    let art = load_art(&album.join("back.mp3"), 512).unwrap();
    assert_eq!(art.dimensions(), (64, 64));
    assert_color(&art, [255, 0, 0], "back cover");

    // No embedded art: folder image, case-insensitive, "cover" preferred over "folder".
    let plain = music.join("plain");
    audio(&plain.join("p.wav"), &[]);
    image(&plain.join("Folder.png"), "lime", 64);
    image(&plain.join("COVER.JPG"), "blue", 100);
    let art = load_art(&plain.join("p.wav"), 50).unwrap();
    assert_eq!(art.dimensions(), (50, 50));
    assert_color(&art, [0, 0, 255], "COVER.JPG");
    fs::remove_file(plain.join("COVER.JPG")).unwrap();
    assert_color(
        &load_art(&plain.join("p.wav"), 50).unwrap(),
        [0, 255, 0],
        "Folder.png",
    );

    // Nothing to find, or nothing that decodes.
    let bare = music.join("bare");
    audio(&bare.join("b.ogg"), &[]);
    assert!(load_art(&bare.join("b.ogg"), 64).is_none());
    fs::write(bare.join("cover.png"), b"not an image").unwrap();
    assert!(load_art(&bare.join("b.ogg"), 64).is_none());
    assert!(load_art(&bare.join("missing.mp3"), 64).is_none());

    let before = snapshot(&music);
    for file in walk_files(&music) {
        let _ = load_art(&file, 16);
    }
    assert_eq!(snapshot(&music), before, "loading art must not touch files");
}
