//! The welcome scene: a cat and a dog sitting either side of Tawny the owlet.
//!
//! A port of `web/scene.js` (itself a port of `PetSceneView` in the Android
//! app): the same 260×170 scene space, the same shapes, the same motion, so
//! every Tawny opens on the same trio. Canvas 2D maps almost call for call onto
//! `tiny-skia`, which keeps this a port rather than a redrawing: when the
//! scene changes there, change this the same way.

#![expect(
    clippy::unreadable_literal,
    clippy::many_single_char_names,
    clippy::manual_midpoint,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::too_many_lines,
    reason = "a line-for-line port of web/scene.js: its arithmetic, names and colours stay as written there"
)]

use std::f32::consts::PI;

use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform,
};

const TWO_PI: f32 = PI * 2.0;
const VW: f32 = 260.0;
const VH: f32 = 170.0;
const LOOP_S: f32 = 4.2;
/// Sky above the scene: the owlet's flight peaks near the top edge.
const HEAD: f32 = 18.0;
/// How long each reaction lasts, s: 1 cat, 2 dog, 3 owlet, 4 owlet flutter (solo).
const REACT_S: [f32; 5] = [0.0, 1.3, 1.5, 2.7, 1.4];

/// Who reacts to a tap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Critter {
    Cat = 1,
    Dog = 2,
    Owlet = 3,
    /// The solo owlet's hop, once per flutter asked for.
    Flutter = 4,
}

/// Who is in the picture: the whole trio, or the owlet on her own, close up
/// (the Support card and sheet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cast {
    Trio,
    Owlet,
}

impl Cast {
    /// What the cast frames, in scene units: x, y, width, height.
    const fn view(self) -> (f32, f32, f32, f32) {
        match self {
            Self::Trio => (0.0, -HEAD, VW, VH + HEAD),
            Self::Owlet => (94.0, 94.0, 72.0, 64.0),
        }
    }
}

/// The scene's colours (style.css `--pet-*`, `--berry`, `--sky`).
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub cream: Rgba,
    pub cream_hi: Rgba,
    pub cream_lo: Rgba,
    pub biscuit: Rgba,
    pub biscuit_lo: Rgba,
    pub dove: Rgba,
    pub ink: Rgba,
    pub edge: Rgba,
    pub hair: Rgba,
    pub tongue: Rgba,
    pub berry: Rgba,
    pub sky: Rgba,
}

impl Palette {
    /// The light theme's colours.
    #[must_use]
    pub const fn light() -> Self {
        Self {
            dark: false,
            cream: Rgba::hex(0xf1e7d3),
            cream_hi: Rgba::hex(0xfdf8ee),
            cream_lo: Rgba::hex(0xddccad),
            biscuit: Rgba::hex(0xd59e6b),
            biscuit_lo: Rgba::hex(0xbb8453),
            dove: Rgba::hex(0xc2b8a5),
            ink: Rgba::hex(0x3c2a1e),
            edge: Rgba::hex(0x48331e),
            hair: Rgba::hex(0x5a5364),
            tongue: Rgba::hex(0xe8728f),
            berry: Rgba::hex(0xd24b6d),
            sky: Rgba::hex(0x5b93b8),
        }
    }

    /// The dark theme's colours.
    #[must_use]
    pub const fn dark() -> Self {
        Self {
            dark: true,
            cream: Rgba::hex(0xe7dbc1),
            cream_hi: Rgba::hex(0xf4ead5),
            cream_lo: Rgba::hex(0xc9b790),
            biscuit: Rgba::hex(0xcc9a63),
            biscuit_lo: Rgba::hex(0xa9784a),
            dove: Rgba::hex(0xb6ad99),
            ink: Rgba::hex(0x2e2116),
            edge: Rgba::hex(0x3a2a1b),
            hair: Rgba::hex(0x5b4634),
            tongue: Rgba::hex(0xe8728f),
            berry: Rgba::hex(0xc79b64),
            sky: Rgba::hex(0xa6b489),
        }
    }

    const fn hi(&self) -> Rgba {
        if self.dark {
            Rgba::white(0.133)
        } else {
            Rgba::white(0.169)
        }
    }

    const fn lo(&self) -> Rgba {
        if self.dark {
            Rgba::black(0.149)
        } else {
            Rgba::black(0.122)
        }
    }
}

/// A colour, straight alpha.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    r: u8,
    g: u8,
    b: u8,
    a: f32,
}

impl Rgba {
    #[must_use]
    pub const fn hex(v: u32) -> Self {
        Self {
            r: ((v >> 16) & 0xff) as u8,
            g: ((v >> 8) & 0xff) as u8,
            b: (v & 0xff) as u8,
            a: 1.0,
        }
    }

    const fn white(a: f32) -> Self {
        Self {
            r: 255,
            g: 255,
            b: 255,
            a,
        }
    }

    const fn black(a: f32) -> Self {
        Self {
            r: 0,
            g: 0,
            b: 0,
            a,
        }
    }

    fn with_alpha(self, a: f32) -> Color {
        Color::from_rgba8(
            self.r,
            self.g,
            self.b,
            (self.a * a * 255.0).round().clamp(0.0, 255.0) as u8,
        )
    }
}

fn clamp(v: f32, a: f32, b: f32) -> f32 {
    v.max(a).min(b)
}
fn ramp(x: f32) -> f32 {
    let u = clamp(x, 0.0, 1.0);
    u * u * (3.0 - 2.0 * u)
}
fn hump(x: f32) -> f32 {
    if x <= 0.0 || x >= 1.0 {
        0.0
    } else {
        (x * PI).sin()
    }
}
fn lerp(a: f32, b: f32, u: f32) -> f32 {
    a + (b - a) * u
}
fn snap(u: f32, out: f32) -> f32 {
    if u <= 0.0 || u >= 1.0 {
        0.0
    } else if u < out {
        ramp(u / out)
    } else {
        1.0 - ramp((u - out) / (1.0 - out))
    }
}
fn hangs(s: f32, k: f32) -> f32 {
    (1.0 + k) * s / (1.0 + k * s.abs())
}

/// Kotlin's Int arithmetic, overflow and all, so the idle schedule matches.
fn hash01(n: i32) -> f32 {
    let mut h = n.wrapping_mul(374_761_393).wrapping_add(668_265_263);
    h = (h ^ ((h as u32) >> 13) as i32).wrapping_mul(1_274_126_177);
    let v = (h ^ ((h as u32) >> 16) as i32) & 0x07ff_ffff;
    v as f32 / 134_217_727.0
}

/// Canvas 2D's state, as far as the scene uses it.
#[derive(Clone, Copy)]
struct State {
    t: Transform,
    alpha: f32,
}

/// A Canvas 2D look-alike over a `tiny-skia` pixmap.
struct Canvas<'a> {
    px: &'a mut Pixmap,
    st: State,
    stack: Vec<State>,
    path: PathBuilder,
}

impl<'a> Canvas<'a> {
    fn new(px: &'a mut Pixmap, t: Transform) -> Self {
        Self {
            px,
            st: State { t, alpha: 1.0 },
            stack: Vec::new(),
            path: PathBuilder::new(),
        }
    }
    fn save(&mut self) {
        self.stack.push(self.st);
    }
    fn restore(&mut self) {
        if let Some(s) = self.stack.pop() {
            self.st = s;
        }
    }
    fn translate(&mut self, x: f32, y: f32) {
        self.st.t = self.st.t.pre_translate(x, y);
    }
    fn scale(&mut self, x: f32, y: f32) {
        self.st.t = self.st.t.pre_scale(x, y);
    }
    fn rotate_rad(&mut self, a: f32) {
        self.st.t = self.st.t.pre_concat(Transform::from_rotate(a.to_degrees()));
    }
    fn begin(&mut self) {
        self.path = PathBuilder::new();
    }
    fn move_to(&mut self, x: f32, y: f32) {
        self.path.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.path.line_to(x, y);
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.path.quad_to(cx, cy, x, y);
    }
    fn cubic_to(&mut self, a: f32, b: f32, c: f32, d: f32, x: f32, y: f32) {
        self.path.cubic_to(a, b, c, d, x, y);
    }
    fn close(&mut self) {
        self.path.close();
    }
    /// `ellipse(cx, cy, rx, ry, 0, a0, a1)`, as Béziers (a quarter turn each at most).
    fn ellipse(&mut self, cx: f32, cy: f32, rx: f32, ry: f32, a0: f32, a1: f32) {
        let pieces = ((a1 - a0).abs() / (PI / 2.0)).ceil().max(1.0) as usize;
        let step = (a1 - a0) / pieces as f32;
        let k = 4.0 / 3.0 * (step / 4.0).tan();
        let at = |a: f32| (cx + rx * a.cos(), cy + ry * a.sin());
        let (x0, y0) = at(a0);
        if self.path.is_empty() {
            self.path.move_to(x0, y0);
        } else {
            self.path.line_to(x0, y0);
        }
        for i in 0..pieces {
            let s = a0 + step * i as f32;
            let e = s + step;
            let (sx, sy) = at(s);
            let (ex, ey) = at(e);
            self.path.cubic_to(
                sx - k * rx * s.sin(),
                sy + k * ry * s.cos(),
                ex + k * rx * e.sin(),
                ey - k * ry * e.cos(),
                ex,
                ey,
            );
        }
    }
    fn arc(&mut self, cx: f32, cy: f32, r: f32) {
        self.ellipse(cx, cy, r, r, 0.0, TWO_PI);
        self.path.close();
    }
    fn round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
        self.path.move_to(x + r, y);
        self.path.line_to(x + w - r, y);
        self.corner(x + w - r, y + r, r, -PI / 2.0);
        self.path.line_to(x + w, y + h - r);
        self.corner(x + w - r, y + h - r, r, 0.0);
        self.path.line_to(x + r, y + h);
        self.corner(x + r, y + h - r, r, PI / 2.0);
        self.path.line_to(x, y + r);
        self.corner(x + r, y + r, r, PI);
        self.path.close();
    }
    fn corner(&mut self, cx: f32, cy: f32, r: f32, a0: f32) {
        let k = 4.0 / 3.0 * (PI / 8.0).tan();
        let a1 = a0 + PI / 2.0;
        self.path.cubic_to(
            cx + r * a0.cos() - k * r * a0.sin(),
            cy + r * a0.sin() + k * r * a0.cos(),
            cx + r * a1.cos() + k * r * a1.sin(),
            cy + r * a1.sin() - k * r * a1.cos(),
            cx + r * a1.cos(),
            cy + r * a1.sin(),
        );
    }
    fn fill(&mut self, c: Rgba) {
        let Some(path) = self.path.clone().finish() else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color(c.with_alpha(self.st.alpha));
        paint.anti_alias = true;
        self.px
            .fill_path(&path, &paint, FillRule::Winding, self.st.t, None);
    }
    fn stroke(&mut self, width: f32, c: Rgba, alpha: f32) {
        let Some(path) = self.path.clone().finish() else {
            return;
        };
        let mut paint = Paint::default();
        paint.set_color(c.with_alpha(self.st.alpha * alpha));
        paint.anti_alias = true;
        let stroke = Stroke {
            width,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Stroke::default()
        };
        self.px.stroke_path(&path, &paint, &stroke, self.st.t, None);
    }
}

/// The scene's clock and who is reacting.
#[derive(Debug, Clone)]
pub struct Scene {
    /// Animations off: a still pose, not a frozen gesture.
    pub still: bool,
    cast: Cast,
    react: Option<(Critter, f32)>,
    /// Hops in the current flutter (1..=4).
    flutters: u8,
}

impl Default for Scene {
    fn default() -> Self {
        Self::new(false)
    }
}

/// One frame's drawing: the scene, its clock and the canvas.
struct Frame<'c, 'p> {
    c: &'c mut Canvas<'p>,
    k: Palette,
    still: bool,
    /// Seconds since the scene was made (0 when still).
    clock: f32,
    react: Option<(Critter, f32)>,
    flutters: u8,
}

impl Scene {
    #[must_use]
    pub const fn new(still: bool) -> Self {
        Self::with_cast(Cast::Trio, still)
    }

    /// The owlet alone, framed close.
    #[must_use]
    pub const fn owlet(still: bool) -> Self {
        Self::with_cast(Cast::Owlet, still)
    }

    const fn with_cast(cast: Cast, still: bool) -> Self {
        Self {
            still,
            cast,
            react: None,
            flutters: 1,
        }
    }

    /// The solo owlet hops `times` times (1..=4), each a little higher: the
    /// Support sheet's answer to an amount. Nothing when still.
    pub fn flutter(&mut self, times: u8, now: f32) {
        if self.still || self.cast != Cast::Owlet {
            return;
        }
        self.flutters = times.clamp(1, 4);
        self.react = Some((Critter::Flutter, now));
    }

    /// A tip landed: the owlet takes off over the other two. Nothing when still.
    pub fn cheer(&mut self, now: f32) {
        if !self.still {
            self.react = Some((Critter::Owlet, now));
        }
    }

    fn span(&self, who: Critter) -> f32 {
        REACT_S[who as usize]
            * if who == Critter::Flutter {
                f32::from(self.flutters)
            } else {
                1.0
            }
    }

    /// A tap at (`x`, `y`) in a `w`×`h` picture: the pet there reacts.
    pub fn tap(&mut self, x: f32, y: f32, w: f32, h: f32, now: f32) -> bool {
        let (vx, vy, vw, vh) = self.cast.view();
        let s = (w / vw).min(h / vh);
        let sx = (x - (w - vw * s) / 2.0) / s + vx;
        let sy = (y - (h - vh * s) / 2.0) / s + vy;
        let who = if self.cast == Cast::Owlet {
            self.flutters = 1;
            Critter::Flutter
        } else if (108.0..=152.0).contains(&sx) && (88.0..=156.0).contains(&sy) {
            Critter::Owlet
        } else if (28.0..=114.0).contains(&sx) && (56.0..=158.0).contains(&sy) {
            Critter::Cat
        } else if (146.0..=244.0).contains(&sx) && (50.0..=158.0).contains(&sy) {
            Critter::Dog
        } else {
            return false;
        };
        self.react = Some((who, now));
        true
    }

    /// Whether frames should keep coming (moving, or a reaction playing).
    #[must_use]
    pub fn animating(&self, now: f32) -> bool {
        !self.still
            || self
                .react
                .is_some_and(|(who, at)| now - at < self.span(who))
    }

    /// Draws the cast into a `w`×`h` picture at `now` seconds, centred and fitted.
    #[must_use]
    pub fn render(&self, w: u32, h: u32, now: f32, k: Palette) -> Option<Pixmap> {
        let mut px = Pixmap::new(w.max(1), h.max(1))?;
        let (wf, hf) = (w as f32, h as f32);
        let (vx, vy, vw, vh) = self.cast.view();
        let s = (wf / vw).min(hf / vh);
        let t =
            Transform::from_translate((wf - vw * s) / 2.0 - vx * s, (hf - vh * s) / 2.0 - vy * s)
                .pre_scale(s, s);
        let mut c = Canvas::new(&mut px, t);
        let react = self.react.filter(|(who, at)| now - at < self.span(*who));
        let mut f = Frame {
            c: &mut c,
            k,
            still: self.still,
            clock: if self.still { 0.0 } else { now },
            react,
            flutters: self.flutters,
        };
        if self.cast == Cast::Owlet {
            f.draw_solo(now);
        } else {
            f.draw(now);
        }
        Some(px)
    }
}

impl Frame<'_, '_> {
    /// The 4.2 s loop, 0..1.
    fn t(&self) -> f32 {
        if self.still {
            0.0
        } else {
            (self.clock % LOOP_S) / LOOP_S
        }
    }
    fn react_p(&self, who: Critter, now: f32) -> f32 {
        match self.react {
            Some((w, at)) if w == who => {
                let span = REACT_S[who as usize]
                    * if who == Critter::Flutter {
                        f32::from(self.flutters)
                    } else {
                        1.0
                    };
                clamp((now - at) / span, 0.0, 1.0)
            }
            _ => 0.0,
        }
    }
    fn react_secs(&self, now: f32) -> f32 {
        self.react.map_or(now, |(_, at)| now - at)
    }
    fn drift(&self, u: f32) -> f32 {
        if self.still {
            return 0.0;
        }
        (u.sin() + 0.62 * (u * 1.73 + 1.3).sin() + 0.41 * (u * 2.91 + 2.6).sin()) * 0.49
    }
    fn breath(&self, at: f32) -> f32 {
        if self.still {
            return 0.5;
        }
        let u = at / 2.9;
        snap(u - u.floor(), 0.42)
    }
    fn beat(&self, seed: i32, every: f32, dur: f32, chance: f32) -> f32 {
        let ck = self.clock;
        if ck <= 0.0 {
            return 0.0;
        }
        let w = ck / every + hash01(seed.wrapping_mul(977).wrapping_add(13));
        let i = w.floor();
        let ii = i as i32;
        if chance < 1.0 && hash01(ii.wrapping_mul(8191).wrapping_add(seed)) > chance {
            return 0.0;
        }
        let at = hash01(ii.wrapping_mul(131).wrapping_add(seed.wrapping_mul(7)))
            * (1.0 - dur / every).max(0.0);
        let u = (w - i - at) * every / dur;
        if u <= 0.0 || u >= 1.0 { 0.0 } else { u }
    }
    fn blink_at(&self, seed: i32) -> f32 {
        let shut = snap(self.beat(seed, 3.6, 0.30, 1.0), 0.38)
            .max(snap(self.beat(seed + 101, 5.3, 0.26, 0.45), 0.38));
        1.0 - 0.94 * shut
    }
    fn blink(&self, offset: f32) -> f32 {
        let d = (((self.t() + offset) % 1.0) - 0.5).abs();
        if d < 0.032 {
            clamp(d / 0.032, 0.06, 1.0)
        } else {
            1.0
        }
    }
    fn contact(&self, seed: i32) -> f32 {
        let u = self.beat(seed * 31 + 8, 8.5, 2.2, 0.55);
        if u <= 0.0 {
            return 0.0;
        }
        clamp(ramp(u / 0.3) * (1.0 - ramp((u - 0.62) / 0.38)), 0.0, 1.0)
    }
    fn gaze_x(&self, seed: i32, home: f32) -> f32 {
        if self.still {
            return home;
        }
        let wander = self.drift(self.clock * 0.29 + seed as f32 * 1.7) * 0.3;
        let away = snap(self.beat(seed * 31 + 7, 9.5, 2.0, 0.45), 0.25);
        let base = (home + wander) * (1.0 - away) - home * away * 0.9;
        base * (1.0 - self.contact(seed))
    }
    fn gaze_y(&self, seed: i32) -> f32 {
        if self.still {
            return 0.0;
        }
        let up = snap(self.beat(seed * 31 + 9, 12.0, 1.6, 0.45), 0.28) * -0.85;
        up * (1.0 - self.contact(seed))
    }
    fn owl_flight(op: f32) -> (f32, f32, f32, f32) {
        let (perch_x, perch_y, cx, cy, rx, ry) = (130.0, 130.0, 130.0, 46.0, 82.0, 34.0);
        let a0 = -PI / 2.0;
        let at = |u: f32| {
            let a = a0 + clamp(u, 0.0, 1.0) * TWO_PI * 1.5;
            (cx + rx * a.cos(), cy + ry * a.sin(), a)
        };
        if op < 0.16 {
            let u = ramp(op / 0.16);
            let (lx, ly, _) = at(0.0);
            return (lerp(perch_x, lx, u), lerp(perch_y, ly, u), u * -8.0, 0.0);
        }
        if op < 0.82 {
            let (lx, ly, a) = at((op - 0.16) / 0.66);
            return (lx, ly, -a.cos() * 15.0, -a.sin() * 1.8);
        }
        let u = ramp((op - 0.82) / 0.18);
        let (lx, ly, _) = at(1.0);
        (
            lerp(lx, perch_x, u),
            lerp(ly, perch_y, u),
            lerp(-8.0, 0.0, u),
            0.0,
        )
    }

    // ---------------------------------------------------------- primitives

    fn edge(&mut self) {
        self.c.stroke(2.6, self.k.edge, 1.0);
    }
    fn rrect(&mut self, l: f32, t: f32, r: f32, b: f32, rad: f32) {
        self.c.begin();
        self.c.round_rect(l, t, r - l, b - t, rad);
    }
    fn oval(&mut self, l: f32, t: f32, r: f32, b: f32) {
        self.c.begin();
        self.c.ellipse(
            (l + r) / 2.0,
            (t + b) / 2.0,
            (r - l).abs() / 2.0,
            (b - t).abs() / 2.0,
            0.0,
            TWO_PI,
        );
        self.c.close();
    }
    fn capsule(&mut self, cx: f32, cy: f32, w: f32, h: f32, p: Rgba, edged: bool) {
        self.rrect(
            cx - w / 2.0,
            cy - h / 2.0,
            cx + w / 2.0,
            cy + h / 2.0,
            w.min(h) / 2.0,
        );
        self.c.fill(p);
        if edged {
            self.edge();
        }
    }
    fn mass(&mut self, cx: f32, cy: f32, w: f32, h: f32, base: Rgba) {
        let r = w.min(h) / 2.0;
        self.rrect(cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0, r);
        self.c.fill(base);
        self.oval(cx - w * 0.36, cy - h * 0.44, cx + w * 0.06, cy - h * 0.02);
        self.c.fill(self.k.hi());
        self.oval(cx - w * 0.40, cy + h * 0.04, cx + w * 0.40, cy + h * 0.46);
        self.c.fill(self.k.lo());
        self.rrect(cx - w / 2.0, cy - h / 2.0, cx + w / 2.0, cy + h / 2.0, r);
        self.edge();
    }
    fn cast_shadow(&mut self, cx: f32, cy: f32, w: f32, alpha: f32) {
        let mul = if self.k.dark { 1.9 } else { 1.0 };
        let a = clamp(alpha * mul, 0.0, 255.0) / 255.0;
        self.oval(cx - w / 2.0, cy - w * 0.11, cx + w / 2.0, cy + w * 0.11);
        self.c.fill(Rgba::black(a));
    }
    fn eye(&mut self, cx: f32, cy: f32, r: f32, open: f32, look: f32, look_y: f32) {
        let o = clamp(open, 0.0, 1.0);
        let y = cy + look_y * o;
        self.oval(cx - r + look, y - r * o, cx + r + look, y + r * o);
        self.c.fill(self.k.ink);
        if o > 0.55 {
            self.c.begin();
            self.c.arc(cx - r * 0.34 + look, cy - r * 0.44, r * 0.36);
            self.c.fill(self.k.cream_hi);
        }
    }
    fn owl_eye(&mut self, cx: f32, cy: f32, r: f32, open: f32, look: f32, look_y: f32) {
        let o = clamp(open, 0.08, 1.0);
        self.c.save();
        self.c.translate(cx, cy);
        self.c.scale(1.0, o);
        self.c.translate(-cx, -cy);
        self.c.begin();
        self.c.arc(cx, cy, r);
        self.c.fill(self.k.sky);
        self.c.begin();
        self.c.arc(cx, cy, r);
        self.c.stroke(2.6 * 0.68, self.k.edge, 1.0);
        let (px, py) = (cx + look, cy + r * 0.12 + look_y * r * 0.32);
        self.c.begin();
        self.c.arc(px, py, r * 0.52);
        self.c.fill(self.k.ink);
        self.c.begin();
        self.c.arc(px - r * 0.18, py - r * 0.36, r * 0.2);
        self.c.fill(self.k.cream_hi);
        self.c.restore();
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "scene.js's softTri, argument for argument"
    )]
    fn soft_tri(
        &mut self,
        ax: f32,
        ay: f32,
        bx: f32,
        by: f32,
        cx: f32,
        cy: f32,
        p: Rgba,
        edged: bool,
    ) {
        self.c.begin();
        self.c.move_to((ax + bx) / 2.0, (ay + by) / 2.0);
        self.c.quad_to(bx, by, (bx + cx) / 2.0, (by + cy) / 2.0);
        self.c.quad_to(cx, cy, (cx + ax) / 2.0, (cy + ay) / 2.0);
        self.c.quad_to(ax, ay, (ax + bx) / 2.0, (ay + by) / 2.0);
        self.c.close();
        self.c.fill(p);
        if edged {
            self.edge();
        }
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "scene.js's tri, argument for argument"
    )]
    fn tri(&mut self, ax: f32, ay: f32, bx: f32, by: f32, cx: f32, cy: f32, p: Rgba) {
        self.c.begin();
        self.c.move_to(ax, ay);
        self.c.line_to(bx, by);
        self.c.line_to(cx, cy);
        self.c.close();
        self.c.fill(p);
    }
    fn hair_alpha(&self) -> f32 {
        if self.k.dark {
            200.0 / 255.0
        } else {
            150.0 / 255.0
        }
    }
    fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32) {
        self.c.begin();
        self.c.move_to(x1, y1);
        self.c.line_to(x2, y2);
        let a = self.hair_alpha();
        self.c.stroke(2.2, self.k.hair, a);
    }
    /// `android.graphics.Canvas#drawArc(oval, start, sweep, useCenter=false)`.
    fn arc_in(&mut self, l: f32, t: f32, r: f32, b: f32, start: f32, sweep: f32) {
        self.c.begin();
        self.c.ellipse(
            (l + r) / 2.0,
            (t + b) / 2.0,
            (r - l) / 2.0,
            (b - t) / 2.0,
            start.to_radians(),
            (start + sweep).to_radians(),
        );
        let a = self.hair_alpha();
        self.c.stroke(2.2, self.k.hair, a);
    }
    fn rotate(&mut self, deg: f32, px: f32, py: f32) {
        self.c.translate(px, py);
        self.c.rotate_rad(deg.to_radians());
        self.c.translate(-px, -py);
    }
    fn scale_at(&mut self, sx: f32, sy: f32, px: f32, py: f32) {
        self.c.translate(px, py);
        self.c.scale(sx, sy);
        self.c.translate(-px, -py);
    }
    fn floppy_ear(&mut self, ax: f32, ay: f32, len: f32, wid: f32, angle: f32, p: Rgba) {
        self.c.save();
        self.rotate(angle, ax, ay);
        self.c.begin();
        self.c.move_to(ax - wid * 0.28, ay);
        self.c.cubic_to(
            ax - wid * 0.60,
            ay + len * 0.38,
            ax - wid * 0.52,
            ay + len * 0.88,
            ax,
            ay + len,
        );
        self.c.cubic_to(
            ax + wid * 0.52,
            ay + len * 0.88,
            ax + wid * 0.60,
            ay + len * 0.38,
            ax + wid * 0.28,
            ay,
        );
        self.c.quad_to(ax, ay - wid * 0.30, ax - wid * 0.28, ay);
        self.c.close();
        self.c.fill(p);
        self.edge();
        self.c.restore();
    }

    // ---------------------------------------------------------- the owlet

    fn owlet_flying(&mut self, x: f32, ground: f32, fly: (f32, f32, f32, f32), flap: f32) {
        let k = self.k;
        let (ax, ay, bank, look) = fly;
        let alt = clamp((ground - ay) / 66.0, 0.0, 1.0);
        self.cast_shadow(
            x,
            ground + 3.0,
            24.0 * (1.0 - 0.6 * alt),
            30.0 * (1.0 - 0.7 * alt),
        );
        self.c.save();
        self.rotate(bank, ax, ay);
        self.c.save();
        self.rotate(-42.0 - 30.0 * flap, ax - 6.0, ay - 1.0);
        self.capsule(ax - 15.0, ay + 1.0, 10.0, 22.0, k.cream, true);
        self.c.restore();
        self.c.save();
        self.rotate(42.0 + 30.0 * flap, ax + 6.0, ay - 1.0);
        self.capsule(ax + 15.0, ay + 1.0, 10.0, 22.0, k.cream, true);
        self.c.restore();
        self.soft_tri(
            ax - 8.0,
            ay - 10.0,
            ax - 3.0,
            ay - 22.0,
            ax + 1.0,
            ay - 11.0,
            k.cream,
            true,
        );
        self.soft_tri(
            ax + 8.0,
            ay - 10.0,
            ax + 3.0,
            ay - 22.0,
            ax - 1.0,
            ay - 11.0,
            k.cream,
            true,
        );
        self.mass(ax, ay, 25.0, 30.0, k.cream);
        self.capsule(ax, ay - 3.0, 21.0, 16.0, k.cream_hi, false);
        self.soft_tri(
            ax - 5.0,
            ay + 12.0,
            ax,
            ay + 22.0,
            ax + 5.0,
            ay + 12.0,
            k.cream_lo,
            true,
        );
        let bl = self.blink(0.45);
        self.owl_eye(ax - 5.2, ay - 4.0, 4.5, bl, look, -0.35);
        self.owl_eye(ax + 5.2, ay - 4.0, 4.5, bl, look, -0.35);
        self.tri(
            ax - 2.0,
            ay + 1.0,
            ax + 2.0,
            ay + 1.0,
            ax,
            ay + 5.0,
            k.berry,
        );
        self.c.restore();
    }

    fn owlet(&mut self, x: f32, ground: f32, t_norm: f32, hop: f32) {
        let k = self.k;
        let cy = ground - 18.0 - hop;
        let flap = clamp(hop / 8.0, 0.0, 1.0);
        let owl_up = 1.0 - self.contact(4);
        let look_x = (t_norm * TWO_PI).cos() * 0.9 * owl_up;
        self.cast_shadow(
            x,
            ground + 3.0,
            26.0 * (1.0 - 0.35 * flap),
            34.0 * (1.0 - 0.5 * flap),
        );
        if hop < 3.0 {
            self.tri(
                x - 4.0,
                ground - 2.0,
                x - 7.0,
                ground + 1.0,
                x - 1.0,
                ground + 1.0,
                k.berry,
            );
            self.tri(
                x + 4.0,
                ground - 2.0,
                x + 1.0,
                ground + 1.0,
                x + 7.0,
                ground + 1.0,
                k.berry,
            );
        }
        self.c.save();
        self.rotate(-18.0 - 24.0 * flap, x - 8.0, cy - 2.0);
        self.capsule(x - 11.0, cy + 3.0, 9.0, 17.0, k.cream, true);
        self.c.restore();
        self.c.save();
        self.rotate(18.0 + 24.0 * flap, x + 8.0, cy - 2.0);
        self.capsule(x + 11.0, cy + 3.0, 9.0, 17.0, k.cream, true);
        self.c.restore();
        self.soft_tri(
            x - 8.0,
            cy - 11.0,
            x - 3.0,
            cy - 23.0,
            x + 1.0,
            cy - 12.0,
            k.cream,
            true,
        );
        self.soft_tri(
            x + 8.0,
            cy - 11.0,
            x + 3.0,
            cy - 23.0,
            x - 1.0,
            cy - 12.0,
            k.cream,
            true,
        );
        self.mass(x, cy, 26.0, 32.0, k.cream);
        self.capsule(x, cy - 3.0, 22.0, 17.0, k.cream_hi, false);
        let bl = self.blink(0.45);
        self.owl_eye(x - 5.4, cy - 4.0, 4.6, bl, look_x, 0.0);
        self.owl_eye(x + 5.4, cy - 4.0, 4.6, bl, look_x, 0.0);
        self.tri(x - 2.0, cy + 1.0, x + 2.0, cy + 1.0, x, cy + 5.0, k.berry);
    }

    // ---------------------------------------------------------- the scene

    /// The owlet alone, close up: idle hops, a flutter when tapped.
    fn draw_solo(&mut self, now: f32) {
        let t = self.t();
        let g = 150.0;
        let fp = self.react_p(Critter::Flutter, now);
        // One hop per flutter asked for, each a little higher than the last.
        let n = f32::from(self.flutters);
        let k = (fp * n).floor().min(n - 1.0);
        let flutter = if fp > 0.0 && fp < 1.0 {
            hump(fp * n - k)
                * (8.0 + 3.0 * k)
                * (0.7 + 0.3 * (self.react_secs(now) * 22.0).sin().abs())
        } else {
            0.0
        };
        let idle = if self.still {
            0.0
        } else {
            (0.5 + 0.5 * (t * TWO_PI).sin()) * 2.0
        };
        self.owlet(130.0, g - 2.0, t, idle.max(flutter));
    }

    fn draw(&mut self, now: f32) {
        let k = self.k;
        let t = self.t();
        let tau = t * TWO_PI;
        let g = 150.0;
        let bob = tau.sin();
        let secs = self.react_secs(now);

        // ---------------- cat, sitting, left ----------------
        {
            let x = 70.0;
            let by = bob * 1.6;
            self.cast_shadow(x + 2.0, g + 4.0, 66.0, 40.0);
            let cp = self.react_p(Critter::Cat, now);
            let crouch = ramp(cp / 0.30) * (1.0 - ramp((cp - 0.34) / 0.18));
            let spring = hump((cp - 0.28) / 0.55);
            let land = hump((cp - 0.82) / 0.18);
            let wiggle = (secs * 44.0).sin() * crouch * 2.6;
            let active = (crouch + spring).min(1.0);
            self.c.save();
            self.c
                .translate(wiggle, crouch * 3.6 - spring * 17.0 + land * 2.4);
            self.scale_at(1.0 + spring * 0.05, 1.0 - spring * 0.06, x, g);

            let flick = (t * 4.0 * PI + 1.0).sin() + active * (secs * 30.0).sin() * 3.2;
            self.c.save();
            self.rotate(flick * 4.0, x + 6.0, g - 6.0);
            self.c.begin();
            self.c.move_to(x + 2.0, g - 4.0);
            self.c
                .cubic_to(x + 30.0, g + 2.0, x + 34.0, g - 26.0, x + 20.0, g - 34.0);
            self.c
                .cubic_to(x + 12.0, g - 39.0, x + 6.0, g - 32.0, x + 11.0, g - 24.0);
            self.c
                .cubic_to(x + 16.0, g - 16.0, x + 12.0, g - 2.0, x - 2.0, g + 1.0);
            self.c.close();
            self.c.fill(k.dove);
            self.edge();
            self.c.restore();

            self.mass(x - 4.0, g - 16.0 + by, 46.0, 30.0, k.dove);
            self.mass(x + 2.0, g - 40.0 + by, 34.0, 46.0, k.dove);
            self.capsule(x + 4.0, g - 30.0 + by, 18.0, 24.0, k.cream_hi, false);
            self.capsule(x - 4.0, g - 3.0, 12.0, 9.0, k.cream, true);
            self.capsule(x + 10.0, g - 3.0, 12.0, 9.0, k.cream, true);

            let hx = x + 3.0;
            let hy = g - 64.0 + by - spring * 3.0;
            let ear_tip = -3.0 - active * 3.0;
            self.soft_tri(
                hx - 16.0,
                hy - 2.0,
                hx - 20.0,
                hy - 22.0 - ear_tip,
                hx - 3.0,
                hy - 12.0,
                k.dove,
                true,
            );
            self.soft_tri(
                hx + 16.0,
                hy - 2.0,
                hx + 20.0,
                hy - 22.0 - ear_tip,
                hx + 3.0,
                hy - 12.0,
                k.dove,
                true,
            );
            self.soft_tri(
                hx - 13.0,
                hy - 4.0,
                hx - 16.0,
                hy - 17.0,
                hx - 5.0,
                hy - 11.0,
                k.berry,
                false,
            );
            self.soft_tri(
                hx + 13.0,
                hy - 4.0,
                hx + 16.0,
                hy - 17.0,
                hx + 5.0,
                hy - 11.0,
                k.berry,
                false,
            );
            self.mass(hx, hy, 32.0, 29.0, k.dove);
            self.capsule(hx, hy + 6.0, 15.0, 12.0, k.cream_hi, false);
            let bl = clamp(self.blink(0.0) - spring, 0.0, 1.0);
            let er = 3.4 + active;
            let (gx, gy) = (self.gaze_x(2, 1.0) * 1.7, self.gaze_y(2) * 1.2);
            self.eye(hx - 6.0, hy - 1.0, er, bl, gx, gy);
            self.eye(hx + 6.0, hy - 1.0, er, bl, gx, gy);
            self.tri(
                hx,
                hy + 8.0,
                hx - 2.4,
                hy + 5.6,
                hx + 2.4,
                hy + 5.6,
                k.berry,
            );
            self.line(hx + 6.0, hy + 5.0, hx + 20.0, hy + 3.0);
            self.line(hx + 6.0, hy + 8.0, hx + 20.0, hy + 9.0);
            self.line(hx - 6.0, hy + 5.0, hx - 20.0, hy + 3.0);
            self.line(hx - 6.0, hy + 8.0, hx - 20.0, hy + 9.0);
            self.c.restore();
        }

        // ---------------- dog, sitting, right ----------------
        {
            let x = 190.0;
            let ck = self.clock;
            let by = (self.breath(ck) - 0.5) * 3.2;
            let hby = (self.breath(ck - 0.13) - 0.5) * 3.4;
            self.cast_shadow(x + 2.0, g + 4.0, 78.0, 40.0);

            let dp = self.react_p(Critter::Dog, now);
            let bow = ramp(dp / 0.20) * (1.0 - ramp((dp - 0.30) / 0.22));
            let bounce = if (0.30..=0.92).contains(&dp) {
                hangs((((dp - 0.30) / 0.62) * PI * 2.0).sin().abs(), 0.5)
            } else {
                0.0
            };
            let dog_a = (bow + bounce).min(1.0);
            let lean =
                snap(self.beat(21, 11.0, 2.4, 0.6), 0.3) - snap(self.beat(22, 13.0, 2.4, 0.6), 0.3);
            self.c.save();
            self.rotate(-13.0 * bow + lean * 2.2, x, g);
            self.c.translate(0.0, -bounce * 9.0);
            self.scale_at(1.0 + bounce * 0.03, 1.0 - bounce * 0.045, x, g);

            let zeal = ramp(self.drift(ck * 0.42 + 2.1) * 1.15 + 0.66);
            let wag_ph = ck * 3.5 + 1.5 * self.drift(ck * 0.44);
            let th = wag_ph * TWO_PI;
            let wag = hangs((th + 0.5 * th.sin()).sin(), 0.4) * (0.18 + 0.82 * zeal);
            let wag_arc =
                wag * (7.0 + 8.0 * zeal + dog_a * 20.0) + dog_a * (secs * 40.0).sin() * 6.0;
            self.c.save();
            self.rotate(
                wag_arc + (1.0 - zeal) * 3.0 - dog_a * 6.0,
                x + 14.0,
                g - 8.0,
            );
            self.c.begin();
            self.c.move_to(x + 12.0, g - 4.0);
            self.c
                .cubic_to(x + 40.0, g - 4.0, x + 48.0, g - 26.0, x + 36.0, g - 40.0);
            self.c
                .cubic_to(x + 31.0, g - 46.0, x + 21.0, g - 44.0, x + 22.0, g - 36.0);
            self.c
                .cubic_to(x + 27.0, g - 30.0, x + 30.0, g - 16.0, x + 12.0, g - 4.0);
            self.c.close();
            self.c.fill(k.biscuit);
            self.edge();
            self.c.restore();

            self.mass(x + 4.0, g - 16.0 + by, 52.0, 28.0, k.biscuit);
            self.mass(x, g - 40.0 + by, 42.0, 46.0, k.biscuit);
            self.capsule(x, g - 30.0 + by, 20.0, 26.0, k.cream, false);
            self.capsule(x - 8.0, g - 3.0, 13.0, 10.0, k.cream, true);
            self.capsule(x + 8.0, g - 3.0, 13.0, 10.0, k.cream, true);

            let cock = (snap(self.beat(24, 9.7, 2.1, 0.6), 0.16)
                - snap(self.beat(23, 8.2, 2.1, 0.6), 0.16))
                * 11.0;
            let sniff = self.beat(25, 12.0, 0.9, 0.5);
            let sniff_y = if sniff > 0.0 {
                (sniff * 3.0 * TWO_PI).sin() * hump(sniff) * 1.7
            } else {
                0.0
            };
            let hx = x;
            let hy = g - 62.0 + hby + sniff_y - bow * 2.0;
            self.c.save();
            self.rotate(cock, hx, hy + 17.0);
            let sway = (self.breath(ck - 0.26) - 0.5) * 5.6
                + self.drift(ck * 0.62) * 1.4
                + (secs * 24.0).sin() * dog_a * 6.0;
            let flick_l = snap(self.beat(26, 5.2, 0.45, 0.55), 0.14) * 12.0;
            let flick_r = snap(self.beat(27, 6.7, 0.45, 0.55), 0.14) * 12.0;
            self.floppy_ear(
                hx - 15.0,
                hy - 9.0,
                33.0,
                18.0,
                22.0 + sway + flick_l,
                k.biscuit_lo,
            );
            self.floppy_ear(
                hx + 15.0,
                hy - 9.0,
                33.0,
                18.0,
                -22.0 - sway - flick_r,
                k.biscuit_lo,
            );
            self.mass(hx, hy, 34.0, 31.0, k.cream);
            self.capsule(hx, hy + 7.0, 18.0, 14.0, k.cream_hi, false);
            let bl = self.blink_at(2);
            let (gaze, gaze_v) = (self.gaze_x(3, -1.0) * 1.7, self.gaze_y(3) * 1.2);
            self.eye(hx - 6.0, hy - 2.0, 3.4, bl, gaze, gaze_v);
            self.eye(hx + 6.0, hy - 2.0, 3.4, bl, gaze, gaze_v);
            self.capsule(hx, hy + 4.0, 6.0, 5.0, k.ink, false);
            self.c.begin();
            self.c.arc(hx - 1.6, hy + 2.6, 1.1);
            self.c.fill(k.cream_hi);
            self.arc_in(hx - 6.0, hy + 6.0, hx, hy + 13.0, 20.0, 130.0);
            self.arc_in(hx, hy + 6.0, hx + 6.0, hy + 13.0, 30.0, 130.0);
            let pant = snap(self.beat(30, 7.0, 3.0, 0.85), 0.10)
                .max(snap(self.beat(31, 11.0, 2.4, 0.55), 0.10))
                .max(dog_a);
            let loll = (pant * (4.4 + 1.2 * (ck * 12.0).sin()) + dog_a * 4.0).max(0.0);
            if loll > 0.5 {
                self.rrect(hx - 2.4, hy + 9.0, hx + 2.4, hy + 9.0 + loll, 2.4);
                self.c.fill(k.tongue);
            }
            self.c.restore();
            self.c.restore();
        }

        // ---------------- owlet, centre (flies a loop when tapped) ----------------
        let op = self.react_p(Critter::Owlet, now);
        if op > 0.0 {
            let fly = Self::owl_flight(op);
            let flap = 0.55 + 0.45 * (secs * 24.0).sin().abs();
            self.owlet_flying(130.0, g - 2.0, fly, flap);
        } else {
            let hop = if self.still {
                0.0
            } else {
                (0.5 + 0.5 * tau.sin()) * 2.0
            };
            self.owlet(130.0, g - 2.0, t, hop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Palette, Scene, hash01};

    #[test]
    fn hash_matches_scene_js() {
        // Values from scene.js's own hash01 (Math.imul, run in Node).
        for (n, want) in [
            (13, 0.138_755_4),
            (977 * 2 + 13, 0.503_063_4),
            (-5, 0.339_847_1),
        ] {
            assert!(
                (hash01(n) - want).abs() < 1e-6,
                "hash01({n}) = {}",
                hash01(n)
            );
        }
    }

    #[test]
    fn the_owlet_fills_her_close_up() {
        let px = Scene::owlet(true).render(80, 72, 0.0, Palette::light());
        let px = px.expect("a pixmap");
        let inked = px.pixels().iter().filter(|p| p.alpha() > 0).count();
        assert!(inked > 1_200, "the owlet fills much of her frame: {inked}");
    }

    #[test]
    fn a_tap_on_the_solo_owlet_flutters() {
        let mut sc = Scene::owlet(false);
        assert!(sc.tap(5.0, 5.0, 80.0, 72.0, 1.0));
        assert!(sc.animating(2.0));
        sc.still = true;
        assert!(!sc.animating(3.0), "a flutter lasts 1.4 s");
    }

    #[test]
    fn the_scene_draws_something() {
        let px = Scene::new(true).render(260, 188, 0.0, Palette::light());
        let px = px.expect("a pixmap");
        let inked = px.pixels().iter().filter(|p| p.alpha() > 0).count();
        assert!(
            inked > 5_000,
            "the trio covers part of the picture: {inked}"
        );
    }
}
