//! Text that slowly scrolls back and forth when it doesn't fit (long playing titles).

use iced::advanced::text::{Paragraph as _, Renderer as _};
use iced::alignment::Vertical;
use iced::mouse::Cursor;
use iced::widget::canvas::{self, Frame, Geometry, Text};
use iced::widget::text::{Alignment, LineHeight, Shaping, Wrapping};
use iced::{Color, Font, Pixels, Point, Rectangle, Renderer, Size, Theme};

use crate::app::Message;

const SPEED: f32 = 28.0;
const PAUSE: f32 = 1.8;

pub struct Marquee {
    pub text: String,
    pub size: f32,
    pub line_height: f32,
    pub font: Font,
    pub color: Color,
    /// Seconds since the text appeared; drives the scroll position.
    pub phase: f32,
}

impl Marquee {
    /// Pixel offset for a ping-pong scroll over `overflow` px with pauses at both ends.
    fn offset(&self, overflow: f32) -> f32 {
        let travel = overflow / SPEED;
        let cycle = 2.0 * (PAUSE + travel);
        let t = self.phase.rem_euclid(cycle);
        if t < PAUSE {
            0.0
        } else if t < PAUSE + travel {
            (t - PAUSE) * SPEED
        } else if t < 2.0 * PAUSE + travel {
            overflow
        } else {
            overflow - (t - 2.0 * PAUSE - travel) * SPEED
        }
    }
}

/// Measured width of `text` as iced would lay it out.
pub fn measure(text: &str, size: f32, line_height: f32, font: Font) -> f32 {
    let paragraph = <<Renderer as iced::advanced::text::Renderer>::Paragraph>::with_text(iced::advanced::Text {
        content: text,
        bounds: Size::INFINITE,
        size: Pixels(size),
        line_height: LineHeight::Absolute(Pixels(line_height)),
        font,
        align_x: Alignment::Left,
        align_y: Vertical::Top,
        shaping: Shaping::Advanced,
        wrapping: Wrapping::None,
    });
    paragraph.min_bounds().width
}

impl canvas::Program<Message> for Marquee {
    type State = ();

    fn draw(&self, _state: &(), renderer: &Renderer, _theme: &Theme, bounds: Rectangle, _cursor: Cursor) -> Vec<Geometry> {
        let _ = renderer.default_font();
        let mut frame = Frame::new(renderer, bounds.size());
        let width = measure(&self.text, self.size, self.line_height, self.font);
        let overflow = (width - bounds.width).max(0.0);
        let x = if overflow > 0.5 { -self.offset(overflow).round() } else { 0.0 };
        let text = Text {
            content: self.text.clone(),
            position: Point::new(x, 0.0),
            max_width: f32::INFINITY,
            color: self.color,
            size: Pixels(self.size),
            line_height: LineHeight::Absolute(Pixels(self.line_height)),
            font: self.font,
            align_x: Alignment::Left,
            align_y: Vertical::Top,
            shaping: Shaping::Advanced,
        };
        frame.with_clip(Rectangle::with_size(bounds.size()), |f| f.fill_text(text));
        vec![frame.into_geometry()]
    }
}
