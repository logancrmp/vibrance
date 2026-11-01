//! Compact bar spectrum with falling peak caps (SPEC 4.4).

use iced::mouse::Cursor;
use iced::widget::canvas::{self, Frame, Geometry, Path};
use iced::{Point, Rectangle, Renderer, Size, Theme};

use crate::app::Message;
use crate::theme::Colors;

pub struct Spectrum<'a> {
    pub bars: &'a [f32],
    pub peaks: &'a [f32],
    pub colors: Colors,
    pub gap: f32,
}

impl canvas::Program<Message> for Spectrum<'_> {
    type State = ();

    fn draw(&self, _state: &(), renderer: &Renderer, _theme: &Theme, bounds: Rectangle, _cursor: Cursor) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let n = self.bars.len().max(1) as f32;
        let bw = (bounds.width - self.gap * (n - 1.0)) / n;
        let cap = (bw / 6.0).round().max(2.0);
        let usable = bounds.height - cap - 2.0;
        let radius = (bw / 3.0).min(4.0);
        for (i, &v) in self.bars.iter().enumerate() {
            let x = i as f32 * (bw + self.gap);
            let h = (v.clamp(0.0, 1.0) * usable).max(2.0);
            let bar = Path::rounded_rectangle(
                Point::new(x, bounds.height - h),
                Size::new(bw, h),
                iced::border::Radius { top_left: radius, top_right: radius, bottom_right: 1.0, bottom_left: 1.0 },
            );
            frame.fill(&bar, self.colors.primary);
            let peak = self.peaks.get(i).copied().unwrap_or(0.0);
            if peak > 0.04 {
                let y = bounds.height - (peak * usable).max(2.0) - 2.0 - cap;
                frame.fill(&Path::rounded_rectangle(Point::new(x, y), Size::new(bw, cap), 1.0.into()), self.colors.tertiary);
            }
        }
        vec![frame.into_geometry()]
    }
}

/// Peak caps: jump up with the bar, then fall at a constant rate.
pub fn update_peaks(bars: &[f32], peaks: &mut Vec<f32>, dt: f32) {
    peaks.resize(bars.len(), 0.0);
    for (peak, &bar) in peaks.iter_mut().zip(bars) {
        *peak = if bar >= *peak { bar } else { (*peak - 0.9 * dt).max(bar) };
    }
}
