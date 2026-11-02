//! Plays files gaplessly into a WAV file, as fast as decoding allows. Never touches a device.
//!
//! cargo run -p vibrance-audio --example render -- out.wav file1 [file2 ...]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use vibrance_audio::{Engine, Event, PlayState, Sink, Track};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: render OUT.wav FILE [FILE ...]");
        return ExitCode::from(2);
    }
    let out = PathBuf::from(&args[0]);
    let tracks: Vec<Track> = args[1..]
        .iter()
        .enumerate()
        .map(|(i, path)| Track::new(i as u64, path))
        .collect();

    let engine = match Engine::new(Sink::Wav { path: out.clone(), realtime: false }) {
        Ok(engine) => engine,
        Err(e) => {
            eprintln!("cannot start engine: {e}");
            return ExitCode::FAILURE;
        }
    };
    let events = engine.events();
    let started = Instant::now();
    engine.play(tracks[0].clone(), Duration::ZERO);
    engine.set_next(tracks.get(1).cloned());

    // The index the engine was last told to join next.
    let mut next = 1;
    let mut failures = 0;
    loop {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Started { id, duration }) => {
                println!("started  {} ({:?})", tracks[id as usize].path.display(), duration);
            }
            Ok(Event::Advanced { to, .. }) => {
                next = to as usize + 1;
                engine.set_next(tracks.get(next).cloned());
            }
            Ok(Event::Failed { id, error }) => {
                failures += 1;
                eprintln!("failed   {}: {error}", tracks[id as usize].path.display());
                if id as usize == next {
                    // The queued file is unplayable: queue the one after it instead.
                    next += 1;
                    engine.set_next(tracks.get(next).cloned());
                }
            }
            Ok(Event::Finished { id }) => {
                println!("finished {}", tracks[id as usize].path.display());
                break;
            }
            Err(_) => {
                let status = engine.status();
                if status.state == PlayState::Stopped && status.track.is_none() {
                    break;
                }
            }
        }
    }
    drop(engine);
    println!("wrote {} in {:.2?}", out.display(), started.elapsed());
    if failures > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}
