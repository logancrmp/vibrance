//! Broken and missing files never stop the engine: they are reported and skipped.

mod common;

use std::path::Path;
use std::time::Duration;

use common::*;
use vibrance_audio::{Engine, Event, PlayState, Sink};

const TIMEOUT: Duration = Duration::from_secs(10);

fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    (0..len)
        .map(|_| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 56) as u8
        })
        .collect()
}

fn good_file(dir: &Dir, name: &str, secs: f64) -> std::path::PathBuf {
    let wav = source(dir, &format!("{name}.wav"), "0.5*sin(2*PI*440*t)", 48_000, secs);
    excerpt(dir, &wav, &format!("{name}.flac"), 0, None, &[])
}

fn fast_engine(dir: &Dir) -> Engine {
    Engine::new(Sink::Wav { path: dir.path("out.wav"), realtime: false }).unwrap()
}

#[test]
fn corrupt_file_fails_and_playback_moves_on() {
    let dir = Dir::new("corrupt");
    let bad = dir.path("bad.mp3");
    std::fs::write(&bad, noise(64 * 1024, 1)).unwrap();
    let good = good_file(&dir, "good", 0.3);
    let engine = fast_engine(&dir);
    let events = engine.events();
    engine.play(track(1, &bad), Duration::ZERO);
    engine.set_next(Some(track(2, &good)));
    let seen = events_until(&events, TIMEOUT, is_finished);
    assert!(
        matches!(
            seen.as_slice(),
            [
                Event::Failed { id: 1, .. },
                Event::Advanced { from: 1, to: 2 },
                Event::Started { id: 2, .. },
                Event::Finished { id: 2 },
            ]
        ),
        "{seen:?}"
    );
}

#[test]
fn missing_file_fails_and_stops() {
    let dir = Dir::new("missing");
    let engine = fast_engine(&dir);
    let events = engine.events();
    engine.play(track(4, Path::new("/nonexistent/vibrance/missing.flac")), Duration::ZERO);
    let seen = events_until(&events, TIMEOUT, |e| matches!(e, Event::Failed { .. }));
    assert!(matches!(seen.as_slice(), [Event::Failed { id: 4, .. }]), "{seen:?}");
    std::thread::sleep(Duration::from_millis(50));
    let status = engine.status();
    assert_eq!((status.state, status.track), (PlayState::Stopped, None));
    assert!(events.try_recv().is_err(), "no further events");

    // The engine still works afterwards.
    let good = good_file(&dir, "good", 0.2);
    engine.play(track(5, &good), Duration::ZERO);
    let seen = events_until(&events, TIMEOUT, is_finished);
    assert!(matches!(seen.as_slice(), [Event::Started { id: 5, .. }, Event::Finished { id: 5 }]));
}

#[test]
fn unplayable_next_is_reported_right_away() {
    let dir = Dir::new("bad-next");
    let bad = dir.path("bad.flac");
    std::fs::write(&bad, noise(4096, 2)).unwrap();
    let good = good_file(&dir, "good", 1.0);
    let engine = Engine::new(Sink::Null).unwrap();
    let events = engine.events();
    engine.play(track(1, &good), Duration::ZERO);
    std::thread::sleep(Duration::from_millis(100));
    engine.set_next(Some(track(2, &bad)));
    let first = events_until(&events, Duration::from_millis(500), |e| matches!(e, Event::Failed { .. }));
    assert!(matches!(first.as_slice(), [Event::Started { id: 1, .. }, Event::Failed { id: 2, .. }]));
    let rest = events_until(&events, TIMEOUT, is_finished);
    assert!(matches!(rest.as_slice(), [Event::Finished { id: 1 }]), "{rest:?}");
}

#[test]
fn damaged_and_truncated_files_play_what_they_can() {
    let dir = Dir::new("damaged");
    let flac = good_file(&dir, "flac", 2.0);
    let mut bytes = std::fs::read(&flac).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid..mid + 8192].copy_from_slice(&noise(8192, 3));
    let damaged = dir.path("damaged.flac");
    std::fs::write(&damaged, bytes).unwrap();

    let wav = source(&dir, "mp3src.wav", "0.5*sin(2*PI*440*t)", 44_100, 2.0);
    let mp3 = excerpt(&dir, &wav, "full.mp3", 0, None, &["-c:a", "libmp3lame"]);
    let bytes = std::fs::read(&mp3).unwrap();
    let truncated = dir.path("truncated.mp3");
    std::fs::write(&truncated, &bytes[..bytes.len() / 2]).unwrap();

    // Damage sits at the middle of the 2 s FLAC: decoding must pick up again after it.
    for (id, file, at_least) in [(1, &damaged, 1.1), (2, &truncated, 0.9)] {
        let out = dir.path(&format!("out{id}.wav"));
        let engine = Engine::new(Sink::Wav { path: out.clone(), realtime: false }).unwrap();
        let events = engine.events();
        engine.play(track(id, file), Duration::ZERO);
        let seen = events_until(&events, TIMEOUT, |e| matches!(e, Event::Finished { .. }));
        assert!(matches!(seen.first(), Some(Event::Started { .. })), "{seen:?}");
        assert!(matches!(seen.last(), Some(Event::Finished { id: done }) if *done == id), "{seen:?}");
        drop(engine);
        let secs = read_wav(&out).0.len() as f64 / f64::from(OUT_RATE);
        assert!(secs > at_least, "file {id}: only {secs} s played");
    }
}
