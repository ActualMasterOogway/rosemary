//! The draw list, and the two-hundred-line immediate-mode UI built on it.

use super::font::Atlas;

/// One vertex, in the layout both backends declare to their shaders.
///
/// `repr(C, align(16))` because the Metal side receives this as a `device` array of a
/// struct whose `float4` member forces sixteen-byte alignment; the OpenGL side reads
/// the same bytes through `glVertexAttribPointer` and does not care.
#[repr(C, align(16))]
#[derive(Clone, Copy, Default)]
pub struct Vertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
    pub color: [f32; 4],
}

/// A colour, straight (not premultiplied) alpha.
pub type Color = [f32; 4];

/// The palette. Deliberately dark and low-contrast except where a number is meant to be
/// read.
pub mod color {
    use super::Color;

    /// Panel background. Dark and mostly opaque — a profiler's numbers have to
    /// be legible over a bright sky as well as a dark cave.
    pub const PANEL: Color = [0.04, 0.05, 0.07, 0.88];
    /// The one-pixel edge that separates the panel from the frame behind it.
    pub const EDGE: Color = [0.35, 0.40, 0.50, 0.55];
    /// A section heading.
    pub const HEADING: Color = [0.55, 0.78, 1.00, 1.0];
    /// A row's label.
    pub const LABEL: Color = [0.62, 0.66, 0.72, 1.0];
    /// A row's value: the thing the panel exists to show.
    pub const VALUE: Color = [0.92, 0.94, 0.97, 1.0];
    /// A figure that is a measurement of the translation itself.
    pub const GUEST: Color = [0.55, 0.90, 0.60, 1.0];
    /// A figure that is this runtime's own overhead.
    pub const RUNTIME: Color = [1.00, 0.70, 0.35, 1.0];
    /// A figure that is neither: the host's libraries.
    pub const HOST: Color = [0.60, 0.65, 0.95, 1.0];
    /// Something that is off, missing, or not being counted.
    pub const MUTED: Color = [0.45, 0.47, 0.52, 1.0];
    /// A recording in progress, and anything else that must be noticed.
    pub const ALERT: Color = [1.00, 0.35, 0.40, 1.0];
    /// The unfilled part of a bar.
    pub const TRACK: Color = [0.16, 0.18, 0.22, 1.0];
}

/// A frame's worth of geometry.
pub struct DrawList {
    pub vertices: Vec<Vertex>,
    /// The surface, in pixels. The projection's divisor.
    pub width: f32,
    pub height: f32,
    atlas: &'static Atlas,
    solid: (f32, f32),
    /// `(x0, y0, x1, y1)`; nothing is drawn outside it. See [`DrawList::clip`].
    clip: (f32, f32, f32, f32),
}

impl DrawList {
    pub fn new(width: f32, height: f32, atlas: &'static Atlas) -> Self {
        DrawList {
            // Enough for a full statistics panel without a reallocation, which
            // is the common case and the only one on the render thread.
            vertices: Vec::with_capacity(8192),
            width,
            height,
            atlas,
            solid: atlas.solid(),
            clip: (0.0, 0.0, width, height),
        }
    }

    pub fn atlas(&self) -> &'static Atlas {
        self.atlas
    }

    pub fn is_empty(&self) -> bool {
        self.vertices.is_empty()
    }

    /// The width `s` will occupy, for a caller sizing a box around it.
    pub fn measure(&self, s: &str) -> f32 {
        self.atlas.measure(s)
    }

    /// Restrict everything drawn from now on to `rect`, as `(x0, y0, x1, y1)`.
    pub fn clip(&mut self, rect: (f32, f32, f32, f32)) {
        self.clip = rect;
    }

    /// Draw without any clip.
    pub fn unclip(&mut self) {
        self.clip = (0.0, 0.0, self.width, self.height);
    }

    /// Two triangles.
    ///
    /// Triangles rather than a strip or an index buffer because a draw list of
    /// unrelated quads cannot be stripped, and six vertices of thirty-two bytes
    /// is cheaper to produce and upload than four plus an index buffer the
    /// backends would both have to manage.
    fn quad(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, uv: (f32, f32, f32, f32), color: Color) {
        let (mut u0, mut v0, mut u1, mut v1) = uv;
        let (mut x0, mut y0, mut x1, mut y1) = (x0, y0, x1, y1);
        let (cx0, cy0, cx1, cy1) = self.clip;
        // Entirely outside: the common case for a scrolled-away row, and the
        // one worth being cheap.
        if x1 <= cx0 || x0 >= cx1 || y1 <= cy0 || y0 >= cy1 {
            return;
        }
        // Each edge trimmed by the fraction of the quad it cut off, and the
        // texture coordinate trimmed by the same fraction.
        let (w, h) = (x1 - x0, y1 - y0);
        if w > 0.0 {
            if x0 < cx0 {
                u0 += (u1 - u0) * (cx0 - x0) / w;
                x0 = cx0;
            }
            if x1 > cx1 {
                u1 -= (u1 - u0) * (x1 - cx1) / w;
                x1 = cx1;
            }
        }
        if h > 0.0 {
            if y0 < cy0 {
                v0 += (v1 - v0) * (cy0 - y0) / h;
                y0 = cy0;
            }
            if y1 > cy1 {
                v1 -= (v1 - v0) * (y1 - cy1) / h;
                y1 = cy1;
            }
        }
        let (u0, v0, u1, v1) = (u0, v0, u1, v1);
        let corner = |x, y, u, v| Vertex {
            pos: [x, y],
            uv: [u, v],
            color,
        };
        let (a, b, c, d) = (
            corner(x0, y0, u0, v0),
            corner(x1, y0, u1, v0),
            corner(x1, y1, u1, v1),
            corner(x0, y1, u0, v1),
        );
        self.vertices.extend_from_slice(&[a, b, c, a, c, d]);
    }

    /// A filled rectangle.
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        if w <= 0.0 || h <= 0.0 || color[3] <= 0.0 {
            return;
        }
        let (u, v) = self.solid;
        self.quad(x, y, x + w, y + h, (u, v, u, v), color);
    }

    /// A one-pixel outline, drawn as four rectangles.
    pub fn frame(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        self.rect(x, y, w, 1.0, color);
        self.rect(x, y + h - 1.0, w, 1.0, color);
        self.rect(x, y, 1.0, h, color);
        self.rect(x + w - 1.0, y, 1.0, h, color);
    }

    /// A string, with its top-left corner at `(x, y)`. Returns the width drawn.
    pub fn text(&mut self, x: f32, y: f32, s: &str, color: Color) -> f32 {
        let (cw, ch) = (self.atlas.cell_w as f32, self.atlas.cell_h as f32);
        let advance = self.atlas.advance;
        let mut pen = x;
        for &byte in s.as_bytes() {
            if byte != b' ' {
                let uv = self.atlas.glyph(byte);
                self.quad(pen, y, pen + cw, y + ch, uv, color);
            }
            pen += advance;
        }
        pen - x
    }
}


/// A cursor over a [`DrawList`], and the handful of widgets the panels need.
pub struct Ui<'a> {
    pub list: &'a mut DrawList,
    /// Where the next row goes, and the left edge rows return to.
    pub x: f32,
    pub y: f32,
    pub left: f32,
    /// The width a row may use.
    pub width: f32,
    /// Distance between the tops of two rows.
    pub leading: f32,
    /// One character's advance, which is the unit columns are measured in.
    pub em: f32,
}

impl<'a> Ui<'a> {
    pub fn new(list: &'a mut DrawList, x: f32, y: f32, width: f32) -> Self {
        // A quarter of a line of leading. Dense enough that a long ranked list
        // fits on a 720p window, loose enough to read.
        let leading = (list.atlas().line_h * 1.25).round();
        let em = list.atlas().advance;
        Ui {
            list,
            x,
            y,
            left: x,
            width,
            leading,
            em,
        }
    }

    /// Move to the start of the next row.
    pub fn newline(&mut self) {
        self.x = self.left;
        self.y += self.leading;
    }

    /// Vertical space, in fractions of a row.
    pub fn gap(&mut self, rows: f32) {
        self.y += self.leading * rows;
    }

    /// Text at the cursor, advancing it.
    pub fn run(&mut self, s: &str, color: Color) {
        self.x += self.list.text(self.x, self.y, s, color);
    }

    /// Move the cursor to a column, measured in characters from the left edge.
    ///
    /// Never backwards.
    pub fn column(&mut self, chars: f32) {
        self.x = self.x.max(self.left + chars * self.em);
    }

    /// A whole line of text.
    pub fn line(&mut self, s: &str, color: Color) {
        self.run(s, color);
        self.newline();
    }

    /// A section heading, with a rule under it.
    pub fn heading(&mut self, s: &str) {
        self.gap(0.35);
        self.run(s, color::HEADING);
        self.newline();
        self.list.rect(
            self.left,
            self.y - self.leading * 0.2,
            self.width,
            1.0,
            color::EDGE,
        );
        self.gap(0.15);
    }

    /// A label on the left and a value at a fixed column.
    pub fn row(&mut self, label: &str, value: &str, color: Color) {
        self.run(label, color::LABEL);
        self.column(22.0);
        self.run(value, color);
        self.newline();
    }

    /// A label, a value, and a proportional bar filling the rest of the row.
    pub fn bar(&mut self, label: &str, value: &str, share: f64, color: Color) {
        self.run(label, color::LABEL);
        self.column(22.0);
        self.run(value, color);
        self.column(34.0);
        let x = self.x;
        let w = (self.left + self.width - x).max(0.0);
        let h = (self.leading * 0.45).round().max(2.0);
        let y = self.y + (self.list.atlas().line_h - h) / 2.0;
        self.list.rect(x, y, w, h, color::TRACK);
        let filled = w * (share / 100.0).clamp(0.0, 1.0) as f32;
        self.list.rect(x, y, filled, h, color);
        self.newline();
    }
}


/// A count with thousands separators, which is the difference between reading
/// `3041548651` and reading `3,041,548,651` off a moving screen.
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A rate, in whichever unit keeps it to three significant figures.
pub fn rate(per_second: f64) -> String {
    match per_second {
        r if r >= 1e9 => format!("{:.2}G/s", r / 1e9),
        r if r >= 1e6 => format!("{:.2}M/s", r / 1e6),
        r if r >= 1e3 => format!("{:.1}K/s", r / 1e3),
        r => format!("{r:.0}/s"),
    }
}

/// A byte count, in binary units.
pub fn bytes(n: u64) -> String {
    match n {
        b if b >= 1 << 30 => format!("{:.2} GiB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{:.1} KiB", b as f64 / (1u64 << 10) as f64),
        b => format!("{b} B"),
    }
}

/// A duration, at whatever scale it happens to be.
pub fn duration(d: std::time::Duration) -> String {
    let s = d.as_secs_f64();
    match s {
        s if s >= 60.0 => format!("{}m{:02}s", (s / 60.0) as u64, (s % 60.0) as u64),
        s if s >= 1.0 => format!("{s:.1}s"),
        s if s >= 1e-3 => format!("{:.1}ms", s * 1e3),
        // `us` rather than `µs`: the atlas is printable ASCII, and a multi-byte
        // character would draw as two question marks.
        s if s >= 1e-6 => format!("{:.1}us", s * 1e6),
        s => format!("{:.0}ns", s * 1e9),
    }
}
