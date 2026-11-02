//! Output sinks: the cpal device stream, or a thread that renders to a WAV file or nowhere.

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::Error;
use crate::player::OutputControl;
use crate::render::Core;
use crate::shared::{GainUpdate, Marker, Notice, Shared};
use crate::tap::TapBuffer;
use crate::wav::WavWriter;

/// Decoded audio buffered ahead of the output.
const RING_SECONDS_DIV: u32 = 2;
const MARKER_CAPACITY: usize = 256;
const NOTICE_CAPACITY: usize = 256;
const GAIN_CAPACITY: usize = 64;
/// Output rate for the file and null sinks.
pub(crate) const FILE_RATE: u32 = 48_000;

/// The decode thread's ends of the rings.
pub(crate) struct DecodeSide {
    pub shared: Arc<Shared>,
    pub tap: Arc<TapBuffer>,
    pub samples: Producer<f32>,
    pub markers: Producer<Marker>,
    pub notices: Consumer<Notice>,
    pub gain_updates: Producer<GainUpdate>,
}

/// A running output and how to stop it.
pub(crate) struct Output {
    pub control: Sender<OutputControl>,
    pub thread: JoinHandle<()>,
}

fn pipeline(rate: u32, realtime: bool) -> (Core, DecodeSide) {
    let shared = Arc::new(Shared::new(rate, realtime));
    let tap = Arc::new(TapBuffer::new(shared.clone()));
    // Stereo frames, so the capacity in samples must stay even.
    let ring_frames = (rate / RING_SECONDS_DIV).max(4096) as usize;
    let (samples, sample_reader) = RingBuffer::new(ring_frames * 2);
    let (markers, marker_reader) = RingBuffer::new(MARKER_CAPACITY);
    let (notice_writer, notices) = RingBuffer::new(NOTICE_CAPACITY);
    let (gain_updates, gain_reader) = RingBuffer::new(GAIN_CAPACITY);
    let core = Core::new(
        sample_reader,
        marker_reader,
        notice_writer,
        gain_reader,
        shared.clone(),
        tap.clone(),
    );
    (core, DecodeSide { shared, tap, samples, markers, notices, gain_updates })
}

/// Starts rendering into a WAV file (or discarding, with no writer) on a thread of its own.
pub(crate) fn spawn_file(
    wav: Option<WavWriter>,
    realtime: bool,
) -> Result<(Output, DecodeSide), Error> {
    let (core, side) = pipeline(FILE_RATE, realtime);
    let shared = side.shared.clone();
    let (control, rx) = crossbeam_channel::unbounded();
    let thread = thread::Builder::new()
        .name("vibrance-output".into())
        .spawn(move || run_file(core, wav, realtime, &shared, &rx))?;
    Ok((Output { control, thread }, side))
}

fn run_file(
    mut core: Core,
    mut wav: Option<WavWriter>,
    realtime: bool,
    shared: &Shared,
    control: &Receiver<OutputControl>,
) {
    let rate = shared.rate;
    // Real-time blocks of 5 ms, like a device period; bigger ones when running flat out.
    let block = if realtime { (rate / 200) as usize } else { 4096 };
    let period = Duration::from_secs_f64(block as f64 / f64::from(rate));
    let mut buf = vec![0.0f32; block * 2];
    let mut next = Instant::now();
    loop {
        match control.try_recv() {
            Ok(OutputControl::Shutdown) | Err(TryRecvError::Disconnected) => break,
            _ => {}
        }
        let frames = if realtime {
            core.render(&mut buf, shared.now_ns());
            block
        } else {
            let n = core.render_ready(&mut buf);
            if n == 0 {
                // Nothing to play yet; check back shortly.
                match control.recv_timeout(Duration::from_millis(1)) {
                    Ok(OutputControl::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                    _ => continue,
                }
            }
            n
        };
        if let Some(w) = &mut wav
            && let Err(e) = w.write(&buf[..frames * 2])
        {
            log::error!("writing WAV output failed: {e}");
            wav = None;
        }
        if realtime {
            next += period;
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            } else if now - next > Duration::from_millis(200) {
                // Fell far behind (e.g. the machine was suspended): don't try to catch up.
                next = now;
            }
        }
    }
    if let Some(w) = wav
        && let Err(e) = w.finish()
    {
        log::error!("finishing WAV output failed: {e}");
    }
}

/// Opens the default output device and starts the stream on a thread that owns it.
pub(crate) fn spawn_device() -> Result<(Output, DecodeSide), Error> {
    let (init_tx, init_rx) = crossbeam_channel::bounded(1);
    let (control, rx) = crossbeam_channel::unbounded();
    let thread = thread::Builder::new()
        .name("vibrance-output".into())
        .spawn(move || run_device(&init_tx, &rx))?;
    match init_rx.recv() {
        Ok(Ok(side)) => Ok((Output { control, thread }, side)),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(Error::Output("output thread exited during setup".into()))
        }
    }
}

fn run_device(init: &Sender<Result<DecodeSide, Error>>, control: &Receiver<OutputControl>) {
    let (stream, side) = match open_device() {
        Ok(v) => v,
        Err(e) => {
            let _ = init.send(Err(e));
            return;
        }
    };
    if init.send(Ok(side)).is_err() {
        return;
    }
    let mut playing = true;
    while let Ok(msg) = control.recv() {
        let result = match msg {
            OutputControl::Shutdown => break,
            OutputControl::Suspend if playing => {
                playing = false;
                stream.pause()
            }
            OutputControl::Resume if !playing => {
                playing = true;
                stream.play()
            }
            _ => Ok(()),
        };
        if let Err(e) = result {
            log::warn!("audio output: {e}");
        }
    }
}

fn open_device() -> Result<(cpal::Stream, DecodeSide), Error> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or(Error::NoDevice)?;
    let config = choose_config(&device)?;
    let mut last_error = None;
    // A fixed small period keeps pause and seek latency low; fall back to the device default.
    let attempts = [config, cpal::StreamConfig { buffer_size: cpal::BufferSize::Default, ..config }];
    for config in attempts {
        let (core, side) = pipeline(config.sample_rate, true);
        match build_stream(&device, config, core, side.shared.clone()) {
            Ok(stream) => {
                stream.play().map_err(|e| Error::Output(e.to_string()))?;
                return Ok((stream, side));
            }
            Err(e) => last_error = Some(e),
        }
        if config.buffer_size == cpal::BufferSize::Default {
            break;
        }
    }
    Err(Error::Output(last_error.map_or_else(|| "cannot open stream".into(), |e| e.to_string())))
}

fn choose_config(device: &cpal::Device) -> Result<cpal::StreamConfig, Error> {
    let default = device.default_output_config().map_err(|e| Error::Output(e.to_string()))?;
    let rate = default.sample_rate();
    let ranges = device.supported_output_configs().map_err(|e| Error::Output(e.to_string()))?;
    let mut best: Option<cpal::SupportedStreamConfigRange> = None;
    for range in ranges {
        let fits = range.sample_format() == cpal::SampleFormat::F32
            && range.min_sample_rate() <= rate
            && rate <= range.max_sample_rate();
        if fits && (best.is_none() || range.channels() == 2) {
            let stereo = range.channels() == 2;
            best = Some(range);
            if stereo {
                break;
            }
        }
    }
    let supported = best
        .ok_or_else(|| Error::Output("device has no 32-bit float output".into()))?
        .with_sample_rate(rate);
    let period = (rate * 5 / 1000).next_power_of_two();
    let buffer_size = match supported.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            cpal::BufferSize::Fixed(period.clamp(*min, (*max).max(*min)))
        }
        cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Fixed(period),
    };
    Ok(cpal::StreamConfig { channels: supported.channels(), sample_rate: rate, buffer_size })
}

fn build_stream(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut core: Core,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, cpal::Error> {
    let channels = usize::from(config.channels.max(1));
    // Pre-allocated, for devices that are not stereo.
    let mut scratch = vec![0.0f32; 2 * 1024];
    let data = move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
        let ts = info.timestamp();
        let latency = ts.playback.duration_since(ts.callback);
        let due = shared.now_ns().saturating_add(u64::try_from(latency.as_nanos()).unwrap_or(0));
        if channels == 2 {
            core.render(out, due);
            return;
        }
        let rate = u64::from(shared.rate.max(1));
        let per_chunk = scratch.len() / 2;
        for (i, chunk) in out.chunks_mut(per_chunk * channels).enumerate() {
            let frames = chunk.len() / channels;
            let stereo = &mut scratch[..frames * 2];
            let offset_ns = (i * per_chunk) as u64 * 1_000_000_000 / rate;
            core.render(stereo, due + offset_ns);
            for (dst, src) in chunk.chunks_exact_mut(channels).zip(stereo.as_chunks::<2>().0) {
                if channels == 1 {
                    dst[0] = (src[0] + src[1]) * 0.5;
                } else {
                    dst[0] = src[0];
                    dst[1] = src[1];
                    dst[2..].fill(0.0);
                }
            }
        }
    };
    let error = |e: cpal::Error| log::warn!("audio output: {e}");
    device.build_output_stream(config, data, error, None)
}
