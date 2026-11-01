//! Minimal WAV writer: 32-bit float stereo, header patched with the final length on finish.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

const CHANNELS: u16 = 2;
const BYTES_PER_SAMPLE: u16 = 4;
const FORMAT_IEEE_FLOAT: u16 = 3;
/// Offsets of the length fields patched by `finish`.
const RIFF_SIZE_AT: u64 = 4;
const FACT_FRAMES_AT: u64 = 46;
const DATA_SIZE_AT: u64 = 54;
const HEADER_LEN: u32 = 58;

pub(crate) struct WavWriter {
    out: BufWriter<File>,
    frames: u64,
    bytes: Vec<u8>,
}

impl WavWriter {
    pub fn create(path: &Path, rate: u32) -> io::Result<Self> {
        let mut out = BufWriter::with_capacity(1 << 16, File::create(path)?);
        let block_align = CHANNELS * BYTES_PER_SAMPLE;
        out.write_all(b"RIFF")?;
        out.write_all(&(HEADER_LEN - 8).to_le_bytes())?;
        out.write_all(b"WAVE")?;
        out.write_all(b"fmt ")?;
        out.write_all(&18u32.to_le_bytes())?;
        out.write_all(&FORMAT_IEEE_FLOAT.to_le_bytes())?;
        out.write_all(&CHANNELS.to_le_bytes())?;
        out.write_all(&rate.to_le_bytes())?;
        out.write_all(&(rate * u32::from(block_align)).to_le_bytes())?;
        out.write_all(&block_align.to_le_bytes())?;
        out.write_all(&(BYTES_PER_SAMPLE * 8).to_le_bytes())?;
        out.write_all(&0u16.to_le_bytes())?;
        out.write_all(b"fact")?;
        out.write_all(&4u32.to_le_bytes())?;
        out.write_all(&0u32.to_le_bytes())?;
        out.write_all(b"data")?;
        out.write_all(&0u32.to_le_bytes())?;
        Ok(WavWriter { out, frames: 0, bytes: Vec::new() })
    }

    /// Appends interleaved stereo samples.
    pub fn write(&mut self, samples: &[f32]) -> io::Result<()> {
        self.bytes.clear();
        self.bytes.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
        self.out.write_all(&self.bytes)?;
        self.frames += (samples.len() / usize::from(CHANNELS)) as u64;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<()> {
        let data = self.frames * u64::from(CHANNELS * BYTES_PER_SAMPLE);
        let clamp = |v: u64| u32::try_from(v).unwrap_or(u32::MAX);
        self.out.flush()?;
        let file = self.out.get_mut();
        file.seek(SeekFrom::Start(RIFF_SIZE_AT))?;
        file.write_all(&clamp(data + u64::from(HEADER_LEN) - 8).to_le_bytes())?;
        file.seek(SeekFrom::Start(FACT_FRAMES_AT))?;
        file.write_all(&clamp(self.frames).to_le_bytes())?;
        file.seek(SeekFrom::Start(DATA_SIZE_AT))?;
        file.write_all(&clamp(data).to_le_bytes())?;
        file.flush()
    }
}
