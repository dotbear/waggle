//! Desktop notifications: Windows toasts.
//!
//! What a notification says comes from `waggle_core::notify`; this only puts
//! it on screen, with a sound per kind so intel can be told apart by ear
//! without looking.

use anyhow::Result;
use waggle_core::notify::{Notification, NotificationSink};

/// The AppUserModelID toasts are sent under. Windows only shows a toast from
/// an unpackaged program if a Start Menu shortcut carries this ID, which the
/// installer creates. Without it (a build run straight from a folder), toasts
/// go out under PowerShell's ID instead so they still appear.
pub const APP_USER_MODEL_ID: &str = "dotbear.Waggle";

#[derive(Debug)]
pub struct Desktop {
    #[cfg_attr(not(windows), allow(dead_code))]
    app_id: String,
}

impl Desktop {
    pub fn detect() -> Self {
        Self {
            app_id: app_id_for_toasts(),
        }
    }
}

#[cfg(windows)]
fn app_id_for_toasts() -> String {
    let installed = std::env::var_os("APPDATA").is_some_and(|appdata| {
        std::path::Path::new(&appdata)
            .join(r"Microsoft\Windows\Start Menu\Programs\Waggle.lnk")
            .exists()
    });
    if installed {
        APP_USER_MODEL_ID.to_owned()
    } else {
        tauri_winrt_notification::Toast::POWERSHELL_APP_ID.to_owned()
    }
}

#[cfg(not(windows))]
fn app_id_for_toasts() -> String {
    APP_USER_MODEL_ID.to_owned()
}

impl NotificationSink for Desktop {
    #[cfg(windows)]
    fn deliver(&self, notification: &Notification) -> Result<()> {
        use tauri_winrt_notification::{Duration, LoopableSound, Sound, Toast};
        use waggle_core::notify::Urgency;

        let sound = match notification.category {
            "intel" => Sound::Single(LoopableSound::Alarm2),
            "ping" => Sound::IM,
            _ => Sound::SMS,
        };
        let mut toast = Toast::new(&self.app_id)
            .title(&notification.title)
            .text1(&notification.body)
            .sound(Some(sound))
            .duration(if notification.urgency == Urgency::High {
                Duration::Long
            } else {
                Duration::Short
            });
        if let Some(image) = notification.image.as_deref() {
            toast = toast.icon(
                image,
                tauri_winrt_notification::IconCrop::Circular,
                &notification.title,
            );
        }
        toast
            .show()
            .map_err(|error| anyhow::anyhow!("Windows refused the toast: {error:?}"))
    }

    #[cfg(not(windows))]
    fn deliver(&self, notification: &Notification) -> Result<()> {
        tracing::info!(
            title = notification.title,
            body = notification.body,
            "Notification (no desktop delivery on this platform)"
        );
        Ok(())
    }
}
