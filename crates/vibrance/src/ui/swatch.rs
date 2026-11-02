//! Palette preview disc: primary on top, secondary container and tertiary below (SPEC 6.3).

use iced::mouse::Cursor;
use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::{Color, Point, Rectangle, Renderer, Size, Theme};

use crate::app::Message;

pub struct Swatch {
    pub top: Color,
    pub left: Color,
    pub right: Color,
    /// Selection ring color, if selected.
    pub ring: Option<Color>,
}

impl canvas::Program<Message> for Swatch {
    type State = ();

    fn draw(&self, _state: &(), renderer: &Renderer, _theme: &Theme, bounds: Rectangle, _cursor: Cursor) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let center = frame.center();
        let outer = bounds.width.min(bounds.height) / 2.0;
        let r = outer - 5.0;
        // Clipped full circles: top half, then bottom-left and bottom-right quarters.
        let disc = Path::circle(center, r);
        let (w, h) = (bounds.width, bounds.height);
        frame.with_clip(Rectangle::new(Point::ORIGIN, Size::new(w, center.y)), |f| f.fill(&disc, self.top));
        frame.with_clip(Rectangle::new(Point::new(0.0, center.y), Size::new(center.x, h - center.y)), |f| f.fill(&disc, self.left));
        frame.with_clip(Rectangle::new(center, Size::new(w - center.x, h - center.y)), |f| f.fill(&disc, self.right));
        if let Some(ring) = self.ring {
            frame.stroke(&Path::circle(center, outer - 1.5), Stroke::default().with_width(3.0).with_color(ring));
        }
        vec![frame.into_geometry()]
    }
}
