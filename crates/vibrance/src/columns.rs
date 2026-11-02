//! Column sets per pane: order, visibility, widths, and their saved form.

use crate::app::Pane;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColId {
    // Artists
    ArtistName,
    ArtistAlbums,
    ArtistTracks,
    ArtistDuration,
    // Albums
    AlbumName,
    AlbumArtist,
    AlbumYear,
    AlbumTracks,
    AlbumDuration,
    AlbumGenre,
    // Songs
    Number,
    Title,
    Artist,
    Album,
    Duration,
    Disc,
    Year,
    Genre,
    AlbumArtistCol,
    Format,
    Bitrate,
    SampleRate,
    DateAdded,
    FilePath,
    // Queue
    QueueIndicator,
    QueuePosition,
    QueueTitle,
    QueueArtist,
    QueueDuration,
}

pub struct Def {
    pub id: ColId,
    pub key: &'static str,
    pub title: &'static str,
    pub numeric: bool,
    /// Fill columns take the space left over; everything else has a pixel width.
    pub fill: bool,
    pub width: f32,
    pub visible: bool,
}

const fn def(id: ColId, key: &'static str, title: &'static str, numeric: bool, fill: bool, width: f32, visible: bool) -> Def {
    Def { id, key, title, numeric, fill, width, visible }
}

const ARTISTS: &[Def] = &[
    def(ColId::ArtistName, "artist", "Artist", false, true, 0.0, true),
    def(ColId::ArtistAlbums, "albums", "Albums", true, false, 52.0, true),
    def(ColId::ArtistTracks, "tracks", "Tracks", true, false, 52.0, true),
    def(ColId::ArtistDuration, "duration", "Duration", true, false, 68.0, false),
];

const ALBUMS: &[Def] = &[
    def(ColId::AlbumName, "album", "Album", false, true, 0.0, true),
    def(ColId::AlbumArtist, "artist", "Artist", false, false, 170.0, true),
    def(ColId::AlbumYear, "year", "Year", true, false, 40.0, true),
    def(ColId::AlbumTracks, "tracks", "Tracks", true, false, 48.0, true),
    def(ColId::AlbumDuration, "duration", "Duration", true, false, 68.0, true),
    def(ColId::AlbumGenre, "genre", "Genre", false, false, 110.0, false),
];

const SONGS: &[Def] = &[
    def(ColId::Number, "number", "#", true, false, 28.0, true),
    def(ColId::Title, "title", "Title", false, true, 0.0, true),
    def(ColId::Artist, "artist", "Artist", false, false, 200.0, true),
    def(ColId::Album, "album", "Album", false, false, 220.0, true),
    def(ColId::Duration, "duration", "Duration", true, false, 60.0, true),
    def(ColId::Disc, "disc", "Disc", true, false, 36.0, false),
    def(ColId::Year, "year", "Year", true, false, 44.0, false),
    def(ColId::Genre, "genre", "Genre", false, false, 110.0, false),
    def(ColId::AlbumArtistCol, "album-artist", "Album Artist", false, false, 170.0, false),
    def(ColId::Format, "format", "Format", false, false, 60.0, false),
    def(ColId::Bitrate, "bitrate", "Bitrate", true, false, 72.0, false),
    def(ColId::SampleRate, "sample-rate", "Sample Rate", true, false, 92.0, false),
    def(ColId::DateAdded, "added", "Date Added", true, false, 96.0, false),
    def(ColId::FilePath, "path", "File Path", false, false, 280.0, false),
];

const QUEUE: &[Def] = &[
    def(ColId::QueueIndicator, "indicator", "", false, false, 20.0, true),
    def(ColId::QueuePosition, "position", "#", true, false, 24.0, true),
    def(ColId::QueueTitle, "title", "Title", false, true, 0.0, true),
    def(ColId::QueueArtist, "artist", "Artist", false, false, 120.0, true),
    def(ColId::QueueDuration, "duration", "Duration", true, false, 52.0, true),
];

pub fn defs(pane: Pane) -> &'static [Def] {
    match pane {
        Pane::Artists => ARTISTS,
        Pane::Albums => ALBUMS,
        Pane::Songs => SONGS,
        Pane::Queue => QUEUE,
    }
}

pub fn def_of(pane: Pane, id: ColId) -> &'static Def {
    defs(pane).iter().find(|d| d.id == id).expect("column belongs to pane")
}

pub const MIN_WIDTH: f32 = 28.0;
pub const MIN_FILL: f32 = 80.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Column {
    pub id: ColId,
    pub visible: bool,
    pub width: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Columns {
    pub pane: Pane,
    pub cols: Vec<Column>,
}

impl Columns {
    pub fn default_for(pane: Pane) -> Columns {
        Columns { pane, cols: defs(pane).iter().map(|d| Column { id: d.id, visible: d.visible, width: d.width }).collect() }
    }

    pub fn visible(&self) -> impl Iterator<Item = &Column> + Clone {
        self.cols.iter().filter(|c| c.visible)
    }

    /// Pixel width of each visible column for a table `width` wide (the fill column absorbs slack).
    pub fn layout(&self, width: f32, gap: f32, padding: f32) -> Vec<f32> {
        let visible: Vec<&Column> = self.visible().collect();
        let fixed: f32 = visible.iter().filter(|c| !def_of(self.pane, c.id).fill).map(|c| c.width).sum();
        let gaps = gap * visible.len().saturating_sub(1) as f32;
        let fill = (width - padding - fixed - gaps).max(MIN_FILL);
        visible.iter().map(|c| if def_of(self.pane, c.id).fill { fill } else { c.width }).collect()
    }

    pub fn toggle(&mut self, id: ColId) {
        if def_of(self.pane, id).fill {
            return;
        }
        if let Some(c) = self.cols.iter_mut().find(|c| c.id == id) {
            c.visible = !c.visible;
        }
    }

    /// Resize the boundary to the right of visible column `index`. Dragging the fill
    /// column's edge resizes its right-hand neighbour instead.
    pub fn resize(&mut self, index: usize, start: (ColId, f32), delta: f32) {
        let visible: Vec<ColId> = self.visible().map(|c| c.id).collect();
        let Some(&id) = visible.get(index) else { return };
        let (target, sign) = if def_of(self.pane, id).fill {
            match visible.get(index + 1) {
                Some(&next) => (next, -1.0),
                None => return,
            }
        } else {
            (id, 1.0)
        };
        if target != start.0 {
            return;
        }
        if let Some(c) = self.cols.iter_mut().find(|c| c.id == target) {
            c.width = (start.1 + sign * delta).max(MIN_WIDTH);
        }
    }

    /// The column whose width a drag on boundary `index` changes, with its current width.
    pub fn resize_target(&self, index: usize) -> Option<(ColId, f32)> {
        let visible: Vec<&Column> = self.visible().collect();
        let col = visible.get(index)?;
        let target = if def_of(self.pane, col.id).fill { visible.get(index + 1)? } else { col };
        Some((target.id, target.width))
    }

    /// Move visible column `from` to sit at visible position `to`.
    pub fn reorder(&mut self, from: usize, to: usize) {
        let visible: Vec<ColId> = self.visible().map(|c| c.id).collect();
        let (Some(&moving), Some(&anchor)) = (visible.get(from), visible.get(to)) else { return };
        if moving == anchor {
            return;
        }
        let src = self.cols.iter().position(|c| c.id == moving).expect("visible column exists");
        let col = self.cols.remove(src);
        let dst = self.cols.iter().position(|c| c.id == anchor).expect("visible column exists");
        let dst = if from < to { dst + 1 } else { dst };
        self.cols.insert(dst, col);
    }

    pub fn to_state(&self) -> String {
        self.cols
            .iter()
            .map(|c| format!("{}:{}:{}", def_of(self.pane, c.id).key, u8::from(c.visible), c.width.round()))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Parse saved columns; unknown keys are dropped and new columns appended with defaults.
    pub fn from_state(pane: Pane, s: &str) -> Columns {
        let mut cols: Vec<Column> = Vec::new();
        for part in s.split(',') {
            let mut it = part.split(':');
            let (Some(key), Some(vis), Some(w)) = (it.next(), it.next(), it.next()) else { continue };
            let Some(d) = defs(pane).iter().find(|d| d.key == key) else { continue };
            if cols.iter().any(|c| c.id == d.id) {
                continue;
            }
            let width = if d.fill { 0.0 } else { w.parse().unwrap_or(d.width).max(MIN_WIDTH) };
            cols.push(Column { id: d.id, visible: vis == "1" || d.fill, width });
        }
        for d in defs(pane) {
            if !cols.iter().any(|c| c.id == d.id) {
                cols.push(Column { id: d.id, visible: d.visible, width: d.width });
            }
        }
        Columns { pane, cols }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_absorbs_slack() {
        let cols = Columns::default_for(Pane::Artists);
        let widths = cols.layout(400.0, 12.0, 32.0);
        assert_eq!(widths.len(), 3);
        assert_eq!(widths[0], 400.0 - 32.0 - 104.0 - 24.0);
    }

    #[test]
    fn reorder_and_round_trip() {
        let mut cols = Columns::default_for(Pane::Songs);
        cols.reorder(4, 1);
        let ids: Vec<ColId> = cols.visible().map(|c| c.id).collect();
        assert_eq!(ids[1], ColId::Duration);
        cols.toggle(ColId::Genre);
        let back = Columns::from_state(Pane::Songs, &cols.to_state());
        assert_eq!(back, cols);
    }

    #[test]
    fn fill_edge_resizes_neighbour() {
        let mut cols = Columns::default_for(Pane::Songs);
        let target = cols.resize_target(1).unwrap();
        assert_eq!(target.0, ColId::Artist);
        cols.resize(1, target, 30.0);
        assert_eq!(cols.cols.iter().find(|c| c.id == ColId::Artist).unwrap().width, 170.0);
    }
}
