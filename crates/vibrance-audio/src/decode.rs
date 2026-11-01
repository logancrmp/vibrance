//! Opening and decoding one file into interleaved stereo f32 at the file's own sample rate.
//!
//! Symphonia handles demuxing and most codecs; Ogg Opus packets are decoded with libopus. Encoder
//! delay and padding are trimmed here so every source yields exactly its presentation frames:
//! frame 0 is the first real sample, and the stream ends after the last one.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use symphonia::core::audio::Channels;
use symphonia::core::audio::Position;
use symphonia::core::codecs::audio::well_known::{
    CODEC_ID_AAC, CODEC_ID_FLAC, CODEC_ID_OPUS, CODEC_ID_VORBIS,
};
use symphonia::core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::packet::Packet;
use symphonia::core::units::{Time, TimeBase, Timestamp};

use crate::mp4;

/// Consecutive undecodable packets tolerated before the file is declared broken.
const MAX_BAD_PACKETS: u32 = 64;
/// Lossy codecs need some audio before the target to converge after a seek.
const SEEK_PREROLL_MS: u64 = 100;
/// Largest Opus packet: 120 ms at 48 kHz.
const OPUS_MAX_FRAMES: usize = 5760;

pub(crate) struct Source {
    path: PathBuf,
    format: Box<dyn FormatReader>,
    decoder: Decoder,
    track_id: u32,
    rate: u32,
    /// Whether timestamps count frames, so packet timestamps locate samples exactly.
    frame_ts: bool,
    time_base: Option<TimeBase>,
    /// Timestamp of presentation frame 0, for delay the demuxer does not account for.
    offset: i64,
    /// Number of presentation frames, when known.
    end: Option<u64>,
    duration: Option<Duration>,
    /// Presentation index of the next frame to come out of the decoder.
    next_frame: Option<i64>,
    /// Frames before this presentation index are dropped (seek target).
    skip_to: u64,
    preroll: u64,
    downmix: Downmix,
    interleaved: Vec<f32>,
    bad_packets: u32,
    produced: u64,
}

enum Decoder {
    Symphonia(Box<dyn AudioDecoder>),
    Opus(OpusDecoder),
}

struct OpusDecoder {
    inner: opus::Decoder,
    channels: usize,
    gain: f32,
}

impl Source {
    pub fn open(path: &Path) -> Result<Source, String> {
        let file = File::open(path).map_err(|e| format!("cannot open file: {e}"))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe()
            .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(|e| format!("unsupported or corrupt file: {e}"))?;

        let track = format
            .default_track(TrackType::Audio)
            .or_else(|| format.first_track(TrackType::Audio))
            .ok_or("no audio track")?
            .clone();
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or("no audio codec parameters")?
            .clone();
        let rate = params.sample_rate.filter(|&r| r > 0).ok_or("unknown sample rate")?;
        let time_base = track.time_base;
        let frame_ts = time_base
            .is_some_and(|tb| tb.numer.get() == 1 && tb.denom.get() == rate);

        let mut offset = 0i64;
        let mut end = track.num_frames;
        let decoder = if params.codec == CODEC_ID_OPUS {
            let opus = OpusDecoder::new(&params)?;
            // Ogg timestamps for Opus include the pre-skip.
            offset = i64::from(track.delay.unwrap_or(0));
            end = end.map(|n| n.saturating_sub(offset as u64));
            Decoder::Opus(opus)
        } else {
            let decoder = symphonia::default::get_codecs()
                .make_audio_decoder(&params, &AudioDecoderOptions::default())
                .map_err(|e| format!("unsupported codec: {e}"))?;
            Decoder::Symphonia(decoder)
        };
        if format.format_info().short_name == "isomp4"
            && let Some(g) = mp4::read_gapless(path, rate)
        {
            offset = i64::try_from(g.delay).unwrap_or(0);
            if g.valid.is_some() {
                end = g.valid;
            }
        }

        let duration = end
            .map(|n| frames_to_duration(n, rate))
            .or_else(|| {
                let secs = time_base?.calc_duration(track.duration?)?.as_secs_f64();
                Duration::try_from_secs_f64(secs).ok()
            });
        let lossy = params.codec >= CODEC_ID_VORBIS && params.codec < CODEC_ID_FLAC
            || params.codec == CODEC_ID_AAC;
        let preroll = if lossy { u64::from(rate) * SEEK_PREROLL_MS / 1000 } else { 0 };
        let downmix = match &decoder {
            Decoder::Opus(o) => Downmix::new(Some(&Channels::Discrete(o.channels as u16))),
            Decoder::Symphonia(_) => Downmix::new(params.channels.as_ref()),
        };

        Ok(Source {
            path: path.to_path_buf(),
            format,
            decoder,
            track_id: track.id,
            rate,
            frame_ts,
            time_base,
            offset,
            end,
            duration,
            next_frame: None,
            skip_to: 0,
            preroll,
            downmix,
            interleaved: Vec::new(),
            bad_packets: 0,
            produced: 0,
        })
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// Repositions so the next frame produced is the one at `pos`.
    pub fn seek(&mut self, pos: Duration) -> Result<(), String> {
        let target = duration_to_frames(pos, self.rate);
        if target == 0 && self.next_frame.is_none() && self.produced == 0 {
            return Ok(());
        }
        let from = target.saturating_sub(self.preroll);
        let ts = i64::try_from(from).unwrap_or(i64::MAX).saturating_add(self.offset);
        let to = if self.frame_ts {
            SeekTo::Timestamp { ts: Timestamp::new(ts), track_id: self.track_id }
        } else {
            let secs = ts as f64 / f64::from(self.rate);
            let time = Time::try_from_secs_f64(secs).ok_or("seek position out of range")?;
            SeekTo::Time { time, track_id: Some(self.track_id) }
        };
        match self.format.seek(SeekMode::Accurate, to) {
            Ok(_) => {
                self.decoder.reset();
                self.next_frame = None;
            }
            Err(e) => {
                // Unseekable or broken index: decode from the start and drop up to the target.
                log::warn!("seek failed in {}: {e}; decoding from the start", self.path.display());
                *self = Source::open(&self.path)?;
            }
        }
        self.skip_to = target;
        Ok(())
    }

    /// Appends the next decoded frames (interleaved stereo) to `out`. Returns `Ok(false)` at the
    /// end of the track.
    pub fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, String> {
        loop {
            if self.end.is_some_and(|end| self.next_frame.is_some_and(|n| n >= end as i64)) {
                return Ok(false);
            }
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) | Err(SymError::ResetRequired) => return self.finish(),
                Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return self.finish();
                }
                Err(e) => return Err(format!("read error: {e}")),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match self.decode(&packet) {
                Ok(()) => self.bad_packets = 0,
                Err(Bad::Packet(e)) => {
                    self.bad_packets += 1;
                    if self.bad_packets > MAX_BAD_PACKETS {
                        return Err(format!("decode error: {e}"));
                    }
                    continue;
                }
                Err(Bad::Fatal(e)) => return Err(e),
            }
            let before = out.len();
            self.emit(&packet, out);
            if out.len() > before {
                return Ok(true);
            }
        }
    }

    fn finish(&self) -> Result<bool, String> {
        if self.produced == 0 && self.bad_packets > 0 {
            return Err("no decodable audio".into());
        }
        Ok(false)
    }

    /// Decodes `packet` into `self.interleaved` (in the source's channel layout).
    fn decode(&mut self, packet: &Packet) -> Result<(), Bad> {
        match &mut self.decoder {
            Decoder::Symphonia(decoder) => {
                let buf = match decoder.decode(packet) {
                    Ok(buf) => buf,
                    Err(SymError::DecodeError(e)) => return Err(Bad::Packet(e.to_string())),
                    Err(SymError::IoError(e)) => return Err(Bad::Packet(e.to_string())),
                    Err(SymError::ResetRequired) => {
                        decoder.reset();
                        return Err(Bad::Packet("decoder reset".into()));
                    }
                    Err(e) => return Err(Bad::Fatal(format!("decode error: {e}"))),
                };
                self.downmix.update(buf.spec().channels());
                buf.copy_to_vec_interleaved(&mut self.interleaved);
                Ok(())
            }
            Decoder::Opus(opus) => {
                self.interleaved.resize(OPUS_MAX_FRAMES * opus.channels, 0.0);
                let frames = opus
                    .inner
                    .decode_float(&packet.data, &mut self.interleaved, false)
                    .map_err(|e| Bad::Packet(e.to_string()))?;
                self.interleaved.truncate(frames * opus.channels);
                if opus.gain != 1.0 {
                    self.interleaved.iter_mut().for_each(|s| *s *= opus.gain);
                }
                Ok(())
            }
        }
    }

    /// Trims the decoded packet to its presentation frames and appends them, downmixed.
    fn emit(&mut self, packet: &Packet, out: &mut Vec<f32>) {
        let channels = self.downmix.channels.max(1);
        let decoded = self.interleaved.len() / channels;
        let (trim_start, trim_end) = (packet.trim_start.get(), packet.trim_end.get());
        let mut lo = 0usize;
        let mut hi = decoded;
        // Decoders that honour the gapless option already trimmed; libopus never does.
        if trim_start + trim_end > 0
            && decoded as u64 == packet.block_dur().get()
            && packet.block_dur() != packet.dur
        {
            lo = (trim_start as usize).min(decoded);
            hi = decoded.saturating_sub(trim_end as usize).max(lo);
        }
        let first_ts = packet.pts.get().saturating_add(i64::try_from(trim_start).unwrap_or(0));
        let first = if self.frame_ts {
            first_ts - self.offset
        } else {
            match self.next_frame {
                Some(n) => n,
                None => self.ts_to_frames(first_ts) - self.offset,
            }
        };
        let count = (hi - lo) as i64;
        self.next_frame = Some(first + count);

        // Keep only presentation frames in [skip_to, end).
        let keep_from = (self.skip_to as i64).max(0);
        let keep_to = self.end.map_or(i64::MAX, |e| e as i64);
        let a = first.max(keep_from).min(first + count);
        let b = (first + count).min(keep_to).max(a);
        if b <= a {
            return;
        }
        let lo = lo + (a - first) as usize;
        let hi = lo + (b - a) as usize;
        let src = &self.interleaved[lo * channels..hi * channels];
        self.downmix.apply(src, out);
        self.produced += (hi - lo) as u64;
    }

    fn ts_to_frames(&self, ts: i64) -> i64 {
        match self.time_base {
            Some(tb) => {
                let v = i128::from(ts) * i128::from(tb.numer.get()) * i128::from(self.rate)
                    / i128::from(tb.denom.get());
                i64::try_from(v).unwrap_or(0)
            }
            None => ts,
        }
    }
}

enum Bad {
    /// This packet is unusable; decoding can continue with the next.
    Packet(String),
    Fatal(String),
}

impl Decoder {
    fn reset(&mut self) {
        match self {
            Decoder::Symphonia(d) => d.reset(),
            Decoder::Opus(o) => {
                let _ = o.inner.reset_state();
            }
        }
    }
}

impl OpusDecoder {
    fn new(params: &AudioCodecParameters) -> Result<Self, String> {
        let head = params.extra_data.as_deref().ok_or("missing Opus header")?;
        // OpusHead: magic(8) version(1) channels(1) pre_skip(2) rate(4) gain(2) mapping(1).
        if head.len() < 19 || &head[..8] != b"OpusHead" {
            return Err("invalid Opus header".into());
        }
        let channels = usize::from(head[9]);
        let gain_q8 = i16::from_le_bytes([head[16], head[17]]);
        let mapping = head[18];
        let layout = match (channels, mapping) {
            (1, _) => opus::Channels::Mono,
            (2, _) => opus::Channels::Stereo,
            _ => return Err(format!("{channels}-channel Opus is not supported")),
        };
        let inner =
            opus::Decoder::new(48_000, layout).map_err(|e| format!("cannot start Opus: {e}"))?;
        let gain = 10f32.powf(f32::from(gain_q8) / (20.0 * 256.0));
        Ok(OpusDecoder { inner, channels, gain })
    }
}

/// Mixes any channel layout down to stereo.
struct Downmix {
    channels: usize,
    layout: Option<Channels>,
    /// Per input channel: gains into left and right.
    weights: Vec<[f32; 2]>,
}

impl Downmix {
    fn new(layout: Option<&Channels>) -> Self {
        let mut d = Downmix { channels: 0, layout: None, weights: Vec::new() };
        if let Some(layout) = layout {
            d.update(layout);
        }
        d
    }

    fn update(&mut self, layout: &Channels) {
        if self.layout.as_ref() == Some(layout) {
            return;
        }
        self.layout = Some(layout.clone());
        self.channels = layout.count();
        self.weights = stereo_weights(layout);
    }

    fn apply(&self, src: &[f32], out: &mut Vec<f32>) {
        match self.channels {
            2 => out.extend_from_slice(src),
            1 => {
                for &s in src {
                    out.extend_from_slice(&[s, s]);
                }
            }
            n if n > 0 => {
                for frame in src.chunks_exact(n) {
                    let (mut l, mut r) = (0.0, 0.0);
                    for (s, w) in frame.iter().zip(&self.weights) {
                        l += s * w[0];
                        r += s * w[1];
                    }
                    out.extend_from_slice(&[l, r]);
                }
            }
            _ => {}
        }
    }
}

fn stereo_weights(layout: &Channels) -> Vec<[f32; 2]> {
    const C: f32 = std::f32::consts::FRAC_1_SQRT_2;
    let count = layout.count();
    let mut weights: Vec<[f32; 2]> = match layout {
        Channels::Positioned(positions) if count > 2 => positions
            .iter()
            .map(|p| {
                let left = Position::FRONT_LEFT
                    | Position::FRONT_LEFT_CENTER
                    | Position::FRONT_LEFT_WIDE
                    | Position::TOP_FRONT_LEFT
                    | Position::BOTTOM_FRONT_LEFT;
                let right = Position::FRONT_RIGHT
                    | Position::FRONT_RIGHT_CENTER
                    | Position::FRONT_RIGHT_WIDE
                    | Position::TOP_FRONT_RIGHT
                    | Position::BOTTOM_FRONT_RIGHT;
                let rear_left = Position::REAR_LEFT
                    | Position::SIDE_LEFT
                    | Position::TOP_REAR_LEFT
                    | Position::TOP_SIDE_LEFT;
                let rear_right = Position::REAR_RIGHT
                    | Position::SIDE_RIGHT
                    | Position::TOP_REAR_RIGHT
                    | Position::TOP_SIDE_RIGHT;
                if left.contains(p) {
                    [1.0, 0.0]
                } else if right.contains(p) {
                    [0.0, 1.0]
                } else if rear_left.contains(p) {
                    [C, 0.0]
                } else if rear_right.contains(p) {
                    [0.0, C]
                } else if p.intersects(Position::LFE1 | Position::LFE2) {
                    [0.0, 0.0]
                } else {
                    [C, C]
                }
            })
            .collect(),
        _ => (0..count)
            .map(|i| match (count, i) {
                (1, _) => [1.0, 1.0],
                (_, 0) => [1.0, 0.0],
                (_, 1) => [0.0, 1.0],
                _ => [0.5, 0.5],
            })
            .collect(),
    };
    if count > 2 {
        // Scale so a full-scale signal on every channel cannot clip.
        let sum = |side: usize| weights.iter().map(|w| w[side]).sum::<f32>();
        let peak = sum(0).max(sum(1));
        if peak > 1.0 {
            weights.iter_mut().for_each(|w| *w = [w[0] / peak, w[1] / peak]);
        }
    }
    weights
}

fn frames_to_duration(frames: u64, rate: u32) -> Duration {
    let rate = u64::from(rate.max(1));
    Duration::from_secs(frames / rate) + Duration::from_nanos((frames % rate) * 1_000_000_000 / rate)
}

fn duration_to_frames(d: Duration, rate: u32) -> u64 {
    u64::try_from(d.as_nanos() * u128::from(rate) / 1_000_000_000).unwrap_or(u64::MAX)
}
