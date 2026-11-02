//! The Rivian-style seek bar: a thick track that lifts into a hump over the thumb, played
//! up to the hump's top. The hump is always centred on the playhead, so at either end of the
//! track half of it is cut off: the track starts (and finishes) on the hump's top.

use iced::mouse::{self, Cursor};
use iced::widget::canvas::{self, Action, Event, Frame, Geometry, LineCap, LineJoin, Path, Stroke};
use iced::{Point, Rectangle, Renderer, Theme};

use crate::app::Message;
use crate::theme::Colors;

pub const HEIGHT: f32 = 36.0;
const BASELINE: f32 = 26.0;
const INSET: f32 = 8.0;
const TRACK: f32 = 6.0;
/// Line segments per half of the hump.
const STEPS: usize = 24;

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

/// The track's centre line from `x0` to `x1`: flat, lifted into a hump of half-width `hw` and
/// height `h` over `tx` (two S-curves meeting at the top), uncut. Its x only ever increases.
fn centre_line(x0: f32, x1: f32, tx: f32, hw: f32, h: f32) -> Vec<Point> {
    let (y, top) = (BASELINE, BASELINE - h);
    let halves = [
        [Point::new(tx - hw, y), Point::new(tx - hw / 2.0, y), Point::new(tx - hw / 2.0, top), Point::new(tx, top)],
        [Point::new(tx, top), Point::new(tx + hw / 2.0, top), Point::new(tx + hw / 2.0, y), Point::new(tx + hw, y)],
    ];
    let cubic = |c: &[Point; 4], t: f32| {
        let u = 1.0 - t;
        let w = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
        Point::new(c.iter().zip(w).map(|(p, w)| p.x * w).sum(), c.iter().zip(w).map(|(p, w)| p.y * w).sum())
    };
    let mut points = vec![Point::new(x0.min(tx - hw), y)];
    for half in &halves {
        points.extend((0..=STEPS).map(|i| cubic(half, i as f32 / STEPS as f32)));
    }
    points.push(Point::new(x1.max(tx + hw), y));
    points
}

/// The part of `line` between `from` and `to` along x.
fn between(line: &[Point], from: f32, to: f32) -> Vec<Point> {
    let mut out = Vec::new();
    for pair in line.windows(2) {
        let (p, q) = (pair[0], pair[1]);
        if q.x <= p.x || q.x < from || p.x > to {
            continue;
        }
        let at = |x: f32| Point::new(x, p.y + (q.y - p.y) * (x - p.x) / (q.x - p.x));
        if out.is_empty() {
            out.push(if p.x < from { at(from) } else { p });
        }
        let end = if q.x > to { at(to) } else { q };
        if out.last() != Some(&end) {
            out.push(end);
        }
    }
    out
}

fn polyline(points: &[Point]) -> Path {
    Path::new(|p| {
        p.move_to(points[0]);
        for &q in &points[1..] {
            p.line_to(q);
        }
    })
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
        let line = centre_line(x0, x1, tx, hw, h);
        let stroke = |color, cap| Stroke::default().with_width(TRACK).with_color(color).with_line_cap(cap).with_line_join(LineJoin::Round);

        // What's left, from the hump's top on.
        if x1 - tx > 0.5 {
            frame.stroke(&polyline(&between(&line, tx, x1)), stroke(self.colors.secondary_container, LineCap::Round));
        }
        // What's played, up to the hump's top: cut square there (over the rest's round end),
        // rounded only where the track itself ends.
        if tx - x0 > 0.5 {
            let played = between(&line, x0, tx);
            frame.stroke(&polyline(&played), stroke(self.colors.primary, LineCap::Butt));
            frame.fill(&Path::circle(played[0], TRACK / 2.0), self.colors.primary);
            if x1 - tx <= 0.5 {
                frame.fill(&Path::circle(played[played.len() - 1], TRACK / 2.0), self.colors.primary);
            }
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
