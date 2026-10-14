//! A labelled check box with the same three states as a menu item, or a
//! radio button, which is the same control with a round box.

use crate::geometry::{Point, Rect};
use crate::menu::State;
use crate::paint::Canvas;
use crate::text::Font;
use crate::theme::Theme;
use crate::view::View;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkbox {
    pub title: String,
    pub state: State,
    pub enabled: bool,
    pub pressed: bool,
    /// Round, with a dot for on, as one of a group of exclusive choices.
    pub radio: bool,
}

impl Default for Checkbox {
    fn default() -> Self {
        Self {
            title: String::new(),
            state: State::Off,
            enabled: true,
            pressed: false,
            radio: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Control,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CheckboxLayout {
    pub bounds: Rect,
    pub box_rect: Rect,
    pub label: Rect,
}

const RADIUS: f32 = 3.5;

impl View for Checkbox {
    type Part = Part;
    type Layout = CheckboxLayout;

    /// A zero width uses the title's natural width; a supplied width clips it.
    fn layout(&self, bounds: Rect, theme: &Theme) -> CheckboxLayout {
        let gap = if self.title.is_empty() {
            0.0
        } else {
            theme.checkbox_gap
        };
        let title_width = Font::system(400)
            .map(|font| font.measure(&self.title, theme.checkbox_text_size))
            .unwrap_or(0.0);
        let natural_width = (theme.checkbox_size + gap + title_width).ceil();
        let width = if bounds.width > 0.0 {
            bounds.width.max(theme.checkbox_size)
        } else {
            natural_width
        };
        let height = bounds
            .height
            .max(theme.checkbox_height)
            .max(theme.checkbox_size);
        let bounds = Rect::new(bounds.x, bounds.y, width, height);
        let box_rect = Rect::new(
            bounds.x,
            (bounds.center().y - theme.checkbox_size / 2.0).round(),
            theme.checkbox_size,
            theme.checkbox_size,
        );
        let label_x = box_rect.right() + gap;
        let label = Rect::new(
            label_x,
            bounds.y,
            (bounds.right() - label_x).max(0.0),
            height,
        );
        CheckboxLayout {
            bounds,
            box_rect,
            label,
        }
    }

    fn paint(&self, canvas: &mut Canvas, layout: &CheckboxLayout, theme: &Theme) {
        let checked = self.state != State::Off;
        let mut fill = if checked {
            if self.pressed && self.enabled {
                theme.checkbox_pressed
            } else {
                theme.checkbox_accent
            }
        } else {
            theme.checkbox_background
        };
        let mut border = theme.checkbox_border;
        let mut mark = theme.checkbox_mark;
        if !self.enabled {
            fill = fill.with_alpha(fill.a * 0.45);
            border = border.with_alpha(border.a * 0.45);
            mark = mark.with_alpha(mark.a * 0.45);
        }
        let radius = match self.radio {
            true => layout.box_rect.width / 2.0,
            false => RADIUS,
        };
        canvas.fill_rounded_rect(layout.box_rect, radius, fill);
        canvas.stroke_rounded_rect(layout.box_rect, radius, 1.0 / canvas.scale(), border);

        let c = layout.box_rect.center();
        match self.state {
            State::Off => {}
            State::On if self.radio => canvas.fill_circle(c, 3.0, mark),
            State::On => {
                canvas.stroke_line(
                    Point::new(c.x - 4.0, c.y),
                    Point::new(c.x - 1.0, c.y + 3.0),
                    1.8,
                    mark,
                );
                canvas.stroke_line(
                    Point::new(c.x - 1.0, c.y + 3.0),
                    Point::new(c.x + 4.0, c.y - 3.5),
                    1.8,
                    mark,
                );
            }
            State::Mixed => canvas.stroke_line(
                Point::new(c.x - 3.5, c.y),
                Point::new(c.x + 3.5, c.y),
                1.8,
                mark,
            ),
        }

        if let Some(font) = Font::system(400) {
            let size = theme.checkbox_text_size;
            let title = font.truncate(&self.title, size, layout.label.width);
            let baseline = font.centered_baseline(size, layout.label.y, layout.label.height);
            let color = if self.enabled {
                theme.checkbox_text
            } else {
                theme.checkbox_disabled_text
            };
            canvas.draw_text(
                &title,
                &font,
                size,
                Point::new(layout.label.x, baseline),
                color,
            );
        }
    }

    fn hit(&self, layout: &CheckboxLayout, point: Point) -> Option<Part> {
        (self.enabled && layout.bounds.contains(point)).then_some(Part::Control)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_and_box_share_one_hit_area() {
        let checkbox = Checkbox {
            title: "Remember choice".into(),
            ..Checkbox::default()
        };
        let layout = checkbox.layout(Rect::new(10.0, 20.0, 140.0, 24.0), &Theme::light());
        assert_eq!(
            checkbox.hit(&layout, layout.box_rect.center()),
            Some(Part::Control)
        );
        assert_eq!(
            checkbox.hit(&layout, layout.label.center()),
            Some(Part::Control)
        );
        assert_eq!(checkbox.hit(&layout, Point::new(150.0, 25.0)), None);
        assert_eq!(
            Checkbox {
                enabled: false,
                ..checkbox
            }
            .hit(&layout, layout.box_rect.center()),
            None
        );
    }

    #[test]
    fn on_and_mixed_marks_differ_from_off() {
        let theme = Theme::light();
        let mut checkbox = Checkbox::default();
        let layout = checkbox.layout(Rect::new(0.0, 0.0, 20.0, 20.0), &theme);
        let render = |checkbox: &Checkbox| {
            let mut pixels = vec![0; 20 * 20 * 4];
            let mut canvas = Canvas::new(&mut pixels, 20, 20, 20 * 4, 1.0);
            checkbox.paint(&mut canvas, &layout, &theme);
            pixels
        };
        let off = render(&checkbox);
        checkbox.state = State::On;
        let on = render(&checkbox);
        checkbox.state = State::Mixed;
        let mixed = render(&checkbox);
        assert_ne!(off, on);
        assert_ne!(on, mixed);
        assert_ne!(off, mixed);
    }

    #[test]
    fn a_radio_button_is_drawn_round() {
        let theme = Theme::light();
        let square = Checkbox::default();
        let round = Checkbox {
            radio: true,
            ..Checkbox::default()
        };
        let layout = square.layout(Rect::new(0.0, 0.0, 20.0, 20.0), &theme);
        let render = |checkbox: &Checkbox| {
            let mut pixels = vec![0; 20 * 20 * 4];
            let mut canvas = Canvas::new(&mut pixels, 20, 20, 20 * 4, 1.0);
            checkbox.paint(&mut canvas, &layout, &theme);
            pixels
        };
        let corner = |pixels: &[u8]| {
            let (x, y) = (layout.box_rect.x as usize, layout.box_rect.y as usize + 1);
            pixels[(y * 20 + x + 1) * 4 + 3]
        };
        assert!(corner(&render(&square)) > corner(&render(&round)));
    }
}
