//! M3 Expressive slider used for volume: split track with a bar handle.

use iced::mouse::{self, Cursor, ScrollDelta};
use iced::widget::canvas::{self, Action, Event, Frame, Geometry, Path};
use iced::{Point, Rectangle, Renderer, Size, Theme};

use crate::app::Message;
use crate::theme::Colors;

pub struct Volume {
    pub value: f32,
    pub colors: Colors,
}

#[derive(Default)]
pub struct State {
    dragging: bool,
}

fn value_at(bounds: Rectangle, x: f32) -> f32 {
    ((x - bounds.x) / bounds.width).clamp(0.0, 1.0)
}

impl canvas::Program<Message> for Volume {
    type State = State;

    fn update(&self, state: &mut State, event: &Event, bounds: Rectangle, cursor: Cursor) -> Option<Action<Message>> {
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let p = cursor.position_over(bounds)?;
                state.dragging = true;
                Some(Action::publish(Message::Volume(value_at(bounds, p.x))).and_capture())
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.dragging => {
                let p = cursor.position()?;
                Some(Action::publish(Message::Volume(value_at(bounds, p.x))).and_capture())
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.dragging => {
                state.dragging = false;
                Some(Action::capture())
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) if cursor.is_over(bounds) => {
                let y = match delta {
                    ScrollDelta::Lines { y, .. } => *y,
                    ScrollDelta::Pixels { y, .. } => y.signum(),
                };
                Some(Action::publish(Message::VolumeStep(if y > 0.0 { 0.02 } else { -0.02 })).and_capture())
            }
            _ => None,
        }
    }

    fn draw(&self, _state: &State, renderer: &Renderer, _theme: &Theme, bounds: Rectangle, _cursor: Cursor) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let (w, h) = (bounds.width, bounds.height);
        let track_h = 16.0;
        let ty = (h - track_h) / 2.0;
        let x = self.value.clamp(0.0, 1.0) * w;
        let gap = 6.0;
        let active = (x - gap).max(0.0);
        if active > 0.0 {
            frame.fill(
                &Path::rounded_rectangle(
                    Point::new(0.0, ty),
                    Size::new(active, track_h),
                    iced::border::Radius { top_left: 8.0, bottom_left: 8.0, top_right: 2.0, bottom_right: 2.0 },
                ),
                self.colors.primary,
            );
        }
        let inactive = (w - x - gap).max(0.0);
        if inactive > 0.0 {
            frame.fill(
                &Path::rounded_rectangle(
                    Point::new(w - inactive, ty),
                    Size::new(inactive, track_h),
                    iced::border::Radius { top_left: 2.0, bottom_left: 2.0, top_right: 8.0, bottom_right: 8.0 },
                ),
                self.colors.secondary_container,
            );
            frame.fill(&Path::circle(Point::new(w - 8.0, h / 2.0), 2.0), self.colors.on_secondary_container);
        }
        let handle_x = (x - 2.0).clamp(0.0, w - 4.0);
        frame.fill(&Path::rounded_rectangle(Point::new(handle_x, (h - 32.0) / 2.0), Size::new(4.0, 32.0), 2.0.into()), self.colors.primary);
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(&self, state: &State, bounds: Rectangle, cursor: Cursor) -> mouse::Interaction {
        if state.dragging || cursor.is_over(bounds) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::default()
        }
    }
}
