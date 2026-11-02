//! Track gain: the v1 → v2 migration, ReplayGain/R128 tags in each container, and measured gain.

mod common;

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use common::*;
use lofty::config::WriteOptions;
use lofty::tag::{ItemKey, Tag, TagExt, TagType};
use vibrance_library::{Library, Track};

/// The schema as v1 of this crate created it.
const V1_SCHEMA: &str = "CREATE TABLE folders (
        id   INTEGER PRIMARY KEY,
        path BLOB NOT NULL UNIQUE
    );
    CREATE TABLE tracks (
        id           INTEGER PRIMARY KEY AUTOINCREMENT,
        path         BLOB NOT NULL UNIQUE,
        folder_id    INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
        title        TEXT NOT NULL,
        artist       TEXT NOT NULL,
        album_artist TEXT NOT NULL,
        album        TEXT NOT NULL,
        track_no     INTEGER,
        disc_no      INTEGER,
        year         INTEGER,
        genre        TEXT,
        duration_ms  INTEGER NOT NULL,
        format       TEXT NOT NULL,
        bitrate_kbps INTEGER,
        sample_rate  INTEGER,
        mtime        INTEGER NOT NULL,
        size         INTEGER NOT NULL,
        added_at     INTEGER NOT NULL
    );
    CREATE INDEX tracks_folder ON tracks(folder_id);
    CREATE TABLE kv (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    ) WITHOUT ROWID;
    PRAGMA user_version = 1;";

const RG_TAGS: &[(&str, &str)] = &[
    ("REPLAYGAIN_TRACK_GAIN", "-6.54 dB"),
    ("REPLAYGAIN_TRACK_PEAK", "0.988547"),
];

fn gain(track: &Track) -> (Option<f32>, Option<f32>) {
    (track.gain_db, track.peak)
}

fn user_version(db: &Path) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

/// Adds a ReplayGain tag of `tag_type` with lofty, for forms ffmpeg can't write.
fn write_gain_tag(path: &Path, tag_type: TagType, gain: &str, peak: &str) {
    let mut tag = Tag::new(tag_type);
    assert!(tag.insert_text(ItemKey::ReplayGainTrackGain, gain.into()));
    assert!(tag.insert_text(ItemKey::ReplayGainTrackPeak, peak.into()));
    tag.save_to_path(path, WriteOptions::default()).unwrap();
}

fn contains(path: &Path, needle: &[u8]) -> bool {
    fs::read(path)
        .unwrap()
        .windows(needle.len())
        .any(|w| w == needle)
}

#[test]
fn v1_database_upgrades_in_place() {
    let tmp = TempDir::new("migrate");
    let music = tmp.music();
    let file = music.join("tagged.flac");
    let mut tags = vec![("title", "New Title")];
    tags.extend_from_slice(RG_TAGS);
    audio(&file, &tags);
    let md = fs::metadata(&file).unwrap();

    fs::create_dir_all(tmp.db().parent().unwrap()).unwrap();
    {
        let conn = rusqlite::Connection::open(tmp.db()).unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO folders (id, path) VALUES (1, ?1)",
            [music.as_os_str().as_bytes()],
        )
        .unwrap();
        // Size and mtime match the file, so only the migration can make the scan re-read it.
        conn.execute(
            "INSERT INTO tracks (id, path, folder_id, title, artist, album_artist, album, \
             duration_ms, format, mtime, size, added_at) \
             VALUES (7, ?1, 1, 'Old Title', 'A', 'A', 'B', 500, 'FLAC', ?2, ?3, 1234567890)",
            rusqlite::params![
                file.as_os_str().as_bytes(),
                md.mtime() * 1_000_000_000 + md.mtime_nsec(),
                md.size() as i64,
            ],
        )
        .unwrap();
        conn.execute("INSERT INTO kv (key, value) VALUES ('queue', '[7]')", [])
            .unwrap();
    }

    let lib = Library::open(&tmp.db()).unwrap();
    assert_eq!(user_version(&tmp.db()), 2);
    let all = lib.tracks().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!((all[0].id, all[0].added), (7, 1234567890));
    assert_eq!(all[0].title, "Old Title");
    assert_eq!(gain(&all[0]), (None, None));
    assert_eq!(lib.get_state("queue").unwrap().as_deref(), Some("[7]"));

    // The first scan after the upgrade re-reads each file once to pick up gain tags.
    let events = run_scan(&tmp.db(), None);
    assert_eq!(counts(&events), (0, 1, 0));
    let track = &lib.tracks().unwrap()[0];
    assert_eq!((track.id, track.added), (7, 1234567890));
    assert_eq!(track.title, "New Title");
    assert_eq!(gain(track), (Some(-6.54), Some(0.988547)));
    assert_eq!(gain(&upserted(&events)[0]), (Some(-6.54), Some(0.988547)));

    assert_eq!(counts(&run_scan(&tmp.db(), None)), (0, 0, 0));
    drop(lib);
    // Reopening a current database is a no-op.
    Library::open(&tmp.db()).unwrap();
    assert_eq!(user_version(&tmp.db()), 2);
}

#[test]
fn replaygain_tags_in_every_form() {
    let tmp = TempDir::new("replaygain");
    let m = tmp.music();
    audio(&m.join("id3v2.mp3"), RG_TAGS); // TXXX:REPLAYGAIN_TRACK_GAIN
    audio(&m.join("vorbis.flac"), RG_TAGS); // Vorbis comments
    audio(&m.join("vorbis.ogg"), RG_TAGS);
    audio(&m.join("id3-chunk.aiff"), RG_TAGS); // ID3 chunk
    audio(
        &m.join("lowercase.mp3"),
        &[("replaygain_track_gain", "+3.20 dB")], // TXXX:replaygain_track_gain, no peak
    );
    audio(&m.join("r128.opus"), &[("R128_TRACK_GAIN", "-1234")]);
    audio(
        &m.join("both.opus"),
        &[
            ("R128_TRACK_GAIN", "256"),
            ("REPLAYGAIN_TRACK_GAIN", "-9.00 dB"),
        ],
    );
    audio(&m.join("untagged.mp3"), &[]);

    // ffmpeg 8 writes no APE tags to MP3, and stores custom MP4 keys as QuickTime `mdta`
    // keys rather than iTunes freeform atoms, so these two are tagged with lofty.
    let ape = m.join("ape.mp3");
    ffmpeg(&[
        "-f",
        "lavfi",
        "-i",
        "anullsrc=r=44100:cl=stereo:d=0.5",
        "-c:a",
        "libmp3lame",
        "-id3v2_version",
        "0",
        "-write_id3v1",
        "0",
        ape.to_str().unwrap(),
    ]);
    write_gain_tag(&ape, TagType::Ape, "-2.50 dB", "0.75");
    assert!(contains(&ape, b"APETAGEX") && contains(&ape, b"REPLAYGAIN_TRACK_GAIN"));
    assert!(!contains(&ape, b"ID3"));

    let freeform = m.join("freeform.m4a");
    audio(&freeform, &[]);
    write_gain_tag(&freeform, TagType::Mp4Ilst, "-4.25 dB", "1.05");
    assert!(contains(&freeform, b"com.apple.iTunes"));
    assert!(contains(&freeform, b"replaygain_track_gain"));

    assert_eq!(counts(&run_scan(&tmp.db(), Some(&m))), (10, 0, 0));
    let all = tracks(&tmp.db());
    let expected = [
        ("id3v2.mp3", Some(-6.54), Some(0.988547)),
        ("vorbis.flac", Some(-6.54), Some(0.988547)),
        ("vorbis.ogg", Some(-6.54), Some(0.988547)),
        ("id3-chunk.aiff", Some(-6.54), Some(0.988547)),
        ("lowercase.mp3", Some(3.2), None),
        // Q7.8 relative to -23 LUFS, moved to the -18 LUFS reference.
        ("r128.opus", Some(-1234.0 / 256.0 + 5.0), None),
        // Opus: R128 wins over REPLAYGAIN_*.
        ("both.opus", Some(6.0), None),
        ("ape.mp3", Some(-2.5), Some(0.75)),
        ("freeform.m4a", Some(-4.25), Some(1.05)),
        ("untagged.mp3", None, None),
    ];
    for (name, gain_db, peak) in expected {
        assert_eq!(gain(by_name(&all, name)), (gain_db, peak), "{name}");
    }

    let lib = Library::open(&tmp.db()).unwrap();
    let missing = lib.tracks_without_gain().unwrap();
    assert_eq!(missing.len(), 1);
    assert_eq!(
        missing[0],
        (by_name(&all, "untagged.mp3").id, m.join("untagged.mp3"))
    );
}

#[test]
fn measured_gain_survives_rescans_until_tags_appear() {
    let tmp = TempDir::new("measured");
    let m = tmp.music();
    audio(&m.join("plain.mp3"), &[]);
    audio(&m.join("tagged.flac"), RG_TAGS);
    let staged_plain = tmp.path().join("staged-plain.mp3");
    let staged_tagged = tmp.path().join("staged-tagged.mp3");
    audio(&staged_plain, &[("title", "Plain Again")]);
    audio(&staged_tagged, &[("REPLAYGAIN_TRACK_GAIN", "-1.50 dB")]);
    let staged_flac = tmp.path().join("staged-plain.flac");
    audio(&staged_flac, &[]);
    run_scan(&tmp.db(), Some(&m));

    let lib = Library::open(&tmp.db()).unwrap();
    let all = lib.tracks().unwrap();
    let (plain, tagged) = (
        by_name(&all, "plain.mp3").id,
        by_name(&all, "tagged.flac").id,
    );
    assert_eq!(
        lib.tracks_without_gain().unwrap(),
        vec![(plain, m.join("plain.mp3"))]
    );

    lib.set_measured_gain(plain, -7.5, 0.9).unwrap();
    // Tags always win: a measurement for a tagged track is ignored.
    lib.set_measured_gain(tagged, 1.0, 0.1).unwrap();
    assert!(lib.set_measured_gain(plain, f32::NAN, 0.5).is_err());
    let gain_of = |name: &str| gain(by_name(&lib.tracks().unwrap(), name));
    assert_eq!(gain_of("plain.mp3"), (Some(-7.5), Some(0.9)));
    assert_eq!(gain_of("tagged.flac"), (Some(-6.54), Some(0.988547)));
    assert!(lib.tracks_without_gain().unwrap().is_empty());

    // Unchanged files aren't re-read.
    assert_eq!(counts(&run_scan(&tmp.db(), None)), (0, 0, 0));
    assert_eq!(gain_of("plain.mp3"), (Some(-7.5), Some(0.9)));

    // Re-read without gain tags: the measurement is kept.
    fs::copy(&staged_plain, m.join("plain.mp3")).unwrap();
    bump_mtime(&m.join("plain.mp3"));
    let events = run_scan(&tmp.db(), None);
    assert_eq!(counts(&events), (0, 1, 0));
    assert_eq!(upserted(&events)[0].title, "Plain Again");
    assert_eq!(gain(&upserted(&events)[0]), (Some(-7.5), Some(0.9)));
    assert_eq!(gain_of("plain.mp3"), (Some(-7.5), Some(0.9)));

    // Re-read with gain tags: they replace the measurement, and later measurements are ignored.
    fs::copy(&staged_tagged, m.join("plain.mp3")).unwrap();
    bump_mtime(&m.join("plain.mp3"));
    let events = run_scan(&tmp.db(), None);
    assert_eq!(gain(&upserted(&events)[0]), (Some(-1.5), None));
    lib.set_measured_gain(plain, -7.5, 0.9).unwrap();
    assert_eq!(gain_of("plain.mp3"), (Some(-1.5), None));

    // Gain tags removed: the stale tag values go, so the track gets measured again.
    fs::copy(&staged_flac, m.join("tagged.flac")).unwrap();
    bump_mtime(&m.join("tagged.flac"));
    run_scan(&tmp.db(), None);
    assert_eq!(gain_of("tagged.flac"), (None, None));
    assert_eq!(
        lib.tracks_without_gain().unwrap(),
        vec![(tagged, m.join("tagged.flac"))]
    );
}
