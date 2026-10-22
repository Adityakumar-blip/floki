use crate::theme::Tokens;
use gpui_kit::prelude::*;
use gpui_kit::*;

pub fn icon(path: &'static str, size: f32, color: u32) -> Svg {
    svg()
        .path(path)
        .size(px(size))
        .flex_none()
        .text_color(rgb(color))
}

pub fn checkbox(id: impl Into<ElementId>, checked: bool, t: &Tokens) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(14.))
        .rounded(px(2.))
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .when(checked, |d| d.bg(rgb(t.text_secondary)))
        .when(!checked, |d| d.border_1().border_color(rgb(t.text_tertiary)))
        .when(checked, |d| d.child(icon("icons/check.svg", 12., t.bg)))
}

/// Small toggle switch used in request settings.
pub fn switch(id: impl Into<ElementId>, on: bool, t: &Tokens) -> Stateful<Div> {
    div()
        .id(id)
        .w(px(28.))
        .h(px(16.))
        .rounded(px(8.))
        .p(px(2.))
        .flex()
        .cursor_pointer()
        .bg(rgb(if on { t.send } else { t.border_strong }))
        .when(on, |d| d.justify_end())
        .child(div().size(px(12.)).rounded(px(6.)).bg(rgb(0xffffff)))
}

/// Text button: 32px control, transparent until hovered.
pub fn ghost_button(id: impl Into<ElementId>, t: &Tokens) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(32.))
        .px_2()
        .gap_1()
        .flex()
        .items_center()
        .rounded(px(4.))
        .cursor_pointer()
        .text_color(rgb(t.text_secondary))
        .hover(|s| s.bg(rgb(t.hover)).text_color(rgb(t.text)))
}

/// Small filled button, like the sidebar's New / Import.
pub fn small_button(id: impl Into<ElementId>, label: &'static str, t: &Tokens) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(24.))
        .px_2()
        .flex()
        .items_center()
        .rounded(px(4.))
        .cursor_pointer()
        .bg(rgb(t.hover))
        .hover(|s| s.bg(rgb(t.border_strong)))
        .text_size(px(11.))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(t.text))
        .child(label)
}

pub fn primary_button(id: impl Into<ElementId>, label: impl Into<SharedString>, t: &Tokens) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(32.))
        .px_3()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .cursor_pointer()
        .bg(rgb(t.send))
        .hover(|s| s.bg(rgb(t.send_hover)))
        .text_color(rgb(0xffffff))
        .font_weight(FontWeight::SEMIBOLD)
        .child(label.into())
}

pub fn outline_button(id: impl Into<ElementId>, label: impl Into<SharedString>, t: &Tokens) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(32.))
        .px_3()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .cursor_pointer()
        .border_1()
        .border_color(rgb(t.border_strong))
        .hover(|s| s.bg(rgb(t.hover)))
        .text_color(rgb(t.text))
        .child(label.into())
}

pub fn icon_button(id: impl Into<ElementId>, path: &'static str, t: &Tokens) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(24.))
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(t.hover)))
        .child(icon(path, 16., t.text_secondary))
}

/// Floating menu surface; place inside `deferred(anchored())`.
pub fn menu(t: &Tokens) -> Div {
    // Occlude so presses on items don't also reach the dismiss backdrop beneath.
    div()
        .occlude()
        .min_w(px(180.))
        .py_1()
        .flex()
        .flex_col()
        .bg(rgb(t.bg_secondary))
        .border_1()
        .border_color(rgb(t.border_strong))
        .rounded(px(4.))
        .shadow_lg()
        .text_size(px(12.))
        .text_color(rgb(t.text))
}

pub fn menu_item(id: impl Into<ElementId>, t: &Tokens) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(32.))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .hover(|s| s.bg(rgb(t.hover)))
}

/// Popover anchored below its parent, which must be `relative()`.
pub fn popover(content: impl IntoElement) -> impl IntoElement {
    div().absolute().left_0().top_full().child(
        deferred(anchored().snap_to_window_with_margin(px(8.)).child(div().mt_1().child(content)))
            .with_priority(2),
    )
}

/// Invisible full-window layer under a menu; clicking it dismisses the menu.
pub fn backdrop(
    window: &Window,
    on_dismiss: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let size = window.viewport_size();
    deferred(
        anchored().position(point(px(0.), px(0.))).child(
            div()
                .id("menu-backdrop")
                .w(size.width)
                .h(size.height)
                .occlude()
                .on_mouse_down(MouseButton::Left, on_dismiss),
        ),
    )
    .with_priority(1)
}

/// Underlined section tab, like Params / Authorization / Headers.
pub fn section_tab(id: impl Into<ElementId>, label: impl Into<SharedString>, active: bool, t: &Tokens) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(32.))
        .flex()
        .items_center()
        .gap_1()
        .cursor_pointer()
        .text_color(rgb(if active { t.text } else { t.text_secondary }))
        .hover(|s| s.text_color(rgb(t.text)))
        .when(active, |d| d.border_b_1().border_color(rgb(t.brand)))
        .child(label.into())
}

pub fn dot(color: u32) -> Div {
    div().size(px(6.)).rounded(px(3.)).bg(rgb(color)).flex_none()
}
