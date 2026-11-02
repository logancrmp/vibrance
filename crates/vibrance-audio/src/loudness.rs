//! EBU R128 loudness of a whole file, for volume normalization of untagged tracks.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use ebur128::{Channel, EbuR128, Mode};
use symphonia::core::audio::{Channels, Position};

use crate::decode::Source;
use crate::{Error, Loudness};

pub(crate) fn measure(path: &Path) -> Result<Loudness, Error> {
    // Report a missing or unreadable file as an I/O error rather than a decode error.
    std::fs::File::open(path)?;
    catch_unwind(AssertUnwindSafe(|| measure_decoded(path)))
        .unwrap_or_else(|_| Err(Error::Decode("decoder crashed".into())))
}

fn measure_decoded(path: &Path) -> Result<Loudness, Error> {
    let mut source = Source::open(path).map_err(Error::Decode)?;
    let rate = source.rate();
    let mut meter: Option<EbuR128> = None;
    while let Some(native) = source.read_native().map_err(Error::Decode)? {
        let layout = native.layout.cloned().unwrap_or(Channels::Discrete(1));
        let samples = native.samples;
        let meter = match &mut meter {
            Some(m) if m.channels() as usize == layout.count().max(1) => m,
            Some(_) => return Err(Error::Decode("channel count changed mid-file".into())),
            None => meter.insert(new_meter(&layout, rate)?),
        };
        meter.add_frames_f32(samples).map_err(|e| Error::Decode(format!("loudness: {e}")))?;
    }
    let Some(meter) = meter else {
        return Ok(Loudness { integrated_lufs: f32::NEG_INFINITY, true_peak: 0.0 });
    };
    let lufs = meter.loudness_global().map_err(|e| Error::Decode(format!("loudness: {e}")))?;
    let peak = (0..meter.channels())
        .filter_map(|c| meter.true_peak(c).ok())
        .fold(0.0f64, f64::max);
    Ok(Loudness {
        integrated_lufs: if lufs.is_nan() { f32::NEG_INFINITY } else { lufs as f32 },
        true_peak: peak as f32,
    })
}

fn new_meter(layout: &Channels, rate: u32) -> Result<EbuR128, Error> {
    let count = layout.count().max(1) as u32;
    let mut meter = EbuR128::new(count, rate, Mode::I | Mode::TRUE_PEAK)
        .map_err(|e| Error::Decode(format!("cannot measure {count} channels at {rate} Hz: {e}")))?;
    if let Channels::Positioned(positions) = layout
        && count > 2
    {
        let map: Vec<Channel> = positions.iter().map(itu_channel).collect();
        meter.set_channel_map(&map).map_err(|e| Error::Decode(format!("loudness: {e}")))?;
    }
    Ok(meter)
}

/// BS.1770 weighting position of a speaker; LFE is left out of the measurement.
fn itu_channel(position: Position) -> Channel {
    match position {
        Position::FRONT_LEFT => Channel::Left,
        Position::FRONT_RIGHT => Channel::Right,
        Position::FRONT_CENTER => Channel::Center,
        Position::REAR_LEFT | Position::SIDE_LEFT => Channel::LeftSurround,
        Position::REAR_RIGHT | Position::SIDE_RIGHT => Channel::RightSurround,
        Position::FRONT_LEFT_CENTER => Channel::MpSC,
        Position::FRONT_RIGHT_CENTER => Channel::MmSC,
        Position::FRONT_LEFT_WIDE => Channel::Mp060,
        Position::FRONT_RIGHT_WIDE => Channel::Mm060,
        Position::REAR_CENTER => Channel::Mp180,
        Position::TOP_CENTER => Channel::Tp000,
        Position::TOP_FRONT_LEFT => Channel::Up030,
        Position::TOP_FRONT_CENTER => Channel::Up000,
        Position::TOP_FRONT_RIGHT => Channel::Um030,
        Position::TOP_SIDE_LEFT => Channel::Up090,
        Position::TOP_SIDE_RIGHT => Channel::Um090,
        Position::TOP_REAR_LEFT => Channel::Up135,
        Position::TOP_REAR_CENTER => Channel::Up180,
        Position::TOP_REAR_RIGHT => Channel::Um135,
        Position::BOTTOM_FRONT_CENTER => Channel::Bp000,
        Position::BOTTOM_FRONT_LEFT => Channel::Bp045,
        Position::BOTTOM_FRONT_RIGHT => Channel::Bm045,
        _ => Channel::Unused,
    }
}
