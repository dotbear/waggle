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
    Settings,
    RefreshPaps,
    /// For this many minutes.
    PauseAlerts(u16),
    ResumeAlerts,
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
    use crate::view::Mood;

    pub struct Tray {
        icon: TrayIcon,
        open: MenuId,
        settings: MenuId,
        refresh: MenuId,
        pause_short: MenuId,
        pause: MenuId,
        resume: MenuId,
        quit: MenuId,
    }

    impl Tray {
        pub fn create() -> Result<Self> {
            let menu = Menu::new();
            let open = MenuItem::new("Open Waggle", true, None);
            let settings = MenuItem::new("Settings…", true, None);
            let pause_short = MenuItem::new("Pause alerts for 15 minutes", true, None);
            let pause = MenuItem::new("Pause alerts for 1 hour", true, None);
            let resume = MenuItem::new("Resume alerts", true, None);
            let refresh = MenuItem::new("Refresh PAPs", true, None);
            let quit = MenuItem::new("Quit", true, None);
            menu.append_items(&[
                &open,
                &settings,
                &refresh,
                &PredefinedMenuItem::separator(),
                &pause_short,
                &pause,
                &resume,
                &PredefinedMenuItem::separator(),
                &quit,
            ])
            .context("could not build the tray menu")?;
            let icon = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("Waggle")
                .with_icon(icon(Mood::Normal)?)
                .build()
                .context("could not create the tray icon")?;
            Ok(Self {
                icon,
                open: open.id().clone(),
                settings: settings.id().clone(),
                pause_short: pause_short.id().clone(),
                pause: pause.id().clone(),
                resume: resume.id().clone(),
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
                } else if event.id == self.settings {
                    actions.push(TrayAction::Settings);
                } else if event.id == self.pause_short {
                    actions.push(TrayAction::PauseAlerts(15));
                } else if event.id == self.pause {
                    actions.push(TrayAction::PauseAlerts(60));
                } else if event.id == self.resume {
                    actions.push(TrayAction::ResumeAlerts);
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

        pub fn set_mood(&self, mood: Mood) {
            if let Ok(icon) = icon(mood) {
                let _ = self.icon.set_icon(Some(icon));
            }
        }
    }

    fn icon(mood: Mood) -> Result<Icon> {
        // Written by scripts/make-icon.py: 32x32 pixels, RGBA.
        let rgba = super::recolour(include_bytes!("../ui/tray-32.rgba"), mood);
        Icon::from_rgba(rgba, 32, 32).context("the tray icon image is invalid")
    }
}

/// The honeycomb in the mood's colour: as drawn when all is well, red when
/// hostiles are near, grey when Jabber is down or alerts are paused. Pixels
/// keep their brightness and alpha, so the outline and shading survive.
#[cfg_attr(not(windows), allow(dead_code))]
fn recolour(rgba: &[u8], mood: crate::view::Mood) -> Vec<u8> {
    use crate::view::Mood;
    let mut out = rgba.to_vec();
    if mood == Mood::Normal {
        return out;
    }
    for pixel in out.as_chunks_mut::<4>().0 {
        let [r, g, b] = [pixel[0], pixel[1], pixel[2]].map(f32::from);
        let light = 0.299 * r + 0.587 * g + 0.114 * b;
        let [r, g, b] = match mood {
            Mood::Danger => [light * 1.6, light * 0.35, light * 0.3],
            _ => [light * 0.8; 3],
        };
        pixel[0] = r.min(255.0) as u8;
        pixel[1] = g.min(255.0) as u8;
        pixel[2] = b.min(255.0) as u8;
    }
    out
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

    pub fn set_mood(&self, _mood: crate::view::Mood) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::Mood;

    #[test]
    fn moods_recolour_without_touching_alpha() {
        let gold = [224, 168, 58, 200];
        assert_eq!(recolour(&gold, Mood::Normal), gold);
        let red = recolour(&gold, Mood::Danger);
        assert!(red[0] > red[1] && red[0] > red[2] && red[3] == 200);
        let grey = recolour(&gold, Mood::Muted);
        assert!(grey[0] == grey[1] && grey[1] == grey[2] && grey[3] == 200);
    }
}
