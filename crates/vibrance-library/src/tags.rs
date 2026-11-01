//! Tag and audio-property reading. Never fails: missing or unreadable tags get fallbacks.

use std::path::Path;

use lofty::config::{ParseOptions, ParsingMode};
use lofty::error::FileParseError;
use lofty::file::{AudioFile, FileType, TaggedFile, TaggedFileExt};
use lofty::id3::v1::GENRES;
use lofty::mp4::{Mp4Codec, Mp4File};
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag};

/// Artist shown for tracks without an artist tag.
pub const UNKNOWN_ARTIST: &str = "Unknown Artist";
/// Album shown for tracks without an album tag.
pub const UNKNOWN_ALBUM: &str = "Unknown Album";

/// File extensions the scanner picks up (compared case-insensitively).
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "ogg", "oga", "opus", "m4a", "mp4", "aac", "wav", "aif", "aiff",
];

/// Whether `path` has a supported audio extension.
pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            SUPPORTED_EXTENSIONS
                .iter()
                .any(|s| s.eq_ignore_ascii_case(ext))
        })
}

/// Everything read from a file, with fallbacks already applied.
#[derive(Debug)]
pub(crate) struct TrackInfo {
    pub title: String,
    pub artist: String,
    pub album_artist: String,
    pub album: String,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub duration_ms: u64,
    pub format: String,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
}

pub(crate) fn read(path: &Path) -> TrackInfo {
    // lofty is fed arbitrary files; a parser panic must not take a scan worker down.
    let parsed = std::panic::catch_unwind(|| {
        read_file(path, ParsingMode::BestAttempt).or_else(|_| read_file(path, ParsingMode::Relaxed))
    });
    match parsed {
        Ok(Ok((file, codec))) => from_file(path, &file, codec),
        Ok(Err(e)) => {
            log::debug!("unreadable tags in {}: {e}", path.display());
            fallback(path)
        }
        Err(_) => {
            log::warn!("tag reader panicked on {}", path.display());
            fallback(path)
        }
    }
}

fn read_file(
    path: &Path,
    mode: ParsingMode,
) -> Result<(TaggedFile, Option<Mp4Codec>), FileParseError> {
    let options = ParseOptions::new().read_cover_art(false).parsing_mode(mode);
    let probe = Probe::open(path)?.options(options).guess_file_type()?;
    if probe.file_type() == Some(FileType::Mp4) {
        // The generic properties don't say whether an MP4 holds AAC or ALAC.
        let mut reader = probe.into_inner();
        let file = Mp4File::read_from(&mut reader, options)?;
        let codec = file.properties().codec();
        return Ok((file.into(), codec));
    }
    Ok((probe.read()?, None))
}

fn from_file(path: &Path, file: &TaggedFile, codec: Option<Mp4Codec>) -> TrackInfo {
    // Primary tag first (e.g. ID3v2 over ID3v1), then any others as fallbacks.
    let primary = file.primary_tag_type();
    let mut tags: Vec<&Tag> = file.tags().iter().collect();
    tags.sort_by_key(|t| t.tag_type() != primary);
    let text = |key: ItemKey| tags.iter().find_map(|t| t.get_string(key).and_then(clean));
    let number = |key: ItemKey| {
        tags.iter()
            .find_map(|t| t.get_string(key).and_then(parse_number))
    };

    let artist = text(ItemKey::TrackArtist).unwrap_or_else(|| UNKNOWN_ARTIST.to_owned());
    let props = file.properties();
    let nonzero = |v: Option<u32>| v.filter(|&v| v > 0);
    TrackInfo {
        title: text(ItemKey::TrackTitle).unwrap_or_else(|| file_stem(path)),
        album_artist: text(ItemKey::AlbumArtist).unwrap_or_else(|| artist.clone()),
        artist,
        album: text(ItemKey::AlbumTitle).unwrap_or_else(|| UNKNOWN_ALBUM.to_owned()),
        track_no: number(ItemKey::TrackNumber),
        disc_no: number(ItemKey::DiscNumber),
        year: [
            ItemKey::RecordingDate,
            ItemKey::Year,
            ItemKey::ReleaseDate,
            ItemKey::OriginalReleaseDate,
        ]
        .into_iter()
        .find_map(|key| {
            tags.iter()
                .find_map(|t| t.get_string(key).and_then(parse_year))
        }),
        genre: tags.iter().find_map(|t| {
            t.get_string(ItemKey::Genre)
                .and_then(clean)
                .and_then(genre_name)
        }),
        duration_ms: props.duration().as_millis() as u64,
        format: format_name(file.file_type(), codec).to_owned(),
        bitrate_kbps: nonzero(props.audio_bitrate()).or(nonzero(props.overall_bitrate())),
        sample_rate: nonzero(props.sample_rate()),
    }
}

fn fallback(path: &Path) -> TrackInfo {
    TrackInfo {
        title: file_stem(path),
        artist: UNKNOWN_ARTIST.to_owned(),
        album_artist: UNKNOWN_ARTIST.to_owned(),
        album: UNKNOWN_ALBUM.to_owned(),
        track_no: None,
        disc_no: None,
        year: None,
        genre: None,
        duration_ms: 0,
        format: format_from_extension(path).to_owned(),
        bitrate_kbps: None,
        sample_rate: None,
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn format_name(file_type: FileType, codec: Option<Mp4Codec>) -> &'static str {
    match file_type {
        FileType::Mpeg => "MP3",
        FileType::Flac => "FLAC",
        FileType::Vorbis => "Vorbis",
        FileType::Opus => "Opus",
        FileType::Speex => "Speex",
        FileType::Aac => "AAC",
        FileType::Wav => "WAV",
        FileType::Aiff => "AIFF",
        FileType::Ape => "APE",
        FileType::Mpc => "MPC",
        FileType::WavPack => "WavPack",
        FileType::Mp4 => match codec {
            Some(Mp4Codec::AAC) => "AAC",
            Some(Mp4Codec::ALAC) => "ALAC",
            Some(Mp4Codec::MP3) => "MP3",
            Some(Mp4Codec::FLAC) => "FLAC",
            _ => "MP4",
        },
        _ => "Unknown",
    }
}

fn format_from_extension(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default();
    match ext.to_ascii_lowercase().as_str() {
        "mp3" => "MP3",
        "flac" => "FLAC",
        "ogg" | "oga" => "Ogg",
        "opus" => "Opus",
        "m4a" | "mp4" => "MP4",
        "aac" => "AAC",
        "wav" => "WAV",
        "aif" | "aiff" => "AIFF",
        _ => "Unknown",
    }
}

/// Trims whitespace and stray NULs; joins NUL-separated multi-values with "; ".
fn clean(s: &str) -> Option<String> {
    let trim = |s: &str| {
        s.trim_matches(|c: char| c.is_whitespace() || c == '\0')
            .to_owned()
    };
    let s = trim(s);
    let s = if s.contains('\0') {
        s.split('\0')
            .map(trim)
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("; ")
    } else {
        s
    };
    (!s.is_empty()).then_some(s)
}

/// Resolves ID3v1-style numeric genres ("17" or "(17)"); unknown numbers (e.g. 255) are dropped.
fn genre_name(genre: String) -> Option<String> {
    let number = genre
        .strip_prefix('(')
        .and_then(|g| g.strip_suffix(')'))
        .unwrap_or(&genre);
    if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) {
        return number
            .parse::<usize>()
            .ok()
            .and_then(|i| GENRES.get(i))
            .map(|g| (*g).to_owned());
    }
    Some(genre)
}

/// Leading number of "3", "03", "3/12" or " 3 of 12". Zero counts as missing.
fn parse_number(s: &str) -> Option<u32> {
    let s = s.trim_start();
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    s[..end].parse().ok().filter(|&n| n > 0)
}

/// Year from "2021", "2021-04-02", "2021-04-02T10:00:00" or "02.04.2021".
fn parse_year(s: &str) -> Option<i32> {
    s.split(|c: char| !c.is_ascii_digit())
        .find(|run| run.len() >= 4)
        .and_then(|run| run[..4].parse().ok())
        .filter(|&y| y > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(parse_number("3"), Some(3));
        assert_eq!(parse_number(" 03/12"), Some(3));
        assert_eq!(parse_number("12 of 14"), Some(12));
        assert_eq!(parse_number("0"), None);
        assert_eq!(parse_number("A1"), None);
        assert_eq!(parse_number(""), None);
    }

    #[test]
    fn years() {
        assert_eq!(parse_year("2021"), Some(2021));
        assert_eq!(parse_year("2021-04-02"), Some(2021));
        assert_eq!(parse_year("2021-04-02T10:00:00"), Some(2021));
        assert_eq!(parse_year("02.04.2021"), Some(2021));
        assert_eq!(parse_year("20210402"), Some(2021));
        assert_eq!(parse_year("'99"), None);
        assert_eq!(parse_year(""), None);
    }

    #[test]
    fn genres() {
        assert_eq!(genre_name("Rock".into()).as_deref(), Some("Rock"));
        assert_eq!(genre_name("17".into()).as_deref(), Some("Rock"));
        assert_eq!(genre_name("(0)".into()).as_deref(), Some("Blues"));
        assert_eq!(genre_name("255".into()), None);
        assert_eq!(genre_name("90s Punk".into()).as_deref(), Some("90s Punk"));
    }

    #[test]
    fn text_cleanup() {
        assert_eq!(clean("  Title \0").as_deref(), Some("Title"));
        assert_eq!(clean("A\0B\0").as_deref(), Some("A; B"));
        assert_eq!(clean(" \t "), None);
    }

    #[test]
    fn extensions() {
        assert!(is_supported(Path::new("/x/a.MP3")));
        assert!(is_supported(Path::new("/x/a.Flac")));
        assert!(is_supported(Path::new("a.aif")));
        assert!(!is_supported(Path::new("/x/a.txt")));
        assert!(!is_supported(Path::new("/x/mp3")));
    }
}
