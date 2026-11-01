//! Virtualized table used by every list pane: only visible rows are built.

use iced::alignment::Horizontal;
use iced::widget::scrollable::{self, Direction, Scrollbar};
use iced::widget::{Id, button, column, container, mouse_area, responsive, row, space};
use iced::{Alignment, Background, Color, Element, Fill, Length, border};

use super::{fit, icon, label, state_button, surface};
use crate::app::{Message, Pane};
use crate::theme::{self, Colors, FONT, FONT_MEDIUM, FONT_SEMIBOLD, scale};

pub const ROW_H: f32 = 32.0;
pub const HEADER_H: f32 = 36.0;
const LIST_PAD: f32 = 4.0;
const CELL_GAP: f32 = 12.0;

#[derive(Debug, Clone, Copy)]
pub enum Width {
    Fixed(f32),
    Portion(u16),
}

impl Width {
    fn length(self) -> Length {
        match self {
            Width::Fixed(px) => Length::Fixed(px),
            Width::Portion(p) => Length::FillPortion(p),
        }
    }
}

pub struct Col {
    pub title: &'static str,
    pub width: Width,
    pub numeric: bool,
}

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
}

impl Cell {
    pub fn new(text: impl Into<String>, tone: Tone) -> Cell {
        Cell { text: text.into(), tone, strong: false }
    }
}

pub struct Row {
    pub cells: Vec<Cell>,
    pub selected: bool,
}

pub fn scroll_id(pane: Pane) -> Id {
    match pane {
        Pane::Artists => Id::new("list-artists"),
        Pane::Albums => Id::new("list-albums"),
        Pane::Songs => Id::new("list-songs"),
        Pane::Queue => Id::new("list-queue"),
    }
}

/// Column header row. `sort` is (column index, descending); `None` hides arrows.
pub fn header<'a>(colors: Colors, pane: Pane, cols: &'a [Col], sort: Option<(usize, bool)>, sortable: bool) -> Element<'a, Message> {
    let cells = cols.iter().enumerate().map(|(i, col)| {
        let arrow = sort.filter(|&(c, _)| c == i).map(|(_, desc)| if desc { "arrow_downward" } else { "arrow_upward" });
        let mut content = row![label(col.title, scale::LABEL_MEDIUM, FONT_MEDIUM, colors.on_surface_variant)]
            .spacing(2)
            .align_y(Alignment::Center);
        if let Some(arrow) = arrow {
            content = content.push(icon(arrow, 16.0, colors.primary));
        }
        let cell = container(content)
            .width(Fill)
            .height(HEADER_H)
            .align_y(Alignment::Center)
            .align_x(if col.numeric { Horizontal::Right } else { Horizontal::Left });
        let el: Element<'a, Message> = if sortable {
            button(cell)
                .padding(0)
                .width(col.width.length())
                .on_press(Message::SortBy(pane, i))
                .style(move |_, status| {
                    let mut s = state_button(None, colors.on_surface_variant, 4.0, status);
                    if !matches!(status, button::Status::Hovered | button::Status::Pressed) {
                        s.background = None;
                    }
                    s
                })
                .into()
        } else {
            container(cell).width(col.width.length()).into()
        };
        el
    });
    container(row(cells).spacing(CELL_GAP).height(HEADER_H))
        .padding([0.0, 16.0])
        .style(move |_| container::Style {
            border: iced::Border { color: Color::TRANSPARENT, width: 0.0, radius: 0.0.into() },
            ..container::Style::default()
        })
        .into()
}

/// Thin divider under the header.
pub fn rule<'a>(colors: Colors) -> Element<'a, Message> {
    container(space()).width(Fill).height(1).style(super::divider(colors.outline_variant)).into()
}

pub struct Body<'a, F> {
    pub pane: Pane,
    pub colors: Colors,
    pub cols: &'a [Col],
    pub total: usize,
    pub offset: f32,
    pub hovered: Option<usize>,
    pub row: F,
}

pub fn body<'a, F>(table: Body<'a, F>) -> Element<'a, Message>
where
    F: Fn(usize) -> Row + 'a,
{
    responsive(move |size| {
        let Body { pane, colors, cols, total, offset, hovered, row: ref make_row } = table;
        let first = (((offset - LIST_PAD) / ROW_H).floor().max(0.0) as usize).min(total);
        let last = (first + (size.height / ROW_H).ceil() as usize + 2).min(total);

        let fixed: f32 = cols.iter().filter_map(|c| match c.width { Width::Fixed(px) => Some(px), _ => None }).sum();
        let portions: f32 = cols.iter().filter_map(|c| match c.width { Width::Portion(p) => Some(p as f32), _ => None }).sum();
        let flexible = (size.width - 32.0 - fixed - CELL_GAP * (cols.len().saturating_sub(1)) as f32).max(0.0);
        let width_of = |c: &Col| match c.width {
            Width::Fixed(px) => px,
            Width::Portion(p) => flexible * p as f32 / portions.max(1.0),
        };

        let mut list = column![space().height(LIST_PAD + first as f32 * ROW_H)];
        for i in first..last {
            let r = make_row(i);
            let selected = r.selected;
            let (fg, fg2) = if selected {
                (colors.on_secondary_container, colors.on_secondary_container)
            } else {
                (colors.on_surface, colors.on_surface_variant)
            };
            let cells = r.cells.into_iter().zip(cols).map(|(cell, col)| {
                let color = match cell.tone {
                    Tone::Primary => fg,
                    Tone::Secondary => fg2,
                    Tone::Accent | Tone::Icon => {
                        if selected { colors.on_secondary_container } else { colors.primary }
                    }
                };
                let content: Element<'a, Message> = if cell.tone == Tone::Icon {
                    icon_cell(cell.text, color)
                } else {
                    let font = if cell.strong { FONT_SEMIBOLD } else { FONT };
                    label(fit(&cell.text, width_of(col), 14.0), scale::BODY_MEDIUM, font, color)
                        .width(Fill)
                        .align_x(if col.numeric { Horizontal::Right } else { Horizontal::Left })
                        .into()
                };
                container(content).width(col.width.length()).clip(true).into()
            });
            let bg = if selected {
                colors.secondary_container
            } else if hovered == Some(i) {
                theme::alpha(colors.on_surface, theme::state::HOVER)
            } else {
                Color::TRANSPARENT
            };
            let line = container(row(cells).spacing(CELL_GAP).align_y(Alignment::Center))
                .height(ROW_H)
                .width(Fill)
                .padding([0.0, 12.0])
                .align_y(Alignment::Center)
                .style(surface(bg, theme::shape::SMALL));
            let hit = mouse_area(container(line).padding([0.0, 4.0]))
                .on_press(Message::RowPress(pane, i))
                .on_double_click(Message::RowActivate(pane, i))
                .on_enter(Message::RowHover(pane, i, true))
                .on_exit(Message::RowHover(pane, i, false));
            list = list.push(hit);
        }
        list = list.push(space().height((total - last) as f32 * ROW_H + LIST_PAD));

        iced::widget::scrollable(list)
            .id(scroll_id(pane))
            .height(Fill)
            .width(Fill)
            .direction(Direction::Vertical(Scrollbar::new().width(6).scroller_width(6).margin(2)))
            .on_scroll(move |vp| Message::Scrolled(pane, vp.absolute_offset().y, vp.bounds().height))
            .style(move |_, status| scroll_style(colors, status))
            .into()
    })
    .into()
}

fn icon_cell<'a>(name: String, color: Color) -> Element<'a, Message> {
    let name: &'static str = match name.as_str() {
        "graphic_eq" => "graphic_eq",
        "pause" => "pause",
        "error" => "error",
        _ => "",
    };
    icon(name, 18.0, color).into()
}

pub fn scroll_style(colors: Colors, status: scrollable::Status) -> scrollable::Style {
    let active = matches!(status, scrollable::Status::Hovered { is_vertical_scrollbar_hovered: true, .. } | scrollable::Status::Dragged { .. });
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
