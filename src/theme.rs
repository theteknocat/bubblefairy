//! The app's chrome and trim colours: the one place they're defined, for CSS and for drawn widgets.
//! Colours that carry meaning (sent bubbles, avatars, status, errors, brand) stay where they're used.

/// A colour as `0xRRGGBB` and an alpha.
#[derive(Clone, Copy)]
pub struct Colour(pub u32, pub f32);

impl Colour {
    fn css(self) -> String {
        let [_, r, g, b] = self.0.to_be_bytes();
        format!("rgba({r}, {g}, {b}, {})", self.1)
    }
}

/// A colour in the light scheme and in the dark one.
pub struct Token {
    light: Colour,
    dark: Colour,
}

impl Token {
    pub fn get(&self, dark: bool) -> Colour {
        if dark { self.dark } else { self.light }
    }
}

const fn pair(light: Colour, dark: Colour) -> Token {
    Token { light, dark }
}

const fn same(c: Colour) -> Token {
    pair(c, c)
}

const fn solid(rgb: u32) -> Colour {
    Colour(rgb, 1.0)
}

const WHITE: u32 = 0xffffff;

// incoming bubbles and their cards
pub const THEM_A: Token = pair(solid(0xfdfbff), solid(0x544d93));
pub const THEM_B: Token = pair(solid(0xf3eefd), solid(0x453f80));
const SHADOW: Token = pair(Colour(0x4c3c96, 0.16), Colour(0x08041e, 0.40));
/// The drawn incoming bubble's outline (a little stronger than `--them-edge` in light).
pub const BUBBLE_EDGE: Token = pair(Colour(0x7a6eb4, 0.20), Colour(WHITE, 0.10));
pub const BUBBLE_SHADOW: Token = SHADOW;
/// Behind a photo that hasn't arrived.
pub const PLACEHOLDER: Token = same(Colour(0x8a7cc8, 0.22));

/// The CSS variables, by name.
const VARS: &[(&str, Token)] = &[
    ("bg-a", pair(solid(0xe6dcfa), solid(0x2d2659))),
    ("bg-b", pair(solid(0xdcd9f8), solid(0x1c2550))),
    ("bg-c", pair(solid(0xf0dcf2), solid(0x3a2455))),
    ("panel", pair(Colour(WHITE, 0.55), Colour(WHITE, 0.075))),
    // the info panel when it slides over the chat
    ("panel-solid", pair(solid(0xebe5fb), solid(0x2c2759))),
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
    ("sel-a", pair(solid(0xdacdff), Colour(0xa58cff, 0.40))),
    ("sel-b", pair(solid(0xc9dbff), Colour(0x5f96ff, 0.32))),
    // the white top edge alone vanishes on light
    ("sel-edge", pair(Colour(0x7b5cff, 0.38), Colour(WHITE, 0.0))),
    ("field", pair(Colour(WHITE, 0.92), Colour(WHITE, 0.09))),
    // trim
    ("accent", same(solid(0x7b5cff))),
    ("accent-2", same(solid(0xff5fa8))),
    ("accent-deep", same(solid(0x6a55e8))),
    ("focus", same(solid(0xb9a6ff))),
    ("neutral", same(solid(0x8076c8))),
];

/// The variable definitions to load ahead of `style.css`; ui.rs puts `.dark` on windows in dark mode.
pub fn css() -> String {
    let block = |selector: &str, dark: bool| {
        let vars: String = VARS
            .iter()
            .map(|(name, t)| format!("  --{name}: {};\n", t.get(dark).css()))
            .collect();
        format!("{selector} {{\n{vars}}}\n")
    };
    block(":root", false) + &block("window.dark", true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_css_variable_is_defined() {
        let style = include_str!("style.css");
        let defined = css() + style;
        for rest in style.split("var(--").skip(1) {
            let name = rest.split(')').next().unwrap_or_default();
            assert!(defined.contains(&format!("--{name}:")), "--{name}");
        }
        assert_eq!(Colour(0x102030, 0.5).css(), "rgba(16, 32, 48, 0.5)");
    }
}
