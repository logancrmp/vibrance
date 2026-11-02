//! Per-track normalization gain, and loudness measurement against ffmpeg's ebur128 filter.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::*;
use vibrance_audio::{Engine, Error, Event, Sink, measure_loudness};

const TIMEOUT: Duration = Duration::from_secs(10);
/// Frames faded in after `play`, skipped when comparing.
const FADE_IN: usize = 200;

fn db(gain_db: f32) -> f32 {
    10f32.powf(gain_db / 20.0)
}

/// Asserts `out[range] == reference[range] * gain`, sample for sample.
fn assert_scaled(out: &[f32], reference: &[f32], range: std::ops::Range<usize>, gain: f32, what: &str) {
    for n in range {
        let (got, want) = (out[n], reference[n] * gain);
        assert!((got - want).abs() < 2e-6, "{what}: sample {n} is {got}, expected {want}");
    }
}

fn render_fast(dir: &Dir, play: impl FnOnce(&Engine)) -> Vec<f32> {
    let out = dir.path("out.wav");
    let engine = Engine::new(Sink::Wav { path: out.clone(), realtime: false }).unwrap();
    let events = engine.events();
    play(&engine);
    events_until(&events, TIMEOUT, is_finished);
    drop(engine);
    read_wav(&out).0
}

#[test]
fn gain_scales_the_output_by_its_decibels() {
    let dir = Dir::new("norm-gain");
    let wav = source(&dir, "src.wav", "0.25*sin(2*PI*997*t)", 48_000, 1.0);
    let file = excerpt(&dir, &wav, "src.flac", 0, None, &[]);
    let (reference, _) = read_wav(&wav);
    for gain_db in [-6.0, 0.0, 3.5, -20.0] {
        let out = render_fast(&dir, |e| e.play(track_with_gain(1, &file, gain_db), Duration::ZERO));
        assert_eq!(out.len(), reference.len());
        let (amp, _) = fit_sine(&out, 997.0, OUT_RATE, FADE_IN..out.len());
        let measured = 20.0 * (amp / 0.25).log10();
        assert!((measured - f64::from(gain_db)).abs() < 0.01, "{gain_db} dB came out as {measured}");
        assert_scaled(&out, &reference, FADE_IN..out.len(), db(gain_db), &format!("{gain_db} dB"));
    }
}

#[test]
fn gapless_segments_each_use_their_own_gain() {
    let dir = Dir::new("norm-gapless");
    let wav = source(&dir, "src.wav", "0.25*sin(2*PI*997*t)", 48_000, 2.0);
    let a = excerpt(&dir, &wav, "a.flac", 0, Some(48_123), &[]);
    let b = excerpt(&dir, &wav, "b.flac", 48_123, None, &[]);
    let (reference, _) = read_wav(&wav);
    let out = render_fast(&dir, |e| {
        e.play(track_with_gain(1, &a, -6.0), Duration::ZERO);
        e.set_next(Some(track_with_gain(2, &b, 3.0)));
    });
    assert_eq!(out.len(), reference.len());
    // The second track's gain starts exactly on its first sample.
    assert_scaled(&out, &reference, FADE_IN..48_123, db(-6.0), "first track");
    assert_scaled(&out, &reference, 48_123..out.len(), db(3.0), "second track");
}

#[test]
fn a_new_gain_for_the_next_instance_of_a_repeated_song() {
    let dir = Dir::new("norm-repeat");
    // 500 whole periods, so the loop is seamless.
    let wav = source(&dir, "src.wav", "0.25*sin(2*PI*1000*t)", 48_000, 0.5);
    let file = excerpt(&dir, &wav, "a.flac", 0, None, &[]);
    let (one, _) = read_wav(&wav);
    let out_path = dir.path("out.wav");
    let engine = Engine::new(Sink::Wav { path: out_path.clone(), realtime: false }).unwrap();
    let events = engine.events();
    engine.play(track_with_gain(4, &file, 0.0), Duration::ZERO);
    engine.set_next(Some(track_with_gain(4, &file, 0.0)));
    // Same track queued again, now with a different gain: only the next pass changes.
    engine.set_next(Some(track_with_gain(4, &file, -6.0)));
    events_until(&events, TIMEOUT, |e| matches!(e, Event::Advanced { .. }));
    engine.set_next(None);
    events_until(&events, TIMEOUT, is_finished);
    drop(engine);
    let out = read_wav(&out_path).0;
    let twice: Vec<f32> = one.iter().chain(&one).copied().collect();
    assert_eq!(out.len(), twice.len());
    assert_scaled(&out, &twice, FADE_IN..one.len(), 1.0, "first pass");
    assert_scaled(&out, &twice, one.len()..twice.len(), db(-6.0), "second pass");
}

fn realtime(dir: &Dir) -> (Engine, PathBuf) {
    let out = dir.path("out.wav");
    (Engine::new(Sink::Wav { path: out.clone(), realtime: true }).unwrap(), out)
}

fn wait_for_position(engine: &Engine, at_least: f64) {
    let deadline = Instant::now() + TIMEOUT;
    while engine.status().position.as_secs_f64() < at_least {
        assert!(Instant::now() < deadline, "position never reached {at_least}");
        sleep(Duration::from_millis(2));
    }
}

#[test]
fn set_gain_glides_without_clicks_and_reaches_the_tap() {
    let dir = Dir::new("norm-set-gain");
    let src = dir.path("dc.wav");
    ffmpeg(&["-f", "lavfi", "-i", "aevalsrc=0.5|0.5:s=48000:d=3", "-c:a", "pcm_f32le", src.to_str().unwrap()]);
    let (engine, out) = realtime(&dir);
    let tap = engine.tap();
    engine.play(track(8, &src), Duration::ZERO);
    sleep(Duration::from_millis(300));
    engine.set_gain(8, -12.0);
    // Ignored: not the playing or queued track.
    engine.set_gain(99, 10.0);
    sleep(Duration::from_millis(200));
    let mut tapped = [0.0f32; 256];
    tap.read(&mut tapped);
    engine.set_volume(0.5);
    sleep(Duration::from_millis(100));
    drop(engine);
    let (out, _) = read_wav(&out);

    let target = db(-12.0);
    assert!(tapped.iter().all(|&v| (v - 0.5 * target).abs() < 1e-5), "tap is post-gain: {}", tapped[0]);
    let gain: Vec<f32> = out.iter().map(|v| v / 0.5).collect();
    let lead = gain.iter().position(|&g| g > 0.0).unwrap() + FADE_IN;
    let limit = (1.0 - target) / 480.0 * 1.01;
    let worst = max_step(&gain[lead..]);
    assert!(worst <= limit, "gain step {worst} > {limit}");
    let start = lead + gain[lead..].iter().position(|&g| g < 0.9999).unwrap();
    let end = start + gain[start..].iter().position(|&g| g <= target + 1e-6).unwrap();
    assert!((470..=490).contains(&(end - start)), "glide took {} samples", end - start);
    assert!((gain[end + 1_000] - target).abs() < 1e-6, "settled at {}", gain[end + 1_000]);
}

#[test]
fn set_gain_on_a_track_decoded_ahead_applies_from_its_first_sample() {
    let dir = Dir::new("norm-next");
    let wav = source(&dir, "src.wav", "0.25*sin(2*PI*997*t)", 48_000, 1.6);
    let a = excerpt(&dir, &wav, "a.flac", 0, Some(38_400), &[]);
    let b = excerpt(&dir, &wav, "b.flac", 38_400, None, &[]);
    let (reference, _) = read_wav(&wav);
    let (engine, out) = realtime(&dir);
    let events = engine.events();
    engine.play(track(1, &a), Duration::ZERO);
    engine.set_next(Some(track(2, &b)));
    // By now the second file is decoded into the buffer with its old gain.
    wait_for_position(&engine, 0.6);
    engine.set_gain(2, -9.0);
    events_until(&events, TIMEOUT, is_finished);
    drop(engine);
    let (out, _) = read_wav(&out);

    // Locate the first track's audio: it starts after some silence, faded in.
    let lead = out.iter().position(|&v| v != 0.0).unwrap();
    let start = (lead.saturating_sub(2)..=lead + 2)
        .min_by(|&x, &y| {
            let err = |s: usize| (s + 300..s + 2_000).map(|n| (out[n] - reference[n - s]).abs()).sum::<f32>();
            err(x).total_cmp(&err(y))
        })
        .unwrap();
    let aligned = &out[start..start + reference.len()];
    assert_scaled(aligned, &reference, FADE_IN..38_400, 1.0, "first track");
    assert_scaled(aligned, &reference, 38_400..reference.len(), db(-9.0), "second track");
}

/// Integrated loudness (LUFS) and true peak (dBFS) according to ffmpeg's ebur128 filter.
fn ffmpeg_loudness(path: &Path) -> (f32, f32) {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostats", "-nostdin", "-i"])
        .arg(path)
        .args(["-af", "ebur128=peak=true", "-f", "null", "-"])
        .output()
        .expect("ffmpeg is required for these tests");
    let log = String::from_utf8_lossy(&output.stderr);
    let summary = &log[log.rfind("Summary:").expect("ebur128 summary")..];
    let value = |label: &str| -> f32 {
        let line = summary.lines().find(|l| l.trim_start().starts_with(label)).unwrap();
        line.split_whitespace().nth(1).unwrap().parse().unwrap()
    };
    (value("I:"), value("Peak:"))
}

#[test]
fn loudness_matches_ffmpeg() {
    let dir = Dir::new("loudness");
    let pink = dir.path("pink.wav");
    ffmpeg(&[
        "-f", "lavfi", "-i", "anoisesrc=color=pink:amplitude=0.4:sample_rate=44100:duration=12:seed=5",
        "-af", "aformat=channel_layouts=stereo,volume=-3dB", "-c:a", "pcm_s16le", pink.to_str().unwrap(),
    ]);
    let mut files: Vec<PathBuf> = Vec::new();
    for (name, codec) in [
        ("pink.flac", &[][..]),
        ("pink.mp3", &["-c:a", "libmp3lame", "-b:a", "192k"][..]),
        ("pink.ogg", &["-c:a", "libvorbis"][..]),
        ("pink.opus", &["-c:a", "libopus", "-b:a", "128k"][..]),
        ("pink.m4a", &["-c:a", "aac", "-b:a", "192k"][..]),
    ] {
        files.push(excerpt(&dir, &pink, name, 0, None, codec));
    }
    let mono = dir.path("mono-tone.wav");
    ffmpeg(&["-f", "lavfi", "-i", "sine=f=1000:r=48000:d=8", "-af", "volume=-6dB", mono.to_str().unwrap()]);
    files.push(mono);
    let hires = dir.path("hires.flac");
    ffmpeg(&[
        "-f", "lavfi", "-i", "anoisesrc=color=white:amplitude=0.2:sample_rate=96000:duration=6:seed=2",
        "-ac", "2", "-c:a", "flac", "-sample_fmt", "s32", hires.to_str().unwrap(),
    ]);
    files.push(hires);
    let surround = dir.path("surround.flac");
    ffmpeg(&[
        "-f", "lavfi", "-i",
        "aevalsrc=0.3*sin(2*PI*440*t)|0.3*sin(2*PI*550*t)|0.2*sin(2*PI*660*t)|0.8*sin(2*PI*50*t)|0.1*sin(2*PI*880*t)|0.1*sin(2*PI*990*t):s=48000:d=6:c=5.1",
        surround.to_str().unwrap(),
    ]);
    files.push(surround);

    let mut report = Vec::new();
    for file in &files {
        let started = Instant::now();
        let ours = measure_loudness(file).unwrap();
        let took = started.elapsed();
        let (lufs, peak_db) = ffmpeg_loudness(file);
        let our_peak_db = 20.0 * ours.true_peak.log10();
        let name = file.file_name().unwrap().to_string_lossy();
        report.push(format!(
            "{name}: {:.2} LUFS (ffmpeg {lufs:.1}), peak {our_peak_db:.2} dBFS (ffmpeg {peak_db:.1}), {took:.0?}",
            ours.integrated_lufs
        ));
        assert!((ours.integrated_lufs - lufs).abs() < 0.5, "{name}: {} vs {lufs} LUFS", ours.integrated_lufs);
        assert!((our_peak_db - peak_db).abs() < 0.5, "{name}: peak {our_peak_db} vs {peak_db} dBFS");
    }
    println!("{}", report.join("\n"));
}

#[test]
fn loudness_of_silence_errors_and_long_files() {
    let dir = Dir::new("loudness-edge");
    let silence = dir.path("silence.flac");
    ffmpeg(&["-f", "lavfi", "-i", "anullsrc=r=44100:cl=stereo", "-t", "3", silence.to_str().unwrap()]);
    let quiet = measure_loudness(&silence).unwrap();
    assert_eq!(quiet.integrated_lufs, f32::NEG_INFINITY);
    assert_eq!(quiet.true_peak, 0.0);

    let empty = dir.path("empty.wav");
    ffmpeg(&["-f", "lavfi", "-i", "anullsrc=r=44100:cl=stereo", "-t", "0", "-c:a", "pcm_s16le", empty.to_str().unwrap()]);
    assert_eq!(measure_loudness(&empty).unwrap().integrated_lufs, f32::NEG_INFINITY);

    let garbage = dir.path("garbage.mp3");
    std::fs::write(&garbage, (0..50_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect::<Vec<_>>()).unwrap();
    assert!(matches!(measure_loudness(&garbage), Err(Error::Decode(_))));
    assert!(matches!(measure_loudness(Path::new("/nonexistent/x.flac")), Err(Error::Io(_))));

    // A typical song's worth of audio, to keep an eye on speed.
    let song = dir.path("song.flac");
    ffmpeg(&[
        "-f", "lavfi", "-i", "anoisesrc=color=pink:amplitude=0.3:sample_rate=44100:duration=240",
        "-ac", "2", song.to_str().unwrap(),
    ]);
    let started = Instant::now();
    let loud = measure_loudness(&song).unwrap();
    let took = started.elapsed();
    println!("4 min stereo 44.1 kHz FLAC: {:.2} LUFS in {took:.0?}", loud.integrated_lufs);
    assert!(loud.integrated_lufs.is_finite());
}
