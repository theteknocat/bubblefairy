//! The app's chrome and trim colours: the one place they're defined, for CSS and for drawn widgets.
//! Colours that carry meaning (sent bubbles, avatars, status, errors, brand) stay where they're used.

//! The user's sliders are applied here. Colour (tint, saturation, brightness) works in OKLCH, where a
//! hue change keeps the perceived lightness and so the text contrast. Bubbliness runs from flat (no
//! shadows, bevels or gradients) through the designed look to glossy.

use gtk::gdk;
use std::cell::{Cell, RefCell};

/// A colour as `0xRRGGBB` and an alpha.
#[derive(Clone, Copy)]
pub struct Colour(pub u32, pub f32);

impl Colour {
    fn css(self) -> String {
        let [_, r, g, b] = self.0.to_be_bytes();
        format!("rgba({r}, {g}, {b}, {})", self.1)
    }
}

/// The slider values for one scheme: tint in degrees, the others -100 to 100. All zero = as designed.
/// Bubbliness: -100 flat, 100 fully glossy.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct Adjust {
    pub tint: i32,
    pub saturation: i32,
    pub brightness: i32,
    pub bubbliness: i32,
}

pub const TINT_MAX: i32 = 180;
pub const LEVEL_MAX: i32 = 100;

impl Adjust {
    /// From the config file's `tint,saturation,brightness,bubbliness`; anything unreadable is 0.
    pub fn parse(s: &str) -> Self {
        let mut n = s.split(',').map(|v| v.trim().parse::<i32>().unwrap_or(0));
        let mut next = |max: i32| n.next().unwrap_or(0).clamp(-max, max);
        Adjust {
            tint: next(TINT_MAX),
            saturation: next(LEVEL_MAX),
            brightness: next(LEVEL_MAX),
            bubbliness: next(LEVEL_MAX),
        }
    }

    pub fn save(self) -> String {
        let Adjust {
            tint,
            saturation,
            brightness,
            bubbliness,
        } = self;
        format!("{tint},{saturation},{brightness},{bubbliness}")
    }
}

/// How a colour answers the sliders.
#[derive(Clone, Copy)]
enum Role {
    /// The sliders leave the colour alone (the send button's blue and green).
    Fixed,
    /// Tint and saturation only: text and shadows keep their lightness.
    Tinted,
    /// A background: brightness moves it, by this much (1 = fully, to near white or near black).
    Surface(f32),
    /// Incoming bubbles: kept soft, barely moved by saturation, so no slider position makes them
    /// look like the blue or green sent ones.
    Incoming,
    /// Buttons, selection, focus: full tint, little saturation or brightness, so they stay distinct.
    Trim,
    /// Avatar discs: no tint, little saturation or brightness (the text shadow gives the contrast).
    Avatar,
}

/// A colour in the light scheme and in the dark one.
#[derive(Clone, Copy)]
pub struct Token {
    light: Colour,
    dark: Colour,
    role: Role,
}

impl Token {
    /// The colour as the current sliders leave it.
    pub fn get(&self, dark: bool) -> Colour {
        let a = ADJUST.get()[usize::from(dark)];
        self.adjusted(dark, a)
    }

    fn adjusted(&self, dark: bool, a: Adjust) -> Colour {
        let base = if dark { self.dark } else { self.light };
        if matches!(self.role, Role::Fixed) || (a.tint, a.saturation, a.brightness) == (0, 0, 0) {
            return base;
        }
        let [mut l, mut c, mut h] = to_oklch(base.0);
        let s = a.saturation as f32 / LEVEL_MAX as f32;
        let b = a.brightness as f32 / LEVEL_MAX as f32;
        if !matches!(self.role, Role::Avatar) {
            h += (a.tint as f32).to_radians();
        }
        c *= 1.0
            + s * match self.role {
                Role::Incoming => 0.15,
                Role::Trim | Role::Avatar | Role::Fixed => 0.25,
                _ if s > 0.0 => 0.8,
                _ => 0.7,
            };
        let role = match self.role {
            Role::Incoming => Role::Surface(0.45),
            r => r,
        };
        match role {
            Role::Surface(k) if dark && b < 0.0 => l *= 1.0 + b * 0.65 * k,
            Role::Surface(k) if dark => l += b * 0.07 * k,
            Role::Surface(k) if b > 0.0 => l += b * (0.995 - l) * 0.95 * k,
            Role::Surface(k) => l += b * 0.05 * k,
            Role::Trim | Role::Avatar => l += b * 0.04,
            Role::Tinted | Role::Incoming | Role::Fixed => {}
        }
        Colour(from_oklch(l, c, h), base.1)
    }
}

const fn pair(light: Colour, dark: Colour) -> Token {
    Token {
        light,
        dark,
        role: Role::Tinted,
    }
}

const fn same(c: Colour) -> Token {
    pair(c, c)
}

const fn surface(strength: f32, light: u32, dark: u32) -> Token {
    Token {
        role: Role::Surface(strength),
        ..pair(solid(light), solid(dark))
    }
}

const fn incoming(light: u32, dark: u32) -> Token {
    Token {
        role: Role::Incoming,
        ..pair(solid(light), solid(dark))
    }
}

const fn trim(light: Colour, dark: Colour) -> Token {
    Token {
        role: Role::Trim,
        ..pair(light, dark)
    }
}

const fn fixed(rgb: u32) -> Token {
    Token {
        role: Role::Fixed,
        ..same(solid(rgb))
    }
}

const fn avatar(rgb: u32) -> Token {
    Token {
        role: Role::Avatar,
        ..same(solid(rgb))
    }
}

const fn solid(rgb: u32) -> Colour {
    Colour(rgb, 1.0)
}

const WHITE: u32 = 0xffffff;

// incoming bubbles and their cards
pub const THEM_A: Token = incoming(0xfdfbff, 0x55537b);
pub const THEM_B: Token = incoming(0xf3eefd, 0x46456c);
const SHADOW: Token = pair(Colour(0x4c3c96, 0.16), Colour(0x08041e, 0.40));
/// The drawn incoming bubble's outline (a little stronger than `--them-edge` in light).
pub const BUBBLE_EDGE: Token = pair(Colour(0x7a6eb4, 0.20), Colour(WHITE, 0.10));
pub const BUBBLE_SHADOW: Token = SHADOW;
/// Behind a photo that hasn't arrived.
pub const PLACEHOLDER: Token = same(Colour(0x8a7cc8, 0.22));

/// The CSS variables, by name.
const VARS: &[(&str, Token)] = &[
    ("bg-a", surface(1.0, 0xe6dcfa, 0x2d2659)),
    ("bg-b", surface(1.0, 0xdcd9f8, 0x1c2550)),
    ("bg-c", surface(1.0, 0xf0dcf2, 0x3a2455)),
    ("panel", pair(Colour(WHITE, 0.55), Colour(WHITE, 0.075))),
    // the info panel when it slides over the chat
    ("panel-solid", surface(0.9, 0xebe5fb, 0x2c2759)),
    ("panel-edge", pair(Colour(WHITE, 0.95), Colour(WHITE, 0.14))),
    ("ink", pair(solid(0x2a2442), solid(0xf6f3ff))),
    ("ink-soft", pair(solid(0x5a5380), solid(0xbdb6e4))),
    ("shadow", SHADOW),
    (
        "shadow-strong",
        pair(Colour(0x4c3c96, 0.26), Colour(0x08041e, 0.55)),
    ),
    ("them-a", THEM_A),
    ("them-b", THEM_B),
    ("them-ink", pair(solid(0x2a2442), solid(0xfbfaff))),
    (
        "them-edge",
        pair(Colour(0x7a6eb4, 0.16), Colour(WHITE, 0.10)),
    ),
    ("sel-a", trim(solid(0xdacdff), Colour(0xa58cff, 0.40))),
    ("sel-b", trim(solid(0xc9dbff), Colour(0x5f96ff, 0.32))),
    // the white top edge alone vanishes on light
    ("sel-edge", trim(Colour(0x7b5cff, 0.38), Colour(WHITE, 0.0))),
    ("field", pair(Colour(WHITE, 0.92), Colour(WHITE, 0.09))),
    // trim
    ("accent", trim(solid(0x7b5cff), solid(0x7b5cff))),
    ("accent-2", trim(solid(0xff5fa8), solid(0xff5fa8))),
    ("accent-deep", trim(solid(0x6a55e8), solid(0x6a55e8))),
    ("focus", trim(solid(0xb9a6ff), solid(0xb9a6ff))),
    ("neutral", trim(solid(0x8076c8), solid(0x8076c8))),
    // the send button, as the sent bubbles
    ("send-a", fixed(0x2f7cf6)),
    ("send-b", fixed(0x1456e0)),
    ("sms-a", fixed(0x17984a)),
    ("sms-b", fixed(0x0a6b2e)),
    // avatar discs, top and bottom of each gradient
    ("av0-a", avatar(0xff9a8b)),
    ("av0-b", avatar(0xff5e7e)),
    ("av1-a", avatar(0xffc56b)),
    ("av1-b", avatar(0xff8a3d)),
    ("av2-a", avatar(0x7ee8b0)),
    ("av2-b", avatar(0x26b98a)),
    ("av3-a", avatar(0x7cc8ff)),
    ("av3-b", avatar(0x3a7bff)),
    ("av4-a", avatar(0xc3a1ff)),
    ("av4-b", avatar(0x7d5cff)),
    ("av5-a", avatar(0xff9fd4)),
    ("av5-b", avatar(0xe8559f)),
    ("av6-a", avatar(0x6ee7e0)),
    ("av6-b", avatar(0x1fa6c4)),
    ("av7-a", avatar(0xffe17a)),
    ("av7-b", avatar(0xf2a81d)),
];

thread_local! {
    /// The sliders, light then dark. Only the GTK thread draws.
    static ADJUST: Cell<[Adjust; 2]> = Cell::default();
    static PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
}

/// Use these slider values and (re)load the app's CSS. Widgets that draw themselves need a redraw.
pub fn apply(light: Adjust, dark: Adjust) {
    ADJUST.set([light, dark]);
    let Some(display) = gdk::Display::default() else {
        return;
    };
    PROVIDER.with_borrow_mut(|p| {
        p.get_or_insert_with(|| {
            let p = gtk::CssProvider::new();
            gtk::style_context_add_provider_for_display(
                &display,
                &p,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            p
        })
        .load_from_string(&(css() + include_str!("style.css") + include_str!("gloss.css")));
    });
}

/// 0 to 1: how far Bubbliness is toward glossy.
pub fn shine(dark: bool) -> f32 {
    ADJUST.get()[usize::from(dark)].bubbliness.max(0) as f32 / LEVEL_MAX as f32
}

/// 1 to 0: how much of the shadows, bevels and gradients is left toward flat.
pub fn depth(dark: bool) -> f32 {
    1.0 + ADJUST.get()[usize::from(dark)].bubbliness.min(0) as f32 / LEVEL_MAX as f32
}

/// `a` moved the fraction `t` of the way to `b`.
fn mix(a: Colour, b: Colour, t: f32) -> Colour {
    let (x, y) = (a.0.to_be_bytes(), b.0.to_be_bytes());
    let rgb = u32::from_be_bytes(std::array::from_fn(|i| {
        (f32::from(x[i]) + (f32::from(y[i]) - f32::from(x[i])) * t).round() as u8
    }));
    Colour(rgb, a.1 + (b.1 - a.1) * t)
}

/// Gradients that go flat by one end moving all the way to the other.
const FLATTEN_TO: [(&str, &str); 3] = [("bg-a", "bg-b"), ("bg-c", "bg-b"), ("accent-2", "accent")];
/// Gradients that go flat by both ends meeting in the middle (and every avatar's).
const FLATTEN_MID: [(&str, &str); 4] = [
    ("sel-a", "sel-b"),
    ("them-a", "them-b"),
    ("send-a", "send-b"),
    ("sms-a", "sms-b"),
];

/// A variable's colour with the sliders applied, flattened as far as Bubbliness says.
fn value(name: &str, dark: bool) -> Option<Colour> {
    let get = |n: &str| VARS.iter().find(|v| v.0 == n).map(|v| v.1.get(dark));
    let own = get(name)?;
    let flat = 1.0 - depth(dark);
    if let Some((_, to)) = FLATTEN_TO.iter().find(|p| p.0 == name) {
        return Some(mix(own, get(to)?, flat));
    }
    let avatar = name.starts_with("av").then(|| {
        let (disc, end) = name.split_at(name.len() - 1);
        format!("{disc}{}", if end == "a" { "b" } else { "a" })
    });
    let other = avatar.as_deref().or_else(|| {
        FLATTEN_MID.iter().find_map(|&(a, b)| match name {
            n if n == a => Some(b),
            n if n == b => Some(a),
            _ => None,
        })
    });
    Some(match other {
        Some(o) => mix(own, get(o)?, flat / 2.0),
        None => own,
    })
}

/// The variable definitions to load ahead of `style.css`; ui.rs puts `.dark` on windows in dark mode.
fn css() -> String {
    let block = |selector: &str, dark: bool| {
        let vars: String = VARS
            .iter()
            .filter_map(|(name, _)| Some(format!("  --{name}: {};\n", value(name, dark)?.css())))
            .collect();
        let (depth, shine) = (depth(dark), shine(dark));
        // text shadows carry the avatars' contrast, so flat keeps a trace of them
        let lift = 0.35 + 0.65 * depth;
        // GTK rejects most calc() inside colours, so style.css only mixes by these; sums are worked out here
        let (plain, glow) = (depth - shine, 0.40 * depth + 0.20 * shine);
        format!(
            "{selector} {{\n{vars}  --depth: {depth};\n  --shine: {shine};\n  --lift: {lift};\n  --plain: {plain};\n  --glow: {glow};\n}}\n"
        )
    };
    block(":root", false) + &block("window.dark", true)
}

/// sRGB `0xRRGGBB` to OKLCH (lightness, chroma, hue in radians). Björn Ottosson's matrices.
fn to_oklch(rgb: u32) -> [f32; 3] {
    let [r, g, b] = linear(rgb);
    let l = (0.4122215 * r + 0.5363325 * g + 0.0514460 * b).cbrt();
    let m = (0.2119035 * r + 0.6806995 * g + 0.107397 * b).cbrt();
    let s = (0.0883025 * r + 0.2817188 * g + 0.6299787 * b).cbrt();
    let a = 1.9779985 * l - 2.4285922 * m + 0.4505937 * s;
    let bb = 0.0259040 * l + 0.7827718 * m - 0.8086758 * s;
    [
        0.2104543 * l + 0.7936178 * m - 0.0040720 * s,
        a.hypot(bb),
        bb.atan2(a),
    ]
}

/// OKLCH back to `0xRRGGBB`; a colour the screen can't show loses chroma, never lightness.
fn from_oklch(l: f32, mut c: f32, h: f32) -> u32 {
    let l = l.clamp(0.0, 1.0);
    loop {
        let (a, b) = (c * h.cos(), c * h.sin());
        let x = (l + 0.3963378 * a + 0.2158038 * b).powi(3);
        let y = (l - 0.1055613 * a - 0.0638542 * b).powi(3);
        let z = (l - 0.0894842 * a - 1.2914855 * b).powi(3);
        let rgb = [
            4.0767417 * x - 3.3077116 * y + 0.2309699 * z,
            -1.268438 * x + 2.6097574 * y - 0.3413194 * z,
            -0.0041961 * x - 0.7034186 * y + 1.7076147 * z,
        ];
        if c < 0.001 || rgb.iter().all(|v| (-0.001..=1.001).contains(v)) {
            return rgb.iter().fold(0, |out, &v| {
                let v = v.clamp(0.0, 1.0);
                let v = if v <= 0.0031308 {
                    12.92 * v
                } else {
                    1.055 * v.powf(1.0 / 2.4) - 0.055
                };
                (out << 8) | (v * 255.0).round() as u32
            });
        }
        c *= 0.95;
    }
}

fn linear(rgb: u32) -> [f32; 3] {
    let [_, r, g, b] = rgb.to_be_bytes();
    [r, g, b].map(|v| {
        let v = f32::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    })
}

/// The WCAG contrast ratio of two opaque colours, 1 to 21.
#[cfg(test)]
fn contrast(a: u32, b: u32) -> f32 {
    let lum = |c| {
        let [r, g, b] = linear(c);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_css_variable_is_defined() {
        let style = concat!(include_str!("style.css"), include_str!("gloss.css"));
        let defined = css() + style;
        for rest in style.split("var(--").skip(1) {
            let name = rest.split(')').next().unwrap_or_default();
            assert!(defined.contains(&format!("--{name}:")), "--{name}");
        }
        assert_eq!(Colour(0x102030, 0.5).css(), "rgba(16, 32, 48, 0.5)");
    }

    #[test]
    fn colours_survive_the_round_trip_and_settings_parse() {
        for (name, t) in VARS {
            let [l, c, h] = to_oklch(t.light.0);
            assert_eq!(from_oklch(l, c, h), t.light.0, "{name}");
        }
        let a = Adjust::parse("-20,300,x"); // an older config has no bubbliness
        assert_eq!(
            (a.tint, a.saturation, a.brightness, a.bubbliness),
            (-20, 100, 0, 0)
        );
        assert_eq!(Adjust::parse(&a.save()), a);
    }

    #[test]
    fn flat_leaves_no_gradient_and_gloss_no_change_of_colour() {
        let at = |bubbliness| {
            let a = Adjust {
                bubbliness,
                ..Adjust::default()
            };
            ADJUST.set([a, a]);
        };
        at(-LEVEL_MAX);
        assert_eq!((depth(false), shine(false)), (0.0, 0.0));
        let ends = |a, b| (value(a, true).map(|c| c.0), value(b, true).map(|c| c.0));
        for (a, b) in FLATTEN_TO
            .iter()
            .chain(&FLATTEN_MID)
            .chain(&[("av7-a", "av7-b")])
        {
            let (x, y) = ends(a, b);
            assert_eq!(x, y, "{a} {b}");
        }
        at(LEVEL_MAX);
        assert_eq!((depth(false), shine(false)), (1.0, 1.0));
        assert_eq!(value("av7-a", false).map(|c| c.0), Some(0xffe17a));
        at(0);
    }

    /// Every slider position at the extremes and in between keeps text and controls readable.
    #[test]
    fn sliders_never_lose_contrast() {
        let var = |name: &str| VARS.iter().find(|v| v.0 == name).map(|v| v.1);
        let get =
            |name: &str, dark: bool, a: Adjust| var(name).map_or(0, |t| t.adjusted(dark, a).0);
        for dark in [false, true] {
            for tint in (-180..=180).step_by(15) {
                for saturation in [-100, 0, 100] {
                    for brightness in [-100, -50, 0, 50, 100] {
                        let a = Adjust {
                            tint,
                            saturation,
                            brightness,
                            ..Adjust::default()
                        };
                        let check = |fg: &str, bg: &str, min: f32| {
                            let got = contrast(get(fg, dark, a), get(bg, dark, a));
                            assert!(got >= min, "{fg} on {bg}: {got} dark={dark} {a:?}");
                        };
                        for bg in ["bg-a", "bg-b", "bg-c", "panel-solid"] {
                            check("ink", bg, 7.0);
                            check("ink-soft", bg, 3.8);
                        }
                        check("them-ink", "them-a", 4.5);
                        check("them-ink", "them-b", 4.5);
                        for name in ["them-a", "them-b"] {
                            let chroma = to_oklch(get(name, dark, a))[1];
                            assert!(chroma < 0.09, "{name}: {chroma} {a:?}"); // sent blue is ~0.2
                        }
                        // the active tab and primary button stand out from what's behind them
                        check("accent", "panel-solid", 1.8);
                    }
                }
            }
        }
    }
}
