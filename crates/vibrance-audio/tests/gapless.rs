//! Gapless joins, rendered faster than real time into a WAV file and checked sample by sample.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use common::*;
use vibrance_audio::{Engine, Event, Sink};

const TIMEOUT: Duration = Duration::from_secs(30);
/// Frames faded in after `play` (3 ms at 48 kHz), skipped when comparing.
const FADE_IN: usize = 200;

/// Plays `files` back to back (each set as the next as soon as the previous starts) and
/// returns the events and the rendered left channel.
fn render(dir: &Dir, files: &[PathBuf]) -> (Vec<Event>, Vec<f32>) {
    let out = dir.path("out.wav");
    let engine = Engine::new(Sink::Wav { path: out.clone(), realtime: false }).unwrap();
    let events = engine.events();
    engine.play(track(0, &files[0]), Duration::ZERO);
    engine.set_next(files.get(1).map(|f| track(1, f)));
    let mut seen = Vec::new();
    loop {
        let batch = events_until(&events, TIMEOUT, |e| {
            matches!(e, Event::Advanced { .. } | Event::Finished { .. })
        });
        let last = batch.last().cloned();
        seen.extend(batch);
        match last {
            Some(Event::Advanced { to, .. }) => {
                let next = to as usize + 1;
                engine.set_next(files.get(next).map(|f| track(next as u64, f)));
            }
            _ => break,
        }
    }
    drop(engine);
    let (left, rate) = read_wav(&out);
    assert_eq!(rate, OUT_RATE);
    (seen, left)
}

fn assert_events_for_two(events: &[Event]) {
    let summary: Vec<String> = events
        .iter()
        .map(|e| match e {
            Event::Started { id, .. } => format!("started {id}"),
            Event::Advanced { from, to } => format!("advanced {from}->{to}"),
            Event::Finished { id } => format!("finished {id}"),
            Event::Failed { id, error } => format!("failed {id}: {error}"),
        })
        .collect();
    assert_eq!(summary, ["started 0", "advanced 0->1", "started 1", "finished 1"]);
}

fn split(dir: &Dir, src: &Path, at: u64, ext: &str, codec: &[&str]) -> Vec<PathBuf> {
    vec![
        excerpt(dir, src, &format!("a.{ext}"), 0, Some(at), codec),
        excerpt(dir, src, &format!("b.{ext}"), at, None, codec),
    ]
}

#[test]
fn flac_join_at_output_rate_is_bit_exact() {
    let dir = Dir::new("flac48");
    let src = source(&dir, "src.wav", "0.5*sin(2*PI*997*t)", 48_000, 2.0);
    let files = split(&dir, &src, 48_123, "flac", &[]);
    let (events, out) = render(&dir, &files);
    assert_events_for_two(&events);

    let (expected, _) = read_wav(&src);
    assert_eq!(out.len(), expected.len(), "output length");
    let worst = (FADE_IN..out.len())
        .map(|n| (out[n] - expected[n]).abs())
        .fold(0.0, f32::max);
    assert!(worst < 1e-6, "output differs from the source by up to {worst}");
}

#[test]
fn resampled_join_is_seamless() {
    let dir = Dir::new("flac44");
    let src = source(&dir, "src.wav", "0.5*sin(2*PI*997*t)", 44_100, 2.0);
    let files = split(&dir, &src, 44_111, "flac", &[]);
    let (events, out) = render(&dir, &files);
    assert_events_for_two(&events);

    assert_eq!(out.len(), 96_000, "2 s at 48 kHz");
    let boundary = 44_111 * 48_000 / 44_100;
    let interior = 1_000..out.len() - 1_000;
    let worst = sine_error(&out, 997.0, OUT_RATE, interior.clone(), interior);
    let at_join = sine_error(&out, 997.0, OUT_RATE, 1_000..95_000, boundary - 300..boundary + 300);
    assert!(worst < 2e-3, "resampled output deviates by {worst}");
    assert!(at_join < 2e-3, "join deviates by {at_join}");
}

#[test]
fn join_across_sample_rates_has_no_gap() {
    let dir = Dir::new("crossrate");
    // Whole periods, so the two halves meet in phase.
    let a = source(&dir, "a.wav", "0.5*sin(2*PI*440*t)", 44_100, 1.0);
    let b = source(&dir, "b.wav", "0.5*sin(2*PI*440*t)", 48_000, 1.0);
    let a = excerpt(&dir, &a, "a.flac", 0, None, &[]);
    let b = excerpt(&dir, &b, "b.flac", 0, None, &[]);
    let (events, out) = render(&dir, &[a, b]);
    assert_events_for_two(&events);

    assert_eq!(out.len(), 96_000);
    let rms = min_window_rms(&out[FADE_IN..out.len() - 240], 240);
    assert!(rms > 0.3, "a 5 ms window dropped to RMS {rms}");
    let normal = max_step(&out[10_000..40_000]);
    let join = max_step(&out[48_000 - 200..48_000 + 200]);
    assert!(join < normal * 1.2, "step at the join {join} vs {normal} elsewhere");
}

#[test]
fn lossy_formats_join_without_gap() {
    let formats: [(&str, &[&str]); 6] = [
        ("mp3", &["-c:a", "libmp3lame", "-b:a", "192k"]),
        ("ogg", &["-c:a", "libvorbis", "-q:a", "6"]),
        ("opus", &["-c:a", "libopus", "-b:a", "160k"]),
        ("m4a", &["-c:a", "aac", "-b:a", "192k"]),
        ("wav", &["-c:a", "pcm_s16le"]),
        ("flac", &[]),
    ];
    let dir = Dir::new("formats");
    let src = source(&dir, "src.wav", "0.5*sin(2*PI*997*t)", 44_100, 2.0);
    let boundary = 44_111 * 48_000 / 44_100;
    let mut report = Vec::new();
    for (ext, codec) in formats {
        let sub = Dir::new(&format!("formats-{ext}"));
        let files = split(&sub, &src, 44_111, ext, codec);
        let (events, out) = render(&sub, &files);
        assert_events_for_two(&events);

        // Delay and padding trimmed: exact length (Opus may round by a frame).
        assert!(out.len().abs_diff(96_000) <= 2, "{ext}: {} frames", out.len());
        // Aligned with the original everywhere, so nothing was added or lost at the join.
        let fit = 2_000..out.len() - 2_000;
        let away = 2_000..boundary - 1_200;
        let error = sine_error(&out, 997.0, OUT_RATE, fit.clone(), away);
        let near = sine_error(&out, 997.0, OUT_RATE, fit, boundary - 1_200..boundary + 1_200);
        let rms = min_window_rms(&out[FADE_IN..out.len() - 480], 240);
        report.push(format!("{ext}: error {error:.4}, at join {near:.4}, min rms {rms:.3}"));
        assert!(error < 0.02, "{ext}: deviates by {error} away from the join");
        // Codecs smear the cut a little; a gap or misalignment would be ~0.5.
        assert!(near < 0.15, "{ext}: deviates by {near} at the join");
        assert!(rms > 0.3, "{ext}: a 5 ms window dropped to RMS {rms}");
    }
    println!("{}", report.join("\n"));
}

#[test]
fn repeat_song_loops_without_a_gap() {
    let dir = Dir::new("repeat");
    // 500 whole periods, so the loop point is continuous.
    let src = source(&dir, "src.wav", "0.5*sin(2*PI*1000*t)", 48_000, 0.5);
    let file = excerpt(&dir, &src, "a.flac", 0, None, &[]);
    let out = dir.path("out.wav");
    let engine = Engine::new(Sink::Wav { path: out.clone(), realtime: false }).unwrap();
    let events = engine.events();
    engine.play(track(7, &file), Duration::ZERO);
    engine.set_next(Some(track(7, &file)));
    let first = events_until(&events, TIMEOUT, |e| matches!(e, Event::Advanced { .. }));
    engine.set_next(None);
    let rest = events_until(&events, TIMEOUT, is_finished);
    drop(engine);
    assert!(matches!(first.as_slice(), [Event::Started { id: 7, .. }, Event::Advanced { from: 7, to: 7 }]));
    assert!(matches!(rest.as_slice(), [Event::Started { id: 7, .. }, Event::Finished { id: 7 }]));

    let (out, _) = read_wav(&out);
    let (one, _) = read_wav(&src);
    assert_eq!(out.len(), one.len() * 2);
    let worst = (FADE_IN..out.len())
        .map(|n| (out[n] - one[n % one.len()]).abs())
        .fold(0.0, f32::max);
    assert!(worst < 1e-6, "loop differs from the source by up to {worst}");
}

#[test]
fn mono_plays_on_both_channels_and_surround_is_downmixed() {
    let dir = Dir::new("channels");
    let mono = dir.path("mono.flac");
    ffmpeg(&["-f", "lavfi", "-i", "sine=f=500:r=48000:d=0.5", mono.to_str().unwrap()]);
    let surround = dir.path("surround.flac");
    ffmpeg(&[
        "-f", "lavfi", "-i", "aevalsrc=0.9*sin(2*PI*300*t)|0.9*sin(2*PI*300*t)|0.9*sin(2*PI*300*t)|0|0.9*sin(2*PI*300*t)|0.9*sin(2*PI*300*t):s=48000:d=0.5:c=5.1",
        surround.to_str().unwrap(),
    ]);
    for (name, file) in [("mono", mono), ("surround", surround)] {
        let out = dir.path(&format!("{name}.wav"));
        let engine = Engine::new(Sink::Wav { path: out.clone(), realtime: false }).unwrap();
        let events = engine.events();
        engine.play(track(1, &file), Duration::ZERO);
        events_until(&events, TIMEOUT, is_finished);
        drop(engine);
        let (frames, _) = read_wav_frames(&out);
        assert_eq!(frames.len(), 24_000, "{name}");
        let peak = frames.iter().map(|f| f[0].abs().max(f[1].abs())).fold(0.0, f32::max);
        assert!(frames.iter().all(|f| (f[0] - f[1]).abs() < 1e-6), "{name}: channels differ");
        assert!(peak > 0.1 && peak <= 1.0, "{name}: peak {peak}");
    }
}

#[test]
fn starting_mid_track_is_sample_accurate_in_every_format() {
    let formats: [(&str, &[&str]); 6] = [
        ("flac", &[]),
        ("wav", &["-c:a", "pcm_s16le"]),
        ("mp3", &["-c:a", "libmp3lame", "-b:a", "192k"]),
        ("ogg", &["-c:a", "libvorbis", "-q:a", "6"]),
        ("opus", &["-c:a", "libopus", "-b:a", "160k"]),
        ("m4a", &["-c:a", "aac", "-b:a", "192k"]),
    ];
    let dir = Dir::new("offsets");
    // A low tone, so a misplaced start shows up as a phase error without ambiguity.
    let freq = 37.0;
    let src = source(&dir, "tone.wav", "0.5*sin(2*PI*37*t)", 44_100, 3.0);
    let mut report = Vec::new();
    for (ext, codec) in formats {
        let file = excerpt(&dir, &src, &format!("src.{ext}"), 0, None, codec);
        // Encoders may shift the phase of a low tone a little (Opus high-passes its input), so
        // offsets are compared with the same file played from the start.
        let mut from_start = None;
        for start in [0.0, 1.3, 2.0213] {
            let out = dir.path("out.wav");
            let engine = Engine::new(Sink::Wav { path: out.clone(), realtime: false }).unwrap();
            let events = engine.events();
            engine.play(track(1, &file), Duration::from_secs_f64(start));
            events_until(&events, TIMEOUT, is_finished);
            drop(engine);
            let (out, _) = read_wav(&out);
            let expected_len = ((3.0 - start) * 48_000.0) as usize;
            assert!(out.len().abs_diff(expected_len) <= 2, "{ext} @ {start}: {} frames", out.len());
            let (amp, phase) = fit_sine(&out, freq, OUT_RATE, 2_000..out.len() - 2_000);
            let want = (2.0 * std::f64::consts::PI * freq * start).rem_euclid(2.0 * std::f64::consts::PI);
            let mut diff = (phase - want).rem_euclid(2.0 * std::f64::consts::PI);
            if diff > std::f64::consts::PI {
                diff -= 2.0 * std::f64::consts::PI;
            }
            let error_us = diff / (2.0 * std::f64::consts::PI * freq) * 1e6;
            report.push(format!("{ext} @ {start}: phase {error_us:+.1} us"));
            assert!((amp - 0.5).abs() < 0.02, "{ext} @ {start}: amplitude {amp}");
            let reference = *from_start.get_or_insert(error_us);
            assert!(reference.abs() < 500.0, "{ext}: playback starts {reference} us off");
            let off = error_us - reference;
            assert!(off.abs() < 25.0, "{ext} @ {start}: lands {off} us off");
        }
    }
    println!("{}", report.join("\n"));
}
