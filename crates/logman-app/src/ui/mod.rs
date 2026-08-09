//! Reusable gpui widgets shared by every logman view.
//!
//! The module is deliberately free of SSH or terminal concepts: it only knows
//! about colors ([`theme`]), text entry ([`text_input`]), buttons ([`button`]),
//! session tabs ([`tab_bar`]), dropdown menus ([`menu`]), hover tooltips
//! ([`tooltip`]), dialogs ([`modal`]), overlay scroll indicators
//! ([`scrollbar`]) and the caption buttons of a self-drawn title bar
//! ([`window_controls`]).
//!
//! Call [`init`] once during application start-up so the widgets that need key
//! bindings get them.

pub mod button;
pub mod checkbox;
pub mod menu;
pub mod modal;
pub mod scheme_select;
pub mod scrollbar;
pub mod segmented;
pub mod select;
pub mod tab_bar;
pub mod text_input;
pub mod theme;
pub mod tooltip;
pub mod window_controls;

pub use button::{Button, ButtonVariant};
pub use checkbox::Checkbox;
pub use menu::{ContextMenu, MenuButton, MenuEntry};
pub use modal::{form_row, modal};
pub use scheme_select::{SchemePreview, SchemeSelect, SchemeSwatch};
pub use scrollbar::{
    DraggedThumb, Scrollbar, ScrollbarAxis, ScrollbarState, hide_later, hide_now, scroll_to,
    scrolled,
};
pub use segmented::Segmented;
pub use select::Select;
pub use tab_bar::{TabBar, TabItem, TabStatus};
pub use text_input::TextInput;
pub use theme::{
    CustomUiTheme, Theme, ThemeColors, ThemeFile, ThemeRegistry, parse_hex, set_theme, theme,
};
pub use tooltip::tooltip_label;
pub use window_controls::{WindowControlIcons, WindowControls};

use gpui::App;

/// Registers everything the widget layer needs before the first window opens.
pub fn init(cx: &mut App) {
    ThemeRegistry::init(cx);
    set_theme(Theme::dark(), cx);
    TextInput::init(cx);
}
