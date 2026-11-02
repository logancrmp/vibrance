//! Virtualized table used by every list pane: only visible rows are built.

use iced::alignment::Horizontal;
use iced::widget::scrollable::{self, Direction, Scrollbar};
use iced::widget::{Id, canvas, column, container, mouse_area, responsive, row, space, stack};
use iced::{Alignment, Background, Color, Element, Fill, Padding, border, mouse};

use super::marquee::Marquee;
use super::{fit, icon, label, surface};
use crate::app::{Message, Pane};
use crate::columns::{ColId, Columns, def_of};
use crate::theme::{self, Colors, FONT, FONT_MEDIUM, FONT_SEMIBOLD, scale};

pub const ROW_H: f32 = 32.0;
pub const HEADER_H: f32 = 36.0;
pub const LIST_PAD: f32 = 4.0;
const GAP: f32 = 12.0;
/// Horizontal padding outside the cells on each side (row inset + row padding).
const EDGE: f32 = 16.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Primary,
    Secondary,
    Accent,
    Icon,
}

pub struct Cell {
    pub text: String,
    pub tone: Tone,
    pub strong: bool,
    /// Scroll the text back and forth when it doesn't fit (seconds since it appeared).
    pub marquee: Option<f32>,
}

impl Cell {
    pub fn new(text: impl Into<String>, tone: Tone) -> Cell {
        Cell { text: text.into(), tone, strong: false, marquee: None }
    }
}

pub struct Row {
    pub cells: Vec<Cell>,
    pub selected: bool,
    /// Unplayable or missing: drawn faded.
    pub dim: bool,
}

pub fn scroll_id(pane: Pane) -> Id {
    match pane {
        Pane::Artists => Id::new("list-artists"),
        Pane::Albums => Id::new("list-albums"),
        Pane::Songs => Id::new("list-songs"),
        Pane::Queue => Id::new("list-queue"),
    }
}

pub struct Table<'a, F, E> {
    pub pane: Pane,
    pub colors: Colors,
    pub columns: &'a Columns,
    pub sort: Option<(ColId, bool)>,
    pub sortable: bool,
    pub total: usize,
    pub offset: f32,
    pub hovered: Option<usize>,
    /// Queue only: where a drag would drop, as an insertion index.
    pub drop_at: Option<usize>,
    pub row: F,
    pub empty: E,
}

pub fn table<'a, F, E>(table: Table<'a, F, E>) -> Element<'a, Message>
where
    F: Fn(usize) -> Row + 'a,
    E: Fn() -> Element<'a, Message> + 'a,
{
    responsive(move |size| {
        let t = &table;
        let widths = t.columns.layout(size.width, GAP, 2.0 * EDGE);
        let list_height = size.height - HEADER_H - 1.0;
        let body: Element<'a, Message> = if t.total == 0 { (t.empty)() } else { list(t, &widths, list_height) };
        column![header(t, &widths), divider(t.colors), body].into()
    })
    .into()
}

fn header<'a, F, E>(t: &Table<'a, F, E>, widths: &[f32]) -> Element<'a, Message> {
    let (pane, colors) = (t.pane, t.colors);
    let visible: Vec<_> = t.columns.visible().collect();
    let mut cells = row![].height(HEADER_H);
    for (i, (col, &w)) in visible.iter().zip(widths).enumerate() {
        let def = def_of(pane, col.id);
        let sorted = t.sort.filter(|&(c, _)| c == col.id);
        let room = if sorted.is_some() { w - 18.0 } else { w };
        let mut content = row![label(fit(def.title, room, 12.0), scale::LABEL_MEDIUM, FONT_MEDIUM, colors.on_surface_variant)]
            .spacing(2)
            .align_y(Alignment::Center);
        if let Some((_, desc)) = sorted {
            content = content.push(icon(if desc { "arrow_downward" } else { "arrow_upward" }, 16.0, colors.primary));
        }
        let cell = container(content)
            .width(w)
            .height(HEADER_H)
            .align_y(Alignment::Center)
            .align_x(if def.numeric { Horizontal::Right } else { Horizontal::Left });
        let mut hit = mouse_area(cell).on_right_press(Message::HeaderMenu(pane)).on_enter(Message::HeaderEnter(pane, i));
        if t.sortable || pane == Pane::Queue {
            hit = hit.on_press(Message::HeaderPress(pane, i)).interaction(mouse::Interaction::Pointer);
        }
        cells = cells.push(hit);
        if i + 1 < visible.len() {
            let grip = container(container(space()).width(1).height(16).style(super::divider(theme::alpha(colors.on_surface_variant, 0.3))))
                .center_x(GAP)
                .center_y(HEADER_H);
            cells = cells.push(mouse_area(grip).on_press(Message::ResizeStart(pane, i)).interaction(mouse::Interaction::ResizingHorizontally));
        }
    }
    container(cells).padding([0.0, EDGE]).into()
}

fn list<'a, F, E>(t: &Table<'a, F, E>, widths: &[f32], height: f32) -> Element<'a, Message>
where
    F: Fn(usize) -> Row + 'a,
{
    let (pane, colors) = (t.pane, t.colors);
    let first = (((t.offset - LIST_PAD) / ROW_H).floor().max(0.0) as usize).min(t.total);
    let last = (first + (height / ROW_H).ceil() as usize + 2).min(t.total);
    let visible: Vec<_> = t.columns.visible().map(|c| def_of(pane, c.id)).collect();

    let mut items = column![space().height(LIST_PAD + first as f32 * ROW_H)];
    for i in first..last {
        let r = (t.row)(i);
        let selected = r.selected;
        let (mut fg, mut fg2) = if selected {
            (colors.on_secondary_container, colors.on_secondary_container)
        } else {
            (colors.on_surface, colors.on_surface_variant)
        };
        if r.dim {
            fg = theme::alpha(fg, 0.5);
            fg2 = theme::alpha(fg2, 0.5);
        }
        let cells = r.cells.into_iter().zip(visible.iter().zip(widths)).map(|(cell, (def, &w))| {
            let color = match cell.tone {
                Tone::Primary => fg,
                Tone::Secondary => fg2,
                Tone::Accent | Tone::Icon if selected => colors.on_secondary_container,
                Tone::Accent | Tone::Icon => colors.primary,
            };
            let font = if cell.strong { FONT_SEMIBOLD } else { FONT };
            let content: Element<'a, Message> = if cell.tone == Tone::Icon {
                icon_cell(&cell.text, color)
            } else if let Some(phase) = cell.marquee {
                canvas(Marquee { text: cell.text, size: 14.0, line_height: 20.0, font, color, phase }).width(w).height(20).into()
            } else {
                label(fit(&cell.text, w, 14.0), scale::BODY_MEDIUM, font, color)
                    .width(w)
                    .align_x(if def.numeric { Horizontal::Right } else { Horizontal::Left })
                    .into()
            };
            container(content).width(w).clip(true).into()
        });
        let bg = if selected {
            colors.secondary_container
        } else if t.hovered == Some(i) {
            theme::alpha(colors.on_surface, theme::state::HOVER)
        } else {
            Color::TRANSPARENT
        };
        let line = container(row(cells).spacing(GAP).align_y(Alignment::Center))
            .height(ROW_H)
            .width(Fill)
            .padding([0.0, 12.0])
            .align_y(Alignment::Center)
            .style(surface(bg, theme::shape::SMALL));
        items = items.push(
            mouse_area(container(line).padding([0.0, 4.0]))
                .on_press(Message::RowPress(pane, i))
                .on_double_click(Message::RowActivate(pane, i))
                .on_right_press(Message::RowMenu(pane, i))
                .on_enter(Message::RowHover(pane, i, true))
                .on_exit(Message::RowHover(pane, i, false)),
        );
    }
    items = items.push(space().height((t.total - last) as f32 * ROW_H + LIST_PAD));

    let scroll = iced::widget::scrollable(items)
        .id(scroll_id(pane))
        .height(Fill)
        .width(Fill)
        .direction(Direction::Vertical(Scrollbar::new().width(6).scroller_width(6).margin(2)))
        .on_scroll(move |vp| Message::Scrolled(pane, vp.absolute_offset().y, vp.bounds().height))
        .style(move |_, status| scroll_style(colors, status));

    let mut layers = stack![scroll].width(Fill).height(Fill);
    if let Some(at) = t.drop_at {
        let y = LIST_PAD + at as f32 * ROW_H - t.offset - 1.0;
        if (-2.0..height).contains(&y) {
            let line = row![
                container(space()).width(8).height(8).style(surface(colors.primary, 4.0)),
                container(space()).width(Fill).height(2).style(surface(colors.primary, 1.0)),
            ]
            .align_y(Alignment::Center)
            .padding([0.0, 6.0]);
            layers = layers.push(container(line).padding(Padding { top: (y - 3.0).max(0.0), ..Padding::ZERO }).width(Fill));
        }
    }
    if pane == Pane::Queue {
        let area = mouse_area(layers).on_move(move |p| Message::DropHover(Some(p.y))).on_exit(Message::DropHover(None));
        let area = if t.drop_at.is_some() { area.interaction(mouse::Interaction::Grabbing) } else { area };
        area.into()
    } else {
        layers.into()
    }
}

fn divider<'a>(colors: Colors) -> Element<'a, Message> {
    container(space()).width(Fill).height(1).style(super::divider(colors.outline_variant)).into()
}

fn icon_cell<'a>(name: &str, color: Color) -> Element<'a, Message> {
    let name: &'static str = match name {
        "graphic_eq" => "graphic_eq",
        "pause" => "pause",
        "error" => "error",
        _ => "",
    };
    icon(name, 18.0, color).into()
}

pub fn scroll_style(colors: Colors, status: scrollable::Status) -> scrollable::Style {
    let active = matches!(
        status,
        scrollable::Status::Hovered { is_vertical_scrollbar_hovered: true, .. } | scrollable::Status::Dragged { .. }
    );
    let rail = scrollable::Rail {
        background: None,
        border: border::rounded(3),
        scroller: scrollable::Scroller {
            background: Background::Color(if active { colors.outline } else { colors.outline_variant }),
            border: border::rounded(3),
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        auto_scroll: scrollable::AutoScroll {
            background: Background::Color(colors.surface_container_high),
            border: border::rounded(12),
            shadow: iced::Shadow::default(),
            icon: colors.on_surface,
        },
    }
}

/// Insertion index in the queue for a cursor `y` (list-local) at scroll `offset`.
pub fn drop_index(y: f32, offset: f32, len: usize) -> usize {
    (((y + offset - LIST_PAD) / ROW_H) + 0.5).floor().clamp(0.0, len as f32) as usize
}
