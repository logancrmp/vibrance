//! Live folder watching against a real inotify instance. These tests wait out the 1.5 s
//! debounce, so each takes several seconds.

mod common;

use std::fs;
use std::io::Write;
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::*;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use vibrance_library::{Library, WatchEvent, load_art, watch};

/// Waits for one `Changed` and returns how long that took.
fn expect_changed(rx: &Receiver<WatchEvent>, what: &str) -> Duration {
    let start = Instant::now();
    match rx.recv_timeout(Duration::from_secs(8)) {
        Ok(WatchEvent::Changed) => start.elapsed(),
        other => panic!("{what}: expected Changed, got {other:?}"),
    }
}

fn expect_quiet(rx: &Receiver<WatchEvent>, what: &str) {
    match rx.recv_timeout(Duration::from_millis(2500)) {
        Err(RecvTimeoutError::Timeout) => {}
        other => panic!("{what}: expected no event, got {other:?}"),
    }
}

/// A tracked folder with one scanned file, plus a template file outside it.
fn setup(name: &str) -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new(name);
    audio(&tmp.music().join("existing.mp3"), &[]);
    let template = tmp.path().join("template.mp3");
    audio(&template, &[("title", "New")]);
    assert_eq!(counts(&run_scan(&tmp.db(), Some(&tmp.music()))), (1, 0, 0));
    (tmp, template)
}

#[test]
fn watch_reports_create_modify_and_delete() {
    let (tmp, template) = setup("watch");
    let music = tmp.music();
    let watcher = watch(tmp.db()).unwrap();
    let rx = watcher.events();

    fs::copy(&template, music.join("new.mp3")).unwrap();
    let waited = expect_changed(&rx, "create");
    assert!(
        waited >= Duration::from_millis(1200),
        "not debounced: {waited:?}"
    );
    assert_eq!(counts(&run_scan(&tmp.db(), None)), (1, 0, 0));

    // Reading files (scans, art), hidden folders and non-audio files don't count.
    fs::write(music.join("notes.txt"), "not audio").unwrap();
    fs::create_dir(music.join(".hidden")).unwrap();
    fs::copy(&template, music.join(".hidden/x.mp3")).unwrap();
    assert_eq!(counts(&run_scan(&tmp.db(), None)), (0, 0, 0));
    let _ = load_art(&music.join("new.mp3"), 64);
    expect_quiet(&rx, "reads and irrelevant files");

    fs::OpenOptions::new()
        .append(true)
        .open(music.join("new.mp3"))
        .unwrap()
        .write_all(&[0; 64])
        .unwrap();
    expect_changed(&rx, "modify");
    assert_eq!(counts(&run_scan(&tmp.db(), None)), (0, 1, 0));

    fs::remove_file(music.join("new.mp3")).unwrap();
    expect_changed(&rx, "delete");
    assert_eq!(counts(&run_scan(&tmp.db(), None)), (0, 0, 1));
}

#[test]
fn watch_debounces_bursts_and_follows_tracked_folders() {
    let (tmp, template) = setup("watch-burst");
    let music = tmp.music();
    let watcher = watch(tmp.db()).unwrap();
    let rx = watcher.events();

    // ~1 s of activity in a new subfolder is reported once.
    let album = music.join("album");
    fs::create_dir(&album).unwrap();
    for i in 0..20 {
        fs::copy(&template, album.join(format!("{i:02}.mp3"))).unwrap();
        sleep(Duration::from_millis(50));
    }
    expect_changed(&rx, "burst");
    expect_quiet(&rx, "after the burst");
    assert_eq!(counts(&run_scan(&tmp.db(), None)), (20, 0, 0));

    // A folder moved out of the tracked tree.
    fs::rename(&album, tmp.path().join("moved-away")).unwrap();
    expect_changed(&rx, "folder moved out");
    assert_eq!(counts(&run_scan(&tmp.db(), None)), (0, 0, 20));

    // A newly tracked folder is watched after `refresh`...
    let other = tmp.path().join("other");
    audio(&other.join("o1.mp3"), &[]);
    run_scan(&tmp.db(), Some(&other));
    watcher.refresh().unwrap();
    fs::copy(&template, other.join("o2.mp3")).unwrap();
    expect_changed(&rx, "newly tracked folder");

    // ...and a removed one no longer is.
    Library::open(&tmp.db())
        .unwrap()
        .remove_folder(&other)
        .unwrap();
    watcher.refresh().unwrap();
    fs::copy(&template, other.join("o3.mp3")).unwrap();
    expect_quiet(&rx, "untracked folder");

    // Dropping the watcher stops everything, which closes the event stream.
    drop(watcher);
    match rx.recv_timeout(Duration::from_secs(5)) {
        Err(RecvTimeoutError::Disconnected) => {}
        other => panic!("expected the stream to close, got {other:?}"),
    }
}
