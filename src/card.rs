//! Draws the welcome card a server posts when a member joins: a night-sky gradient with soft orbs of
//! light, the member's avatar in a glowing ring, "WELCOME", their name, "to <server>" and which member
//! they are. The drawing and the fonts are the same as the HoneyPot bot's cards. Returns PNG bytes.

use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::prelude::{LocationRef, MetadataProvider, Size};
use skrifa::{FontRef, GlyphId};
use tiny_skia::{
    Color, FillRule, FilterQuality, GradientStop, LinearGradient, Mask, Paint, Path, PathBuilder, Pixmap, PixmapPaint,
    Point, Shader, SpreadMode, Stroke, Transform,
};
use unicode_normalization::UnicodeNormalization;

const REGULAR: &[u8] = include_bytes!("../assets/fonts/NotoSans-Regular.ttf");
const BOLD: &[u8] = include_bytes!("../assets/fonts/NotoSans-Bold.ttf");

const WIDTH: u32 = 1100;
const HEIGHT: u32 = 500;

const SKY_TOP: u32 = 0x1a1033;
const SKY_BOTTOM: u32 = 0x0b1026;
const CARD: u32 = 0x12162a;
const WHITE: u32 = 0xf5f7ff;
const GREY: u32 = 0xa9b1d6;
const CYAN: u32 = 0x22d3ee;
const PINK: u32 = 0xf472b6;
const VIOLET: u32 = 0x8b5cf6;

/// The member the card is about: the names to try in order, their picture as PNG bytes, and their banner
/// colour as `#rrggbb` (empty for none). Without a picture the card shows the initial the app shows, on the
/// same colour: the banner colour, or else a colour picked from the name.
pub struct Member {
    pub names: Vec<String>,
    pub avatar: Option<Vec<u8>>,
    pub banner: String,
}

/// The colours the app gives a name without a banner colour (the first theme's avatar colours).
const AVATAR_COLOURS: [u32; 8] = [0x3daee9, 0x9b59b6, 0x27ae60, 0xfdbc4b, 0xda4453, 0xf67400, 0x1d99f3, 0x8e44ad];

/// The colour the app gives a name, the same hash the app uses.
fn name_colour(name: &str) -> u32 {
    let mut hash: u32 = 5381;
    for b in name.bytes() {
        hash = hash.wrapping_mul(33) ^ u32::from(b);
    }
    AVATAR_COLOURS[(hash % 8) as usize]
}

/// A banner colour such as "#1abc9c", as its rgb value.
fn banner_colour(banner: &str) -> Option<u32> {
    let hex = banner.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok()
}

/// A new member's welcome: "WELCOME", their name, "to <server>" and which member they are.
pub fn welcome(member: &Member, server: &str, member_count: u64) -> Option<Vec<u8>> {
    let pill = format!("{} member", ordinal(member_count));
    let (sky_top, accent, name, pill_colours) = (SKY_TOP, (CYAN, PINK), CYAN, (VIOLET, PINK));
    let orbs = [VIOLET, CYAN, PINK, PINK];
    render(
        member,
        &Text { headline: "W E L C O M E", line: ("to ", server), pill: &pill },
        &Look { sky_top, accent, name, pill: pill_colours, orbs },
    )
}

/// "1st", "2nd", "23rd", "1,234th"...
pub fn ordinal(n: u64) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    // Thousands separated, like "1,234th"
    let digits = n.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{grouped}{suffix}")
}

/// The colours of the card.
struct Look {
    sky_top: u32,
    /// Ring, border and name colours, from left to right
    accent: (u32, u32),
    name: u32,
    pill: (u32, u32),
    orbs: [u32; 4],
}

/// The text on the card, top to bottom.
struct Text<'a> {
    headline: &'a str,
    /// A plain lead-in then a bold part, like ("to ", "Server Name")
    line: (&'a str, &'a str),
    pill: &'a str,
}

fn render(member: &Member, text: &Text, look: &Look) -> Option<Vec<u8>> {
    let mut pixmap = Pixmap::new(WIDTH, HEIGHT)?;
    let fonts = Fonts::load()?;
    let (w, h) = (WIDTH as f32, HEIGHT as f32);

    // Night sky with soft orbs of light
    let sky = rounded_rect(0.0, 0.0, w, h, 0.0)?;
    fill(&mut pixmap, &sky, &vgradient(0.0, h, look.sky_top, SKY_BOTTOM, 255), None);
    let [a, b, c, d] = look.orbs;
    for (x, y, r, rgb) in [(120.0, 90.0, 140.0, a), (980.0, 420.0, 170.0, b), (900.0, 70.0, 90.0, c), (210.0, 450.0, 110.0, d)] {
        // Rings of falling opacity fake a soft blur
        for step in 0..12 {
            let radius = r * (1.0 - step as f32 / 12.0);
            if let Some(orb) = PathBuilder::from_circle(x, y, radius) {
                fill(&mut pixmap, &orb, &solid(rgb, 9), None);
            }
        }
    }
    // Tiny stars
    for i in 0..60u32 {
        let x = (i * 397 % WIDTH) as f32;
        let y = (i * 211 % HEIGHT) as f32;
        let size = 1.0 + (i % 3) as f32 * 0.6;
        if let Some(star) = PathBuilder::from_circle(x, y, size) {
            fill(&mut pixmap, &star, &solid(WHITE, 40 + (i % 5) as u8 * 25), None);
        }
    }

    // Frosted card with a glowing gradient border
    let (card_x, card_y, card_w, card_h) = (40.0, 40.0, w - 80.0, h - 80.0);
    let card = rounded_rect(card_x, card_y, card_w, card_h, 28.0)?;
    fill(&mut pixmap, &card, &solid(CARD, 150), None);
    let (from, to) = look.accent;
    glow(&mut pixmap, &card, |alpha| gradient(card_x, card_x + card_w, from, to, alpha / 2), 1.5);
    let card_mask = mask_for(&card, WIDTH, HEIGHT)?;
    // A sheen across the top of the card
    let sheen = rounded_rect(card_x, card_y, card_w, 90.0, 0.0)?;
    fill(&mut pixmap, &sheen, &vgradient(card_y, card_y + 90.0, WHITE, CARD, 10), Some(&card_mask));

    // Avatar in a glowing ring
    let (cx, cy, r) = (w / 2.0, 150.0, 78.0);
    let ring = PathBuilder::from_circle(cx, cy, r + 7.0)?;
    fill(&mut pixmap, &ring, &solid(SKY_BOTTOM, 255), None);
    match &member.avatar {
        Some(avatar) => draw_png(&mut pixmap, avatar, cx - r, cy - r, r * 2.0, true),
        None => {
            // The initial on the app's colour, as the app shows a member with no picture
            let shown = member.names.first().cloned().unwrap_or_default();
            let colour = banner_colour(&member.banner).unwrap_or_else(|| name_colour(&shown));
            let blank = PathBuilder::from_circle(cx, cy, r)?;
            fill(&mut pixmap, &blank, &solid(colour, 255), None);
            let letter: String = shown.nfkc().collect::<String>().chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_else(|| "?".into());
            fonts.draw_spans(&mut pixmap, &[(&letter, Weight::Bold, WHITE)], 70.0, cx, cy + 25.0, Align::Center);
        }
    }
    glow(&mut pixmap, &ring, |alpha| gradient(cx - r, cx + r, from, to, alpha), 4.0);

    // The headline, spaced out
    let (headline, size) = fonts.fit(Weight::Bold, text.headline, 54.0, 30.0, card_w - 80.0);
    fonts.draw_spans(&mut pixmap, &[(&headline, Weight::Bold, WHITE)], size, cx, 300.0, Align::Center);

    // Their name; stylised letters become plain ones, and names the font can't draw are skipped
    let name = member
        .names
        .iter()
        .map(|name| name.nfkc().collect::<String>())
        .find(|name| !fonts.glyphs(Weight::Bold, name).is_empty())
        .unwrap_or_default();
    let (name, size) = fonts.fit(Weight::Bold, &name, 40.0, 24.0, card_w - 120.0);
    fonts.draw_spans(&mut pixmap, &[(&name, Weight::Bold, look.name)], size, cx, 352.0, Align::Center);

    // Like "to <server>", leaving room for the lead-in
    let (lead, server) = text.line;
    let server = server.nfkc().collect::<String>();
    let lead_w = fonts.width(Weight::Regular, lead, 26.0);
    let (server, size) = fonts.fit(Weight::Bold, &server, 26.0, 18.0, card_w - 200.0 - lead_w);
    fonts.draw_spans(&mut pixmap, &[(lead, Weight::Regular, GREY), (&server, Weight::Bold, WHITE)], size, cx, 392.0, Align::Center);

    // The pill
    let pill_w = fonts.width(Weight::Bold, text.pill, 20.0) + 48.0;
    let pill = rounded_rect(cx - pill_w / 2.0, 410.0, pill_w, 34.0, 17.0)?;
    fill(&mut pixmap, &pill, &gradient(cx - pill_w / 2.0, cx + pill_w / 2.0, look.pill.0, look.pill.1, 200), None);
    stroke(&mut pixmap, &pill, &solid(WHITE, 60), 1.0);
    fonts.draw_spans(&mut pixmap, &[(text.pill, Weight::Bold, WHITE)], 20.0, cx, 434.0, Align::Center);

    pixmap.encode_png().ok()
}

fn colour(rgb: u32, alpha: u8) -> Color {
    Color::from_rgba8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8, alpha)
}

fn solid(rgb: u32, alpha: u8) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(colour(rgb, alpha));
    paint.anti_alias = true;
    paint
}

/// A gradient between two colours from `start` to `end`.
fn linear(start: Point, end: Point, from: u32, to: u32, alpha: u8) -> Paint<'static> {
    let stops = vec![GradientStop::new(0.0, colour(from, alpha)), GradientStop::new(1.0, colour(to, alpha))];
    let shader = LinearGradient::new(start, end, stops, SpreadMode::Pad, Transform::identity())
        .unwrap_or(Shader::SolidColor(colour(from, alpha)));
    Paint { shader, anti_alias: true, ..Default::default() }
}

/// A left-to-right gradient over `x0..x1`.
fn gradient(x0: f32, x1: f32, from: u32, to: u32, alpha: u8) -> Paint<'static> {
    linear(Point::from_xy(x0, 0.0), Point::from_xy(x1, 0.0), from, to, alpha)
}

/// A top-to-bottom gradient over `y0..y1`.
fn vgradient(y0: f32, y1: f32, from: u32, to: u32, alpha: u8) -> Paint<'static> {
    linear(Point::from_xy(0.0, y0), Point::from_xy(0.0, y1), from, to, alpha)
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

fn fill(pixmap: &mut Pixmap, path: &Path, paint: &Paint, mask: Option<&Mask>) {
    pixmap.fill_path(path, paint, FillRule::Winding, Transform::identity(), mask);
}

fn stroke(pixmap: &mut Pixmap, path: &Path, paint: &Paint, width: f32) {
    let stroke = Stroke { width, ..Default::default() };
    pixmap.stroke_path(path, paint, &stroke, Transform::identity(), None);
}

/// A soft glow: wide, faint strokes underneath a crisp one.
fn glow(pixmap: &mut Pixmap, path: &Path, paint_at: impl Fn(u8) -> Paint<'static>, width: f32) {
    for (extra, alpha) in [(18.0, 10), (12.0, 18), (7.0, 32), (3.0, 70)] {
        stroke(pixmap, path, &paint_at(alpha), width + extra);
    }
    if width > 0.0 {
        stroke(pixmap, path, &paint_at(255), width);
    }
}

/// A mask covering `path`, for clipping drawing to it.
fn mask_for(path: &Path, width: u32, height: u32) -> Option<Mask> {
    let mut mask = Mask::new(width, height)?;
    mask.fill_path(path, FillRule::Winding, true, Transform::identity());
    Some(mask)
}

/// Draws a PNG scaled into a square box, optionally clipped to a circle.
fn draw_png(pixmap: &mut Pixmap, png: &[u8], x: f32, y: f32, size: f32, circle: bool) {
    let Ok(image) = Pixmap::decode_png(png) else { return };
    let transform = Transform::from_row(size / image.width() as f32, 0.0, 0.0, size / image.height() as f32, x, y);
    let mask = if circle {
        PathBuilder::from_circle(x + size / 2.0, y + size / 2.0, size / 2.0)
            .and_then(|p| mask_for(&p, pixmap.width(), pixmap.height()))
    } else {
        None
    };
    let paint = PixmapPaint { quality: FilterQuality::Bicubic, ..Default::default() };
    pixmap.draw_pixmap(0, 0, image.as_ref(), &paint, transform, mask.as_ref());
}

#[derive(Clone, Copy)]
enum Weight {
    Regular,
    Bold,
}

#[derive(Clone, Copy)]
enum Align {
    Center,
}

/// Writes a glyph's outline into the text's path, moved to the pen position and flipped so
/// the font's y-up outline sits on the baseline.
struct GlyphPath<'a> {
    path: &'a mut PathBuilder,
    x: f32,
    baseline: f32,
}

impl OutlinePen for GlyphPath<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.path.move_to(self.x + x, self.baseline - y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.path.line_to(self.x + x, self.baseline - y);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.path.quad_to(self.x + cx0, self.baseline - cy0, self.x + x, self.baseline - y);
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.path.cubic_to(self.x + cx0, self.baseline - cy0, self.x + cx1, self.baseline - cy1, self.x + x, self.baseline - y);
    }
    fn close(&mut self) {
        self.path.close();
    }
}

/// Noto Sans Regular and Bold, built into the binary.
struct Fonts {
    regular: FontRef<'static>,
    bold: FontRef<'static>,
}

impl Fonts {
    fn load() -> Option<Self> {
        Some(Self { regular: FontRef::new(REGULAR).ok()?, bold: FontRef::new(BOLD).ok()? })
    }

    fn font(&self, weight: Weight) -> &FontRef<'static> {
        match weight {
            Weight::Regular => &self.regular,
            Weight::Bold => &self.bold,
        }
    }

    /// Glyphs the font can draw; anything else (like emoji) is skipped.
    fn glyphs(&self, weight: Weight, text: &str) -> Vec<GlyphId> {
        let charmap = self.font(weight).charmap();
        text.chars().filter_map(|c| charmap.map(c)).filter(|id| id.to_u32() != 0).collect()
    }

    /// The width of `text` at `size`, where size is the em size as in CSS.
    fn width(&self, weight: Weight, text: &str, size: f32) -> f32 {
        let metrics = self.font(weight).glyph_metrics(Size::new(size), LocationRef::default());
        self.glyphs(weight, text).iter().map(|id| metrics.advance_width(*id).unwrap_or(0.0)).sum()
    }

    /// Draws runs of differently styled text on one baseline at `y`.
    fn draw_spans(&self, pixmap: &mut Pixmap, spans: &[(&str, Weight, u32)], size: f32, x: f32, y: f32, align: Align) {
        let total: f32 = spans.iter().map(|(text, weight, _)| self.width(*weight, text, size)).sum();
        let mut pen = match align {
            Align::Center => x - total / 2.0,
        };
        for (text, weight, rgb) in spans {
            self.draw_run(pixmap, (text, *weight, *rgb), size, pen, y);
            pen += self.width(*weight, text, size);
        }
    }

    fn draw_run(&self, pixmap: &mut Pixmap, (text, weight, rgb): (&str, Weight, u32), size: f32, x: f32, y: f32) {
        let font = self.font(weight);
        let outlines = font.outline_glyphs();
        let metrics = font.glyph_metrics(Size::new(size), LocationRef::default());

        // Every glyph's outline goes into one path, which is then filled and anti-aliased
        let mut path = PathBuilder::new();
        let mut pen = x;
        for id in self.glyphs(weight, text) {
            if let Some(outline) = outlines.get(id) {
                let mut glyph = GlyphPath { path: &mut path, x: pen, baseline: y };
                let _ = outline.draw(DrawSettings::unhinted(Size::new(size), LocationRef::default()), &mut glyph);
            }
            pen += metrics.advance_width(id).unwrap_or(0.0);
        }
        if let Some(path) = path.finish() {
            fill(pixmap, &path, &solid(rgb, 255), None);
        }
    }

    /// Shrinks `text` (down to `min_size`, then by trimming with "…") until it fits `max_width`.
    fn fit(&self, weight: Weight, text: &str, size: f32, min_size: f32, max_width: f32) -> (String, f32) {
        let mut size = size;
        while size > min_size && self.width(weight, text, size) > max_width {
            size -= 1.0;
        }
        let mut text = text.to_string();
        while self.width(weight, &text, size) > max_width && text.chars().count() > 1 {
            text.pop();
            while text.ends_with('…') {
                text.pop();
            }
            text.push('…');
        }
        (text, size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_welcome_card_is_a_png_of_the_card_size() {
        let member = Member { names: vec!["CreativeCodeCat".into()], avatar: None, banner: String::new() };
        let png = welcome(&member, "Home", 1234).expect("image renders");
        let decoded = Pixmap::decode_png(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (WIDTH, HEIGHT));
    }

    #[test]
    fn ordinals_are_grouped_and_suffixed() {
        assert_eq!((ordinal(1), ordinal(12), ordinal(23), ordinal(111)), ("1st".into(), "12th".into(), "23rd".into(), "111th".into()));
        assert_eq!(ordinal(1234), "1,234th");
    }
}
