//! The Rivian-style seek bar: a thick track whose remaining part lifts
//! into a hump over the thumb.

use iced::mouse::{self, Cursor};
use iced::widget::canvas::{self, Action, Event, Frame, Geometry, LineCap, LineJoin, Path, Stroke};
use iced::{Point, Rectangle, Renderer, Theme};

use crate::app::Message;
use crate::theme::Colors;

pub const HEIGHT: f32 = 36.0;
const BASELINE: f32 = 26.0;
const INSET: f32 = 8.0;
const TRACK: f32 = 6.0;

pub struct SeekBar {
    /// Displayed playhead, 0..=1.
    pub fraction: f32,
    /// 0 at rest, 1 when hovered (animated by the app).
    pub lift: f32,
    /// 0..1 while dragging.
    pub drag: f32,
    pub colors: Colors,
    pub enabled: bool,
}

#[derive(Default)]
pub struct State {
    hovered: bool,
    dragging: bool,
    down_x: f32,
}

fn fraction_at(bounds: Rectangle, x: f32) -> f32 {
    ((x - bounds.x - INSET) / (bounds.width - 2.0 * INSET)).clamp(0.0, 1.0)
}

impl canvas::Program<Message> for SeekBar {
    type State = State;

    fn update(&self, state: &mut State, event: &Event, bounds: Rectangle, cursor: Cursor) -> Option<Action<Message>> {
        if !self.enabled {
            return None;
        }
        match event {
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if state.dragging {
                    let p = cursor.position()?;
                    return Some(Action::publish(Message::SeekPreview(fraction_at(bounds, p.x))).and_capture());
                }
                let over = cursor.is_over(bounds);
                if over != state.hovered {
                    state.hovered = over;
                    return Some(Action::publish(Message::SeekHover(over)));
                }
                None
            }
            Event::Mouse(mouse::Event::CursorLeft) if state.hovered && !state.dragging => {
                state.hovered = false;
                Some(Action::publish(Message::SeekHover(false)))
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = cursor.position_over(bounds)?;
                state.dragging = true;
                state.down_x = p.x;
                Some(Action::publish(Message::SeekPreview(fraction_at(bounds, p.x))).and_capture())
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.dragging => {
                state.dragging = false;
                let x = cursor.position().map_or(state.down_x, |p| p.x);
                let glide = (x - state.down_x).abs() < 3.0;
                state.hovered = cursor.is_over(bounds);
                Some(Action::publish(Message::SeekCommit { fraction: fraction_at(bounds, x), glide }).and_capture())
            }
            _ => None,
        }
    }

    fn draw(&self, _state: &State, renderer: &Renderer, _theme: &Theme, bounds: Rectangle, _cursor: Cursor) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let (x0, x1, y) = (INSET, bounds.width - INSET, BASELINE);
        let tx = x0 + (x1 - x0) * self.fraction.clamp(0.0, 1.0);
        let hw = 26.0 + 8.0 * self.lift + 4.0 * self.drag;
        let h = 13.0 + 6.0 * self.lift + 3.0 * self.drag;
        let r = 7.0 + 2.0 * self.lift + 1.0 * self.drag;
        // Near either end the hump narrows on that side instead of spilling past the track.
        let lw = hw.min(tx - x0 + TRACK).max(TRACK);
        let rw = hw.min(x1 - tx + TRACK).max(TRACK);
        let (a, b) = (tx - lw, tx + rw);

        let remaining = Path::new(|p| {
            p.move_to(Point::new(a, y));
            p.bezier_curve_to(Point::new(tx - lw / 2.0, y), Point::new(tx - lw / 2.0, y - h), Point::new(tx, y - h));
            p.bezier_curve_to(Point::new(tx + rw / 2.0, y - h), Point::new(tx + rw / 2.0, y), Point::new(b, y));
            if b < x1 {
                p.line_to(Point::new(x1, y));
            }
        });
        let stroke = |color| Stroke::default().with_width(TRACK).with_color(color).with_line_cap(LineCap::Round).with_line_join(LineJoin::Round);
        frame.stroke(&remaining, stroke(self.colors.secondary_container));

        let elapsed_end = a.max(x0);
        if elapsed_end > x0 + 0.5 {
            frame.stroke(&Path::line(Point::new(x0, y), Point::new(elapsed_end, y)), stroke(self.colors.primary));
        } else {
            frame.fill(&Path::circle(Point::new(x0, y), TRACK / 2.0), self.colors.primary);
        }
        frame.fill(&Path::circle(Point::new(tx, y), r), self.colors.primary);
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(&self, state: &State, bounds: Rectangle, cursor: Cursor) -> mouse::Interaction {
        if self.enabled && (state.dragging || cursor.is_over(bounds)) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
}
