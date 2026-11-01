//! Encoder delay and padding for MP4/M4A files.
//!
//! Symphonia's MP4 reader does not apply edit lists or iTunes gapless tags, so AAC tracks would
//! otherwise start with ~1024-2112 frames of priming and end with padding. This reads just enough
//! of the `moov` box to recover them: the `iTunSMPB` tag when present, else the audio track's
//! edit list.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Frames to drop at the start, and the number of valid frames after that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Gapless {
    pub delay: u64,
    pub valid: Option<u64>,
}

/// Largest `moov` box we are willing to read into memory.
const MAX_MOOV: u64 = 64 << 20;

pub(crate) fn read_gapless(path: &Path, sample_rate: u32) -> Option<Gapless> {
    let moov = read_moov(path)?;
    if let Some(g) = itunsmpb(&moov) {
        return Some(g);
    }
    edit_list(&moov, sample_rate)
}

fn read_moov(path: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut pos = 0u64;
    while pos + 8 <= len {
        file.seek(SeekFrom::Start(pos)).ok()?;
        let mut header = [0u8; 16];
        file.read_exact(&mut header[..8]).ok()?;
        let mut size = u64::from(u32::from_be_bytes(header[..4].try_into().ok()?));
        let kind: [u8; 4] = header[4..8].try_into().ok()?;
        let mut header_len = 8;
        if size == 1 {
            file.read_exact(&mut header[8..16]).ok()?;
            size = u64::from_be_bytes(header[8..16].try_into().ok()?);
            header_len = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < header_len {
            return None;
        }
        if &kind == b"moov" {
            let body = size - header_len;
            if body > MAX_MOOV {
                return None;
            }
            let mut buf = vec![0u8; usize::try_from(body).ok()?];
            file.read_exact(&mut buf).ok()?;
            return Some(buf);
        }
        pos = pos.checked_add(size)?;
    }
    None
}

/// Iterates over the boxes directly inside `data`, yielding (type, body).
fn boxes(data: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut rest = data;
    std::iter::from_fn(move || {
        if rest.len() < 8 {
            return None;
        }
        let size = u32::from_be_bytes(rest[..4].try_into().ok()?) as usize;
        let kind = &rest[4..8];
        let (header, size) = match size {
            0 => (8, rest.len()),
            1 => {
                let big = u64::from_be_bytes(rest.get(8..16)?.try_into().ok()?);
                (16, usize::try_from(big).ok()?)
            }
            n => (8, n),
        };
        if size < header || size > rest.len() {
            return None;
        }
        let body = &rest[header..size];
        rest = &rest[size..];
        Some((kind, body))
    })
}

fn child<'a>(data: &'a [u8], kind: &[u8]) -> Option<&'a [u8]> {
    boxes(data).find(|(k, _)| *k == kind).map(|(_, body)| body)
}

fn be_u32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn be_u64(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

/// Timescale from an `mvhd` or `mdhd` body.
fn timescale(body: &[u8]) -> Option<u32> {
    match body.first()? {
        0 => be_u32(body, 12),
        1 => be_u32(body, 20),
        _ => None,
    }
}

/// Duration from an `mvhd` or `mdhd` body, in its own timescale.
fn duration(body: &[u8]) -> Option<u64> {
    match body.first()? {
        0 => be_u32(body, 16).map(u64::from),
        1 => be_u64(body, 24),
        _ => None,
    }
}

fn itunsmpb(moov: &[u8]) -> Option<Gapless> {
    let meta = child(child(moov, b"udta")?, b"meta")?;
    // `meta` is a full box: skip version and flags.
    let ilst = child(meta.get(4..)?, b"ilst")?;
    for (kind, item) in boxes(ilst) {
        if kind != b"----" {
            continue;
        }
        let name = child(item, b"name")?;
        if name.get(4..) != Some(b"iTunSMPB".as_slice()) {
            continue;
        }
        let data = child(item, b"data")?;
        let text = std::str::from_utf8(data.get(8..)?).ok()?;
        let fields: Vec<u64> = text
            .split_whitespace()
            .map_while(|f| u64::from_str_radix(f, 16).ok())
            .collect();
        let delay = *fields.get(1)?;
        let valid = fields.get(3).copied().filter(|&n| n > 0);
        return Some(Gapless { delay, valid });
    }
    None
}

fn edit_list(moov: &[u8], sample_rate: u32) -> Option<Gapless> {
    let movie_scale = timescale(child(moov, b"mvhd")?)?;
    for (kind, trak) in boxes(moov) {
        if kind != b"trak" {
            continue;
        }
        let mdia = child(trak, b"mdia")?;
        let handler = child(mdia, b"hdlr")?;
        if handler.get(8..12) != Some(b"soun".as_slice()) {
            continue;
        }
        let mdhd = child(mdia, b"mdhd")?;
        let media_scale = timescale(mdhd)?;
        let media_duration = duration(mdhd);
        let elst = child(child(trak, b"edts")?, b"elst")?;
        let version = *elst.first()?;
        let count = be_u32(elst, 4)? as usize;
        let entry_len = if version == 1 { 20 } else { 12 };
        for i in 0..count {
            let at = 8 + i * entry_len;
            let (duration, media_time) = if version == 1 {
                (be_u64(elst, at)?, be_u64(elst, at + 8)? as i64)
            } else {
                (u64::from(be_u32(elst, at)?), i64::from(be_u32(elst, at + 4)? as i32))
            };
            if media_time < 0 {
                // An empty edit (presentation offset); not part of the audio.
                continue;
            }
            let rate = u128::from(sample_rate);
            let media_scale = u128::from(media_scale.max(1));
            let movie_scale = u128::from(movie_scale.max(1));
            let delay = media_time as u128 * rate / media_scale;
            let mut valid = u128::from(duration) * rate / movie_scale;
            // The edit is in movie ticks, often milliseconds. When the media itself ends within
            // a tick of it, its length in samples is the exact answer.
            if let Some(total) = media_duration {
                let exact = (u128::from(total) * rate / media_scale).saturating_sub(delay);
                if exact.abs_diff(valid) <= rate / movie_scale + 1 {
                    valid = exact;
                }
            }
            return Some(Gapless {
                delay: u64::try_from(delay).ok()?,
                valid: u64::try_from(valid).ok().filter(|&v| v > 0),
            });
        }
        return None;
    }
    None
}
