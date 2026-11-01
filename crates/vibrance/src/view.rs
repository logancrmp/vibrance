//! The window: title bar, now-playing strip, pane grid, and menus (SPEC 5).

use iced::alignment::{Horizontal, Vertical};
use iced::widget::{
    Space, button, canvas, column, container, image, mouse_area, pane_grid, responsive, row, space, stack,
};
use iced::{Alignment, Background, Border, Color, Element, Fill, Length, Padding, border, mouse, window};
use vibrance_audio::PlayState;

use crate::app::{App, Menu, Message, Pane, Slot, thousands};
use crate::model::{AlbumCol, ArtistCol, SongCol, format_duration};
use crate::queue::Repeat;
use crate::theme::{self, Colors, FONT, FONT_MEDIUM, PaletteChoice, scale, shape};
use crate::ui::seek::{self, SeekBar};
use crate::ui::spectrum::Spectrum;
use crate::ui::table::{self, Body, Cell, Col, Tone, Width};
use crate::ui::volume::Volume;
use crate::ui::{fit, floating, icon, icon_button, label, state_button, surface};

const ARTIST_COLS: [Col; 3] = [
    Col { title: "Artist", width: Width::Portion(1), numeric: false },
    Col { title: "Albums", width: Width::Fixed(52.0), numeric: true },
    Col { title: "Tracks", width: Width::Fixed(52.0), numeric: true },
];
const ALBUM_COLS: [Col; 5] = [
    Col { title: "Album", width: Width::Portion(3), numeric: false },
    Col { title: "Artist", width: Width::Portion(2), numeric: false },
    Col { title: "Year", width: Width::Fixed(40.0), numeric: true },
    Col { title: "Tracks", width: Width::Fixed(48.0), numeric: true },
    Col { title: "Duration", width: Width::Fixed(68.0), numeric: true },
];
const SONG_COLS: [Col; 5] = [
    Col { title: "#", width: Width::Fixed(28.0), numeric: true },
    Col { title: "Title", width: Width::Portion(4), numeric: false },
    Col { title: "Artist", width: Width::Portion(2), numeric: false },
    Col { title: "Album", width: Width::Portion(2), numeric: false },
    Col { title: "Duration", width: Width::Fixed(60.0), numeric: true },
];
const QUEUE_COLS: [Col; 5] = [
    Col { title: "", width: Width::Fixed(20.0), numeric: false },
    Col { title: "#", width: Width::Fixed(24.0), numeric: true },
    Col { title: "Title", width: Width::Portion(3), numeric: false },
    Col { title: "Artist", width: Width::Portion(2), numeric: false },
    Col { title: "Duration", width: Width::Fixed(52.0), numeric: true },
];

impl App {
    pub fn view(&self) -> Element<'_, Message> {
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
        layers.into()
    }

    fn title_bar(&self) -> Element<'_, Message> {
        let c = self.colors;
        let mut bar = row![
            icon("graphic_eq", 20.0, c.primary),
            label("Vibrance", scale::TITLE_SMALL, FONT_MEDIUM, c.on_surface),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if let Some(scan) = &self.scan_label {
            bar = bar.push(
                container(
                    row![icon("sync", 16.0, c.on_secondary_container), label(scan.clone(), scale::LABEL_MEDIUM, FONT_MEDIUM, c.on_secondary_container)]
                        .spacing(6)
                        .align_y(Alignment::Center),
                )
                .padding([2.0, 10.0])
                .style(surface(c.secondary_container, 12.0)),
            );
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
            _ => container(icon("album", 36.0, c.on_surface_variant))
                .center(80)
                .style(surface(c.surface_container_highest, shape::MEDIUM))
                .into(),
        };

        let (title, sub) = match track {
            Some(t) => (t.title.clone(), format!("{} · {}", t.artist, t.album)),
            None => ("Nothing playing".to_owned(), "Double-click a song to play it".to_owned()),
        };
        let info = column![
            label(fit(&title, 236.0, 22.0), scale::TITLE_LARGE, FONT, c.on_surface),
            label(fit(&sub, 236.0, 16.0), scale::BODY_LARGE, FONT, c.on_surface_variant),
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

        let fraction = self.displayed_fraction();
        let duration = self.duration.unwrap_or_default();
        let shown = duration.mul_f32(fraction.clamp(0.0, 1.0));
        let seek = column![
            canvas(SeekBar { fraction, lift: self.seek_lift, drag: self.seek_drag, colors: c, enabled: track.is_some() })
                .width(Fill)
                .height(seek::HEIGHT),
            row![
                label(format_duration(shown.as_millis() as u64), scale::LABEL_MEDIUM, FONT_MEDIUM, c.on_surface_variant),
                space().width(Fill),
                label(format!("-{}", format_duration(duration.saturating_sub(shown).as_millis() as u64)), scale::LABEL_MEDIUM, FONT_MEDIUM, c.on_surface_variant),
            ]
            .padding([0.0, 2.0]),
        ]
        .width(Fill);

        let spectrum = container(canvas(Spectrum { bars: &self.bars, peaks: &self.peaks, colors: c, gap: 2.0 }).width(168).height(48)).padding(4);

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

    fn pane_grid(&self) -> Element<'_, Message> {
        let c = self.colors;
        pane_grid(&self.panes, |_, slot, _| match *slot {
            Slot::List(pane) => pane_grid::Content::new(self.pane_body(pane))
                .title_bar(pane_grid::TitleBar::new(self.pane_title(pane)).style(|_| container::Style::default()))
                .style(surface(c.surface_container_low, shape::LARGE)),
            Slot::AlbumArt => pane_grid::Content::new(self.album_art()).style(surface(c.surface_container_low, shape::LARGE)),
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

    fn pane_title(&self, pane: Pane) -> Element<'_, Message> {
        let c = self.colors;
        let (name, count) = match pane {
            Pane::Artists => ("Artists", self.artist_rows.len()),
            Pane::Albums => ("Albums", self.album_rows.len()),
            Pane::Songs => ("Songs", self.song_rows.len()),
            Pane::Queue => ("Queue", self.queue.items.len()),
        };
        let mut bar = row![
            label(name, scale::TITLE_SMALL, FONT_MEDIUM, c.on_surface),
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
        container(bar).height(44).width(Fill).padding(Padding { top: 0.0, right: 6.0, bottom: 0.0, left: 16.0 }).align_y(Alignment::Center).into()
    }

    fn pane_body(&self, pane: Pane) -> Element<'_, Message> {
        let c = self.colors;
        let (cols, sort): (&[Col], Option<(usize, bool)>) = match pane {
            Pane::Artists => (&ARTIST_COLS, Some(match self.sort_artists {
                None => (0, false),
                Some((col, d)) => ([ArtistCol::Name, ArtistCol::Albums, ArtistCol::Tracks].iter().position(|&x| x == col).unwrap_or(0), d),
            })),
            Pane::Albums => (&ALBUM_COLS, Some(match self.sort_albums {
                None => (1, false),
                Some((col, d)) => ([AlbumCol::Name, AlbumCol::Artist, AlbumCol::Year, AlbumCol::Tracks, AlbumCol::Duration].iter().position(|&x| x == col).unwrap_or(0), d),
            })),
            Pane::Songs => (&SONG_COLS, self.sort_songs.map(|(col, d)| {
                ([SongCol::Number, SongCol::Title, SongCol::Artist, SongCol::Album, SongCol::Duration].iter().position(|&x| x == col).unwrap_or(0), d)
            })),
            Pane::Queue => (&QUEUE_COLS, None),
        };
        let total = self.row_count(pane);
        let list: Element<'_, Message> = if total == 0 {
            self.empty_state(pane)
        } else {
            table::body(Body {
                pane,
                colors: c,
                cols,
                total,
                offset: self.offsets[pane as usize],
                hovered: self.hovered[pane as usize],
                row: move |i| self.table_row(pane, i),
            })
        };

        let mut body = column![table::header(c, pane, cols, sort, pane != Pane::Queue), table::rule(c), list];
        if pane == Pane::Queue {
            let total_ms: u64 = self.queue.items.iter().filter_map(|&id| self.model.track(id)).map(|t| t.duration_ms).sum();
            let footer = if self.queue.items.is_empty() {
                if self.queue.now.is_some() { "Empty · the current song keeps playing".to_owned() } else { "Empty".to_owned() }
            } else {
                format!("{} songs · {}", thousands(self.queue.items.len()), format_duration(total_ms))
            };
            body = body.push(table::rule(c)).push(
                container(label(footer, scale::LABEL_MEDIUM, FONT_MEDIUM, c.on_surface_variant))
                    .height(40)
                    .padding([0.0, 16.0])
                    .align_y(Alignment::Center),
            );
        }

        let mut layers = stack![body.height(Fill)].width(Fill).height(Fill);
        if let Some(term) = self.jump_term(pane) {
            let chip = container(label(term.to_owned(), scale::TITLE_MEDIUM, FONT_MEDIUM, c.on_surface))
                .padding([4.0, 12.0])
                .style(move |_| container::Style {
                    background: Some(Background::Color(c.surface_container_highest)),
                    border: border::rounded(shape::SMALL),
                    shadow: iced::Shadow { color: Color::from_rgba(0.0, 0.0, 0.0, 0.3), offset: iced::Vector::new(0.0, 1.0), blur_radius: 3.0 },
                    ..container::Style::default()
                });
            layers = layers.push(container(chip).width(Fill).align_x(Horizontal::Right).padding(Padding { top: 44.0, right: 16.0, bottom: 0.0, left: 0.0 }));
        }
        mouse_area(layers).on_press(Message::Focus(pane)).into()
    }

    fn table_row(&self, pane: Pane, i: usize) -> table::Row {
        let selected = self.is_selected(pane, i);
        let cells = match pane {
            Pane::Artists if i == 0 => {
                let tracks: usize = self.model.artists.iter().map(|a| a.tracks).sum();
                vec![
                    Cell::new(format!("All ({})", thousands(self.model.artists.len())), Tone::Primary),
                    Cell::new(thousands(self.model.albums.len()), Tone::Secondary),
                    Cell::new(thousands(tracks), Tone::Secondary),
                ]
            }
            Pane::Artists => {
                let a = &self.model.artists[self.artist_rows[i - 1]];
                vec![
                    Cell::new(a.name.clone(), Tone::Primary),
                    Cell::new(a.albums.len().to_string(), Tone::Secondary),
                    Cell::new(thousands(a.tracks), Tone::Secondary),
                ]
            }
            Pane::Albums if i == 0 => {
                let tracks: usize = self.album_rows.iter().map(|&a| self.model.albums[a].tracks.len()).sum();
                let ms: u64 = self.album_rows.iter().map(|&a| self.model.albums[a].duration_ms).sum();
                vec![
                    Cell::new(format!("All ({})", thousands(self.album_rows.len())), Tone::Primary),
                    Cell::new("", Tone::Secondary),
                    Cell::new("", Tone::Secondary),
                    Cell::new(thousands(tracks), Tone::Secondary),
                    Cell::new(format_duration(ms), Tone::Secondary),
                ]
            }
            Pane::Albums => {
                let a = &self.model.albums[self.album_rows[i - 1]];
                vec![
                    Cell::new(a.name.clone(), Tone::Primary),
                    Cell::new(a.artist.clone(), Tone::Secondary),
                    Cell::new(a.year.map(|y| y.to_string()).unwrap_or_default(), Tone::Secondary),
                    Cell::new(a.tracks.len().to_string(), Tone::Secondary),
                    Cell::new(format_duration(a.duration_ms), Tone::Secondary),
                ]
            }
            Pane::Songs => {
                let t = &self.model.tracks[self.song_rows[i]];
                let playing = self.queue.now == Some(t.id);
                vec![
                    Cell::new(t.track_no.map(|n| n.to_string()).unwrap_or_default(), Tone::Secondary),
                    Cell::new(t.title.clone(), if playing { Tone::Accent } else { Tone::Primary }),
                    Cell::new(t.artist.clone(), Tone::Secondary),
                    Cell::new(t.album.clone(), Tone::Secondary),
                    Cell::new(format_duration(t.duration_ms), Tone::Secondary),
                ]
            }
            Pane::Queue => {
                let id = self.queue.items[i];
                let current = self.queue.current == Some(i);
                let played = self.queue.current.is_some_and(|c| i < c) || (self.queue.current.is_none() && i < self.queue.cursor());
                let (title, artist, dur) = match self.model.track(id) {
                    Some(t) => (t.title.clone(), t.artist.clone(), format_duration(t.duration_ms)),
                    None => ("Missing file".to_owned(), String::new(), String::new()),
                };
                let indicator = if current {
                    if self.play_state == PlayState::Playing { "graphic_eq" } else { "pause" }
                } else {
                    ""
                };
                vec![
                    Cell::new(indicator, Tone::Icon),
                    Cell::new((i + 1).to_string(), Tone::Secondary),
                    Cell { text: title, tone: if current { Tone::Accent } else if played { Tone::Secondary } else { Tone::Primary }, strong: current },
                    Cell::new(artist, Tone::Secondary),
                    Cell::new(dur, Tone::Secondary),
                ]
            }
        };
        table::Row { cells, selected }
    }

    fn empty_state(&self, pane: Pane) -> Element<'_, Message> {
        let c = self.colors;
        let (glyph, title, hint): (&str, &str, &str) = match pane {
            Pane::Queue => ("queue_music", "Queue is empty", "Double-click anything to play it."),
            _ if self.model.tracks.is_empty() => ("library_music", "Your library is empty", "Import a folder to get started."),
            _ => ("search_off", "Nothing here", ""),
        };
        let mut col = column![
            icon(glyph, 40.0, c.outline),
            label(title, scale::BODY_LARGE, FONT, c.on_surface),
            label(hint, scale::BODY_MEDIUM, FONT, c.on_surface_variant),
        ]
        .spacing(8)
        .align_x(Alignment::Center);
        if pane == Pane::Songs && self.model.tracks.is_empty() {
            col = col.push(Space::new().height(8)).push(
                button(row![icon("library_add", 18.0, c.on_secondary_container), label("Import Folder", scale::TITLE_SMALL, FONT_MEDIUM, c.on_secondary_container)].spacing(8).align_y(Alignment::Center))
                    .padding(Padding { top: 10.0, right: 24.0, bottom: 10.0, left: 16.0 })
                    .on_press(Message::ImportFolder)
                    .style(move |_, status| state_button(Some(c.secondary_container), c.on_secondary_container, 20.0, status)),
            );
        }
        container(col).center(Fill).padding(24).into()
    }

    fn album_art(&self) -> Element<'_, Message> {
        let c = self.colors;
        match &self.art {
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
            _ => container(
                container(icon("album", 64.0, c.outline)).center(Fill).style(surface(c.surface_container_high, shape::MEDIUM)),
            )
            .padding(12)
            .into(),
        }
    }

    fn menus(&self) -> Element<'_, Message> {
        let c = self.colors;
        let item = |glyph: &'static str, text: &'static str, hint: &'static str, sub: bool, highlighted: bool, msg: Message| -> Element<'_, Message> {
            let mut r = row![icon(glyph, 20.0, c.on_surface_variant), label(text, scale::BODY_MEDIUM, FONT, c.on_surface).width(Fill)]
                .spacing(12)
                .align_y(Alignment::Center);
            if !hint.is_empty() {
                r = r.push(label(hint, scale::LABEL_MEDIUM, FONT, c.on_surface_variant));
            }
            if sub {
                r = r.push(icon("arrow_right", 20.0, c.on_surface_variant));
            }
            let fill = highlighted.then_some(c.surface_container_highest);
            button(r)
                .width(Fill)
                .height(40)
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
        };
        let main = container(
            column![
                item("library_add", "Import Folder", "Ctrl+O", false, false, Message::ImportFolder),
                item("refresh", "Rescan", "F5", false, false, Message::Rescan),
                item("palette", "Palette", "", true, self.menu == Menu::Palette, Message::OpenPaletteMenu),
                item(
                    if self.config.color_mode == theme::ColorMode::Light { "dark_mode" } else { "light_mode" },
                    if self.config.color_mode == theme::ColorMode::Light { "Dark mode" } else { "Light mode" },
                    "",
                    false,
                    false,
                    Message::ToggleColorMode,
                ),
            ]
            .padding([4.0, 0.0]),
        )
        .width(232)
        .clip(true)
        .style(floating(c, shape::MEDIUM));

        let mut layers = stack![
            mouse_area(container(space()).width(Fill).height(Fill)).on_press(Message::CloseMenu),
            container(main).width(Fill).height(Fill).align_x(Horizontal::Right).align_y(Vertical::Top).padding(Padding { top: 148.0, right: 16.0, bottom: 0.0, left: 0.0 }),
        ];

        if self.menu == Menu::Palette {
            let palettes = PaletteChoice::ALL.into_iter().map(|choice| {
                let (seed, variant) = choice.seed(self.art.as_ref().map(|a| a.seed));
                let dark = self.config.color_mode != theme::ColorMode::Light;
                let pc = Colors::generate(seed, variant, dark);
                let on = self.config.palette == choice;
                let dot = |color: Color| container(space()).width(14).height(14).style(surface(color, 7.0));
                let r = row![
                    row![dot(pc.primary), dot(pc.secondary_container), dot(pc.tertiary)].spacing(3),
                    label(choice.label(), scale::BODY_MEDIUM, FONT, c.on_surface).width(Fill),
                    if on { icon("check", 20.0, c.primary) } else { icon("", 20.0, c.primary) },
                ]
                .spacing(12)
                .align_y(Alignment::Center);
                let fill = on.then_some(c.secondary_container);
                button(r)
                    .width(Fill)
                    .height(40)
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
            });
            let sub = container(column(palettes).padding([4.0, 0.0])).width(240).clip(true).style(floating(c, shape::MEDIUM));
            layers = layers.push(
                container(sub).width(Fill).height(Fill).align_x(Horizontal::Right).align_y(Vertical::Top).padding(Padding { top: 228.0, right: 252.0, bottom: 0.0, left: 0.0 }),
            );
        }
        layers.into()
    }
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
