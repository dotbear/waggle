//! The tray icon and its menu.
//!
//! Created on the main thread once Slint's event loop runs, so winit pumps
//! its messages; menu clicks are read by a timer the app keeps alive
//! (docs/windows-notes.md: a dropped `slint::Timer` cancels itself).

/// What the tray asks the app to do.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum TrayAction {
    Open,
    RefreshPaps,
    Quit,
}

#[cfg(windows)]
pub use windows::Tray;

#[cfg(windows)]
mod windows {
    use anyhow::{Context, Result};
    use tray_icon::{
        Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent,
        menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
    };

    use super::TrayAction;

    pub struct Tray {
        icon: TrayIcon,
        open: MenuId,
        refresh: MenuId,
        quit: MenuId,
    }

    impl Tray {
        pub fn create() -> Result<Self> {
            let menu = Menu::new();
            let open = MenuItem::new("Open Waggle", true, None);
            let refresh = MenuItem::new("Refresh PAPs", true, None);
            let quit = MenuItem::new("Quit", true, None);
            menu.append_items(&[&open, &refresh, &PredefinedMenuItem::separator(), &quit])
                .context("could not build the tray menu")?;
            let icon = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("Waggle")
                .with_icon(icon()?)
                .build()
                .context("could not create the tray icon")?;
            Ok(Self {
                icon,
                open: open.id().clone(),
                refresh: refresh.id().clone(),
                quit: quit.id().clone(),
            })
        }

        /// Everything clicked since the last call.
        pub fn actions(&self) -> Vec<TrayAction> {
            let mut actions = Vec::new();
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id == self.open {
                    actions.push(TrayAction::Open);
                } else if event.id == self.refresh {
                    actions.push(TrayAction::RefreshPaps);
                } else if event.id == self.quit {
                    actions.push(TrayAction::Quit);
                }
            }
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                // A left click opens the window, as most tray apps do; the
                // right click is the menu.
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: tray_icon::MouseButtonState::Up,
                    ..
                } = event
                {
                    actions.push(TrayAction::Open);
                }
            }
            actions
        }

        pub fn set_tooltip(&self, text: &str) {
            let _ = self.icon.set_tooltip(Some(text));
        }
    }

    fn icon() -> Result<Icon> {
        // Written by scripts/make-icon.py: 32x32 pixels, RGBA.
        let rgba = include_bytes!("../ui/tray-32.rgba");
        Icon::from_rgba(rgba.to_vec(), 32, 32).context("the tray icon image is invalid")
    }
}

/// On other platforms there is no tray yet; the window is the app.
#[cfg(not(windows))]
pub struct Tray;

#[cfg(not(windows))]
impl Tray {
    pub fn create() -> anyhow::Result<Self> {
        Ok(Self)
    }

    pub fn actions(&self) -> Vec<TrayAction> {
        Vec::new()
    }

    pub fn set_tooltip(&self, _text: &str) {}
}
