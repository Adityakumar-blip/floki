//! Colors and sizes for the dark and light themes, taken from the Floki logo: fjord navy, steel, amber and sea teal.
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
    bg: 0x0f1624,
    bg_secondary: 0x131c2d,
    bg_tertiary: 0x1a2538,
    text: 0xe8edf5,
    text_secondary: 0x9aa8be,
    text_tertiary: 0x61708a,
    border: 0x22304a,
    border_strong: 0x2c3c5a,
    hover: 0x1e2a40,
    brand: 0xe3a15a,
    send: 0x0b7f93,
    send_hover: 0x096c7d,
    link: 0x5cc8db,
    success: 0x5bd69a,
    warning: 0xf2c46d,
    error: 0xf28b82,
    info: 0x7cb4f8,
    patch: 0xb79cf0,
    options: 0xf07fbf,
};

pub const LIGHT: Tokens = Tokens {
    mode: ThemeMode::Light,
    bg: 0xffffff,
    bg_secondary: 0xf6f8fb,
    bg_tertiary: 0xeef2f7,
    text: 0x0f1624,
    text_secondary: 0x55637a,
    text_tertiary: 0x8d99ae,
    border: 0xe3e8f0,
    border_strong: 0xd5dce7,
    hover: 0xebf0f6,
    brand: 0xc47f35,
    send: 0x0b7f93,
    send_hover: 0x096c7d,
    link: 0x0b6f80,
    success: 0x0e7a43,
    warning: 0x9a6a00,
    error: 0xb4231a,
    info: 0x1d5fbf,
    patch: 0x6b3fb0,
    options: 0xa3166b,
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
