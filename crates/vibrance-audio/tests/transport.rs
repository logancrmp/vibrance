//! Transport behaviour against a real-time WAV or null sink: seek, pause, stop, volume, status,
//! and event timing. Real-time WAV sample `i` is rendered at about `t0 + i / 48000`, which is
//! how latencies are measured here.

mod common;

use std::path::PathBuf;
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::*;
use vibrance_audio::{Engine, Event, PlayState, Sink};

const TIMEOUT: Duration = Duration::from_secs(10);
const RATE: f64 = OUT_RATE as f64;

struct Session {
    engine: Engine,
    out: PathBuf,
    t0: Instant,
}

impl Session {
    fn wav(dir: &Dir) -> Session {
        let out = dir.path("out.wav");
        let t0 = Instant::now();
        let engine = Engine::new(Sink::Wav { path: out.clone(), realtime: true }).unwrap();
        Session { engine, out, t0 }
    }

    /// Seconds since the output started, for comparing with WAV sample positions.
    fn now(&self) -> f64 {
        self.t0.elapsed().as_secs_f64()
    }

    fn finish(self) -> Vec<f32> {
        drop(self.engine);
        read_wav(&self.out).0
    }
}

fn first_nonzero(x: &[f32], from: usize) -> usize {
    from + x[from..].iter().position(|&v| v != 0.0).expect("audio after silence")
}

/// Where in `out`, near `around`, the audio lines up with `reference` (whose first samples
/// may be zero, or faded in).
fn align(out: &[f32], around: usize, reference: &[f32]) -> usize {
    let error = |start: usize| {
        (200..2_000).map(|k| (out[start + k] - reference[k]).abs()).fold(0.0, f32::max)
    };
    (around.saturating_sub(3)..=around + 3)
        .min_by(|&a, &b| error(a).total_cmp(&error(b)))
        .unwrap()
}

fn wait_for_position(engine: &Engine, at_least: f64) {
    let deadline = Instant::now() + TIMEOUT;
    while engine.status().position.as_secs_f64() < at_least {
        assert!(Instant::now() < deadline, "position never reached {at_least}");
        sleep(Duration::from_millis(2));
    }
}

#[test]
fn seek_lands_on_the_exact_sample() {
    let dir = Dir::new("seek");
    // A one-second sawtooth: every sample value tells its position within the second.
    let src = dir.path("saw.wav");
    ffmpeg(&[
        "-f", "lavfi", "-i", "aevalsrc='0.9*mod(t,1)|0.9*mod(t,1)':s=48000:d=6",
        "-c:a", "pcm_f32le", src.to_str().unwrap(),
    ]);
    let s = Session::wav(&dir);
    s.engine.play(track(1, &src), Duration::ZERO);
    sleep(Duration::from_millis(300));
    let seek_at = s.now();
    s.engine.seek(Duration::from_millis(3_750));
    let status = s.engine.status();
    sleep(Duration::from_millis(300));
    let later = s.engine.status();
    let out = s.finish();

    // Right after the call (or at most one poll later) the status shows the new position.
    assert!(status.position.as_secs_f64() < 0.4 || (status.position.as_secs_f64() - 3.75).abs() < 0.02);
    assert!((later.position.as_secs_f64() - 4.05).abs() < 0.05, "status {:?}", later.position);

    // Find the new segment: values jump above anything played before the seek.
    let jump = out.iter().position(|&v| v > 0.5).expect("seeked audio") + 600;
    let start = jump as f64 - (f64::from(out[jump]) / 0.9 - 0.75) * RATE;
    let first = start.round() as usize;
    assert!((start - start.round()).abs() < 0.01, "not on a sample boundary: {start}");
    for k in 200..11_900 {
        let expected = 0.9 * (0.75 + k as f64 / RATE);
        let got = f64::from(out[first + k]);
        assert!((got - expected).abs() < 1e-5, "sample {k} after the seek: {got} vs {expected}");
    }
    let latency = first as f64 / RATE - seek_at;
    println!("seek -> audible at the new position: {:.1} ms", latency * 1e3);
    assert!(latency < 0.03, "seek latency {latency}");
}

#[test]
fn pause_resume_and_stop_go_silent() {
    let dir = Dir::new("pause");
    let wav = source(&dir, "src.wav", "0.5*sin(2*PI*440*t)", 48_000, 5.0);
    let src = excerpt(&dir, &wav, "src.flac", 0, None, &[]);
    let (reference, _) = read_wav(&wav);

    let s = Session::wav(&dir);
    let play_at = s.now();
    s.engine.play(track(3, &src), Duration::ZERO);
    sleep(Duration::from_millis(400));
    let pause_at = s.now();
    s.engine.pause();
    sleep(Duration::from_millis(50));
    let paused = s.engine.status();
    sleep(Duration::from_millis(350));
    let resume_at = s.now();
    s.engine.resume();
    sleep(Duration::from_millis(400));
    let stop_at = s.now();
    s.engine.stop();
    sleep(Duration::from_millis(300));
    let stopped = s.engine.status();
    let out = s.finish();

    assert_eq!(paused.state, PlayState::Paused);
    assert_eq!(stopped.state, PlayState::Stopped);
    assert_eq!(stopped.track, Some(3));
    assert_eq!(stopped.position, Duration::ZERO);

    let lead = first_nonzero(&out, 0);
    let runs = silent_runs(&out[lead..], 4_800);
    assert_eq!(runs.len(), 2, "expected a pause gap and the stopped tail: {runs:?}");
    let (pause_start, pause_len) = (lead + runs[0].0, runs[0].1);
    let stop_start = lead + runs[1].0;
    assert_eq!(stop_start + runs[1].1, out.len(), "silent until the end after stop");

    // Fades, not cuts: no step bigger than the signal's own around either transition.
    let normal = max_step(&out[lead + 1_000..lead + 10_000]);
    for edge in [pause_start, pause_start + pause_len, stop_start] {
        let step = max_step(&out[edge - 400..edge + 400]);
        assert!(step <= normal * 1.05, "click at {edge}: step {step} vs {normal}");
    }

    // Resume continues exactly where the pause stopped consuming.
    let start = align(&out, lead, &reference);
    let resumed = pause_start + pause_len;
    let before = pause_start - start;
    let error = |played: usize| {
        (200..2_000).map(|k| (out[resumed + k] - reference[played + k]).abs()).fold(0.0, f32::max)
    };
    let played = (before - 2..=before + 2).min_by(|&a, &b| error(a).total_cmp(&error(b))).unwrap();
    for k in 200..10_000 {
        let (got, want) = (out[resumed + k], reference[played + k]);
        assert!((got - want).abs() < 1e-4, "after resume, sample {k}: {got} vs {want}");
    }

    let ms = |samples: usize, at: f64| (samples as f64 / RATE - at) * 1e3;
    let play_latency = ms(lead, play_at);
    let pause_latency = ms(pause_start, pause_at);
    let resume_latency = ms(resumed, resume_at);
    let stop_latency = ms(stop_start, stop_at);
    println!(
        "play -> first sample {play_latency:.1} ms, pause -> silent {pause_latency:.1} ms, \
         resume -> sound {resume_latency:.1} ms, stop -> silent {stop_latency:.1} ms"
    );
    for (what, latency) in [
        ("play", play_latency),
        ("pause", pause_latency),
        ("resume", resume_latency),
        ("stop", stop_latency),
    ] {
        assert!(latency < 30.0, "{what} took {latency} ms");
    }
}

#[test]
fn volume_glides_without_clicks_and_the_tap_ignores_it() {
    let dir = Dir::new("volume");
    let src = dir.path("dc.wav");
    ffmpeg(&["-f", "lavfi", "-i", "aevalsrc=0.5|0.5:s=48000:d=4", "-c:a", "pcm_f32le", src.to_str().unwrap()]);

    let s = Session::wav(&dir);
    let tap = s.engine.tap();
    s.engine.play(track(1, &src), Duration::ZERO);
    sleep(Duration::from_millis(300));
    let change_at = s.now();
    s.engine.set_volume(0.5);
    sleep(Duration::from_millis(300));
    s.engine.set_muted(true);
    sleep(Duration::from_millis(300));
    let mut tapped = [0.0f32; 1024];
    let tap_rate = tap.read(&mut tapped);
    s.engine.set_muted(false);
    sleep(Duration::from_millis(300));
    let out = s.finish();

    assert_eq!(tap_rate, 48_000);
    assert!(tapped.iter().all(|&v| (v - 0.5).abs() < 1e-6), "tap is pre-volume: {:?}", &tapped[..4]);

    let gain: Vec<f32> = out.iter().map(|v| v / 0.5).collect();
    let lead = first_nonzero(&gain, 0) + 200;
    // A 10 ms glide from 1 to 0.125 moves at most 0.875/480 per sample.
    let limit = 0.875 / 480.0 * 1.01;
    let worst = max_step(&gain[lead..]);
    assert!(worst <= limit, "volume step {worst} > {limit}");

    let first_change = lead + gain[lead..].iter().position(|&g| g < 0.9999).unwrap();
    let settled = first_change + gain[first_change..].iter().position(|&g| g <= 0.125 + 1e-6).unwrap();
    let glide = settled - first_change;
    assert!((470..=490).contains(&glide), "glide took {glide} samples");
    let levels: Vec<f32> = [first_change - 100, settled + 2_000, out.len() - 100]
        .iter()
        .map(|&i| gain[i])
        .collect();
    assert!((levels[0] - 1.0).abs() < 1e-6 && (levels[1] - 0.125).abs() < 1e-6, "{levels:?}");
    assert!((levels[2] - 0.125).abs() < 1e-6, "unmuted back to 0.125: {levels:?}");
    assert!(gain.iter().skip(settled).any(|&g| g == 0.0), "mute reached silence");

    let latency = (first_change as f64 / RATE - change_at) * 1e3;
    println!("volume change -> audible: {latency:.1} ms");
    assert!(latency < 30.0, "volume latency {latency} ms");
}

#[test]
fn status_follows_the_audible_position() {
    let dir = Dir::new("status");
    let wav = source(&dir, "src.wav", "0.5*sin(2*PI*440*t)", 44_100, 2.0);
    let src = excerpt(&dir, &wav, "src.flac", 0, None, &[]);
    let engine = Engine::new(Sink::Null).unwrap();
    let events = engine.events();
    assert_eq!(engine.status().state, PlayState::Stopped);
    assert_eq!(engine.status().track, None);

    let t0 = Instant::now();
    engine.play(track(9, &src), Duration::ZERO);
    let started = events_until(&events, TIMEOUT, |e| matches!(e, Event::Started { .. }));
    let started_after = t0.elapsed();
    assert!(matches!(started[0], Event::Started { id: 9, duration: Some(d) } if (d.as_secs_f64() - 2.0).abs() < 1e-3));

    sleep(Duration::from_millis(500));
    let status = engine.status();
    let expected = t0.elapsed().as_secs_f64();
    assert_eq!(status.state, PlayState::Playing);
    assert_eq!(status.track, Some(9));
    let pos = status.position.as_secs_f64();
    assert!((pos - expected).abs() < 0.03, "position {pos} vs wall clock {expected}");

    engine.pause();
    sleep(Duration::from_millis(60));
    let a = engine.status();
    sleep(Duration::from_millis(60));
    let b = engine.status();
    assert_eq!(a.state, PlayState::Paused);
    assert_eq!(a.position, b.position, "position holds while paused");

    // Dragging the seek bar: a burst of seeks, of which only the last one matters.
    for ms in (0..40).map(|i| 100 + i * 25) {
        engine.seek(Duration::from_millis(ms));
    }
    engine.seek(Duration::from_millis(1_600));
    sleep(Duration::from_millis(20));
    let sought = engine.status();
    assert_eq!(sought.state, PlayState::Paused);
    assert!((sought.position.as_secs_f64() - 1.6).abs() < 1e-3, "{:?}", sought.position);

    let resumed_at = Instant::now();
    engine.resume();
    let finished = events_until(&events, TIMEOUT, is_finished);
    let remaining = resumed_at.elapsed().as_secs_f64();
    assert!(matches!(finished.as_slice(), [Event::Finished { id: 9 }]));
    assert!((remaining - 0.4).abs() < 0.05, "finished {remaining} s after resuming at 1.6 of 2.0");
    sleep(Duration::from_millis(30));
    let done = engine.status();
    assert_eq!((done.state, done.track, done.position), (PlayState::Stopped, Some(9), Duration::ZERO));
    println!("play -> Started event: {:.1} ms", started_after.as_secs_f64() * 1e3);
}

#[test]
fn events_fire_when_audible_and_in_order() {
    let dir = Dir::new("events");
    let a = source(&dir, "a.wav", "0.5*sin(2*PI*440*t)", 48_000, 0.6);
    let b = source(&dir, "b.wav", "0.5*sin(2*PI*660*t)", 44_100, 0.4);
    let engine = Engine::new(Sink::Null).unwrap();
    let events = engine.events();
    let t0 = Instant::now();
    engine.play(track(1, &a), Duration::ZERO);
    engine.set_next(Some(track(2, &b)));
    let mut timed = Vec::new();
    loop {
        let e = events_until(&events, TIMEOUT, |_| true).remove(0);
        let done = is_finished(&e);
        timed.push((t0.elapsed().as_secs_f64(), e));
        if done {
            break;
        }
    }
    let kinds: Vec<String> = timed.iter().map(|(_, e)| format!("{e:?}")).collect();
    assert!(matches!(timed[0].1, Event::Started { id: 1, .. }), "{kinds:?}");
    assert!(matches!(timed[1].1, Event::Advanced { from: 1, to: 2 }), "{kinds:?}");
    assert!(matches!(timed[2].1, Event::Started { id: 2, .. }), "{kinds:?}");
    assert!(matches!(timed[3].1, Event::Finished { id: 2 }), "{kinds:?}");
    assert_eq!(timed.len(), 4);
    let (advanced, finished) = (timed[1].0, timed[3].0);
    assert!((advanced - 0.6).abs() < 0.04, "advanced at {advanced}");
    assert!((finished - 1.0).abs() < 0.04, "finished at {finished}");
}

/// Two halves of one continuous tone, plus something else.
fn halves(dir: &Dir) -> (PathBuf, PathBuf, PathBuf, Vec<f32>) {
    let wav = source(dir, "src.wav", "0.5*sin(2*PI*997*t)", 48_000, 1.6);
    let a = excerpt(dir, &wav, "a.flac", 0, Some(38_400), &[]);
    let b = excerpt(dir, &wav, "b.flac", 38_400, None, &[]);
    let other = source(dir, "other.wav", "0.5*sin(2*PI*300*t)", 48_000, 1.0);
    (a, b, other, read_wav(&wav).0)
}

fn assert_continues(out: &[f32], reference: &[f32]) {
    let lead = align(out, first_nonzero(out, 0), reference);
    let audio = &out[lead..lead + reference.len()];
    let worst = (200..reference.len())
        .map(|n| (audio[n] - reference[n]).abs())
        .fold(0.0, f32::max);
    assert!(worst < 1e-6, "not one continuous tone (off by {worst})");
    assert!(out[lead + reference.len()..].iter().all(|&v| v == 0.0), "silence after the end");
}

#[test]
fn late_set_next_still_joins_without_a_gap() {
    let dir = Dir::new("late-next");
    let (a, b, _, reference) = halves(&dir);
    let s = Session::wav(&dir);
    let events = s.engine.events();
    s.engine.play(track(1, &a), Duration::ZERO);
    // By now the whole first file has been decoded and its end queued.
    wait_for_position(&s.engine, 0.6);
    s.engine.set_next(Some(track(2, &b)));
    let seen = events_until(&events, TIMEOUT, is_finished);
    let out = s.finish();
    assert!(seen.iter().any(|e| matches!(e, Event::Advanced { from: 1, to: 2 })), "{seen:?}");
    assert!(matches!(seen.last(), Some(Event::Finished { id: 2 })));
    assert_continues(&out, &reference);
}

#[test]
fn replacing_a_next_track_that_was_decoded_ahead() {
    let dir = Dir::new("replace-next");
    let (a, b, other, reference) = halves(&dir);
    let s = Session::wav(&dir);
    let events = s.engine.events();
    s.engine.play(track(1, &a), Duration::ZERO);
    s.engine.set_next(Some(track(5, &other)));
    // The decoder has joined `other` by now, but it is not audible yet.
    wait_for_position(&s.engine, 0.6);
    s.engine.set_next(Some(track(2, &b)));
    let seen = events_until(&events, TIMEOUT, is_finished);
    let out = s.finish();
    assert!(!seen.iter().any(|e| format!("{e:?}").contains('5')), "replaced track surfaced: {seen:?}");
    assert!(seen.iter().any(|e| matches!(e, Event::Advanced { from: 1, to: 2 })), "{seen:?}");
    assert_continues(&out, &reference);
}

#[test]
fn clearing_next_after_it_was_decoded_ahead_stops_at_the_end() {
    let dir = Dir::new("clear-next");
    let (a, _, other, reference) = halves(&dir);
    let s = Session::wav(&dir);
    let events = s.engine.events();
    s.engine.play(track(1, &a), Duration::ZERO);
    s.engine.set_next(Some(track(5, &other)));
    wait_for_position(&s.engine, 0.6);
    s.engine.set_next(None);
    let seen = events_until(&events, TIMEOUT, is_finished);
    let out = s.finish();
    assert!(matches!(seen.as_slice(), [Event::Started { id: 1, .. }, Event::Finished { id: 1 }]), "{seen:?}");
    assert_continues(&out, &reference[..38_400]);
}

#[test]
fn seek_latency_in_every_format() {
    let formats: [(&str, &[&str]); 6] = [
        ("flac", &[]),
        ("wav", &["-c:a", "pcm_s16le"]),
        ("mp3", &["-c:a", "libmp3lame", "-b:a", "192k"]),
        ("ogg", &["-c:a", "libvorbis"]),
        ("opus", &["-c:a", "libopus"]),
        ("m4a", &["-c:a", "aac"]),
    ];
    let dir = Dir::new("seek-latency");
    let wav = source(&dir, "source.wav", "0.5*sin(2*PI*37*t)+0.1", 44_100, 20.0);
    let mut report = Vec::new();
    for (ext, codec) in formats {
        let file = excerpt(&dir, &wav, &format!("tone.{ext}"), 0, None, codec);
        let sub = Dir::new(&format!("seek-latency-{ext}"));
        let s = Session::wav(&sub);
        s.engine.play(track(1, &file), Duration::ZERO);
        sleep(Duration::from_millis(200));
        let seek_at = s.now();
        s.engine.seek(Duration::from_secs(15));
        sleep(Duration::from_millis(200));
        let out = s.finish();
        // The fade out ends on one frame at exactly zero gain; the offset keeps the tone
        // itself away from zero.
        let lead = first_nonzero(&out, 0);
        let cut = lead + out[lead..].iter().position(|&v| v == 0.0).expect("seek cut");
        let latency = (cut as f64 / RATE - seek_at) * 1e3;
        report.push(format!("{ext}: {latency:.1} ms"));
        assert!(latency < 30.0, "{ext}: seek took {latency} ms");
    }
    println!("seek -> new audio, by format: {}", report.join(", "));
}
