//! Styled text: runs of plain, bold, monospace and linked text, wrapped
//! together as one paragraph and hit-tested for their links.
//!
//! Only Rosetta 3's own dialogs build it. A program's alert text stays plain,
//! as it is on a Mac unless the program supplies an attributed string.

use std::sync::Arc;

use crate::geometry::{Point, Rect};
use crate::paint::{Canvas, Color};
use crate::text::Font;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub monospace: bool,
}

impl Style {
    /// The font this style draws with: the monospace one falls back to the
    /// interface font on a host with none.
    pub fn font(self) -> Option<Arc<Font>> {
        let weight = match self.bold {
            true => 700,
            false => 400,
        };
        match self.monospace {
            true => Font::monospace(weight).or_else(|| Font::system(weight)),
            false => Font::system(weight),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Run {
    pub text: String,
    pub style: Style,
    /// Where clicking the run goes.
    pub link: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RichText {
    pub runs: Vec<Run>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    Left,
    Center,
}

/// A piece of one run on one line.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fragment {
    /// Index into [`RichText::runs`].
    pub run: usize,
    pub text: String,
    /// The left end of the baseline.
    pub origin: Point,
    pub width: f32,
}

/// Where a link can be clicked: one per line a link spans.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkArea {
    pub run: usize,
    pub url: String,
    pub rect: Rect,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RichLayout {
    pub bounds: Rect,
    pub fragments: Vec<Fragment>,
    pub links: Vec<LinkArea>,
}

impl RichLayout {
    /// The link under `point`, as an index into [`RichLayout::links`].
    pub fn link_at(&self, point: Point) -> Option<usize> {
        self.links.iter().position(|link| link.rect.contains(point))
    }
}

impl From<&str> for RichText {
    fn from(text: &str) -> RichText {
        RichText::new().plain(text)
    }
}

/// Advances are summed a piece at a time here and over a whole line by
/// `Font::wrap`; rounding must not make the two break differently.
const SLACK: f32 = 0.01;

/// What wrapping sees: words, which may span runs, and the gaps between them.
enum Token {
    Word(Vec<(usize, String)>),
    /// Any run of spaces, drawn as one in the style of its first run.
    Space(usize),
    Break,
}

impl RichText {
    pub fn new() -> RichText {
        RichText::default()
    }

    pub fn push(mut self, text: impl Into<String>, style: Style, link: Option<String>) -> Self {
        let text = text.into();
        if !text.is_empty() {
            self.runs.push(Run { text, style, link });
        }
        self
    }

    pub fn plain(self, text: impl Into<String>) -> Self {
        self.push(text, Style::default(), None)
    }

    pub fn bold(self, text: impl Into<String>) -> Self {
        let style = Style {
            bold: true,
            ..Style::default()
        };
        self.push(text, style, None)
    }

    /// Text set in the monospace font, as a command or a path is.
    pub fn code(self, text: impl Into<String>) -> Self {
        let style = Style {
            monospace: true,
            ..Style::default()
        };
        self.push(text, style, None)
    }

    pub fn link(self, text: impl Into<String>, url: impl Into<String>) -> Self {
        self.push(text, Style::default(), Some(url.into()))
    }

    pub fn is_empty(&self) -> bool {
        self.runs.iter().all(|run| run.text.is_empty())
    }

    /// The text with its styling dropped.
    pub fn to_plain(&self) -> String {
        self.runs.iter().map(|run| run.text.as_str()).collect()
    }

    fn tokens(&self) -> Vec<Token> {
        let mut tokens = Vec::new();
        let mut word: Vec<(usize, String)> = Vec::new();
        let flush = |word: &mut Vec<(usize, String)>, tokens: &mut Vec<Token>| {
            if !word.is_empty() {
                tokens.push(Token::Word(std::mem::take(word)));
            }
        };
        for (at, run) in self.runs.iter().enumerate() {
            for c in run.text.chars() {
                if c == '\n' {
                    flush(&mut word, &mut tokens);
                    tokens.push(Token::Break);
                } else if c.is_whitespace() {
                    flush(&mut word, &mut tokens);
                    if !matches!(tokens.last(), Some(Token::Space(_))) {
                        tokens.push(Token::Space(at));
                    }
                } else {
                    match word.last_mut() {
                        Some((run, piece)) if *run == at => piece.push(c),
                        _ => word.push((at, c.to_string())),
                    }
                }
            }
        }
        flush(&mut word, &mut tokens);
        tokens
    }

    /// The runs broken into lines no wider than `width`, as [`Font::wrap`]
    /// breaks plain text: at spaces where it can, and inside a word only when
    /// the word alone is too wide. A word may change style partway, and a
    /// break never falls inside one that fits.
    fn wrap(&self, size: f32, width: f32) -> Vec<Vec<(usize, String)>> {
        let fonts: Vec<Option<Arc<Font>>> = self.runs.iter().map(|run| run.style.font()).collect();
        let measure = |run: usize, text: &str| {
            fonts[run]
                .as_ref()
                .map_or(0.0, |font| font.measure(text, size))
        };
        let mut lines = vec![Vec::new()];
        let mut used = 0.0;
        let mut space = None;
        let push = |line: &mut Vec<(usize, String)>, run: usize, text: &str| match line.last_mut() {
            Some((last, piece)) if *last == run => piece.push_str(text),
            _ => line.push((run, text.to_string())),
        };
        for token in self.tokens() {
            let line = lines.last_mut().expect("never empty");
            match token {
                Token::Break => {
                    lines.push(Vec::new());
                    used = 0.0;
                    space = None;
                }
                // Spaces at the start of a line are dropped, as they are by
                // `Font::wrap`.
                Token::Space(run) => {
                    if !line.is_empty() {
                        space = Some(run);
                    }
                }
                Token::Word(pieces) => {
                    let advance: f32 = pieces.iter().map(|(run, text)| measure(*run, text)).sum();
                    let gap = space.map_or(0.0, |run| measure(run, " "));
                    if !line.is_empty() && used + gap + advance > width + SLACK {
                        lines.push(Vec::new());
                        used = 0.0;
                    } else if let Some(run) = space {
                        push(line, run, " ");
                        used += gap;
                    }
                    space = None;
                    let line = lines.last_mut().expect("never empty");
                    if used + advance <= width + SLACK {
                        for (run, text) in &pieces {
                            push(line, *run, text);
                        }
                        used += advance;
                        continue;
                    }
                    // Too wide for any line: break between characters.
                    let mut line = line;
                    for (run, text) in &pieces {
                        for c in text.chars() {
                            let c = c.encode_utf8(&mut [0; 4]).to_string();
                            let step = measure(*run, &c);
                            if !line.is_empty() && used + step > width + SLACK {
                                lines.push(Vec::new());
                                line = lines.last_mut().expect("never empty");
                                used = 0.0;
                            }
                            push(line, *run, &c);
                            used += step;
                        }
                    }
                }
            }
        }
        lines
    }

    /// Lay the text out in a column `width` wide whose top left is `origin`,
    /// at `size` points. Beyond `max_lines` the rest is cut and the last line
    /// marked with an ellipsis.
    pub fn layout(
        &self,
        origin: Point,
        width: f32,
        size: f32,
        align: Align,
        max_lines: usize,
    ) -> RichLayout {
        let Some(base) = Font::system(400) else {
            return RichLayout {
                bounds: Rect::new(origin.x, origin.y, width, 0.0),
                ..RichLayout::default()
            };
        };
        let mut lines = self.wrap(size, width);
        if lines.len() > max_lines {
            lines.truncate(max_lines);
            if let Some(line) = lines.last_mut() {
                match line.last_mut() {
                    Some((_, text)) => text.push('\u{2026}'),
                    None => line.push((self.runs.len().saturating_sub(1), "\u{2026}".into())),
                }
            }
        }
        let height = (size * 1.25).round();
        let mut layout = RichLayout::default();
        let mut top = origin.y;
        for line in lines {
            let widths: Vec<f32> = line
                .iter()
                .map(|(run, text)| {
                    self.runs[*run]
                        .style
                        .font()
                        .map_or(0.0, |font| font.measure(text, size))
                })
                .collect();
            let total: f32 = widths.iter().sum();
            let mut pen = match align {
                Align::Left => origin.x,
                Align::Center => (origin.x + (width - total) / 2.0).round(),
            };
            // Every face sits on the interface font's baseline, so a change of
            // font partway through a line does not make it jump.
            let baseline = base.centered_baseline(size, top, height);
            for ((run, text), advance) in line.into_iter().zip(widths) {
                if let Some(url) = &self.runs[run].link {
                    layout.links.push(LinkArea {
                        run,
                        url: url.clone(),
                        rect: Rect::new(pen, top, advance, height),
                    });
                }
                layout.fragments.push(Fragment {
                    run,
                    text,
                    origin: Point::new(pen, baseline),
                    width: advance,
                });
                pen += advance;
            }
            top += height;
        }
        layout.bounds = Rect::new(origin.x, origin.y, width, top - origin.y);
        layout
    }

    /// Draw `layout`, made from this text at `size`: `color` for the text and
    /// `link` for links, which are underlined too.
    pub fn paint(
        &self,
        canvas: &mut Canvas,
        layout: &RichLayout,
        size: f32,
        color: Color,
        link: Color,
    ) {
        for fragment in &layout.fragments {
            let Some(run) = self.runs.get(fragment.run) else {
                continue;
            };
            let Some(font) = run.style.font() else {
                continue;
            };
            let color = match run.link {
                Some(_) => link,
                None => color,
            };
            canvas.draw_text(&fragment.text, &font, size, fragment.origin, color);
            if run.link.is_some() {
                let below = (size * 0.12).round().max(1.0);
                let line = Rect::new(
                    fragment.origin.x,
                    fragment.origin.y + below,
                    fragment.width,
                    1.0,
                );
                canvas.fill_rect(line, color);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(layout: &RichLayout) -> Vec<String> {
        let mut out: Vec<(f32, String)> = Vec::new();
        for fragment in &layout.fragments {
            match out.last_mut() {
                Some((y, text)) if *y == fragment.origin.y => text.push_str(&fragment.text),
                _ => out.push((fragment.origin.y, fragment.text.clone())),
            }
        }
        out.into_iter().map(|(_, text)| text).collect()
    }

    #[test]
    fn plain_text_wraps_where_font_wrap_does() {
        let Some(font) = Font::system(400) else {
            return;
        };
        let text = "alpha beta gamma delta epsilon zeta eta theta";
        let width = font.measure("alpha beta gamma", 13.0);
        let rich = RichText::from(text);
        let layout = rich.layout(Point::default(), width, 13.0, Align::Left, usize::MAX);
        assert_eq!(lines(&layout), font.wrap(text, 13.0, width));
    }

    #[test]
    fn a_word_split_across_runs_is_not_broken() {
        let Some(font) = Font::system(400) else {
            return;
        };
        let width = font.measure("aaaa bbbbcccc", 13.0) - 1.0;
        let rich = RichText::new().plain("aaaa bbbb").bold("cccc");
        let layout = rich.layout(Point::default(), width, 13.0, Align::Left, usize::MAX);
        assert_eq!(lines(&layout), ["aaaa", "bbbbcccc"]);
    }

    #[test]
    fn newlines_start_lines_and_blank_lines_are_kept() {
        if Font::system(400).is_none() {
            return;
        }
        let rich = RichText::new().plain("one\n\n").bold("two");
        let layout = rich.layout(Point::default(), 1000.0, 13.0, Align::Left, usize::MAX);
        assert_eq!(layout.bounds.height, 3.0 * (13.0f32 * 1.25).round());
        let texts: Vec<_> = layout.fragments.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(texts, ["one", "two"]);
    }

    #[test]
    fn a_link_is_hit_on_every_line_it_spans() {
        let Some(font) = Font::system(400) else {
            return;
        };
        let url = "https://example.com/a/very/long/path/to/a/download.zip";
        let width = font.measure(&url[..20], 11.0);
        let rich = RichText::new().plain("From ").link(url, url);
        let layout = rich.layout(Point::default(), width, 11.0, Align::Left, usize::MAX);
        assert!(layout.links.len() > 1);
        assert!(layout.links.iter().all(|link| link.url == url));
        let joined: String = layout
            .fragments
            .iter()
            .filter(|f| f.run == 1)
            .map(|f| f.text.as_str())
            .collect();
        assert_eq!(joined, url);
        assert_eq!(layout.link_at(layout.links[0].rect.center()), Some(0));
        assert_eq!(layout.link_at(Point::new(1.0, 1.0)), None);
    }

    #[test]
    fn centred_lines_sit_in_the_middle_of_the_column() {
        if Font::system(400).is_none() {
            return;
        }
        let rich = RichText::new().plain("short");
        let layout = rich.layout(Point::new(10.0, 0.0), 300.0, 13.0, Align::Center, 4);
        let fragment = &layout.fragments[0];
        let left = fragment.origin.x - 10.0;
        let right = 300.0 - left - fragment.width;
        assert!((left - right).abs() <= 1.0);
    }

    #[test]
    fn lines_past_the_limit_are_cut_with_an_ellipsis() {
        if Font::system(400).is_none() {
            return;
        }
        let rich = RichText::from("a\nb\nc\nd");
        let layout = rich.layout(Point::default(), 1000.0, 13.0, Align::Left, 2);
        let texts: Vec<_> = layout.fragments.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(texts, ["a", "b\u{2026}"]);
    }

    #[test]
    fn styling_drops_out_of_the_plain_text() {
        let rich = RichText::new()
            .plain("Run ")
            .code("rosetta3 prefix create")
            .plain(" or see ")
            .link("the docs", "https://example.com");
        assert_eq!(
            rich.to_plain(),
            "Run rosetta3 prefix create or see the docs"
        );
    }

    #[test]
    fn links_paint_in_the_link_color() {
        if Font::system(400).is_none() {
            return;
        }
        let rich = RichText::new().link("link", "https://example.com");
        let layout = rich.layout(Point::new(2.0, 2.0), 100.0, 13.0, Align::Left, 1);
        let (width, height) = (104, 24);
        let mut pixels = vec![0u8; width * height * 4];
        let mut canvas = Canvas::new(&mut pixels, width, height, width * 4, 1.0);
        let red = Color::rgb(0xFF, 0, 0);
        rich.paint(&mut canvas, &layout, 13.0, Color::rgb(0, 0xFF, 0), red);
        // BGRA: some red is drawn, and no green.
        assert!(pixels.chunks(4).any(|texel| texel[2] > 0));
        assert!(pixels.chunks(4).all(|texel| texel[1] == 0));
    }
}
