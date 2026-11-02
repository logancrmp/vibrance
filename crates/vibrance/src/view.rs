//! The window: title bar, now-playing strip, pane grid, menus, and dialogs.

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    Space, button, canvas, column, container, image, mouse_area, opaque, pane_grid, pin, responsive, row, scrollable, space, stack,
    text_input,
};
use iced::{Alignment, Background, Border, Color, Element, Fill, Length, Padding, Point, Shadow, Vector, border, mouse, window};
use vibrance_audio::PlayState;

use crate::app::{Action, App, ArtMode, ContextKind, Menu, Message, Pane, Slot, thousands};
use crate::columns::{ColId, defs};
use crate::config::TitleBar;
use crate::layout;
use crate::model::{format_date, format_duration};
use crate::queue::Repeat;
use crate::theme::{self, ColorMode, Colors, FONT, FONT_MEDIUM, PaletteChoice, scale, shape};
use crate::ui::marquee::Marquee;
use crate::ui::seek::{self, SeekBar};
use crate::ui::spectrum::Spectrum;
use crate::ui::swatch::Swatch;
use crate::ui::table::{self, Cell, Table, Tone};
use crate::ui::volume::Volume;
use crate::ui::{fit, floating, icon, icon_button, label, state_button, surface};
use crate::viz;

const MENU_W: f32 = 248.0;
const ITEM_H: f32 = 40.0;

/// What sits at the right end of a menu row.
enum Trailing {
    None,
    Hint(&'static str),
    Submenu,
    Check(bool),
}

impl App {
    pub fn view(&self) -> Element<'_, Message> {
        if self.viz_fullscreen {
            return self.viz_fullscreen_view();
        }
        let colors = self.colors;
        let mut content = column![].width(Fill).height(Fill);
        if self.custom_title_bar() {
            content = content.push(self.title_bar());
        }
        content = content.push(
            column![self.now_playing(), self.pane_grid()]
                .spacing(8)
                .padding(Padding { top: if self.custom_title_bar() { 0.0 } else { 8.0 }, right: 8.0, bottom: 8.0, left: 8.0 }),
        );

        let radius = if self.maximized || !self.custom_title_bar() { 0.0 } else { 12.0 };
        let base = container(content).width(Fill).height(Fill).style(move |_| container::Style {
            background: Some(Background::Color(colors.surface)),
            border: Border { radius: radius.into(), width: if radius > 0.0 { 1.0 } else { 0.0 }, color: theme::alpha(colors.outline_variant, 0.6) },
            text_color: Some(colors.on_surface),
            ..container::Style::default()
        });

        let mut layers = stack![base].width(Fill).height(Fill);
        if self.custom_title_bar() && !self.maximized {
            layers = layers.push(resize_handles());
        }
        if self.menu != Menu::Closed {
            layers = layers.push(self.menus());
        }
        if self.context.is_some() {
            layers = layers.push(self.context_menu());
        }
        if self.drag.is_some() {
            layers = layers.push(self.drag_ghost());
        }
        if self.settings {
            layers = layers.push(self.settings_dialog());
        }
        if self.viz_menu.is_some() {
            layers = layers.push(self.viz_menu());
        }
        layers.into()
    }

    fn title_bar(&self) -> Element<'_, Message> {
        let c = self.colors;
        let mut bar = row![icon("graphic_eq", 20.0, c.primary), label("Vibrance", scale::TITLE_SMALL, FONT_MEDIUM, c.on_surface)]
            .spacing(8)
            .align_y(Alignment::Center);
        if let Some(scan) = &self.scan_label {
            bar = bar.push(chip(c, "sync", scan.clone(), None));
        }
        if let Some(notice) = &self.notice {
            bar = bar.push(chip(c, "info", notice.clone(), Some(Message::DismissNotice)));
        }
        bar = bar
            .push(space().width(Fill))
            .push(icon_button("remove", 32.0, 20.0, c.on_surface_variant, None, 16.0, Some(Message::Minimize)))
            .push(icon_button(if self.maximized { "filter_none" } else { "crop_square" }, 32.0, 18.0, c.on_surface_variant, None, 16.0, Some(Message::ToggleMaximize)))
            .push(icon_button("close", 32.0, 20.0, c.on_surface_variant, None, 16.0, Some(Message::CloseWindow)));
        mouse_area(container(bar).height(40).padding(Padding { top: 0.0, right: 8.0, bottom: 0.0, left: 16.0 }).align_y(Alignment::Center).width(Fill))
            .on_press(Message::DragWindow)
            .on_double_click(Message::ToggleMaximize)
            .into()
    }

    fn now_playing(&self) -> Element<'_, Message> {
        let c = self.colors;
        let track = self.queue.now.and_then(|id| self.model.track(id));

        let art: Element<'_, Message> = match &self.art {
            Some(art) if Some(art.track) == self.queue.now => image(art.small.clone())
                .width(80)
                .height(80)
                .content_fit(iced::ContentFit::Cover)
                .border_radius(shape::MEDIUM)
                .into(),
            _ => container(icon("album", 36.0, c.on_surface_variant)).center(80).style(surface(c.surface_container_highest, shape::MEDIUM)).into(),
        };

        let (title, sub) = match track {
            Some(t) => (t.title.clone(), format!("{} · {}", t.artist, t.album)),
            None => ("Nothing playing".to_owned(), "Double-click a song to play it".to_owned()),
        };
        // Long titles scroll back and forth instead of being cut off.
        let phase = self.marquee_phase;
        let info = column![
            canvas(Marquee { text: title, size: scale::TITLE_LARGE.0, line_height: scale::TITLE_LARGE.1, font: FONT, color: c.on_surface, phase }).width(236).height(28),
            canvas(Marquee { text: sub, size: scale::BODY_LARGE.0, line_height: scale::BODY_LARGE.1, font: FONT, color: c.on_surface_variant, phase }).width(236).height(24),
        ]
        .width(236);

        let playing = self.play_state == PlayState::Playing;
        let play = icon_button(
            if playing { "pause" } else { "play_arrow" },
            56.0,
            30.0,
            c.on_primary,
            Some(c.primary),
            if playing { 18.0 } else { 28.0 },
            Some(Message::PlayPause),
        );
        let transport = row![
            icon_button("skip_previous", 40.0, 26.0, c.on_surface_variant, None, 20.0, Some(Message::Prev)),
            play,
            icon_button("skip_next", 40.0, 26.0, c.on_surface_variant, None, 20.0, Some(Message::Next)),
            icon_button("stop", 40.0, 24.0, c.on_surface_variant, None, 20.0, Some(Message::Stop)),
        ]
        .spacing(4)
        .align_y(Alignment::Center);

        let seek = self.seek_column(track.is_some());

        // Clicking the compact spectrum opens the full-screen visualizer.
        let spectrum = mouse_area(container(canvas(Spectrum { bars: &self.bars, peaks: &self.peaks, colors: c, gap: 2.0 }).width(168).height(48)).padding(4))
            .on_press(Message::VizFullscreen(true))
            .interaction(mouse::Interaction::Pointer);

        let volume_value = if self.muted { 0.0 } else { self.volume };
        let volume_icon = if self.muted || volume_value == 0.0 {
            "volume_off"
        } else if volume_value < 0.5 {
            "volume_down"
        } else {
            "volume_up"
        };
        let volume = row![
            icon_button(volume_icon, 40.0, 22.0, c.on_surface_variant, None, 20.0, Some(Message::ToggleMute)),
            canvas(Volume { value: volume_value, colors: c }).width(120).height(40),
        ]
        .spacing(4)
        .align_y(Alignment::Center);

        let more = icon_button("more_vert", 40.0, 24.0, c.on_surface_variant, None, 20.0, Some(Message::ToggleMenu));

        container(row![art, info, transport, seek, spectrum, volume, more].spacing(20).align_y(Alignment::Center))
            .height(104)
            .width(Fill)
            .padding(12)
            .align_y(Alignment::Center)
            .style(surface(c.surface_container, shape::LARGE))
            .into()
    }

    fn seek_column(&self, enabled: bool) -> Element<'_, Message> {
        let c = self.colors;
        let fraction = self.displayed_fraction();
        let duration = self.duration.unwrap_or_default();
        let shown = duration.mul_f32(fraction.clamp(0.0, 1.0));
        column![
            canvas(SeekBar { fraction, lift: self.seek_lift, drag: self.seek_drag, colors: c, enabled }).width(Fill).height(seek::HEIGHT),
            row![
                label(format_duration(shown.as_millis() as u64), scale::LABEL_MEDIUM, FONT_MEDIUM, c.on_surface_variant).width(64),
                space().width(Fill),
                label(format!("-{}", format_duration(duration.saturating_sub(shown).as_millis() as u64)), scale::LABEL_MEDIUM, FONT_MEDIUM, c.on_surface_variant)
                    .width(64)
                    .align_x(Horizontal::Right),
            ]
            .padding([0.0, 2.0]),
        ]
        .width(Fill)
        .into()
    }

    fn pane_grid(&self) -> Element<'_, Message> {
        let c = self.colors;
        pane_grid(&self.panes, |_, slot, _| {
            let title = pane_grid::TitleBar::new(self.pane_title(*slot)).style(|_| container::Style::default());
            let body = match *slot {
                Slot::List(pane) => self.pane_body(pane),
                Slot::AlbumArt => self.album_art(),
            };
            pane_grid::Content::new(body).title_bar(title).style(surface(c.surface_container_low, shape::LARGE))
        })
        .width(Fill)
        .height(Fill)
        .spacing(8)
        .on_resize(8, Message::PaneResized)
        .on_drag(Message::PaneDragged)
        .style(move |_| pane_grid::Style {
            hovered_region: pane_grid::Highlight {
                background: Background::Color(theme::alpha(c.primary_container, 0.28)),
                border: Border { color: c.primary, width: 2.0, radius: shape::LARGE.into() },
            },
            picked_split: pane_grid::Line { color: c.primary, width: 2.0 },
            hovered_split: pane_grid::Line { color: c.outline, width: 2.0 },
        })
        .into()
    }

    fn pane_title(&self, slot: Slot) -> Element<'_, Message> {
        let c = self.colors;
        if slot == Slot::AlbumArt {
            // Album Art stays nearly headerless: its label only shows on hover.
            let fg = if self.art_hover { c.on_surface_variant } else { Color::TRANSPARENT };
            let viz = self.art_mode == ArtMode::Visualizer;
            let toggle = |glyph: &'static str, on: bool, msg: Message| {
                let (fg, fill) = if on { (c.on_secondary_container, Some(c.secondary_container)) } else { (c.on_surface_variant, None) };
                icon_button(glyph, 26.0, 16.0, fg, fill, 13.0, Some(msg))
            };
            let title = if viz { "Visualizer" } else { "Album Art" };
            let mut bar = row![icon("drag_indicator", 18.0, fg), label(title, scale::LABEL_MEDIUM, FONT_MEDIUM, fg), space().width(Fill)]
                .spacing(4)
                .align_y(Alignment::Center);
            if viz {
                bar = bar
                    .push(self.preset_chip(26.0, 140.0))
                    .push(icon_button("open_in_full", 26.0, 16.0, c.on_surface_variant, None, 13.0, Some(Message::VizFullscreen(true))));
            }
            let bar = bar
                .push(row![toggle("image", !viz, Message::SetArtMode(ArtMode::Art)), toggle("graphic_eq", viz, Message::SetArtMode(ArtMode::Visualizer))].spacing(2));
            return mouse_area(container(bar).height(34).width(Fill).padding(Padding { top: 4.0, right: 8.0, bottom: 0.0, left: 10.0 }))
                .on_enter(Message::ArtHover(true))
                .on_exit(Message::ArtHover(false))
                .on_right_press(Message::PaneMenu(slot))
                .into();
        }
        let Slot::List(pane) = slot else { unreachable!() };
        let count = match pane {
            Pane::Artists => self.artist_rows.len(),
            Pane::Albums => self.album_rows.len(),
            Pane::Songs => self.song_rows.len(),
            // The footer already shows the queue's size.
            Pane::Queue => 0,
        };
        let mut bar = row![
            label(slot.label(), scale::TITLE_SMALL, FONT_MEDIUM, c.on_surface),
            label(if count > 0 { thousands(count) } else { String::new() }, scale::LABEL_MEDIUM, FONT_MEDIUM, c.on_surface_variant),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if pane == Pane::Queue {
            let toggle = |name: &'static str, on: bool, msg: Message| {
                let (fg, fill) = if on { (c.on_secondary_container, Some(c.secondary_container)) } else { (c.on_surface_variant, None) };
                icon_button(name, 36.0, 20.0, fg, fill, 18.0, Some(msg))
            };
            bar = bar
                .push(space().width(Fill))
                .push(toggle("shuffle", self.queue.shuffle, Message::ToggleShuffle))
                .push(toggle(if self.queue.repeat == Repeat::Song { "repeat_one" } else { "repeat" }, self.queue.repeat != Repeat::Off, Message::CycleRepeat))
                .push(icon_button("clear_all", 36.0, 20.0, c.on_surface_variant, None, 18.0, Some(Message::ClearQueue)));
        }
        mouse_area(container(bar).height(44).width(Fill).padding(Padding { top: 0.0, right: 6.0, bottom: 0.0, left: 16.0 }).align_y(Alignment::Center))
            .on_right_press(Message::PaneMenu(slot))
            .into()
    }

    fn pane_body(&self, pane: Pane) -> Element<'_, Message> {
        let c = self.colors;
        let list = table::table(Table {
            pane,
            colors: c,
            columns: &self.columns[pane.ix()],
            sort: self.shown_sort(pane),
            sortable: pane != Pane::Queue,
            total: self.row_count(pane),
            offset: self.offsets[pane.ix()],
            hovered: self.hovered[pane.ix()],
            drop_at: if pane == Pane::Queue && self.drag.is_some() { self.drop_at } else { None },
            row: move |i| self.table_row(pane, i),
            empty: move || self.empty_state(pane),
        });

        let mut body = column![list];
        if pane == Pane::Queue {
            let total_ms: u64 = self.queue.items.iter().filter_map(|&id| self.model.track(id)).map(|t| t.duration_ms).sum();
            let n = self.queue.items.len();
            let footer = match (n, self.queue.now.is_some()) {
                (0, true) => "Empty · the current song keeps playing".to_owned(),
                (0, false) => "Empty".to_owned(),
                (1, _) => format!("1 song · {}", format_duration(total_ms)),
                _ => format!("{} songs · {}", thousands(n), format_duration(total_ms)),
            };
            body = body.push(container(space()).width(Fill).height(1).style(crate::ui::divider(c.outline_variant))).push(
                container(label(footer, scale::LABEL_MEDIUM, FONT_MEDIUM, c.on_surface_variant)).height(40).padding([0.0, 16.0]).align_y(Alignment::Center),
            );
        }

        let mut layers = stack![body.height(Fill)].width(Fill).height(Fill);
        if let Some(term) = self.jump_term(pane) {
            let chip = container(label(term.to_owned(), scale::TITLE_MEDIUM, FONT_MEDIUM, c.on_surface))
                .padding([4.0, 12.0])
                .style(move |_| container::Style {
                    background: Some(Background::Color(c.surface_container_highest)),
                    border: border::rounded(shape::SMALL),
                    shadow: Shadow { color: Color::from_rgba(0.0, 0.0, 0.0, 0.3), offset: Vector::new(0.0, 1.0), blur_radius: 3.0 },
                    ..container::Style::default()
                });
            layers = layers.push(container(chip).width(Fill).align_x(Horizontal::Right).padding(Padding { top: 44.0, right: 16.0, bottom: 0.0, left: 0.0 }));
        }
        mouse_area(layers).on_press(Message::Focus(pane)).into()
    }

    fn table_row(&self, pane: Pane, i: usize) -> table::Row {
        let selected = self.is_selected(pane, i);
        let cols = self.columns[pane.ix()].visible();
        let mut dim = false;
        let cells = match pane {
            Pane::Artists if i == 0 => {
                let tracks: usize = self.model.artists.iter().map(|a| a.tracks).sum();
                let ms: u64 = self.model.artists.iter().map(|a| a.duration_ms).sum();
                cols.map(|col| match col.id {
                    ColId::ArtistName => Cell::new(format!("All ({})", thousands(self.model.artists.len())), Tone::Primary),
                    ColId::ArtistAlbums => Cell::new(thousands(self.model.albums.len()), Tone::Secondary),
                    ColId::ArtistTracks => Cell::new(thousands(tracks), Tone::Secondary),
                    _ => Cell::new(format_duration(ms), Tone::Secondary),
                })
                .collect()
            }
            Pane::Artists => {
                let a = &self.model.artists[self.artist_rows[i - 1]];
                cols.map(|col| match col.id {
                    ColId::ArtistName => Cell::new(a.name.clone(), Tone::Primary),
                    ColId::ArtistAlbums => Cell::new(a.albums.len().to_string(), Tone::Secondary),
                    ColId::ArtistTracks => Cell::new(thousands(a.tracks), Tone::Secondary),
                    _ => Cell::new(format_duration(a.duration_ms), Tone::Secondary),
                })
                .collect()
            }
            Pane::Albums if i == 0 => {
                let tracks: usize = self.album_rows.iter().map(|&a| self.model.albums[a].tracks.len()).sum();
                let ms: u64 = self.album_rows.iter().map(|&a| self.model.albums[a].duration_ms).sum();
                cols.map(|col| match col.id {
                    ColId::AlbumName => Cell::new(format!("All ({})", thousands(self.album_rows.len())), Tone::Primary),
                    ColId::AlbumTracks => Cell::new(thousands(tracks), Tone::Secondary),
                    ColId::AlbumDuration => Cell::new(format_duration(ms), Tone::Secondary),
                    _ => Cell::new("", Tone::Secondary),
                })
                .collect()
            }
            Pane::Albums => {
                let a = &self.model.albums[self.album_rows[i - 1]];
                cols.map(|col| match col.id {
                    ColId::AlbumName => Cell::new(a.name.clone(), Tone::Primary),
                    ColId::AlbumArtist => Cell::new(a.artist.clone(), Tone::Secondary),
                    ColId::AlbumYear => Cell::new(a.year.map(|y| y.to_string()).unwrap_or_default(), Tone::Secondary),
                    ColId::AlbumTracks => Cell::new(a.tracks.len().to_string(), Tone::Secondary),
                    ColId::AlbumDuration => Cell::new(format_duration(a.duration_ms), Tone::Secondary),
                    _ => Cell::new(a.genre.clone().unwrap_or_default(), Tone::Secondary),
                })
                .collect()
            }
            Pane::Songs => {
                let t = &self.model.tracks[self.song_rows[i]];
                let playing = self.queue.now == Some(t.id);
                dim = self.failed.contains(&t.id);
                cols.map(|col| self.song_cell(t, col.id, playing)).collect()
            }
            Pane::Queue => {
                let id = self.queue.items[i];
                let current = self.queue.current == Some(i);
                let played = self.queue.current.is_some_and(|c| i < c) || (self.queue.current.is_none() && i < self.queue.cursor());
                let track = self.model.track(id);
                dim = track.is_none() || self.failed.contains(&id);
                let indicator = if dim {
                    "error"
                } else if current {
                    if self.play_state == PlayState::Playing { "graphic_eq" } else { "pause" }
                } else {
                    ""
                };
                cols.map(|col| match col.id {
                    ColId::QueueIndicator => Cell::new(indicator, Tone::Icon),
                    ColId::QueuePosition => Cell::new((i + 1).to_string(), Tone::Secondary),
                    ColId::QueueTitle => Cell {
                        text: track.map_or_else(|| "Missing file".to_owned(), |t| t.title.clone()),
                        tone: if current { Tone::Accent } else if played { Tone::Secondary } else { Tone::Primary },
                        strong: current,
                        marquee: current.then_some(self.marquee_phase),
                    },
                    ColId::QueueArtist => Cell::new(track.map(|t| t.artist.clone()).unwrap_or_default(), Tone::Secondary),
                    _ => Cell::new(track.map(|t| format_duration(t.duration_ms)).unwrap_or_default(), Tone::Secondary),
                })
                .collect()
            }
        };
        table::Row { cells, selected, dim }
    }

    fn song_cell(&self, t: &vibrance_library::Track, id: ColId, playing: bool) -> Cell {
        let opt = |v: Option<String>| Cell::new(v.unwrap_or_default(), Tone::Secondary);
        match id {
            ColId::Number => opt(t.track_no.map(|n| n.to_string())),
            ColId::Title => Cell { text: t.title.clone(), tone: if playing { Tone::Accent } else { Tone::Primary }, strong: false, marquee: None },
            ColId::Artist => Cell::new(t.artist.clone(), Tone::Secondary),
            ColId::Album => Cell::new(t.album.clone(), Tone::Secondary),
            ColId::Duration => Cell::new(format_duration(t.duration_ms), Tone::Secondary),
            ColId::Disc => opt(t.disc_no.map(|n| n.to_string())),
            ColId::Year => opt(t.year.map(|y| y.to_string())),
            ColId::Genre => opt(t.genre.clone()),
            ColId::AlbumArtistCol => Cell::new(t.album_artist.clone(), Tone::Secondary),
            ColId::Format => Cell::new(t.format.clone(), Tone::Secondary),
            ColId::Bitrate => opt(t.bitrate_kbps.map(|b| format!("{b} kbps"))),
            ColId::SampleRate => opt(t.sample_rate.map(|r| format!("{:.1} kHz", r as f32 / 1000.0))),
            ColId::DateAdded => Cell::new(format_date(t.added), Tone::Secondary),
            _ => Cell::new(t.path.display().to_string(), Tone::Secondary),
        }
    }

    fn empty_state(&self, pane: Pane) -> Element<'_, Message> {
        let c = self.colors;
        let (glyph, title, hint): (&str, &str, &str) = match pane {
            Pane::Queue => ("queue_music", "Queue is empty", "Double-click anything to play it, or drag it here."),
            _ if self.model.tracks.is_empty() => ("library_music", "Your library is empty", "Import a folder to get started."),
            _ => ("search_off", "Nothing here", ""),
        };
        let mut col = column![icon(glyph, 40.0, c.outline), label(title, scale::BODY_LARGE, FONT, c.on_surface), label(hint, scale::BODY_MEDIUM, FONT, c.on_surface_variant)]
            .spacing(8)
            .align_x(Alignment::Center);
        if pane == Pane::Songs && self.model.tracks.is_empty() {
            col = col.push(Space::new().height(8)).push(tonal_button(c, "library_add", "Import Folder", Message::ImportFolder));
        }
        let el = container(col).center(Fill).padding(24);
        if pane == Pane::Queue {
            // An empty queue still accepts drops.
            mouse_area(el).on_move(|_| Message::DropHover(Some(0.0))).on_exit(Message::DropHover(None)).into()
        } else {
            el.into()
        }
    }

    fn album_art(&self) -> Element<'_, Message> {
        let c = self.colors;
        if self.art_mode == ArtMode::Visualizer {
            let shader = iced::widget::shader(viz::gpu::Program { frame: self.viz_frame.clone() }).width(Fill).height(Fill);
            let mut layers = stack![shader].width(Fill).height(Fill);
            if !self.viz.active() {
                layers = layers.push(
                    container(label("Starts with the music", scale::BODY_MEDIUM, FONT, c.on_surface_variant)).center(Fill),
                );
            }
            // A click moves on to the next preset.
            return mouse_area(container(layers).padding(Padding { top: 0.0, right: 12.0, bottom: 12.0, left: 12.0 }))
                .on_press(Message::VizNext)
                .on_enter(Message::ArtHover(true))
                .on_exit(Message::ArtHover(false))
                .into();
        }
        let content: Element<'_, Message> = match &self.art {
            Some(art) if Some(art.track) == self.queue.now => {
                let handle = art.large.clone();
                responsive(move |size| {
                    let side = (size.width.min(size.height) - 24.0).max(0.0);
                    container(image(handle.clone()).width(side).height(side).content_fit(iced::ContentFit::Cover).border_radius(shape::MEDIUM))
                        .center(Fill)
                        .into()
                })
                .into()
            }
            _ => container(container(icon("album", 64.0, c.outline)).center(Fill).style(surface(c.surface_container_high, shape::MEDIUM)))
                .padding(Padding { top: 0.0, right: 12.0, bottom: 12.0, left: 12.0 })
                .into(),
        };
        mouse_area(content).on_enter(Message::ArtHover(true)).on_exit(Message::ArtHover(false)).into()
    }

    // ---- menus ----

    fn menu_item<'a>(&self, glyph: &'a str, text: &'a str, trailing: Trailing, highlighted: bool, msg: Message) -> Element<'a, Message> {
        let c = self.colors;
        let mut r = row![icon(glyph, 20.0, c.on_surface_variant), label(text, scale::BODY_MEDIUM, FONT, c.on_surface).width(Fill)]
            .spacing(12)
            .align_y(Alignment::Center);
        r = match trailing {
            Trailing::None => r,
            Trailing::Hint(hint) => r.push(label(hint, scale::LABEL_MEDIUM, FONT, c.on_surface_variant)),
            Trailing::Submenu => r.push(icon("arrow_right", 20.0, c.on_surface_variant)),
            Trailing::Check(on) => r.push(icon(if on { "check" } else { "" }, 20.0, c.primary)),
        };
        let fill = highlighted.then_some(c.surface_container_highest);
        button(r)
            .width(Fill)
            .height(ITEM_H)
            .padding([0.0, 12.0])
            .on_press(msg)
            .style(move |_, status| {
                let mut s = state_button(fill, c.on_surface, 0.0, status);
                if fill.is_none() && matches!(status, button::Status::Active) {
                    s.background = None;
                }
                s
            })
            .into()
    }

    fn menu_box<'a>(&self, items: Vec<Element<'a, Message>>, width: f32) -> Element<'a, Message> {
        container(column(items).padding([4.0, 0.0])).width(width).clip(true).style(floating(self.colors, shape::MEDIUM)).into()
    }

    fn menu_divider<'a>(&self) -> Element<'a, Message> {
        container(container(space()).width(Fill).height(1).style(crate::ui::divider(self.colors.outline_variant))).padding([4.0, 0.0]).into()
    }

    fn menus(&self) -> Element<'_, Message> {
        let c = self.colors;
        let main = self.menu_box(
            vec![
                self.menu_item("library_add", "Import Folder", Trailing::Hint("Ctrl+O"), false, Message::ImportFolder),
                self.menu_item("refresh", "Rescan", Trailing::Hint("F5"), false, Message::Rescan),
                self.menu_divider(),
                self.menu_item("palette", "Palette", Trailing::Submenu, self.menu == Menu::Palette, Message::OpenSubmenu(Menu::Palette)),
                self.menu_item("dashboard", "Layout", Trailing::Submenu, self.menu == Menu::Layout, Message::OpenSubmenu(Menu::Layout)),
                self.menu_item(
                    if self.dark() { "light_mode" } else { "dark_mode" },
                    if self.dark() { "Light mode" } else { "Dark mode" },
                    Trailing::None,
                    false,
                    Message::ToggleColorMode,
                ),
                self.menu_item("equalizer", "Volume normalization", Trailing::Check(self.config.normalization), false, Message::ToggleNormalization),
                self.menu_divider(),
                self.menu_item("settings", "Settings", Trailing::Hint("Ctrl+,"), false, Message::OpenSettings),
            ],
            MENU_W,
        );
        let right = 16.0;
        let top = if self.custom_title_bar() { 148.0 } else { 108.0 };
        let mut layers = stack![
            mouse_area(container(space()).width(Fill).height(Fill)).on_press(Message::CloseMenu).on_right_press(Message::CloseMenu),
            container(main).width(Fill).height(Fill).align_x(Horizontal::Right).align_y(Vertical::Top).padding(Padding { top, right, bottom: 0.0, left: 0.0 }),
        ];

        let sub: Option<(Element<'_, Message>, f32)> = match self.menu {
            Menu::Palette => {
                let items = PaletteChoice::ALL
                    .into_iter()
                    .map(|choice| {
                        let (seed, variant) = choice.seed(self.art.as_ref().map(|a| a.seed));
                        let pc = Colors::generate(seed, variant, self.dark());
                        let on = self.config.palette == choice;
                        let dot = |color: Color| container(space()).width(14).height(14).style(surface(color, 7.0));
                        let r = row![
                            row![dot(pc.primary), dot(pc.secondary_container), dot(pc.tertiary)].spacing(3),
                            label(choice.label(), scale::BODY_MEDIUM, FONT, c.on_surface).width(Fill),
                            icon(if on { "check" } else { "" }, 20.0, c.primary),
                        ]
                        .spacing(12)
                        .align_y(Alignment::Center);
                        let fill = on.then_some(c.secondary_container);
                        button(r)
                            .width(Fill)
                            .height(ITEM_H)
                            .padding([0.0, 12.0])
                            .on_press(Message::PickPalette(choice))
                            .style(move |_, status| {
                                let mut s = state_button(fill, c.on_surface, 0.0, status);
                                if fill.is_none() && matches!(status, button::Status::Active) {
                                    s.background = None;
                                }
                                s
                            })
                            .into()
                    })
                    .collect();
                Some((self.menu_box(items, 240.0), 2.0))
            }
            Menu::Layout => {
                let mut items: Vec<Element<'_, Message>> = [Slot::List(Pane::Artists), Slot::List(Pane::Albums), Slot::List(Pane::Songs), Slot::AlbumArt]
                    .into_iter()
                    .map(|slot| {
                        let glyph = match slot {
                            Slot::List(Pane::Artists) => "person",
                            Slot::List(Pane::Albums) => "album",
                            Slot::List(Pane::Songs) => "music_note",
                            _ => "image",
                        };
                        self.menu_item(glyph, slot.label(), Trailing::Check(layout::is_visible(&self.panes, slot)), false, Message::TogglePane(slot))
                    })
                    .collect();
                items.push(self.menu_divider());
                items.push(self.menu_item("restart_alt", "Reset Layout", Trailing::None, false, Message::ResetLayout));
                Some((self.menu_box(items, 220.0), 3.0))
            }
            _ => None,
        };
        if let Some((sub, row_index)) = sub {
            // Align with the opening item: menu padding (4) + one divider (9) + rows above, minus the submenu's own padding.
            let sub_top = top + 9.0 + row_index * ITEM_H;
            layers = layers.push(
                container(sub)
                    .width(Fill)
                    .height(Fill)
                    .align_x(Horizontal::Right)
                    .align_y(Vertical::Top)
                    .padding(Padding { top: sub_top, right: right + MENU_W + 4.0, bottom: 0.0, left: 0.0 }),
            );
        }
        layers.into()
    }

    fn context_menu<'a>(&'a self) -> Element<'a, Message> {
        let Some(ctx) = &self.context else { return space().into() };
        let mut items: Vec<Element<'a, Message>> = Vec::new();
        let mut height = 8.0;
        let mut push = |el: Element<'a, Message>, h: f32| {
            items.push(el);
            height += h;
        };
        match ctx.kind {
            ContextKind::Rows(Pane::Queue) => {
                push(self.menu_item("play_arrow", "Play", Trailing::Hint("Enter"), false, Message::Context(Action::Play)), ITEM_H);
                push(self.menu_item("remove_circle_outline", "Remove", Trailing::Hint("Delete"), false, Message::Context(Action::Remove)), ITEM_H);
                push(self.menu_divider(), 9.0);
                push(self.menu_item("person", "Go to Artist", Trailing::None, false, Message::Context(Action::GoToArtist)), ITEM_H);
                push(self.menu_item("album", "Go to Album", Trailing::None, false, Message::Context(Action::GoToAlbum)), ITEM_H);
                push(self.menu_divider(), 9.0);
                push(self.menu_item("folder_open", "Show in Folder", Trailing::None, false, Message::Context(Action::ShowInFolder)), ITEM_H);
            }
            ContextKind::Rows(pane) => {
                push(self.menu_item("play_arrow", "Play", Trailing::Hint("Enter"), false, Message::Context(Action::Play)), ITEM_H);
                push(self.menu_item("queue_play_next", "Play Next", Trailing::Hint("Ctrl+Enter"), false, Message::Context(Action::PlayNext)), ITEM_H);
                push(self.menu_item("playlist_add", "Add to Queue", Trailing::Hint("Shift+Enter"), false, Message::Context(Action::AddToQueue)), ITEM_H);
                push(self.menu_divider(), 9.0);
                if pane != Pane::Artists {
                    push(self.menu_item("person", "Go to Artist", Trailing::None, false, Message::Context(Action::GoToArtist)), ITEM_H);
                }
                if pane == Pane::Songs {
                    push(self.menu_item("album", "Go to Album", Trailing::None, false, Message::Context(Action::GoToAlbum)), ITEM_H);
                }
                if pane != Pane::Artists {
                    push(self.menu_divider(), 9.0);
                }
                push(self.menu_item("folder_open", "Show in Folder", Trailing::None, false, Message::Context(Action::ShowInFolder)), ITEM_H);
            }
            ContextKind::Header(pane) => {
                for def in defs(pane) {
                    if def.fill || def.title.is_empty() {
                        continue;
                    }
                    let visible = self.columns[pane.ix()].cols.iter().any(|c| c.id == def.id && c.visible);
                    push(self.menu_item(if visible { "check_box" } else { "check_box_outline_blank" }, def.title, Trailing::None, false, Message::ToggleColumn(pane, def.id)), ITEM_H);
                }
                push(self.menu_divider(), 9.0);
                push(self.menu_item("restart_alt", "Reset Columns", Trailing::None, false, Message::ResetColumns(pane)), ITEM_H);
            }
            ContextKind::Pane(slot) => {
                push(self.menu_item("visibility_off", "Hide Pane", Trailing::None, false, Message::TogglePane(slot)), ITEM_H);
            }
        }
        let width = 260.0;
        let size = self.window_size;
        let at = Point::new(ctx.at.x.min(size.width - width - 8.0).max(8.0), ctx.at.y.min(size.height - height - 8.0).max(8.0));
        stack![
            mouse_area(container(space()).width(Fill).height(Fill)).on_press(Message::CloseContext).on_right_press(Message::CloseContext),
            pin(self.menu_box(items, width)).x(at.x).y(at.y),
        ]
        .into()
    }

    fn drag_ghost(&self) -> Element<'_, Message> {
        let c = self.colors;
        let Some(drag) = &self.drag else { return space().into() };
        let mut text_col = column![label(fit(&drag.title, 240.0, 14.0), scale::BODY_MEDIUM, FONT_MEDIUM, c.on_surface)];
        if !drag.detail.is_empty() {
            text_col = text_col.push(label(fit(&drag.detail, 240.0, 12.0), scale::LABEL_MEDIUM, FONT, c.on_surface_variant));
        }
        let ghost = container(row![icon(drag.icon, 22.0, c.primary), text_col].spacing(10).align_y(Alignment::Center))
            .padding(Padding { top: 8.0, right: 14.0, bottom: 8.0, left: 10.0 })
            .style(move |_| container::Style {
                background: Some(Background::Color(c.surface_container_high)),
                border: border::rounded(shape::MEDIUM),
                shadow: Shadow { color: Color::from_rgba(0.0, 0.0, 0.0, 0.35), offset: Vector::new(0.0, 4.0), blur_radius: 12.0 },
                ..container::Style::default()
            });
        // No full-window cursor layer here: a layer that sets an interaction hides the cursor
        // from everything below it, and the queue needs it to place the drop line.
        pin(ghost).x(self.cursor.x + 14.0).y(self.cursor.y + 10.0).into()
    }

    // ---- full-screen visualizer ----

    fn viz_fullscreen_view(&self) -> Element<'_, Message> {
        let c = self.colors;
        let glass = |radius: f32| {
            move |_: &iced::Theme| container::Style {
                background: Some(Background::Color(theme::alpha(c.surface_container, 0.82))),
                border: border::rounded(radius),
                text_color: Some(c.on_surface),
                ..container::Style::default()
            }
        };
        let shader = iced::widget::shader(viz::gpu::Program { frame: self.viz_frame.clone() }).width(Fill).height(Fill);
        let screen = container(shader).width(Fill).height(Fill).style(surface(Color::BLACK, 0.0));
        let mut layers = stack![mouse_area(screen).on_press(Message::VizNext)].width(Fill).height(Fill);

        // The preset's name shows briefly whenever it changes.
        if self.viz.since_change() < 3.0 {
            let toast = container(row![icon("auto_awesome", 18.0, c.primary), label(self.viz.preset_name(), scale::TITLE_SMALL, FONT_MEDIUM, c.on_surface)].spacing(8).align_y(Alignment::Center))
                .padding(Padding { top: 6.0, right: 14.0, bottom: 6.0, left: 10.0 })
                .style(glass(12.0));
            layers = layers.push(container(toast).padding(Padding { top: 16.0, right: 0.0, bottom: 0.0, left: 20.0 }));
        }

        if self.viz_ui_visible() {
            let lock = self.viz.locked;
            let (lock_fg, lock_fill) = if lock { (c.on_secondary_container, Some(c.secondary_container)) } else { (c.on_surface_variant, None) };
            let controls = container(
                row![
                    self.preset_chip(32.0, 220.0),
                    label("Click or N next · L lock · Esc exit", scale::LABEL_MEDIUM, FONT_MEDIUM, c.on_surface_variant),
                    icon_button("auto_awesome", 40.0, 22.0, c.on_surface_variant, None, 20.0, Some(Message::VizNext)),
                    icon_button(if lock { "lock" } else { "lock_open" }, 40.0, 22.0, lock_fg, lock_fill, 20.0, Some(Message::VizLock)),
                    icon_button("close_fullscreen", 40.0, 22.0, c.on_surface_variant, None, 20.0, Some(Message::VizFullscreen(false))),
                ]
                .spacing(4)
                .align_y(Alignment::Center),
            )
            .padding(Padding { top: 4.0, right: 4.0, bottom: 4.0, left: 6.0 })
            .style(glass(24.0));
            // Clicks on the controls' glass don't fall through to the visualizer.
            layers = layers.push(container(opaque(controls)).width(Fill).align_x(Horizontal::Right).padding(Padding { top: 12.0, right: 16.0, bottom: 0.0, left: 0.0 }));

            let track = self.queue.now.and_then(|id| self.model.track(id));
            let art: Element<'_, Message> = match &self.art {
                Some(art) if Some(art.track) == self.queue.now => {
                    image(art.small.clone()).width(64).height(64).content_fit(iced::ContentFit::Cover).border_radius(shape::MEDIUM).into()
                }
                _ => container(icon("album", 30.0, c.on_surface_variant)).center(64).style(surface(c.surface_container_highest, shape::MEDIUM)).into(),
            };
            let (title, sub) = track.map_or((String::new(), String::new()), |t| (t.title.clone(), format!("{} · {}", t.artist, t.album)));
            let playing = self.play_state == PlayState::Playing;
            let bar = container(
                row![
                    art,
                    column![
                        canvas(Marquee { text: title, size: scale::TITLE_LARGE.0, line_height: scale::TITLE_LARGE.1, font: FONT, color: c.on_surface, phase: self.marquee_phase }).width(300).height(28),
                        canvas(Marquee { text: sub, size: scale::BODY_LARGE.0, line_height: scale::BODY_LARGE.1, font: FONT, color: c.on_surface_variant, phase: self.marquee_phase }).width(300).height(24),
                    ]
                    .width(300),
                    icon_button(if playing { "pause" } else { "play_arrow" }, 56.0, 30.0, c.on_primary, Some(c.primary), if playing { 18.0 } else { 28.0 }, Some(Message::PlayPause)),
                    self.seek_column(track.is_some()),
                ]
                .spacing(24)
                .align_y(Alignment::Center),
            )
            .padding(Padding { top: 12.0, right: 20.0, bottom: 12.0, left: 12.0 })
            .style(glass(20.0));
            layers = layers.push(container(opaque(bar)).width(Fill).height(Fill).align_y(Vertical::Bottom).padding(Padding { top: 0.0, right: 20.0, bottom: 16.0, left: 20.0 }));
        }
        if self.viz_menu.is_some() {
            layers = layers.push(self.viz_menu());
        }
        layers.into()
    }

    /// The preset's name as a dropdown button: a shuffle mark while presets change by
    /// themselves, a lock once one is chosen.
    fn preset_chip(&self, height: f32, max_width: f32) -> Element<'_, Message> {
        let c = self.colors;
        let mark = if self.viz.locked { "lock" } else { "shuffle" };
        let r = row![
            icon(mark, 14.0, c.on_surface_variant),
            label(fit(self.viz.preset_name(), max_width - 44.0, 12.0), scale::LABEL_MEDIUM, FONT_MEDIUM, c.on_surface),
            icon("arrow_drop_down", 18.0, c.on_surface_variant),
        ]
        .spacing(4)
        .align_y(Alignment::Center);
        button(r)
            .height(height)
            .padding(Padding { top: 0.0, right: 4.0, bottom: 0.0, left: 10.0 })
            .on_press(Message::VizMenu(true))
            .style(move |_, status| state_button(Some(theme::alpha(c.surface_container_highest, 0.6)), c.on_surface, height / 2.0, status))
            .into()
    }

    /// Every preset, with "Automatic" at the top to let them change by themselves again.
    fn viz_menu(&self) -> Element<'_, Message> {
        let Some(at) = self.viz_menu else { return space().into() };
        let current = self.viz.index();
        let mut items = vec![
            self.menu_item("shuffle", "Automatic", Trailing::Check(!self.viz.locked), false, Message::VizPick(None)),
            self.menu_divider(),
        ];
        for (i, preset) in viz::PRESETS.iter().enumerate() {
            let glyph = if preset.fire > 0.0 {
                "local_fire_department"
            } else if preset.tunnel + preset.corridor > 0.0 {
                "view_in_ar"
            } else {
                "auto_awesome"
            };
            let check = Trailing::Check(self.viz.locked && i == current);
            items.push(self.menu_item(glyph, preset.name, check, i == current, Message::VizPick(Some(i))));
        }
        let width = 280.0;
        let size = self.window_size;
        let full = 8.0 + 9.0 + ITEM_H * (viz::PRESETS.len() + 1) as f32;
        let height = full.min(520.0).min(size.height - 16.0);
        let colors = self.colors;
        let list = scrollable(column(items).padding([4.0, 0.0]))
            .height(height)
            .style(move |_, status| crate::ui::table::scroll_style(colors, status));
        let menu = container(list).width(width).clip(true).style(floating(colors, shape::MEDIUM));
        let x = (at.x - 24.0).clamp(8.0, (size.width - width - 8.0).max(8.0));
        let y = if at.y + 16.0 + height <= size.height - 8.0 { at.y + 16.0 } else { (size.height - height - 8.0).max(8.0) };
        stack![
            mouse_area(container(space()).width(Fill).height(Fill)).on_press(Message::VizMenu(false)).on_right_press(Message::VizMenu(false)),
            pin(menu).x(x).y(y),
        ]
        .into()
    }

    // ---- settings ----

    fn settings_dialog<'a>(&'a self) -> Element<'a, Message> {
        let c = self.colors;
        let section = |title: &'static str| label(title, scale::TITLE_SMALL, FONT_MEDIUM, c.primary);
        let caption = |text: &'static str| label(text, scale::LABEL_MEDIUM, FONT, c.on_surface_variant).wrapping(iced::widget::text::Wrapping::Word);
        let divider = || container(space()).width(Fill).height(1).style(crate::ui::divider(c.outline_variant));

        let swatches = row(PaletteChoice::ALL.into_iter().map(|choice| {
            let (seed, variant) = choice.seed(self.art.as_ref().map(|a| a.seed));
            let pc = Colors::generate(seed, variant, self.dark());
            let on = self.config.palette == choice;
            let disc = canvas(Swatch { top: pc.primary, left: pc.secondary_container, right: pc.tertiary, ring: on.then_some(c.on_surface) }).width(60).height(60);
            let face = stack![disc, container(icon(if on { "check" } else { "" }, 22.0, pc.on_primary)).center(60)];
            button(column![face, label(choice.label(), scale::LABEL_MEDIUM, FONT_MEDIUM, c.on_surface)].spacing(6).align_x(Alignment::Center))
                .width(88)
                .padding([6.0, 0.0])
                .on_press(Message::PickPalette(choice))
                .style(move |_, status| {
                    let mut s = state_button(None, c.on_surface, 12.0, status);
                    if matches!(status, button::Status::Active) {
                        s.background = None;
                    }
                    s
                })
                .into()
        }))
        .spacing(4);

        let segmented = |options: Vec<(&'static str, bool, Message)>| {
            let n = options.len();
            let buttons = options.into_iter().enumerate().map(move |(i, (text, on, msg))| {
                let (fg, fill) = if on { (c.on_secondary_container, Some(c.secondary_container)) } else { (c.on_surface, None) };
                let mut content = row![].spacing(6).align_y(Alignment::Center);
                if on {
                    content = content.push(icon("check", 18.0, fg));
                }
                let content = content.push(label(text, scale::TITLE_SMALL, FONT_MEDIUM, fg));
                let el: Element<'_, Message> = button(container(content).center_x(Fill))
                    .width(Fill)
                    .height(38)
                    .padding([0.0, 12.0])
                    .on_press(msg)
                    .style(move |_, status| {
                        let mut s = state_button(fill, fg, 0.0, status);
                        if fill.is_none() && matches!(status, button::Status::Active) {
                            s.background = None;
                        }
                        // Outer segments follow the pill's rounded ends.
                        let (l, r) = (if i == 0 { 19.0 } else { 0.0 }, if i + 1 == n { 19.0 } else { 0.0 });
                        s.border.radius = iced::border::Radius { top_left: l, bottom_left: l, top_right: r, bottom_right: r };
                        s
                    })
                    .into();
                if i + 1 < n { row![el, container(space()).width(1).height(38).style(crate::ui::divider(c.outline))].into() } else { el }
            });
            container(row(buttons).width(330)).clip(true).style(move |_| container::Style {
                border: Border { radius: 20.0.into(), width: 1.0, color: c.outline },
                ..container::Style::default()
            })
        };
        let mode = self.config.color_mode;
        let color_mode = segmented(vec![
            ("Dark", mode == ColorMode::Dark, Message::SetColorMode(ColorMode::Dark)),
            ("Light", mode == ColorMode::Light, Message::SetColorMode(ColorMode::Light)),
            ("System", mode == ColorMode::System, Message::SetColorMode(ColorMode::System)),
        ]);
        let bar = self.config.title_bar;
        let title_bar = segmented(vec![
            ("Custom", bar == TitleBar::Custom, Message::SetTitleBar(TitleBar::Custom)),
            ("System", bar == TitleBar::System, Message::SetTitleBar(TitleBar::System)),
        ]);

        let switch = toggle_switch(c, self.config.normalization, Message::ToggleNormalization);

        let folders: Element<'_, Message> = if self.folders.is_empty() {
            caption("No folders yet. Import one to build your library.").into()
        } else {
            let rows = self.folders.iter().enumerate().map(|(i, folder)| {
                let tracks = self.model.tracks.iter().filter(|t| &t.folder == folder).count();
                let watching = if self.watching() { " · watching for changes" } else { "" };
                let r = row![
                    icon("folder", 24.0, c.on_surface_variant),
                    column![
                        label(fit(&folder.display().to_string(), 420.0, 14.0), scale::BODY_MEDIUM, FONT, c.on_surface),
                        label(format!("{} tracks{watching}", thousands(tracks)), scale::LABEL_MEDIUM, FONT, c.on_surface_variant),
                    ]
                    .width(Fill),
                    icon_button("close", 40.0, 20.0, c.on_surface_variant, None, 20.0, Some(Message::RemoveFolder(folder.clone()))),
                ]
                .spacing(16)
                .align_y(Alignment::Center);
                let mut col = column![container(r).height(56).padding(Padding { top: 0.0, right: 8.0, bottom: 0.0, left: 16.0 }).align_y(Alignment::Center)];
                if i + 1 < self.folders.len() {
                    col = col.push(divider());
                }
                col.into()
            });
            container(column(rows)).clip(true).style(surface(c.surface_container_low, shape::LARGE)).into()
        };

        let delay = text_input("1000", &self.delay_input).on_input(Message::DelayInput).width(90).size(16).padding([10.0, 12.0]).style(move |_, status| text_input::Style {
            background: Background::Color(Color::TRANSPARENT),
            border: Border {
                radius: 4.0.into(),
                width: if matches!(status, text_input::Status::Focused { .. }) { 2.0 } else { 1.0 },
                color: if matches!(status, text_input::Status::Focused { .. }) { c.primary } else { c.outline },
            },
            icon: c.on_surface_variant,
            placeholder: c.on_surface_variant,
            value: c.on_surface,
            selection: theme::alpha(c.primary, 0.3),
        });

        let line = |left: Element<'a, Message>, right: Element<'a, Message>| -> Element<'a, Message> {
            row![container(left).width(Fill), right].spacing(16).align_y(Alignment::Center).into()
        };
        let text_pair = |title: &'static str, sub: &'static str| -> Element<'a, Message> {
            column![
                label(title, scale::BODY_MEDIUM, FONT, c.on_surface),
                label(sub, scale::LABEL_MEDIUM, FONT, c.on_surface_variant).wrapping(iced::widget::text::Wrapping::Word),
            ]
            .into()
        };

        let body = column![
            label("Settings", (24.0, 32.0), FONT, c.on_surface),
            section("Appearance"),
            column![label("Palette", scale::BODY_MEDIUM, FONT, c.on_surface_variant), swatches].spacing(6),
            line(text_pair("Color mode", "System follows your desktop's light/dark setting"), color_mode.into()),
            line(text_pair("Title bar", "System uses your desktop's window decorations"), title_bar.into()),
            divider(),
            section("Playback"),
            line(text_pair("Volume normalization", "Uses ReplayGain tags; measures untagged files in the background"), switch),
            line(
                text_pair("Reduce flashing", "Limits how fast the visualizer can brighten or darken (recommended)"),
                toggle_switch(c, self.config.reduce_flashing, Message::ToggleReduceFlashing),
            ),
            divider(),
            section("Library"),
            folders,
            row![tonal_button(c, "library_add", "Import Folder", Message::ImportFolder), text_button(c, "refresh", "Rescan", Message::Rescan)].spacing(8),
            divider(),
            section("Behavior"),
            line(
                text_pair("Type-to-jump reset", "Pause, in milliseconds, before a new search term starts (200–5000)"),
                row![delay, label("ms", scale::BODY_LARGE, FONT, c.on_surface_variant)].spacing(8).align_y(Alignment::Center).into(),
            ),
            row![space().width(Fill), text_button(c, "", "Done", Message::CloseSettings)],
        ]
        .spacing(16);

        let dialog = container(scrollable(body.padding(Padding { top: 0.0, right: 12.0, bottom: 0.0, left: 0.0 })).style(move |_, status| table::scroll_style(c, status)))
            .width(680)
            .max_height(self.window_size.height - 80.0)
            .padding(Padding { top: 24.0, right: 12.0, bottom: 20.0, left: 24.0 })
            .style(move |_| container::Style {
                background: Some(Background::Color(c.surface_container_high)),
                border: border::rounded(28),
                shadow: Shadow { color: Color::from_rgba(0.0, 0.0, 0.0, 0.3), offset: Vector::new(0.0, 4.0), blur_radius: 16.0 },
                text_color: Some(c.on_surface),
                ..container::Style::default()
            });
        stack![
            mouse_area(container(space()).width(Fill).height(Fill).style(surface(Color::from_rgba(0.0, 0.0, 0.0, 0.32), 0.0))).on_press(Message::CloseSettings),
            container(mouse_area(dialog).on_press(Message::DismissNotice)).center(Fill),
        ]
        .into()
    }
}

/// M3 switch: a pill track with a handle that grows and gains a check when on.
fn toggle_switch<'a>(c: Colors, on: bool, msg: Message) -> Element<'a, Message> {
    button(
        container(container(icon(if on { "check" } else { "" }, 16.0, c.primary)).center(if on { 24.0 } else { 16.0 }).style(surface(if on { c.on_primary } else { c.outline }, 12.0)))
            .width(Fill)
            .height(Fill)
            .align_x(if on { Horizontal::Right } else { Horizontal::Left })
            .align_y(Vertical::Center)
            .padding([0.0, if on { 4.0 } else { 6.0 }]),
    )
    .width(52)
    .height(32)
    .padding(0)
    .on_press(msg)
    .style(move |_, _| button::Style {
        background: Some(Background::Color(if on { c.primary } else { c.surface_container_highest })),
        border: Border { radius: 16.0.into(), width: 2.0, color: if on { c.primary } else { c.outline } },
        ..button::Style::default()
    })
    .into()
}

fn chip<'a>(c: Colors, glyph: &'a str, text: String, dismiss: Option<Message>) -> Element<'a, Message> {
    let mut r = row![icon(glyph, 16.0, c.on_secondary_container), label(text, scale::LABEL_MEDIUM, FONT_MEDIUM, c.on_secondary_container)]
        .spacing(6)
        .align_y(Alignment::Center);
    if let Some(msg) = dismiss {
        r = r.push(icon_button("close", 20.0, 14.0, c.on_secondary_container, None, 10.0, Some(msg)));
    }
    container(r).padding([2.0, 10.0]).style(surface(c.secondary_container, 12.0)).into()
}

fn tonal_button<'a>(c: Colors, glyph: &'a str, text: &'a str, msg: Message) -> Element<'a, Message> {
    button(row![icon(glyph, 18.0, c.on_secondary_container), label(text, scale::TITLE_SMALL, FONT_MEDIUM, c.on_secondary_container)].spacing(8).align_y(Alignment::Center))
        .padding(Padding { top: 10.0, right: 24.0, bottom: 10.0, left: 16.0 })
        .on_press(msg)
        .style(move |_, status| state_button(Some(c.secondary_container), c.on_secondary_container, 20.0, status))
        .into()
}

fn text_button<'a>(c: Colors, glyph: &'a str, text: &'a str, msg: Message) -> Element<'a, Message> {
    let mut r = row![].spacing(8).align_y(Alignment::Center);
    if !glyph.is_empty() {
        r = r.push(icon(glyph, 18.0, c.primary));
    }
    button(r.push(label(text, scale::TITLE_SMALL, FONT_MEDIUM, c.primary)))
        .padding([10.0, 12.0])
        .on_press(msg)
        .style(move |_, status| {
            let mut s = state_button(None, c.primary, 20.0, status);
            if matches!(status, button::Status::Active) {
                s.background = None;
            }
            s
        })
        .into()
}

/// Invisible edge and corner grips for the undecorated window.
fn resize_handles<'a>() -> Element<'a, Message> {
    use window::Direction as D;
    const G: f32 = 6.0;
    let grip = |w: Length, h: Length, dir: D, cursor: mouse::Interaction| -> Element<'a, Message> {
        mouse_area(Space::new().width(w).height(h)).on_press(Message::ResizeWindow(dir)).interaction(cursor).into()
    };
    let h = mouse::Interaction::ResizingHorizontally;
    let v = mouse::Interaction::ResizingVertically;
    let d1 = mouse::Interaction::ResizingDiagonallyDown;
    let d2 = mouse::Interaction::ResizingDiagonallyUp;
    column![
        row![grip(G.into(), G.into(), D::NorthWest, d1), grip(Fill, G.into(), D::North, v), grip(G.into(), G.into(), D::NorthEast, d2)],
        row![grip(G.into(), Fill, D::West, h), Space::new().width(Fill).height(Fill), grip(G.into(), Fill, D::East, h)].height(Fill),
        row![grip(G.into(), G.into(), D::SouthWest, d2), grip(Fill, G.into(), D::South, v), grip(G.into(), G.into(), D::SouthEast, d1)],
    ]
    .width(Fill)
    .height(Fill)
    .into()
}
