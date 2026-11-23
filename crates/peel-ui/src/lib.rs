//! Peel Studio's GPU widget toolkit.
//!
//! Everything on screen is drawn by two pipelines: instanced signed-distance
//! shapes and glyphon text. Widgets are immediate-mode with stable ids; the
//! runner repaints only on input or when something animates.
//!
//! Widgets also describe themselves to screen readers through AccessKit and
//! can be reached and operated from the keyboard; see [`a11y`] and [`focus`].

pub mod a11y;
pub mod app;
pub mod chrome;
pub mod focus;
pub mod geom;
pub mod icons;
pub mod input;
pub mod menu;
#[cfg(any(target_os = "macos", target_os = "windows"))]
mod native_menu;
pub mod render;
pub mod scene;
pub mod text;
pub mod theme;
pub mod ui;
pub mod widgets;

#[cfg(test)]
mod tests;

pub use a11y::{Action, ActionRequest, Role, Semantics};
pub use app::{App, Waker, WindowControl, WindowOptions, run};
pub use chrome::Edge;
pub use focus::{Adjust, FocusKind};
pub use geom::{Color, Rect, Vec2, rect, vec2};
pub use input::{Button, Input, KeyPress, Modifiers};
pub use menu::{
    Menu, MenuBar, MenuCommand, MenuEntry, MenuRole, Shortcut, ShortcutKey, StandardItem,
};
pub use scene::{Align, Dir, LayerId, Scene};
pub use text::{FontKind, FontWeight, TextStyle};
pub use theme::Theme;
pub use ui::{DragPayload, Id, Response, Ui};
pub use widgets::{MenuItem, meter_pos};
pub use winit::keyboard::{Key, KeyCode, NamedKey};
pub use winit::window::CursorIcon;
