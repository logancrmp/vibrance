//! Pure conversions from `PlayerState` to MPRIS values: track object paths, `file://` URLs,
//! the `Metadata` map, and the set of properties that differ between two states.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Duration;

use mpris_server::{Metadata, PlaybackStatus, Property, Time};

use crate::{LoopStatus, PlayerState, Status, TrackId, TrackInfo};

const TRACK_PATH_PREFIX: &str = "/org/vibrance/track/";

/// D-Bus object path naming a track for `mpris:trackid`, e.g. `/org/vibrance/track/42`.
///
/// Object path elements allow only `[A-Za-z0-9_]`, so a negative id is written as `n<abs>`.
pub fn track_object_path(id: TrackId) -> String {
    if id < 0 {
        format!("{TRACK_PATH_PREFIX}n{}", id.unsigned_abs())
    } else {
        format!("{TRACK_PATH_PREFIX}{id}")
    }
}

/// `file://` URL for `path`, made absolute against the working directory if it is relative.
///
/// Every byte except RFC 3986 unreserved characters and `/` is percent-encoded, so spaces,
/// `#`, `?`, `%`, unicode and even non-UTF-8 file names round-trip.
pub fn file_url(path: &Path) -> String {
    let path: Cow<'_, Path> = if path.is_absolute() {
        Cow::Borrowed(path)
    } else {
        std::path::absolute(path).map_or(Cow::Borrowed(path), Cow::Owned)
    };
    let bytes = path.as_os_str().as_bytes();
    let mut url = String::with_capacity(7 + bytes.len());
    url.push_str("file://");
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'.' | b'_' | b'~') {
            url.push(char::from(b));
        } else {
            let _ = write!(url, "%{b:02X}");
        }
    }
    url
}

/// Microseconds as MPRIS `Time`, saturating at `i64::MAX`.
pub fn to_time(d: Duration) -> Time {
    Time::from_micros(i64::try_from(d.as_micros()).unwrap_or(i64::MAX))
}

/// The MPRIS `Metadata` map. Empty when there is no current track (MPRIS spec).
pub fn build_metadata(track: Option<&TrackInfo>) -> Metadata {
    let Some(t) = track else {
        return Metadata::new();
    };
    let mut m = Metadata::new();
    let trackid = mpris_server::TrackId::try_from(track_object_path(t.id))
        .expect("track object paths are always valid");
    m.set_trackid(Some(trackid));
    m.set_length(t.duration.map(to_time));
    let title = if t.title.is_empty() {
        // Panels show nothing at all without a title; the file name is better than that.
        t.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    } else {
        t.title.clone()
    };
    m.set_title(non_empty(&title));
    m.set_artist(non_empty(&t.artist).map(|a| [a]));
    m.set_album(non_empty(&t.album));
    m.set_album_artist(non_empty(&t.album_artist).map(|a| [a]));
    m.set_track_number(t.track_no.map(|n| i32::try_from(n).unwrap_or(i32::MAX)));
    m.set_art_url(t.art.as_deref().map(file_url));
    m.set_url(Some(file_url(&t.path)));
    m
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_owned())
}

pub fn playback_status(s: Status) -> PlaybackStatus {
    match s {
        Status::Playing => PlaybackStatus::Playing,
        Status::Paused => PlaybackStatus::Paused,
        Status::Stopped => PlaybackStatus::Stopped,
    }
}

pub fn to_mpris_loop(l: LoopStatus) -> mpris_server::LoopStatus {
    match l {
        LoopStatus::None => mpris_server::LoopStatus::None,
        LoopStatus::Track => mpris_server::LoopStatus::Track,
        LoopStatus::Playlist => mpris_server::LoopStatus::Playlist,
    }
}

pub fn from_mpris_loop(l: mpris_server::LoopStatus) -> LoopStatus {
    match l {
        mpris_server::LoopStatus::None => LoopStatus::None,
        mpris_server::LoopStatus::Track => LoopStatus::Track,
        mpris_server::LoopStatus::Playlist => LoopStatus::Playlist,
    }
}

/// `CanPlay`: a current track, or something to start (the UI starts the queue on `Play`).
pub fn can_play(s: &PlayerState) -> bool {
    s.track.is_some() || s.can_next
}

/// `CanPause` and `CanSeek`: there is a current track.
pub fn has_track(s: &PlayerState) -> bool {
    s.track.is_some()
}

/// The `PropertiesChanged` entries needed to go from `old` to `new`.
pub fn changed_properties(old: &PlayerState, new: &PlayerState) -> Vec<Property> {
    let mut out = Vec::new();
    if old.status != new.status {
        out.push(Property::PlaybackStatus(playback_status(new.status)));
    }
    if old.track != new.track {
        out.push(Property::Metadata(build_metadata(new.track.as_ref())));
    }
    if old.volume.to_bits() != new.volume.to_bits() {
        out.push(Property::Volume(new.volume));
    }
    if old.shuffle != new.shuffle {
        out.push(Property::Shuffle(new.shuffle));
    }
    if old.loop_status != new.loop_status {
        out.push(Property::LoopStatus(to_mpris_loop(new.loop_status)));
    }
    if old.can_next != new.can_next {
        out.push(Property::CanGoNext(new.can_next));
    }
    if old.can_previous != new.can_previous {
        out.push(Property::CanGoPrevious(new.can_previous));
    }
    if can_play(old) != can_play(new) {
        out.push(Property::CanPlay(can_play(new)));
    }
    if has_track(old) != has_track(new) {
        out.push(Property::CanPause(has_track(new)));
        out.push(Property::CanSeek(has_track(new)));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::PathBuf;

    use super::*;

    fn track() -> TrackInfo {
        TrackInfo {
            id: 42,
            title: "Swollen Moon".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            album_artist: "Album Artist".into(),
            track_no: Some(7),
            duration: Some(Duration::from_millis(215_250)),
            path: PathBuf::from("/music/Björk/Homogenic/07 Jóga #1 (100%).flac"),
            art: Some(PathBuf::from("/music/Björk/Homogenic/cover art.jpg")),
        }
    }

    #[test]
    fn object_paths_are_valid_dbus_paths() {
        assert_eq!(track_object_path(0), "/org/vibrance/track/0");
        assert_eq!(track_object_path(42), "/org/vibrance/track/42");
        assert_eq!(track_object_path(-5), "/org/vibrance/track/n5");
        for id in [0, 1, 42, i64::MAX, -1, i64::MIN] {
            let p = track_object_path(id);
            assert!(mpris_server::TrackId::try_from(p.as_str()).is_ok(), "{p}");
        }
        assert_ne!(track_object_path(5), track_object_path(-5));
    }

    #[test]
    fn file_urls_percent_encode_odd_names() {
        assert_eq!(file_url(Path::new("/a/b c.flac")), "file:///a/b%20c.flac");
        assert_eq!(
            file_url(Path::new("/m/Jóga #1 (100%)?.mp3")),
            "file:///m/J%C3%B3ga%20%231%20%28100%25%29%3F.mp3"
        );
        assert_eq!(file_url(Path::new("/x/日本")), "file:///x/%E6%97%A5%E6%9C%AC");
        assert_eq!(file_url(Path::new("/keep/A-z_0.9~")), "file:///keep/A-z_0.9~");
        // Non-UTF-8 bytes are encoded rather than lost.
        let raw = Path::new(OsStr::from_bytes(b"/bad/\xff\xfe.ogg"));
        assert_eq!(file_url(raw), "file:///bad/%FF%FE.ogg");
    }

    #[test]
    fn relative_paths_become_absolute_urls() {
        let url = file_url(Path::new("rel dir/x.mp3"));
        assert!(url.starts_with("file:///"), "{url}");
        assert!(url.ends_with("/rel%20dir/x.mp3"), "{url}");
    }

    #[test]
    fn metadata_has_all_fields() {
        let m = build_metadata(Some(&track()));
        assert_eq!(m.trackid().unwrap().as_str(), "/org/vibrance/track/42");
        assert_eq!(m.length(), Some(Time::from_micros(215_250_000)));
        assert_eq!(m.title(), Some("Swollen Moon"));
        assert_eq!(m.artist(), Some(vec!["Artist".to_owned()]));
        assert_eq!(m.album(), Some("Album"));
        assert_eq!(m.album_artist(), Some(vec!["Album Artist".to_owned()]));
        assert_eq!(m.track_number(), Some(7));
        assert_eq!(
            m.url().as_deref(),
            Some("file:///music/Bj%C3%B6rk/Homogenic/07%20J%C3%B3ga%20%231%20%28100%25%29.flac")
        );
        assert_eq!(m.art_url().as_deref(), Some("file:///music/Bj%C3%B6rk/Homogenic/cover%20art.jpg"));
    }

    #[test]
    fn length_is_in_microseconds() {
        assert_eq!(to_time(Duration::from_secs(3)).as_micros(), 3_000_000);
        assert_eq!(to_time(Duration::from_nanos(1_999)).as_micros(), 1);
        assert_eq!(to_time(Duration::MAX).as_micros(), i64::MAX);
    }

    #[test]
    fn metadata_omits_missing_fields() {
        let t = TrackInfo {
            id: 1,
            path: PathBuf::from("/m/untagged song.wav"),
            ..TrackInfo::default()
        };
        let m = build_metadata(Some(&t));
        assert_eq!(m.title(), Some("untagged song"));
        assert_eq!(m.length(), None);
        assert_eq!(m.artist(), None);
        assert_eq!(m.album(), None);
        assert_eq!(m.album_artist(), None);
        assert_eq!(m.track_number(), None);
        assert_eq!(m.art_url(), None);
        assert_eq!(m.url().as_deref(), Some("file:///m/untagged%20song.wav"));
    }

    #[test]
    fn no_track_means_empty_metadata() {
        assert_eq!(build_metadata(None), Metadata::new());
    }

    #[test]
    fn diff_reports_only_changes() {
        let a = PlayerState { volume: 0.5, ..PlayerState::default() };
        assert!(changed_properties(&a, &a.clone()).is_empty());

        let b = PlayerState { volume: 0.25, ..a.clone() };
        assert_eq!(changed_properties(&a, &b), vec![Property::Volume(0.25)]);

        let c = PlayerState { status: Status::Playing, track: Some(track()), ..a.clone() };
        let names: Vec<_> = changed_properties(&a, &c).iter().map(name).collect();
        assert_eq!(names, ["PlaybackStatus", "Metadata", "CanPlay", "CanPause", "CanSeek"]);

        let d = PlayerState {
            shuffle: true,
            loop_status: LoopStatus::Playlist,
            can_next: true,
            can_previous: true,
            ..c.clone()
        };
        assert_eq!(
            changed_properties(&c, &d),
            vec![
                Property::Shuffle(true),
                Property::LoopStatus(mpris_server::LoopStatus::Playlist),
                Property::CanGoNext(true),
                Property::CanGoPrevious(true),
            ]
        );

        // Same track, new title: only Metadata.
        let mut e = d.clone();
        e.track.as_mut().unwrap().title = "Other".into();
        assert_eq!(changed_properties(&d, &e).iter().map(name).collect::<Vec<_>>(), ["Metadata"]);
    }

    #[test]
    fn can_play_with_queue_but_no_track() {
        let s = PlayerState { can_next: true, ..PlayerState::default() };
        assert!(can_play(&s));
        assert!(!has_track(&s));
        assert!(!can_play(&PlayerState::default()));
    }

    fn name(p: &Property) -> &'static str {
        match p {
            Property::PlaybackStatus(_) => "PlaybackStatus",
            Property::Metadata(_) => "Metadata",
            Property::Volume(_) => "Volume",
            Property::CanPlay(_) => "CanPlay",
            Property::CanPause(_) => "CanPause",
            Property::CanSeek(_) => "CanSeek",
            other => panic!("unexpected {other:?}"),
        }
    }
}
