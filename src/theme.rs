//! Colors and sizes for the dark and light themes.
use crate::model::ThemeMode;
use gpui_kit::component::{Theme as ComponentTheme, ThemeMode as ComponentMode};
use gpui_kit::*;

#[derive(Clone, Copy)]
pub struct Tokens {
    pub mode: ThemeMode,
    pub bg: u32,
    pub bg_secondary: u32,
    pub bg_tertiary: u32,
    pub text: u32,
    pub text_secondary: u32,
    pub text_tertiary: u32,
    pub border: u32,
    pub border_strong: u32,
    pub hover: u32,
    pub brand: u32,
    pub send: u32,
    pub send_hover: u32,
    pub link: u32,
    pub success: u32,
    pub warning: u32,
    pub error: u32,
    pub info: u32,
    pub patch: u32,
    pub options: u32,
}

impl Global for Tokens {}

pub const DARK: Tokens = Tokens {
    mode: ThemeMode::Dark,
    bg: 0x212121,
    bg_secondary: 0x262626,
    bg_tertiary: 0x2b2b2b,
    text: 0xffffff,
    text_secondary: 0xa6a6a6,
    text_tertiary: 0x6b6b6b,
    border: 0x303030,
    border_strong: 0x3b3b3b,
    hover: 0x303030,
    brand: 0xff6c37,
    send: 0x097bed,
    send_hover: 0x0265d2,
    link: 0x3e92f2,
    success: 0x6bdd9a,
    warning: 0xffe47e,
    error: 0xf79a8e,
    info: 0x74aef6,
    patch: 0xc0a8e1,
    options: 0xf15eb0,
};

pub const LIGHT: Tokens = Tokens {
    mode: ThemeMode::Light,
    bg: 0xffffff,
    bg_secondary: 0xf9f9f9,
    bg_tertiary: 0xf2f2f2,
    text: 0x212121,
    text_secondary: 0x6b6b6b,
    text_tertiary: 0xa6a6a6,
    border: 0xededed,
    border_strong: 0xe6e6e6,
    hover: 0xededed,
    brand: 0xff6c37,
    send: 0x097bed,
    send_hover: 0x0265d2,
    link: 0x0265d2,
    success: 0x007f31,
    warning: 0xad7a03,
    error: 0x8e1a10,
    info: 0x0053b8,
    patch: 0x623497,
    options: 0xa61468,
};

pub const FONT: &str = "Inter";
pub const MONO: &str = "IBM Plex Mono";

impl Tokens {
    pub fn get(cx: &App) -> Tokens {
        *cx.global::<Tokens>()
    }

    pub fn method(&self, method: &str) -> u32 {
        match method {
            "GET" | "HEAD" => self.success,
            "POST" => self.warning,
            "PUT" => self.info,
            "DELETE" => self.error,
            "PATCH" => self.patch,
            "OPTIONS" => self.options,
            _ => self.text_secondary,
        }
    }

    pub fn status(&self, code: u16) -> u32 {
        match code {
            200..=299 => self.success,
            300..=399 => self.info,
            _ => self.error,
        }
    }
}

fn hsla(color: u32) -> Hsla {
    rgb(color).into()
}

/// Applies Floki tokens globally and to the component library's inputs and editors.
pub fn apply(mode: ThemeMode, cx: &mut App) {
    let t = if mode == ThemeMode::Dark { DARK } else { LIGHT };
    cx.set_global(t);
    ComponentTheme::change(
        if mode == ThemeMode::Dark {
            ComponentMode::Dark
        } else {
            ComponentMode::Light
        },
        None,
        cx,
    );
    let theme = ComponentTheme::global_mut(cx);
    theme.font_family = FONT.into();
    theme.font_size = px(12.);
    theme.mono_font_family = MONO.into();
    theme.mono_font_size = px(12.);
    theme.radius = px(4.);
    let c = &mut theme.colors;
    c.background = hsla(t.bg);
    c.foreground = hsla(t.text);
    c.border = hsla(t.border);
    c.input = hsla(t.border);
    c.ring = hsla(t.send).opacity(0.5);
    c.muted = hsla(t.bg_tertiary);
    c.muted_foreground = hsla(t.text_tertiary);
    c.caret = hsla(t.text);
    c.selection = hsla(t.send).opacity(0.35);
    c.accent = hsla(t.hover);
    c.accent_foreground = hsla(t.text);
    c.popover = hsla(t.bg_secondary);
    c.popover_foreground = hsla(t.text);
    c.scrollbar = hsla(t.bg).opacity(0.);
    c.scrollbar_thumb = hsla(t.text_tertiary).opacity(0.5);
    c.scrollbar_thumb_hover = hsla(t.text_tertiary);
    c.list = hsla(t.bg);
    c.list_hover = hsla(t.hover);
    let mut highlight = (*theme.highlight_theme).clone();
    highlight.style.editor_background = Some(hsla(t.bg));
    highlight.style.editor_gutter_background = Some(hsla(t.bg));
    highlight.style.editor_active_line = Some(hsla(t.bg_secondary));
    highlight.style.editor_line_number = Some(hsla(t.text_tertiary));
    highlight.style.editor_active_line_number = Some(hsla(t.text_secondary));
    theme.highlight_theme = std::sync::Arc::new(highlight);
    cx.refresh_windows();
}
