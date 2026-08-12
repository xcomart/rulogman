//! A stateless push button.

use gpui::{App, ClickEvent, ElementId, Hsla, SharedString, Window, div, prelude::*, px};

use super::theme::{shift_lightness, theme};

/// Visual weight of a [`Button`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ButtonVariant {
    /// Filled with the accent color; use for the single main action of a view.
    #[default]
    Primary,
    /// Filled with a neutral surface color and outlined.
    Secondary,
    /// No background until hovered; use inside dense toolbars.
    Ghost,
    /// Filled with the danger color; use for destructive actions.
    Danger,
}

/// The resolved colors of one button state.
struct ButtonColors {
    background: Hsla,
    hover: Hsla,
    active: Hsla,
    label: Hsla,
    border: Option<Hsla>,
}

/// Callback fired when a [`Button`] is clicked.
type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// A stateless button element.
///
/// `Button` owns no state, so it is rebuilt on every render of its parent:
///
/// ```ignore
/// Button::new("connect", "Connect")
///     .variant(ButtonVariant::Primary)
///     .on_click(cx.listener(|this, _, _, cx| this.connect(cx)))
/// ```
#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: SharedString,
    variant: ButtonVariant,
    disabled: bool,
    full_width: bool,
    tab_index: Option<isize>,
    on_click: Option<ClickHandler>,
}

impl Button {
    /// Creates a [`ButtonVariant::Primary`] button.
    ///
    /// `id` must be unique among the siblings of the button.
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            variant: ButtonVariant::default(),
            disabled: false,
            full_width: false,
            tab_index: None,
            on_click: None,
        }
    }

    /// Sets the visual weight of the button.
    pub fn variant(mut self, variant: ButtonVariant) -> Self {
        self.variant = variant;
        self
    }

    /// Greys the button out and stops it from reacting to clicks.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Stretches the button across the width of its parent.
    pub fn full_width(mut self, full_width: bool) -> Self {
        self.full_width = full_width;
        self
    }

    /// Places the button at `index` in the window's tab order.
    ///
    /// A focused button draws an accent outline and is activated by `Enter` or
    /// `Space`, which gpui turns into an ordinary click. Disabled buttons are
    /// skipped, mirroring how the platform treats a disabled control.
    pub fn tab_index(mut self, index: isize) -> Self {
        self.tab_index = Some(index);
        self
    }

    /// Sets the click callback. Ignored while the button is disabled.
    pub fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }
}

/// Resolves the palette for `variant` against the active theme.
fn colors_for(variant: ButtonVariant, cx: &App) -> ButtonColors {
    let theme = theme(cx);
    match variant {
        ButtonVariant::Primary => ButtonColors {
            background: theme.accent,
            hover: shift_lightness(theme.accent, 0.06),
            active: shift_lightness(theme.accent, -0.06),
            label: theme.background,
            border: None,
        },
        ButtonVariant::Secondary => ButtonColors {
            background: theme.surface_hover,
            hover: theme.surface_active,
            active: shift_lightness(theme.surface_active, -0.04),
            label: theme.text,
            border: Some(theme.border),
        },
        ButtonVariant::Ghost => ButtonColors {
            background: gpui::transparent_black(),
            hover: theme.surface_hover,
            active: theme.surface_active,
            label: theme.text,
            border: None,
        },
        ButtonVariant::Danger => ButtonColors {
            background: theme.danger,
            hover: shift_lightness(theme.danger, 0.06),
            active: shift_lightness(theme.danger, -0.06),
            label: theme.background,
            border: None,
        },
    }
}

impl RenderOnce for Button {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let accent = theme(cx).accent;
        let colors = colors_for(self.variant, cx);
        let disabled = self.disabled;
        let label_color = if disabled {
            colors.label.opacity(0.5)
        } else {
            colors.label
        };
        // The outline is always present, merely transparent, so that gaining
        // focus recolours it instead of resizing the button.
        let border = match colors.border {
            Some(border) if disabled => border.opacity(0.5),
            Some(border) => border,
            None => gpui::transparent_black(),
        };

        div()
            .id(self.id)
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .h(px(30.))
            .px(px(12.))
            .rounded_md()
            .text_size(px(13.))
            .whitespace_nowrap()
            .bg(if disabled {
                colors.background.opacity(0.5)
            } else {
                colors.background
            })
            .text_color(label_color)
            .border_1()
            .border_color(border)
            .when_some(self.tab_index.filter(|_| !disabled), |this, index| {
                this.tab_index(index)
                    .focus(move |style| style.border_color(accent))
            })
            .when(self.full_width, |this| this.w_full())
            .when(!disabled, |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(colors.hover))
                    .active(|style| style.bg(colors.active))
            })
            .when_some(self.on_click.filter(|_| !disabled), |this, handler| {
                this.on_click(move |event, window, cx| handler(event, window, cx))
            })
            .child(self.label)
    }
}
