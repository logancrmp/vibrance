//! Widgets and style helpers that turn M3 roles into iced styles.

pub mod marquee;
pub mod seek;
pub mod spectrum;
pub mod swatch;
pub mod table;
pub mod volume;

use iced::widget::{Button, Text, button, container, text};
use iced::{Background, Color, Font, Shadow, Vector, border};

use crate::app::Message;
use crate::theme::{self, Colors, ICONS};

pub fn label<'a>(content: impl text::IntoFragment<'a>, (size, line): (f32, f32), font: Font, color: Color) -> Text<'a> {
    text(content)
        .size(size)
        .line_height(text::LineHeight::Absolute(line.into()))
        .font(font)
        .color(color)
        .wrapping(text::Wrapping::None)
}

/// A Material Symbols glyph by ligature name.
pub fn icon<'a>(name: &'a str, size: f32, color: Color) -> Text<'a> {
    text(name)
        .font(ICONS)
        .size(size)
        .line_height(text::LineHeight::Absolute(size.into()))
        .shaping(text::Shaping::Advanced)
        .color(color)
        .wrapping(text::Wrapping::None)
}

/// M3 icon button. `fill` is the container color for filled/tonal/selected variants.
pub fn icon_button<'a>(
    name: &'a str,
    size: f32,
    icon_size: f32,
    fg: Color,
    fill: Option<Color>,
    radius: f32,
    on_press: Option<Message>,
) -> Button<'a, Message> {
    button(container(icon(name, icon_size, fg)).center(size))
        .width(size)
        .height(size)
        .padding(0)
        .on_press_maybe(on_press)
        .style(move |_, status| state_button(fill, fg, radius, status))
}

/// Container plus an `fg`-tinted state layer for hover and press.
pub fn state_button(fill: Option<Color>, fg: Color, radius: f32, status: button::Status) -> button::Style {
    let overlay = match status {
        button::Status::Hovered => theme::state::HOVER,
        button::Status::Pressed => theme::state::PRESSED,
        _ => 0.0,
    };
    let background = match fill {
        Some(fill) => theme::layer(fill, fg, overlay),
        None => theme::alpha(fg, overlay),
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: fg,
        border: border::rounded(radius),
        shadow: Shadow::default(),
        snap: true,
    }
}

pub fn surface(color: Color, radius: f32) -> impl Fn(&iced::Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(color)),
        border: border::rounded(radius),
        ..container::Style::default()
    }
}

/// M3 elevation for floating surfaces (menus, dragged panes).
pub fn floating(colors: Colors, radius: f32) -> impl Fn(&iced::Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(colors.surface_container)),
        border: border::rounded(radius),
        shadow: Shadow { color: Color::from_rgba(0.0, 0.0, 0.0, 0.35), offset: Vector::new(0.0, 2.0), blur_radius: 8.0 },
        text_color: Some(colors.on_surface),
        ..container::Style::default()
    }
}

pub fn divider(color: Color) -> impl Fn(&iced::Theme) -> container::Style {
    move |_| container::Style { background: Some(Background::Color(color)), ..container::Style::default() }
}

/// Cut `s` to roughly fit `width` px at `size` px with an ellipsis (iced 0.14 has no ellipsizing).
pub fn fit(s: &str, width: f32, size: f32) -> String {
    let budget = (width / (size * 0.53)).floor().max(1.0) as usize;
    if s.chars().count() <= budget {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(budget.saturating_sub(1)).collect();
    out.truncate(out.trim_end().len());
    out.push('…');
    out
}
