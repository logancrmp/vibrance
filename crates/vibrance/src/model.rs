//! In-memory library model: artists, albums, sort orders, filtering.

use std::cmp::Ordering;
use std::collections::HashMap;

use unicode_normalization::UnicodeNormalization;
use vibrance_library::{Track, TrackId};

use crate::columns::ColId;

/// Case- and accent-insensitive key for sorting and type-to-jump.
pub fn fold(s: &str) -> String {
    s.nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Natural order on folded keys: digit runs compare numerically ("Track 2" < "Track 10").
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut n = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        n.push(c);
                        it.next();
                    }
                    n
                };
                let (na, nb) = (take(&mut a), take(&mut b));
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                a.next();
                b.next();
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct Album {
    pub name: String,
    pub artist: String,
    pub year: Option<i32>,
    pub genre: Option<String>,
    /// Track indices in disc → number → title order.
    pub tracks: Vec<usize>,
    pub duration_ms: u64,
    pub key: AlbumKey,
    name_key: String,
    artist_key: String,
}

#[derive(Debug, Clone)]
pub struct Artist {
    pub name: String,
    pub albums: Vec<usize>,
    pub tracks: usize,
    pub duration_ms: u64,
    pub key: String,
}

/// Stable identity of an album across model rebuilds.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AlbumKey {
    pub artist: String,
    pub name: String,
}

#[derive(Debug, Default)]
pub struct Model {
    pub tracks: Vec<Track>,
    pub index: HashMap<TrackId, usize>,
    pub albums: Vec<Album>,
    pub artists: Vec<Artist>,
    /// Album index for each track.
    pub track_album: Vec<usize>,
    title_keys: Vec<String>,
    artist_keys: Vec<String>,
}

impl Model {
    pub fn build(mut tracks: Vec<Track>) -> Model {
        tracks.sort_by_key(|t| t.id);
        let index = tracks.iter().enumerate().map(|(i, t)| (t.id, i)).collect();
        let title_keys: Vec<String> = tracks.iter().map(|t| fold(&t.title)).collect();
        let artist_keys: Vec<String> = tracks.iter().map(|t| fold(&t.artist)).collect();

        let mut album_ix: HashMap<AlbumKey, usize> = HashMap::new();
        let mut albums: Vec<Album> = Vec::new();
        let mut track_album = Vec::with_capacity(tracks.len());
        for (i, t) in tracks.iter().enumerate() {
            let key = AlbumKey { artist: fold(&t.album_artist), name: fold(&t.album) };
            let ix = *album_ix.entry(key.clone()).or_insert_with(|| {
                albums.push(Album {
                    name: t.album.clone(),
                    artist: t.album_artist.clone(),
                    year: None,
                    genre: None,
                    tracks: Vec::new(),
                    duration_ms: 0,
                    name_key: key.name.clone(),
                    artist_key: key.artist.clone(),
                    key: key.clone(),
                });
                albums.len() - 1
            });
            let album = &mut albums[ix];
            album.tracks.push(i);
            if album.genre.is_none() {
                album.genre = t.genre.clone().filter(|g| !g.is_empty());
            }
            album.duration_ms += t.duration_ms;
            album.year = match (album.year, t.year) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            track_album.push(ix);
        }
        for album in &mut albums {
            album.tracks.sort_by(|&a, &b| {
                let (ta, tb) = (&tracks[a], &tracks[b]);
                ta.disc_no
                    .unwrap_or(1)
                    .cmp(&tb.disc_no.unwrap_or(1))
                    .then(ta.track_no.unwrap_or(u32::MAX).cmp(&tb.track_no.unwrap_or(u32::MAX)))
                    .then_with(|| natural_cmp(&title_keys[a], &title_keys[b]))
            });
        }

        let mut artist_ix: HashMap<String, usize> = HashMap::new();
        let mut artists: Vec<Artist> = Vec::new();
        for (ix, album) in albums.iter().enumerate() {
            let a = *artist_ix.entry(album.artist_key.clone()).or_insert_with(|| {
                artists.push(Artist {
                    name: album.artist.clone(),
                    albums: Vec::new(),
                    tracks: 0,
                    duration_ms: 0,
                    key: album.artist_key.clone(),
                });
                artists.len() - 1
            });
            artists[a].albums.push(ix);
            artists[a].tracks += album.tracks.len();
            artists[a].duration_ms += album.duration_ms;
        }
        for artist in &mut artists {
            artist.albums.sort_by(|&a, &b| album_default_cmp(&albums[a], &albums[b]));
        }

        Model { tracks, index, albums, artists, track_album, title_keys, artist_keys }
    }

    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.index.get(&id).map(|&i| &self.tracks[i])
    }

    pub fn artist_by_key(&self, key: &str) -> Option<usize> {
        self.artists.iter().position(|a| a.key == key)
    }

    pub fn album_by_key(&self, key: &AlbumKey) -> Option<usize> {
        self.albums.iter().position(|a| &a.key == key)
    }

    /// Songs of an artist: albums by year then name, tracks in album order.
    pub fn artist_track_ids(&self, artist: usize) -> Vec<TrackId> {
        self.artists[artist]
            .albums
            .iter()
            .flat_map(|&al| self.albums[al].tracks.iter().map(|&t| self.tracks[t].id))
            .collect()
    }

    pub fn album_track_ids(&self, album: usize) -> Vec<TrackId> {
        self.albums[album].tracks.iter().map(|&t| self.tracks[t].id).collect()
    }

    pub fn sorted_artists(&self, sort: Option<(ColId, bool)>) -> Vec<usize> {
        let mut rows: Vec<usize> = (0..self.artists.len()).collect();
        let name = |i: usize| &self.artists[i].key;
        rows.sort_by(|&a, &b| natural_cmp(name(a), name(b)));
        if let Some((col, desc)) = sort {
            rows.sort_by(|&a, &b| {
                let (x, y) = (&self.artists[a], &self.artists[b]);
                let ord = match col {
                    ColId::ArtistAlbums => x.albums.len().cmp(&y.albums.len()),
                    ColId::ArtistTracks => x.tracks.cmp(&y.tracks),
                    ColId::ArtistDuration => x.duration_ms.cmp(&y.duration_ms),
                    _ => natural_cmp(&x.key, &y.key),
                };
                flip(ord, desc)
            });
        }
        rows
    }

    /// Albums of the selected artists (all when none are selected).
    pub fn sorted_albums(&self, artists: &[usize], sort: Option<(ColId, bool)>) -> Vec<usize> {
        let mut rows: Vec<usize> = if artists.is_empty() {
            (0..self.albums.len()).collect()
        } else {
            artists.iter().flat_map(|&a| self.artists[a].albums.iter().copied()).collect()
        };
        rows.sort_by(|&a, &b| album_default_cmp(&self.albums[a], &self.albums[b]));
        if let Some((col, desc)) = sort {
            rows.sort_by(|&a, &b| {
                let (x, y) = (&self.albums[a], &self.albums[b]);
                let ord = match col {
                    ColId::AlbumArtist => natural_cmp(&x.artist_key, &y.artist_key),
                    ColId::AlbumYear => x.year.cmp(&y.year),
                    ColId::AlbumTracks => x.tracks.len().cmp(&y.tracks.len()),
                    ColId::AlbumDuration => x.duration_ms.cmp(&y.duration_ms),
                    ColId::AlbumGenre => natural_cmp(&fold(x.genre.as_deref().unwrap_or("")), &fold(y.genre.as_deref().unwrap_or(""))),
                    _ => natural_cmp(&x.name_key, &y.name_key),
                };
                flip(ord, desc)
            });
        }
        rows
    }

    /// Songs of the given albums, in the albums' default order unless sorted.
    pub fn sorted_songs(&self, albums: &[usize], sort: Option<(ColId, bool)>) -> Vec<usize> {
        let mut ordered = albums.to_vec();
        ordered.sort_by(|&a, &b| album_default_cmp(&self.albums[a], &self.albums[b]));
        let mut rows: Vec<usize> = ordered.iter().flat_map(|&al| self.albums[al].tracks.iter().copied()).collect();
        if let Some((col, desc)) = sort {
            rows.sort_by(|&a, &b| {
                let (x, y) = (&self.tracks[a], &self.tracks[b]);
                let album = |t: usize| &self.albums[self.track_album[t]];
                let ord = match col {
                    ColId::Number => x.track_no.cmp(&y.track_no),
                    ColId::Artist => natural_cmp(&self.artist_keys[a], &self.artist_keys[b]),
                    ColId::Album => natural_cmp(&album(a).name_key, &album(b).name_key),
                    ColId::Duration => x.duration_ms.cmp(&y.duration_ms),
                    ColId::Disc => x.disc_no.cmp(&y.disc_no),
                    ColId::Year => x.year.cmp(&y.year),
                    ColId::Genre => natural_cmp(&fold(x.genre.as_deref().unwrap_or("")), &fold(y.genre.as_deref().unwrap_or(""))),
                    ColId::AlbumArtistCol => natural_cmp(&album(a).artist_key, &album(b).artist_key),
                    ColId::Format => x.format.cmp(&y.format),
                    ColId::Bitrate => x.bitrate_kbps.cmp(&y.bitrate_kbps),
                    ColId::SampleRate => x.sample_rate.cmp(&y.sample_rate),
                    ColId::DateAdded => date_added(x).cmp(&date_added(y)),
                    ColId::FilePath => x.path.cmp(&y.path),
                    _ => natural_cmp(&self.title_keys[a], &self.title_keys[b]),
                };
                flip(ord, desc)
            });
        }
        rows
    }

    pub fn title_key(&self, track: usize) -> &str {
        &self.title_keys[track]
    }
}

/// When the track entered the library, in Unix seconds.
pub fn date_added(t: &Track) -> i64 {
    t.added
}

fn flip(ord: Ordering, desc: bool) -> Ordering {
    if desc { ord.reverse() } else { ord }
}

fn album_default_cmp(a: &Album, b: &Album) -> Ordering {
    natural_cmp(&a.artist_key, &b.artist_key)
        .then(a.year.cmp(&b.year))
        .then_with(|| natural_cmp(&a.name_key, &b.name_key))
}

/// Type-to-jump: first row starting with the term, else first containing it.
pub fn best_match<'a>(rows: impl Iterator<Item = &'a str> + Clone, term: &str) -> Option<usize> {
    let term = fold(term);
    rows.clone()
        .position(|label| label.starts_with(&term))
        .or_else(|| rows.into_iter().position(|label| label.contains(&term)))
}

/// Calendar date for the Date Added column, from Unix seconds (UTC).
pub fn format_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn format_duration(ms: u64) -> String {
    let s = ms / 1000;
    let (h, m, s) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        assert_eq!(natural_cmp("track 2", "track 10"), Ordering::Less);
        assert_eq!(natural_cmp("a", "b"), Ordering::Less);
        assert_eq!(natural_cmp("007", "7"), Ordering::Equal);
    }

    #[test]
    fn dates() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(1_793_577_600), "2026-11-02");
    }

    #[test]
    fn folding_strips_accents_and_case() {
        assert_eq!(fold("Échoes"), "echoes");
    }

    #[test]
    fn jump_prefers_prefix() {
        let rows = ["paper satellites", "swoon theory", "sworn atlas", "the swoon"];
        assert_eq!(best_match(rows.iter().copied(), "swo"), Some(1));
        assert_eq!(best_match(rows.iter().copied(), "atlas"), Some(2));
        assert_eq!(best_match(rows.iter().copied(), "zzz"), None);
    }
}
