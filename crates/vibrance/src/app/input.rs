//! Keyboard shortcuts, mouse gestures (drag to queue, column resize/reorder), and context menus.

use iced::keyboard::Key;
use iced::keyboard::key::Named;

use super::*;

impl App {
    pub(super) fn key(&mut self, event: keyboard::Event, captured: bool) -> Task<Message> {
        let keyboard::Event::KeyPressed { key, modifiers, text, .. } = event else {
            if let keyboard::Event::ModifiersChanged(m) = event {
                self.modifiers = m;
            }
            return Task::none();
        };
        self.modifiers = modifiers;
        let (ctrl, shift) = (modifiers.control(), modifiers.shift());

        if self.settings {
            if matches!(key.as_ref(), Key::Named(Named::Escape)) {
                return self.dispatch(Message::CloseSettings);
            }
            // The dialog's text field owns typing; nothing else reacts while it is open.
            let _ = captured;
            return Task::none();
        }

        if self.viz_menu.is_some() && matches!(key.as_ref(), Key::Named(Named::Escape)) {
            self.viz_menu = None;
            return Task::none();
        }
        if matches!(key.as_ref(), Key::Named(Named::F11)) {
            return self.dispatch(Message::VizFullscreen(!self.viz_fullscreen));
        }
        if self.viz_fullscreen {
            // No panes are visible, so plain letters are free for visualizer controls (SPEC 4.6).
            self.viz_ui_until = Instant::now() + Duration::from_millis(2500);
            match key.as_ref() {
                Key::Named(Named::Escape) => return self.dispatch(Message::VizFullscreen(false)),
                Key::Named(Named::Space) => self.play_pause(),
                Key::Named(Named::ArrowLeft) => self.seek_to(self.position.saturating_sub(Duration::from_secs(5))),
                Key::Named(Named::ArrowRight) => self.seek_to(self.position + Duration::from_secs(5)),
                Key::Character("n") if !ctrl => self.viz.next_preset(),
                Key::Character("l") if !ctrl => self.viz.locked = !self.viz.locked,
                _ => {}
            }
            return Task::none();
        }

        let jump_active = self.jump_term(self.focus).is_some();
        match key.as_ref() {
            Key::Named(Named::Escape) => {
                if self.drag.take().is_some() {
                    self.drop_at = None;
                } else if self.context.is_some() {
                    self.context = None;
                } else if self.menu != Menu::Closed {
                    self.menu = Menu::Closed;
                } else {
                    self.jump = None;
                }
            }
            Key::Named(Named::Space) if !jump_active && !ctrl => self.play_pause(),
            Key::Named(Named::ArrowLeft) if ctrl => return self.dispatch(Message::Prev),
            Key::Named(Named::ArrowRight) if ctrl => return self.dispatch(Message::Next),
            Key::Named(Named::ArrowLeft) => self.seek_to(self.position.saturating_sub(Duration::from_secs(5))),
            Key::Named(Named::ArrowRight) => self.seek_to(self.position + Duration::from_secs(5)),
            Key::Named(Named::ArrowUp) if ctrl => self.set_volume(self.volume + 0.05, false),
            Key::Named(Named::ArrowDown) if ctrl => self.set_volume(self.volume - 0.05, false),
            Key::Named(Named::ArrowUp) => return self.move_selection(-1, shift),
            Key::Named(Named::ArrowDown) => return self.move_selection(1, shift),
            Key::Named(Named::PageUp) => return self.move_selection(-12, shift),
            Key::Named(Named::PageDown) => return self.move_selection(12, shift),
            Key::Named(Named::Home) => return self.move_selection(-(self.row_count(self.focus) as isize), shift),
            Key::Named(Named::End) => return self.move_selection(self.row_count(self.focus) as isize, shift),
            Key::Named(Named::Tab) => {
                let visible: Vec<Pane> = Pane::ALL.into_iter().filter(|&p| layout::is_visible(&self.panes, Slot::List(p))).collect();
                if let Some(i) = visible.iter().position(|&p| p == self.focus) {
                    let n = visible.len();
                    self.focus = visible[if shift { (i + n - 1) % n } else { (i + 1) % n }];
                } else if let Some(&first) = visible.first() {
                    self.focus = first;
                }
                self.jump = None;
            }
            Key::Named(Named::Enter) if ctrl => {
                let ids = self.selection_ids(self.focus);
                self.queue.insert_next(&ids);
                self.queue_changed();
            }
            Key::Named(Named::Enter) if shift => {
                let ids = self.selection_ids(self.focus);
                self.queue.append(&ids);
                self.queue_changed();
            }
            Key::Named(Named::Enter) => self.play_selection(self.focus),
            Key::Named(Named::Delete) if ctrl && shift => return self.dispatch(Message::ClearQueue),
            Key::Named(Named::Delete) if self.focus == Pane::Queue => self.remove_selected_queue_rows(),
            Key::Named(Named::Backspace) if jump_active => {
                if let Some(j) = &mut self.jump {
                    j.term.pop();
                    j.at = Instant::now();
                    if j.term.is_empty() {
                        self.jump = None;
                    } else {
                        return self.jump_to_term();
                    }
                }
            }
            Key::Named(Named::F5) => return self.dispatch(Message::Rescan),
            Key::Character(c) if ctrl => match c {
                "o" => return self.dispatch(Message::ImportFolder),
                "q" => return self.dispatch(Message::CloseWindow),
                "s" => return self.dispatch(Message::ToggleShuffle),
                "r" => return self.dispatch(Message::CycleRepeat),
                "m" => return self.dispatch(Message::ToggleMute),
                "." => return self.dispatch(Message::Stop),
                "," => return self.dispatch(Message::OpenSettings),
                "a" => self.select_all(self.focus),
                "j" => {
                    if let Some(c) = self.queue.current {
                        self.focus = Pane::Queue;
                        self.sel_queue = vec![c];
                        return self.scroll_into_view(Pane::Queue, c, true);
                    }
                }
                _ => {}
            },
            _ => {
                let printable = text.as_deref().filter(|t| !t.is_empty() && t.chars().all(|c| !c.is_control()));
                if let Some(typed) = printable
                    && !ctrl
                    && !modifiers.alt()
                    && !modifiers.logo()
                {
                    return self.type_to_jump(typed);
                }
            }
        }
        Task::none()
    }

    pub(super) fn cursor_moved(&mut self, p: Point) {
        self.cursor = p;
        if self.viz_fullscreen {
            self.viz_ui_until = Instant::now() + Duration::from_millis(2500);
        }
        if let Some(press) = &self.press
            && self.drag.is_none()
            && press.origin.distance(p) > 5.0
        {
            self.start_drag();
        }
        if let Some(r) = &self.resize {
            self.columns[r.pane.ix()].resize(r.index, r.target, p.x - r.start_x);
        }
        if let Some(h) = &mut self.header_press
            && !h.reordering
            && (p.x - h.origin_x).abs() > 6.0
        {
            h.reordering = true;
        }
    }

    pub(super) fn mouse_up(&mut self) -> Task<Message> {
        if let Some(r) = self.resize.take() {
            self.save_columns(r.pane);
        }
        if let Some(h) = self.header_press.take() {
            if h.reordering {
                self.save_columns(h.pane);
            } else {
                self.sort_by(h.pane, h.index);
            }
        }
        let press = self.press.take();
        if let Some(drag) = self.drag.take() {
            if let Some(at) = self.drop_at.take() {
                if drag.source == Pane::Queue {
                    self.sel_queue = self.queue.move_rows(&drag.rows, at);
                } else {
                    self.queue.insert_at(at, &drag.ids);
                }
                self.queue_changed();
            }
        } else if let Some(press) = press.filter(|p| p.deferred) {
            self.select(press.pane, press.row, Modifiers::default());
        }
        self.drop_at = None;
        Task::none()
    }

    pub(super) fn row_press(&mut self, pane: Pane, row: usize) {
        self.focus = pane;
        self.menu = Menu::Closed;
        self.context = None;
        let m = self.modifiers;
        let all_row = pane.has_all_row() && row == 0;
        let deferred = !m.control() && !m.shift() && !all_row && self.is_selected(pane, row);
        if !deferred {
            self.select(pane, row, m);
        }
        self.press = Some(Press { pane, row, origin: self.cursor, deferred });
    }

    /// The pressed row (or the selection it belongs to) starts travelling toward the queue.
    pub(super) fn start_drag(&mut self) {
        let Some(press) = self.press.take() else { return };
        let (pane, row) = (press.pane, press.row);
        if pane.has_all_row() && row == 0 {
            return;
        }
        let selected = self.is_selected(pane, row);
        let (ids, rows) = match pane {
            Pane::Queue => {
                let mut rows = if selected { self.sel_queue.clone() } else { vec![row] };
                rows.sort_unstable();
                let ids = rows.iter().filter_map(|&r| self.queue.items.get(r).copied()).collect();
                (ids, rows)
            }
            _ => (if selected { self.selection_ids(pane) } else { self.row_ids(pane, row) }, Vec::new()),
        };
        if ids.is_empty() {
            return;
        }
        let count = |n: usize, one: &str, many: &str| format!("{} {}", thousands(n), if n == 1 { one } else { many });
        let songs = count(ids.len(), "song", "songs");
        let items = match pane {
            Pane::Artists if selected => self.sel_artists.len(),
            Pane::Albums if selected => self.sel_albums.len(),
            Pane::Queue => rows.len(),
            _ => if selected { self.sel_songs.len() } else { 1 },
        };
        let first_name = |pane: Pane| -> String {
            match pane {
                Pane::Artists => self.artist_rows.get(row - 1).map(|&a| self.model.artists[a].name.clone()),
                Pane::Albums => self.album_rows.get(row - 1).map(|&a| self.model.albums[a].name.clone()),
                _ => ids.first().and_then(|&id| self.model.track(id)).map(|t| t.title.clone()),
            }
            .unwrap_or_default()
        };
        let (title, detail, icon) = match pane {
            Pane::Artists if items > 1 => (count(items, "artist", "artists"), songs, "person"),
            Pane::Artists => (first_name(pane), songs, "person"),
            Pane::Albums if items > 1 => (count(items, "album", "albums"), songs, "album"),
            Pane::Albums => (first_name(pane), songs, "album"),
            _ if ids.len() > 1 => (songs, String::new(), "queue_music"),
            _ => (first_name(pane), ids.first().and_then(|&id| self.model.track(id)).map(|t| t.artist.clone()).unwrap_or_default(), "music_note"),
        };
        self.menu = Menu::Closed;
        self.drag = Some(Drag { source: pane, ids, rows, title, detail, icon });
    }

    pub(super) fn header_enter(&mut self, pane: Pane, index: usize) {
        if let Some(h) = &mut self.header_press
            && h.pane == pane
            && h.reordering
            && h.index != index
        {
            self.columns[pane.ix()].reorder(h.index, index);
            h.index = index;
        }
    }

    pub(super) fn row_menu(&mut self, pane: Pane, row: usize) {
        if pane.has_all_row() && row == 0 {
            return;
        }
        self.focus = pane;
        if !self.is_selected(pane, row) {
            self.select(pane, row, Modifiers::default());
        }
        self.open_context(ContextKind::Rows(pane));
    }

    pub(super) fn open_context(&mut self, kind: ContextKind) {
        self.menu = Menu::Closed;
        self.context = Some(ContextMenu { at: self.cursor, kind });
    }

    pub(super) fn context_action(&mut self, action: Action) -> Task<Message> {
        let Some(ContextMenu { kind: ContextKind::Rows(pane), .. }) = self.context.take() else { return Task::none() };
        match action {
            Action::Play => self.play_selection(pane),
            Action::PlayNext => {
                let ids = self.selection_ids(pane);
                self.queue.insert_next(&ids);
                self.queue_changed();
            }
            Action::AddToQueue => {
                let ids = self.selection_ids(pane);
                self.queue.append(&ids);
                self.queue_changed();
            }
            Action::GoToArtist => {
                let key = match pane {
                    Pane::Artists => self.sel_artists.first().cloned(),
                    Pane::Albums => self.sel_albums.first().map(|a| a.artist.clone()),
                    _ => self.first_selected_track(pane).map(|t| crate::model::fold(&t.album_artist)),
                };
                if let Some(key) = key {
                    return self.go_to_artist(key);
                }
            }
            Action::GoToAlbum => {
                let album = self
                    .first_selected_track(pane)
                    .and_then(|t| self.model.index.get(&t.id))
                    .map(|&i| self.model.albums[self.model.track_album[i]].key.clone());
                if let Some(album) = album {
                    return self.go_to_album(album);
                }
            }
            Action::ShowInFolder => {
                if let Some(t) = self.first_selected_track(pane) {
                    vibrance_desktop::show_in_folder(&t.path);
                }
            }
            Action::Remove => self.remove_selected_queue_rows(),
        }
        Task::none()
    }

    fn remove_selected_queue_rows(&mut self) {
        let rows = std::mem::take(&mut self.sel_queue);
        self.queue.remove(&rows);
        self.queue_changed();
    }
}
