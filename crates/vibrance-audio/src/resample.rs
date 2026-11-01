//! Sample rate conversion to the fixed output rate (decode thread only).
//!
//! The resampler's startup delay is trimmed and its tail is flushed exactly, so a track of `n`
//! input frames always yields `round(n * out / in)` output frames. That keeps track boundaries
//! sample-accurate in the output timeline. Consecutive tracks at the same rate are fed through
//! one resampler without a reset, so a gapless join between them stays continuous.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};

const CHUNK: usize = 1024;
const CHANNELS: usize = 2;

pub(crate) struct Resample {
    inner: Fft<f32>,
    in_rate: u32,
    out_rate: u32,
    /// Interleaved input waiting for a full chunk.
    pending: Vec<f32>,
    scratch: Vec<f32>,
    /// Output frames still to drop to compensate for the resampler's delay.
    delay: usize,
    fed: u64,
    emitted: u64,
}

impl Resample {
    pub fn new(in_rate: u32, out_rate: u32) -> Result<Self, String> {
        let inner = Fft::<f32>::new(
            in_rate as usize,
            out_rate as usize,
            CHUNK,
            CHANNELS,
            FixedSync::Input,
        )
        .map_err(|e| format!("cannot resample {in_rate} Hz: {e}"))?;
        let scratch = vec![0.0; inner.output_frames_max() * CHANNELS];
        let delay = inner.output_delay();
        Ok(Resample {
            inner,
            in_rate,
            out_rate,
            pending: Vec::with_capacity(CHUNK * CHANNELS * 4),
            scratch,
            delay,
            fed: 0,
            emitted: 0,
        })
    }

    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    /// Output frames that correspond to all the input fed so far.
    pub fn expected(&self) -> u64 {
        let (fed, i, o) = (u128::from(self.fed), u128::from(self.in_rate), u128::from(self.out_rate));
        u64::try_from((fed * o + i / 2) / i).unwrap_or(u64::MAX)
    }

    /// Feeds interleaved stereo input and appends whatever output is ready to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<(), String> {
        self.pending.extend_from_slice(input);
        self.fed += (input.len() / CHANNELS) as u64;
        while self.pending.len() / CHANNELS >= self.inner.input_frames_next() {
            self.process(None, out)?;
        }
        Ok(())
    }

    /// Flushes the remaining input and the resampler's tail, then resets for a new stream.
    pub fn finish(&mut self, out: &mut Vec<f32>) -> Result<(), String> {
        let target = self.expected();
        let mut guard = 0;
        while self.emitted < target {
            let partial = (self.pending.len() / CHANNELS).min(self.inner.input_frames_next());
            self.process(Some((partial, target)), out)?;
            guard += 1;
            if guard > 64 {
                break;
            }
        }
        self.reset();
        Ok(())
    }

    /// Drops all state so the next input starts a fresh stream.
    pub fn reset(&mut self) {
        self.inner.reset();
        self.pending.clear();
        self.delay = self.inner.output_delay();
        self.fed = 0;
        self.emitted = 0;
    }

    fn process(&mut self, partial: Option<(usize, u64)>, out: &mut Vec<f32>) -> Result<(), String> {
        let in_frames = self.pending.len() / CHANNELS;
        let input = InterleavedSlice::new(&self.pending[..], CHANNELS, in_frames)
            .map_err(|e| e.to_string())?;
        let out_frames = self.scratch.len() / CHANNELS;
        let mut output = InterleavedSlice::new_mut(&mut self.scratch[..], CHANNELS, out_frames)
            .map_err(|e| e.to_string())?;
        let indexing = Indexing {
            partial_len: partial.map(|(n, _)| n),
            ..Default::default()
        };
        let (used, produced) = self
            .inner
            .process_into_buffer(&input, &mut output, Some(&indexing))
            .map_err(|e| e.to_string())?;
        let used = match partial {
            Some((n, _)) => n,
            None => used,
        };
        self.pending.drain(..used.min(in_frames) * CHANNELS);

        let skip = self.delay.min(produced);
        self.delay -= skip;
        let mut keep = produced - skip;
        if let Some((_, target)) = partial {
            keep = keep.min(usize::try_from(target - self.emitted).unwrap_or(usize::MAX));
        }
        out.extend_from_slice(&self.scratch[skip * CHANNELS..(skip + keep) * CHANNELS]);
        self.emitted += keep as u64;
        Ok(())
    }
}
