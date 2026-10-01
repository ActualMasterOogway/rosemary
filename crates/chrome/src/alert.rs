//! A modal alert, after a current Mac's: the icon, the message in bold and the
//! informative text under it, all centred, then the buttons.

use crate::checkbox::{Checkbox, CheckboxLayout};
use crate::geometry::{Point, Rect};
use crate::menu::State;
use crate::paint::{Canvas, Image};
use crate::rich::{Align, RichLayout, RichText};
use crate::text::Font;
use crate::theme::Theme;
use crate::view::View;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Alert {
    pub message: String,
    pub informative: String,
    /// Styled informative text with links, in place of `informative`. Only
    /// Rosetta 3's own alerts set it; a program's text is always plain.
    pub rich_informative: Option<RichText>,
    /// The first is the default, answered by Return.
    pub buttons: Vec<String>,
    pub icon: Option<Image>,
    /// The button being pressed, drawn darker.
    pub pressed: Option<usize>,
    /// Whether the window has room around the corners for them to be round.
    pub rounded: bool,
    pub checkbox: Option<String>,
    pub checkbox_checked: bool,
    pub checkbox_tip: String,
    pub accessories: Vec<Accessory>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Accessory {
    pub title: String,
    pub checked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Button(usize),
    Checkbox,
    Accessory(usize),
    /// A link in the rich informative text, by its index in
    /// [`RichLayout::links`].
    Link(usize),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    pub text: String,
    /// The left end of the baseline.
    pub origin: Point,
    pub bold: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AlertLayout {
    pub bounds: Rect,
    pub icon: Option<Rect>,
    pub lines: Vec<Line>,
    /// The rich informative text, when the alert has some.
    pub rich: Option<RichLayout>,
    /// One per button, in the order they were given.
    pub buttons: Vec<Rect>,
    pub checkbox: Option<CheckboxLayout>,
    pub accessories: Vec<CheckboxLayout>,
    pub tip: Vec<Line>,
}

const TOP: f32 = 20.0;
const SIDE: f32 = 16.0;
const BOTTOM: f32 = 16.0;
const AFTER_ICON: f32 = 12.0;
const BEFORE_INFORMATIVE: f32 = 6.0;
const BEFORE_BUTTONS: f32 = 20.0;
const BEFORE_CHECKBOX: f32 = 12.0;
const CHECKBOX_ALERT_MIN_WIDTH: f32 = 440.0;
const BUTTON_GAP: f32 = 8.0;
const BUTTON_RADIUS: f32 = 6.0;
/// Room a button's title needs on each side before the buttons stack.
const TITLE_PADDING: f32 = 12.0;
/// Beyond this the rest of the text is cut, so an alert still fits on the
/// screen with its buttons.
const MAX_LINES: usize = 40;

impl AlertLayout {
    /// Where the link `index` of [`Part::Link`] goes.
    pub fn link_url(&self, index: usize) -> Option<&str> {
        Some(self.rich.as_ref()?.links.get(index)?.url.as_str())
    }

    /// Everything in the alert that takes a click.
    pub fn controls(&self) -> impl Iterator<Item = Rect> + '_ {
        self.buttons
            .iter()
            .copied()
            .chain(self.checkbox.as_ref().map(|c| c.bounds))
            .chain(self.accessories.iter().map(|c| c.bounds))
            .chain(
                self.rich
                    .iter()
                    .flat_map(|r| r.links.iter().map(|l| l.rect)),
            )
    }
}

impl Alert {
    /// The button Escape answers: the one titled Cancel, as in `NSAlert`.
    pub fn cancel_button(&self) -> Option<usize> {
        self.buttons
            .iter()
            .position(|title| title.eq_ignore_ascii_case("cancel"))
    }
}

/// Up to `limit` of `lines`, the last marked with an ellipsis when some
/// were cut.
fn cap(mut lines: Vec<String>, limit: usize) -> Vec<String> {
    if lines.len() > limit {
        lines.truncate(limit);
        if let Some(last) = lines.last_mut() {
            last.push('\u{2026}');
        }
    }
    lines
}

impl View for Alert {
    type Part = Part;
    type Layout = AlertLayout;

    /// Only `bounds`' origin is used: an alert is as tall as what it says.
    fn layout(&self, bounds: Rect, theme: &Theme) -> AlertLayout {
        let width = self.checkbox.as_ref().map_or(theme.alert_width, |title| {
            let control = Checkbox {
                title: title.clone(),
                ..Checkbox::default()
            };
            let natural = control.layout(Rect::default(), theme).bounds.width;
            theme
                .alert_width
                .max(CHECKBOX_ALERT_MIN_WIDTH)
                .max(natural + 2.0 * SIDE + 16.0)
        });
        let width = self.accessories.iter().fold(width, |width, item| {
            let natural = Checkbox {
                title: item.title.clone(),
                ..Checkbox::default()
            }
            .layout(Rect::default(), theme)
            .bounds
            .width;
            width
                .max(CHECKBOX_ALERT_MIN_WIDTH)
                .max(natural + 2.0 * SIDE + 16.0)
        });
        let text_width = width - 2.0 * SIDE;
        let (x, mut y) = (bounds.x, bounds.y + TOP);

        let icon = self.icon.as_ref().map(|_| {
            let size = theme.alert_icon_size;
            let rect = Rect::new(x + (width - size) / 2.0, y, size, size);
            y += size + AFTER_ICON;
            rect
        });

        let mut lines = Vec::new();
        let mut text =
            |font: Option<std::sync::Arc<Font>>, body: &str, size: f32, bold, y: &mut f32| {
                let Some(font) = font else {
                    return;
                };
                let height = (size * 1.25).round();
                for line in cap(font.wrap(body, size, text_width), MAX_LINES) {
                    let line = line.trim().to_string();
                    let advance = font.measure(&line, size);
                    let baseline = font.centered_baseline(size, *y, height);
                    lines.push(Line {
                        origin: Point::new((x + (width - advance) / 2.0).round(), baseline),
                        text: line,
                        bold,
                    });
                    *y += height;
                }
            };
        if !self.message.is_empty() {
            let size = theme.alert_message_size;
            text(Font::system(700), &self.message, size, true, &mut y);
        }
        let mut rich = None;
        match &self.rich_informative {
            Some(body) if !body.is_empty() => {
                if !self.message.is_empty() {
                    y += BEFORE_INFORMATIVE;
                }
                let layout = body.layout(
                    Point::new(x + SIDE, y),
                    text_width,
                    theme.alert_informative_size,
                    Align::Center,
                    MAX_LINES,
                );
                y = layout.bounds.bottom();
                rich = Some(layout);
            }
            Some(_) => {}
            None if !self.informative.is_empty() => {
                if !self.message.is_empty() {
                    y += BEFORE_INFORMATIVE;
                }
                let size = theme.alert_informative_size;
                text(Font::system(400), &self.informative, size, false, &mut y);
            }
            None => {}
        }
        let checkbox = self.checkbox.as_ref().map(|title| {
            y += BEFORE_CHECKBOX;
            let control = Checkbox {
                title: title.clone(),
                ..Checkbox::default()
            };
            let layout = control.layout(Rect::new(x + SIDE, y, text_width, 0.0), theme);
            y = layout.bounds.bottom();
            layout
        });
        let accessories: Vec<_> = self
            .accessories
            .iter()
            .map(|item| {
                y += BEFORE_CHECKBOX;
                let layout = Checkbox {
                    title: item.title.clone(),
                    ..Checkbox::default()
                }
                .layout(Rect::new(x + SIDE, y, text_width, 0.0), theme);
                y = layout.bounds.bottom();
                layout
            })
            .collect();
        let mut tip = Vec::new();
        if checkbox.is_some() && !self.checkbox_tip.is_empty() {
            y += BEFORE_INFORMATIVE;
            if let Some(font) = Font::system(400) {
                let size = theme.alert_informative_size;
                let height = (size * 1.25).round();
                for line in cap(font.wrap(&self.checkbox_tip, size, text_width), MAX_LINES) {
                    tip.push(Line {
                        origin: Point::new(x + SIDE, font.centered_baseline(size, y, height)),
                        text: line,
                        bold: false,
                    });
                    y += height;
                }
            }
        }
        y += BEFORE_BUTTONS;

        // Two buttons share a row, the default on the right, when both titles
        // fit; otherwise each has a row, the default on top.
        let half = (text_width - BUTTON_GAP) / 2.0;
        let fits = Font::system(400).is_some_and(|font| {
            self.buttons.iter().all(|title| {
                font.measure(title, theme.alert_message_size) + 2.0 * TITLE_PADDING <= half
            })
        });
        let height = theme.alert_button_height;
        let buttons = match (self.buttons.len(), fits) {
            (0, _) => Vec::new(),
            (1, _) => vec![Rect::new(x + SIDE, y, text_width, height)],
            (2, true) => vec![
                Rect::new(x + SIDE + half + BUTTON_GAP, y, half, height),
                Rect::new(x + SIDE, y, half, height),
            ],
            _ => (0..self.buttons.len())
                .map(|at| {
                    let top = y + at as f32 * (height + BUTTON_GAP);
                    Rect::new(x + SIDE, top, text_width, height)
                })
                .collect(),
        };
        let bottom = buttons
            .iter()
            .map(Rect::bottom)
            .fold(y - BEFORE_BUTTONS, f32::max);

        AlertLayout {
            bounds: Rect::new(x, bounds.y, width, (bottom + BOTTOM - bounds.y).ceil()),
            icon,
            lines,
            rich,
            buttons,
            checkbox,
            accessories,
            tip,
        }
    }

    fn paint(&self, canvas: &mut Canvas, layout: &AlertLayout, theme: &Theme) {
        let radius = match self.rounded {
            true => theme.alert_corner_radius,
            false => 0.0,
        };
        let bounds = layout.bounds;
        canvas.fill_rounded_rect(bounds, radius, theme.alert_background);
        canvas.stroke_rounded_rect(bounds, radius, 1.0 / canvas.scale(), theme.alert_border);

        if let (Some(image), Some(rect)) = (&self.icon, layout.icon) {
            canvas.draw_image(image, rect);
        }

        for line in &layout.lines {
            let (weight, size) = match line.bold {
                true => (700, theme.alert_message_size),
                false => (400, theme.alert_informative_size),
            };
            if let Some(font) = Font::system(weight) {
                canvas.draw_text(&line.text, &font, size, line.origin, theme.alert_text);
            }
        }
        if let (Some(body), Some(rich)) = (&self.rich_informative, &layout.rich) {
            body.paint(
                canvas,
                rich,
                theme.alert_informative_size,
                theme.alert_text,
                theme.link,
            );
        }
        if let (Some(title), Some(placement)) = (&self.checkbox, &layout.checkbox) {
            Checkbox {
                title: title.clone(),
                state: if self.checkbox_checked {
                    State::On
                } else {
                    State::Off
                },
                ..Checkbox::default()
            }
            .paint(canvas, placement, theme);
        }
        for (item, placement) in self.accessories.iter().zip(&layout.accessories) {
            Checkbox {
                title: item.title.clone(),
                state: if item.checked { State::On } else { State::Off },
                ..Checkbox::default()
            }
            .paint(canvas, placement, theme);
        }
        if self.checkbox_checked
            && let Some(font) = Font::system(400)
        {
            for line in &layout.tip {
                canvas.draw_text(
                    &line.text,
                    &font,
                    theme.alert_informative_size,
                    line.origin,
                    theme.alert_text,
                );
            }
        }

        let Some(font) = Font::system(400) else {
            return;
        };
        let size = theme.alert_message_size;
        for (at, (title, rect)) in self.buttons.iter().zip(&layout.buttons).enumerate() {
            let pressed = self.pressed == Some(at);
            let (fill, text) = match (at == 0, pressed) {
                (true, false) => (theme.alert_default, theme.alert_default_text),
                (true, true) => (theme.alert_default_pressed, theme.alert_default_text),
                (false, false) => (theme.alert_button, theme.alert_button_text),
                (false, true) => (theme.alert_button_pressed, theme.alert_button_text),
            };
            canvas.fill_rounded_rect(*rect, BUTTON_RADIUS, fill);
            let title = font.truncate(title, size, rect.width - 2.0 * TITLE_PADDING);
            let advance = font.measure(&title, size);
            let origin = Point::new(
                (rect.center().x - advance / 2.0).round(),
                font.centered_baseline(size, rect.y, rect.height),
            );
            canvas.draw_text(&title, &font, size, origin, text);
        }
    }

    fn hit(&self, layout: &AlertLayout, point: Point) -> Option<Part> {
        if let Some(index) = layout.rich.as_ref().and_then(|rich| rich.link_at(point)) {
            return Some(Part::Link(index));
        }
        if layout
            .checkbox
            .as_ref()
            .is_some_and(|box_layout| box_layout.bounds.contains(point))
        {
            return Some(Part::Checkbox);
        }
        if let Some(index) = layout
            .accessories
            .iter()
            .position(|checkbox| checkbox.bounds.contains(point))
        {
            return Some(Part::Accessory(index));
        }
        layout
            .buttons
            .iter()
            .position(|rect| rect.contains(point))
            .map(Part::Button)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accessory_checkboxes_have_distinct_hit_areas_above_buttons() {
        let alert = Alert {
            buttons: vec!["OK".into()],
            accessories: vec![
                Accessory {
                    title: "First".into(),
                    checked: false,
                },
                Accessory {
                    title: "Second".into(),
                    checked: true,
                },
            ],
            ..Alert::default()
        };
        let layout = alert.layout(Rect::default(), &Theme::light());
        assert!(layout.accessories[0].bounds.bottom() < layout.accessories[1].bounds.y);
        assert!(layout.accessories[1].bounds.bottom() < layout.buttons[0].y);
        assert_eq!(
            alert.hit(&layout, layout.accessories[0].bounds.center()),
            Some(Part::Accessory(0))
        );
        assert_eq!(
            alert.hit(&layout, layout.accessories[1].bounds.center()),
            Some(Part::Accessory(1))
        );
    }

    fn alert(buttons: &[&str]) -> Alert {
        Alert {
            message: "Delete the file?".into(),
            informative: "This cannot be undone.".into(),
            buttons: buttons.iter().map(|title| title.to_string()).collect(),
            ..Alert::default()
        }
    }

    fn laid_out(alert: &Alert) -> AlertLayout {
        alert.layout(Rect::default(), &Theme::light())
    }

    #[test]
    fn two_short_buttons_share_a_row_with_the_default_on_the_right() {
        if Font::system(400).is_none() {
            return;
        }
        let layout = laid_out(&alert(&["Delete", "Cancel"]));
        let [default, other] = layout.buttons[..] else {
            panic!("two buttons");
        };
        assert_eq!(default.y, other.y);
        assert!(default.x > other.x);
    }

    #[test]
    fn three_buttons_stack_with_the_default_on_top() {
        let layout = laid_out(&alert(&["Save", "Don't Save", "Cancel"]));
        assert_eq!(layout.buttons.len(), 3);
        assert!(
            layout
                .buttons
                .windows(2)
                .all(|pair| pair[0].bottom() < pair[1].y)
        );
        assert!(layout.bounds.bottom() > layout.buttons[2].bottom());
    }

    #[test]
    fn a_button_is_hit_and_the_rest_is_not() {
        let alert = alert(&["OK"]);
        let layout = laid_out(&alert);
        let button = layout.buttons[0];
        assert_eq!(alert.hit(&layout, button.center()), Some(Part::Button(0)));
        assert_eq!(alert.hit(&layout, Point::new(1.0, 1.0)), None);
    }

    #[test]
    fn escape_answers_the_button_titled_cancel() {
        assert_eq!(alert(&["OK", "Cancel"]).cancel_button(), Some(1));
        assert_eq!(alert(&["OK"]).cancel_button(), None);
    }

    #[test]
    fn checkbox_has_its_own_hit_area_and_tip_space() {
        let alert = Alert {
            checkbox: Some("Don't remind me".into()),
            checkbox_tip: "Update manually later.".into(),
            ..alert(&["Update", "Not Now"])
        };
        let layout = laid_out(&alert);
        let checkbox = layout.checkbox.unwrap();
        assert_eq!(
            alert.hit(&layout, checkbox.box_rect.center()),
            Some(Part::Checkbox)
        );
        assert!(!layout.tip.is_empty());
        assert!(layout.buttons[0].y > checkbox.bounds.bottom());
    }

    #[test]
    fn update_checkbox_label_fits_inside_the_alert() {
        let Some(font) = Font::system(400) else {
            return;
        };
        let theme = Theme::light();
        let alert = Alert {
            checkbox: Some("Don't remind me about updates again".into()),
            ..alert(&["Update", "Not Now"])
        };
        let layout = alert.layout(Rect::default(), &theme);
        let checkbox = layout.checkbox.unwrap();
        let label_width =
            font.measure(alert.checkbox.as_deref().unwrap(), theme.checkbox_text_size);
        assert!(layout.bounds.width >= CHECKBOX_ALERT_MIN_WIDTH);
        assert!(checkbox.label.width >= label_width);
        assert_eq!(
            font.truncate(
                alert.checkbox.as_deref().unwrap(),
                theme.checkbox_text_size,
                checkbox.label.width,
            ),
            alert.checkbox.as_deref().unwrap()
        );
        assert!(checkbox.bounds.right() < layout.bounds.right());
    }

    #[test]
    fn plain_alerts_have_no_rich_text_or_links() {
        let alert = alert(&["OK"]);
        let layout = laid_out(&alert);
        assert!(layout.rich.is_none());
        assert!(layout.lines.iter().any(|line| !line.bold));
        assert_eq!(layout.controls().count(), 1);
    }

    #[test]
    fn rich_informative_text_replaces_the_plain_and_its_links_are_hit() {
        if Font::system(400).is_none() {
            return;
        }
        let url = "https://example.com/base.zip";
        let alert = Alert {
            rich_informative: Some(RichText::new().plain("From ").link(url, url)),
            ..alert(&["Download", "Not Now"])
        };
        let layout = laid_out(&alert);
        assert!(layout.lines.iter().all(|line| line.bold));
        let rich = layout.rich.as_ref().unwrap();
        let link = rich.links[0].rect;
        assert!(link.bottom() < layout.buttons[0].y);
        assert_eq!(alert.hit(&layout, link.center()), Some(Part::Link(0)));
        assert_eq!(layout.link_url(0), Some(url));
        assert!(layout.controls().any(|rect| rect == link));
    }

    #[test]
    fn an_icon_pushes_the_text_down() {
        let plain = laid_out(&alert(&["OK"]));
        let with_icon = laid_out(&Alert {
            icon: Some(Image {
                width: 1,
                height: 1,
                pixels: vec![0; 4],
            }),
            ..alert(&["OK"])
        });
        assert!(with_icon.icon.is_some());
        assert!(with_icon.bounds.height > plain.bounds.height);
    }

    #[test]
    fn painting_covers_the_whole_square_alert() {
        let alert = Alert {
            pressed: Some(0),
            ..alert(&["OK", "Cancel"])
        };
        let theme = Theme::dark();
        let layout = alert.layout(Rect::default(), &theme);
        let (width, height) = (layout.bounds.width as usize, layout.bounds.height as usize);
        let mut pixels = vec![0u8; width * height * 4];
        let mut canvas = Canvas::new(&mut pixels, width, height, width * 4, 1.0);
        alert.paint(&mut canvas, &layout, &theme);
        assert!(pixels.chunks(4).all(|texel| texel[3] == 255));
    }
}
