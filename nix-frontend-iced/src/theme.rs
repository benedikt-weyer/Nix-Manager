//! Colours and widget styles. Everything visual that isn't layout lives here
//! so the views stay readable.

use iced::border::Radius;
use iced::widget::{container, text_input};
use iced::{theme, Background, Border, Color, Shadow, Theme, Vector};

pub const BG: Color = Color::from_rgb8(0x0d, 0x10, 0x16);
pub const SURFACE: Color = Color::from_rgb8(0x14, 0x19, 0x21);
pub const RAISED: Color = Color::from_rgb8(0x1b, 0x22, 0x2d);
pub const BORDER: Color = Color::from_rgb8(0x27, 0x30, 0x3d);
pub const TEXT: Color = Color::from_rgb8(0xe6, 0xea, 0xf0);
pub const MUTED: Color = Color::from_rgb8(0x8a, 0x95, 0xa6);
pub const ACCENT: Color = Color::from_rgb8(0x6f, 0xa8, 0xff);
pub const ACCENT_ALT: Color = Color::from_rgb8(0x9d, 0x86, 0xff);
pub const SUCCESS: Color = Color::from_rgb8(0x3d, 0xd6, 0x8c);
pub const DANGER: Color = Color::from_rgb8(0xff, 0x6b, 0x6b);
pub const TEAL: Color = Color::from_rgb8(0x4f, 0xd1, 0xc5);
pub const WARNING: Color = Color::from_rgb8(0xf5, 0xb9, 0x4a);

pub const RADIUS: f32 = 16.0;

pub fn theme() -> Theme {
    Theme::custom(
        "Nix Manager".to_owned(),
        theme::Palette {
            background: BG,
            text: TEXT,
            primary: ACCENT,
            success: SUCCESS,
            warning: WARNING,
            danger: DANGER,
        },
    )
}

pub fn with_alpha(color: Color, alpha: f32) -> Color {
    Color { a: color.a * alpha.clamp(0.0, 1.0), ..color }
}

pub fn mix(from: Color, to: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color {
        r: from.r + (to.r - from.r) * t,
        g: from.g + (to.g - from.g) * t,
        b: from.b + (to.b - from.b) * t,
        a: from.a + (to.a - from.a) * t,
    }
}

pub fn page(_: &Theme) -> container::Style {
    container::Style { background: Some(Background::Color(BG)), text_color: Some(TEXT), ..Default::default() }
}

pub fn card(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(SURFACE)),
        border: Border { color: BORDER, width: 1.0, radius: Radius::from(RADIUS) },
        ..Default::default()
    }
}

/// A card that reacts to `glow` (0..1): the border and fill warm towards the
/// accent and a soft shadow blooms underneath.
pub fn interactive_card(glow: f32, enabled: bool) -> impl Fn(&Theme) -> container::Style {
    move |_| {
        let glow = if enabled { glow } else { 0.0 };
        container::Style {
            background: Some(Background::Color(mix(SURFACE, RAISED, glow))),
            border: Border {
                color: mix(BORDER, with_alpha(ACCENT, 0.7), glow),
                width: 1.0,
                radius: Radius::from(RADIUS),
            },
            shadow: Shadow {
                color: with_alpha(ACCENT, 0.22 * glow),
                offset: Vector::new(0.0, 6.0 * glow),
                blur_radius: 22.0 * glow,
            },
            ..Default::default()
        }
    }
}

/// Rounded square that holds an icon.
pub fn icon_tile(tint: Color, strength: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(with_alpha(tint, 0.12 + 0.16 * strength))),
        border: Border { color: with_alpha(tint, 0.18 + 0.3 * strength), width: 1.0, radius: Radius::from(12.0) },
        ..Default::default()
    }
}

pub fn chip(tint: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(with_alpha(tint, 0.14))),
        border: Border { color: with_alpha(tint, 0.35), width: 1.0, radius: Radius::from(999.0) },
        ..Default::default()
    }
}

pub fn solid(color: Color, radius: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(color)),
        border: Border { radius: Radius::from(radius), ..Default::default() },
        ..Default::default()
    }
}

pub fn glowing(color: Color, radius: f32, glow: f32) -> impl Fn(&Theme) -> container::Style {
    move |_| container::Style {
        background: Some(Background::Color(color)),
        border: Border { radius: Radius::from(radius), ..Default::default() },
        shadow: Shadow { color: with_alpha(color, 0.55 * glow), offset: Vector::ZERO, blur_radius: 14.0 * glow },
        ..Default::default()
    }
}

pub fn well(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb8(0x0a, 0x0d, 0x12))),
        border: Border { color: BORDER, width: 1.0, radius: Radius::from(12.0) },
        ..Default::default()
    }
}

pub fn input(_: &Theme, status: text_input::Status) -> text_input::Style {
    let border = match status {
        text_input::Status::Focused { .. } => ACCENT,
        text_input::Status::Hovered => mix(BORDER, ACCENT, 0.4),
        _ => BORDER,
    };
    text_input::Style {
        background: Background::Color(RAISED),
        border: Border { color: border, width: 1.0, radius: Radius::from(10.0) },
        icon: MUTED,
        placeholder: with_alpha(MUTED, 0.7),
        value: TEXT,
        selection: with_alpha(ACCENT, 0.35),
    }
}
