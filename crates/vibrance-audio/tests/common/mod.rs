//! Shared helpers: fixture generation with ffmpeg, WAV reading, and signal checks.
//!
//! Tests only ever use the WAV and null sinks; nothing here opens an audio device.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use vibrance_audio::{Event, Track};

pub const OUT_RATE: u32 = 48_000;

/// A scratch directory for one test, under cargo's per-target temp dir.
pub struct Dir(pub PathBuf);

impl Dir {
    pub fn new(name: &str) -> Dir {
        let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("vibrance-audio").join(name);
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create test dir");
        Dir(path)
    }

    pub fn path(&self, file: &str) -> PathBuf {
        self.0.join(file)
    }
}

pub fn ffmpeg(args: &[&str]) {
    let status = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(args)
        .status()
        .expect("ffmpeg is required for these tests");
    assert!(status.success(), "ffmpeg failed: {args:?}");
}

/// Writes `expr` (an aevalsrc expression in `t`, same on both channels) as a 16-bit WAV.
pub fn source(dir: &Dir, name: &str, expr: &str, rate: u32, secs: f64) -> PathBuf {
    let out = dir.path(name);
    let src = format!("aevalsrc='{expr}|{expr}':s={rate}:d={secs}");
    ffmpeg(&["-f", "lavfi", "-i", &src, "-c:a", "pcm_s16le", out.to_str().unwrap()]);
    out
}

/// Encodes frames `[start, end)` of `input` into `name`, with timestamps starting at zero.
pub fn excerpt(dir: &Dir, input: &Path, name: &str, start: u64, end: Option<u64>, codec: &[&str]) -> PathBuf {
    let out = dir.path(name);
    let trim = match end {
        Some(end) => format!("atrim=start_sample={start}:end_sample={end},asetpts=PTS-STARTPTS"),
        None => format!("atrim=start_sample={start},asetpts=PTS-STARTPTS"),
    };
    let mut args = vec!["-i", input.to_str().unwrap(), "-af", &trim];
    args.extend_from_slice(codec);
    args.push(out.to_str().unwrap());
    ffmpeg(&args);
    out
}

pub fn track(id: u64, path: &Path) -> Track {
    Track { id, path: path.to_path_buf() }
}

/// Reads a 16-bit or float WAV, returning the left channel and the rate.
pub fn read_wav(path: &Path) -> (Vec<f32>, u32) {
    let (frames, rate) = read_wav_frames(path);
    (frames.into_iter().map(|f| f[0]).collect(), rate)
}

/// Reads a 16-bit or float WAV as (left, right) frames; mono files repeat the channel.
pub fn read_wav_frames(path: &Path) -> (Vec<[f32; 2]>, u32) {
    let data = std::fs::read(path).expect("read wav");
    assert_eq!(&data[..4], b"RIFF");
    let mut i = 12;
    let (mut format, mut channels, mut rate, mut bits) = (0u16, 0u16, 0u32, 0u16);
    let mut samples = None;
    while i + 8 <= data.len() {
        let id = &data[i..i + 4];
        let size = u32::from_le_bytes(data[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &data[i + 8..(i + 8 + size).min(data.len())];
        if id == b"fmt " {
            format = u16::from_le_bytes([body[0], body[1]]);
            channels = u16::from_le_bytes([body[2], body[3]]);
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            samples = Some(body);
        }
        i += 8 + size + (size & 1);
    }
    let body = samples.expect("data chunk");
    let all: Vec<f32> = match (format, bits) {
        (3, 32) => body.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect(),
        (1, 16) => body
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| f32::from(i16::from_le_bytes(*b)) / 32768.0)
            .collect(),
        other => panic!("unsupported wav format {other:?}"),
    };
    let frames = all
        .chunks_exact(channels as usize)
        .map(|f| [f[0], f[(channels as usize).min(2) - 1]])
        .collect();
    (frames, rate)
}

/// Collects events until one matches `stop` (inclusive). Panics after `timeout`.
pub fn events_until(rx: &Receiver<Event>, timeout: Duration, stop: impl Fn(&Event) -> bool) -> Vec<Event> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(event) => {
                let done = stop(&event);
                seen.push(event);
                if done {
                    return seen;
                }
            }
            Err(_) => panic!("timed out waiting for an event; saw {seen:?}"),
        }
    }
}

pub fn is_finished(e: &Event) -> bool {
    matches!(e, Event::Finished { .. })
}

/// Least-squares fit of `amp * sin(2 pi f n / rate + phase)` over `range`.
pub fn fit_sine(x: &[f32], freq: f64, rate: u32, range: std::ops::Range<usize>) -> (f64, f64) {
    let w = 2.0 * std::f64::consts::PI * freq / f64::from(rate);
    let (mut ss, mut sc, mut cc, mut xs, mut xc) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for n in range {
        let (s, c) = (w * n as f64).sin_cos();
        let v = f64::from(x[n]);
        ss += s * s;
        sc += s * c;
        cc += c * c;
        xs += v * s;
        xc += v * c;
    }
    let det = ss * cc - sc * sc;
    let a = (xs * cc - xc * sc) / det;
    let b = (xc * ss - xs * sc) / det;
    (a.hypot(b), b.atan2(a))
}

/// Largest deviation of `x[range]` from the sine fitted over `fit`.
pub fn sine_error(x: &[f32], freq: f64, rate: u32, fit: std::ops::Range<usize>, range: std::ops::Range<usize>) -> f64 {
    let (amp, phase) = fit_sine(x, freq, rate, fit);
    let w = 2.0 * std::f64::consts::PI * freq / f64::from(rate);
    range
        .map(|n| (f64::from(x[n]) - amp * (w * n as f64 + phase).sin()).abs())
        .fold(0.0, f64::max)
}

/// RMS over each `window`-sample block; returns the smallest.
pub fn min_window_rms(x: &[f32], window: usize) -> f32 {
    x.chunks(window)
        .filter(|c| c.len() == window)
        .map(|c| (c.iter().map(|v| v * v).sum::<f32>() / window as f32).sqrt())
        .fold(f32::INFINITY, f32::min)
}

/// Largest absolute difference between neighbouring samples.
pub fn max_step(x: &[f32]) -> f32 {
    x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
}

/// Runs of exact digital silence at least `min_len` long, as (start, len).
pub fn silent_runs(x: &[f32], min_len: usize) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start = None;
    for (i, &v) in x.iter().chain(std::iter::once(&1.0)).enumerate() {
        match (v == 0.0, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                if i - s >= min_len {
                    runs.push((s, i - s));
                }
                start = None;
            }
            _ => {}
        }
    }
    runs
}
