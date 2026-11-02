//! Library panes: rows, selection, sorting, type-to-jump, and navigation.

use super::*;
use crate::model;
use crate::ui::table;

impl App {
    pub fn refresh_rows(&mut self) {
        self.artist_rows = self.model.sorted_artists(self.sorts[Pane::Artists.ix()]);
        let artists: Vec<usize> = self.sel_artists.iter().filter_map(|k| self.model.artist_by_key(k)).collect();
        self.album_rows = self.model.sorted_albums(&artists, self.sorts[Pane::Albums.ix()]);
        let albums: Vec<usize> = if self.sel_albums.is_empty() {
            self.album_rows.clone()
        } else {
            self.sel_albums.iter().filter_map(|k| self.model.album_by_key(k)).collect()
        };
        self.song_rows = self.model.sorted_songs(&albums, self.sorts[Pane::Songs.ix()]);
        self.sel_queue.retain(|&r| r < self.queue.items.len());
    }

    pub fn row_count(&self, pane: Pane) -> usize {
        match pane {
            Pane::Artists => self.artist_rows.len() + 1,
            Pane::Albums => self.album_rows.len() + 1,
            Pane::Songs => self.song_rows.len(),
            Pane::Queue => self.queue.items.len(),
        }
    }

    pub(super) fn select(&mut self, pane: Pane, row: usize, modifiers: Modifiers) {
        let anchor = self.anchors[pane.ix()];
        let range: Vec<usize> = match anchor {
            Some(a) if modifiers.shift() => (a.min(row)..=a.max(row)).collect(),
            _ => vec![row],
        };
        let toggle = modifiers.control() && !modifiers.shift();
        let extend = modifiers.shift() && modifiers.control();
        if !modifiers.shift() {
            self.anchors[pane.ix()] = Some(row);
        }
        fn apply<K: PartialEq + Clone>(sel: &mut Vec<K>, keys: Vec<K>, toggle: bool, extend: bool) {
            if toggle {
                for k in keys {
                    if let Some(i) = sel.iter().position(|s| *s == k) {
                        sel.remove(i);
                    } else {
                        sel.push(k);
                    }
                }
            } else if extend {
                for k in keys {
                    if !sel.contains(&k) {
                        sel.push(k);
                    }
                }
            } else {
                *sel = keys;
            }
        }
        match pane {
            Pane::Artists => {
                if row == 0 && !modifiers.shift() {
                    self.sel_artists.clear();
                } else {
                    let keys = range.iter().filter(|&&r| r > 0).filter_map(|&r| self.artist_rows.get(r - 1)).map(|&a| self.model.artists[a].key.clone()).collect();
                    apply(&mut self.sel_artists, keys, toggle, extend);
                }
                self.sel_albums.clear();
                self.offsets[Pane::Albums.ix()] = 0.0;
                self.offsets[Pane::Songs.ix()] = 0.0;
                self.refresh_rows();
            }
            Pane::Albums => {
                if row == 0 && !modifiers.shift() {
                    self.sel_albums.clear();
                } else {
                    let keys = range.iter().filter(|&&r| r > 0).filter_map(|&r| self.album_rows.get(r - 1)).map(|&a| self.model.albums[a].key.clone()).collect();
                    apply(&mut self.sel_albums, keys, toggle, extend);
                }
                self.offsets[Pane::Songs.ix()] = 0.0;
                self.refresh_rows();
            }
            Pane::Songs => {
                let keys = range.iter().filter_map(|&r| self.song_rows.get(r)).map(|&t| self.model.tracks[t].id).collect();
                apply(&mut self.sel_songs, keys, toggle, extend);
            }
            Pane::Queue => {
                let keys = range.into_iter().filter(|&r| r < self.queue.items.len()).collect();
                apply(&mut self.sel_queue, keys, toggle, extend);
            }
        }
    }

    /// Ctrl+A: everything in the focused pane (for Artists/Albums, every real row).
    pub(super) fn select_all(&mut self, pane: Pane) {
        match pane {
            Pane::Artists => {
                self.sel_artists = self.artist_rows.iter().map(|&a| self.model.artists[a].key.clone()).collect();
                self.sel_albums.clear();
                self.refresh_rows();
            }
            Pane::Albums => {
                self.sel_albums = self.album_rows.iter().map(|&a| self.model.albums[a].key.clone()).collect();
                self.refresh_rows();
            }
            Pane::Songs => self.sel_songs = self.song_rows.iter().map(|&t| self.model.tracks[t].id).collect(),
            Pane::Queue => self.sel_queue = (0..self.queue.items.len()).collect(),
        }
    }

    pub fn is_selected(&self, pane: Pane, row: usize) -> bool {
        match pane {
            Pane::Artists if row == 0 => self.sel_artists.is_empty(),
            Pane::Artists => self.artist_rows.get(row - 1).is_some_and(|&a| self.sel_artists.contains(&self.model.artists[a].key)),
            Pane::Albums if row == 0 => self.sel_albums.is_empty(),
            Pane::Albums => self.album_rows.get(row - 1).is_some_and(|&a| self.sel_albums.contains(&self.model.albums[a].key)),
            Pane::Songs => self.song_rows.get(row).is_some_and(|&t| self.sel_songs.contains(&self.model.tracks[t].id)),
            Pane::Queue => self.sel_queue.contains(&row),
        }
    }

    /// Double-click. A song queues the whole Songs pane and plays from its position.
    pub(super) fn activate(&mut self, pane: Pane, row: usize) {
        let ids = match pane {
            Pane::Artists if row > 0 => self.artist_rows.get(row - 1).map(|&a| self.model.artist_track_ids(a)),
            Pane::Albums if row > 0 => self.album_rows.get(row - 1).map(|&a| self.model.album_track_ids(a)),
            Pane::Songs if row < self.song_rows.len() => {
                let ids = self.song_rows.iter().map(|&t| self.model.tracks[t].id).collect();
                self.play_ids_at(ids, row);
                return;
            }
            Pane::Queue => {
                let step = self.queue.jump(row);
                self.apply(step);
                return;
            }
            _ => None,
        };
        if let Some(ids) = ids.filter(|ids| !ids.is_empty()) {
            self.play_ids(ids);
        }
    }

    /// The tracks a single row stands for.
    pub(super) fn row_ids(&self, pane: Pane, row: usize) -> Vec<TrackId> {
        match pane {
            Pane::Artists if row > 0 => self.artist_rows.get(row - 1).map(|&a| self.model.artist_track_ids(a)).unwrap_or_default(),
            Pane::Albums if row > 0 => self.album_rows.get(row - 1).map(|&a| self.model.album_track_ids(a)).unwrap_or_default(),
            Pane::Songs => self.song_rows.get(row).map(|&t| vec![self.model.tracks[t].id]).unwrap_or_default(),
            Pane::Queue => self.queue.items.get(row).map(|&id| vec![id]).unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// Enter / "Play": the selection in display order. One selected song plays from there down.
    pub(super) fn play_selection(&mut self, pane: Pane) {
        if pane == Pane::Songs && self.sel_songs.len() == 1 {
            let id = self.sel_songs[0];
            if let Some(row) = self.song_rows.iter().position(|&t| self.model.tracks[t].id == id) {
                self.activate(Pane::Songs, row);
                return;
            }
        }
        if pane == Pane::Queue {
            if let Some(&first) = self.sel_queue.iter().min() {
                let step = self.queue.jump(first);
                self.apply(step);
            }
            return;
        }
        let ids = self.selection_ids(pane);
        if !ids.is_empty() {
            self.play_ids(ids);
        }
    }

    /// Tracks of the selection, in display order.
    pub(super) fn selection_ids(&self, pane: Pane) -> Vec<TrackId> {
        match pane {
            Pane::Artists => self.artist_rows.iter().filter(|&&a| self.sel_artists.contains(&self.model.artists[a].key)).flat_map(|&a| self.model.artist_track_ids(a)).collect(),
            Pane::Albums => self.album_rows.iter().filter(|&&a| self.sel_albums.contains(&self.model.albums[a].key)).flat_map(|&a| self.model.album_track_ids(a)).collect(),
            Pane::Songs => self.song_rows.iter().map(|&t| self.model.tracks[t].id).filter(|id| self.sel_songs.contains(id)).collect(),
            Pane::Queue => {
                let mut rows = self.sel_queue.clone();
                rows.sort_unstable();
                rows.iter().filter_map(|&r| self.queue.items.get(r).copied()).collect()
            }
        }
    }

    /// The first selected track, for Go to Artist/Album and Show in Folder.
    pub(super) fn first_selected_track(&self, pane: Pane) -> Option<&Track> {
        self.selection_ids(pane).first().and_then(|&id| self.model.track(id))
    }

    /// Click a column header: sort by it, then reverse; the queue keeps its play order.
    pub(super) fn sort_by(&mut self, pane: Pane, index: usize) {
        if pane == Pane::Queue {
            return;
        }
        let Some(col) = self.columns[pane.ix()].visible().nth(index).map(|c| c.id) else { return };
        let default = default_sort(pane);
        let current = self.sorts[pane.ix()];
        self.sorts[pane.ix()] = match current {
            Some((c, desc)) if c == col => Some((col, !desc)),
            None if default == Some(col) => Some((col, true)),
            _ => Some((col, false)),
        };
        self.refresh_rows();
    }

    /// The sort shown on the header: the chosen one, or the pane's default order's lead column.
    pub fn shown_sort(&self, pane: Pane) -> Option<(ColId, bool)> {
        self.sorts[pane.ix()].or_else(|| default_sort(pane).map(|c| (c, false)))
    }

    pub(super) fn type_to_jump(&mut self, typed: &str) -> Task<Message> {
        let pane = self.focus;
        let term = match &self.jump {
            Some(j) if j.pane == pane => format!("{}{typed}", j.term),
            _ => typed.to_owned(),
        };
        self.jump = Some(Jump { pane, term, at: Instant::now() });
        self.jump_to_term()
    }

    pub(super) fn jump_to_term(&mut self) -> Task<Message> {
        let Some(j) = &self.jump else { return Task::none() };
        let (pane, term) = (j.pane, j.term.clone());
        let labels: Vec<String> = match pane {
            Pane::Artists => self.artist_rows.iter().map(|&a| self.model.artists[a].key.clone()).collect(),
            Pane::Albums => self.album_rows.iter().map(|&a| model::fold(&self.model.albums[a].name)).collect(),
            Pane::Songs => self.song_rows.iter().map(|&t| self.model.title_key(t).to_owned()).collect(),
            Pane::Queue => self.queue.items.iter().map(|id| self.model.index.get(id).map(|&t| self.model.title_key(t).to_owned()).unwrap_or_default()).collect(),
        };
        let Some(hit) = model::best_match(labels.iter().map(String::as_str), &term) else { return Task::none() };
        let row = hit + usize::from(pane.has_all_row());
        self.select(pane, row, Modifiers::default());
        self.scroll_into_view(pane, row, true)
    }

    /// Bring `row` into view; `center` puts it mid-list, otherwise scroll as little as possible.
    pub(super) fn scroll_into_view(&mut self, pane: Pane, row: usize, center: bool) -> Task<Message> {
        let top = table::LIST_PAD + row as f32 * table::ROW_H;
        let current = self.offsets[pane.ix()];
        let height = self.viewports[pane.ix()];
        let visible = top >= current && top + table::ROW_H <= current + height;
        let target = if center && !visible {
            top - (height - table::ROW_H) / 2.0
        } else if top < current {
            top - table::LIST_PAD
        } else if top + table::ROW_H > current + height {
            top + table::ROW_H - height + table::LIST_PAD
        } else {
            return Task::none();
        }
        .max(0.0);
        self.offsets[pane.ix()] = target;
        iced::widget::operation::scroll_to(table::scroll_id(pane), iced::widget::scrollable::AbsoluteOffset { x: None, y: Some(target) })
    }

    pub(super) fn move_selection(&mut self, delta: isize, extend: bool) -> Task<Message> {
        let pane = self.focus;
        let count = self.row_count(pane);
        if count == 0 {
            return Task::none();
        }
        let current = self.anchors[pane.ix()].unwrap_or(0) as isize;
        let row = (current + delta).clamp(0, count as isize - 1) as usize;
        if extend {
            self.select(pane, row, Modifiers::SHIFT);
        } else {
            self.select(pane, row, Modifiers::default());
        }
        self.scroll_into_view(pane, row, false)
    }

    /// Select an artist, cascading the filter, and scroll to it.
    pub(super) fn go_to_artist(&mut self, key: String) -> Task<Message> {
        self.sel_artists = vec![key.clone()];
        self.sel_albums.clear();
        self.refresh_rows();
        self.focus = Pane::Artists;
        let row = self.artist_rows.iter().position(|&a| self.model.artists[a].key == key).map_or(0, |r| r + 1);
        self.anchors[Pane::Artists.ix()] = Some(row);
        self.scroll_into_view(Pane::Artists, row, true)
    }

    pub(super) fn go_to_album(&mut self, album: AlbumKey) -> Task<Message> {
        let to_artist = self.go_to_artist(album.artist.clone());
        self.sel_albums = vec![album.clone()];
        self.refresh_rows();
        self.focus = Pane::Albums;
        let row = self.album_rows.iter().position(|&a| self.model.albums[a].key == album).map_or(0, |r| r + 1);
        self.anchors[Pane::Albums.ix()] = Some(row);
        Task::batch([to_artist, self.scroll_into_view(Pane::Albums, row, true)])
    }
}

fn default_sort(pane: Pane) -> Option<ColId> {
    match pane {
        Pane::Artists => Some(ColId::ArtistName),
        Pane::Albums => Some(ColId::AlbumArtist),
        _ => None,
    }
}
