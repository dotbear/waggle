//! Desktop notifications: Windows toasts.
//!
//! What a notification says comes from `waggle_core::notify`; this only puts
//! it on screen, with a sound per kind so intel can be told apart by ear
//! without looking.

use anyhow::Result;
use waggle_core::notify::{Notification, NotificationSink};

/// The AppUserModelID Waggle runs and sends toasts under. Windows only shows
/// a toast from an unpackaged program if a Start Menu shortcut carries this
/// ID; the installer creates one (it is the installer's `identifier`). A
/// build run straight from a folder has no shortcut, so its toasts go out
/// under PowerShell's ID instead and still appear.
pub const APP_USER_MODEL_ID: &str = "io.github.dotbear.waggle";

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
    // The installer puts the shortcut in a Waggle folder by default; someone
    // may have moved it to the top level.
    let installed = std::env::var_os("APPDATA").is_some_and(|appdata| {
        let programs =
            std::path::Path::new(&appdata).join(r"Microsoft\Windows\Start Menu\Programs");
        programs.join(r"Waggle\Waggle.lnk").exists() || programs.join("Waggle.lnk").exists()
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

/// Tells Windows which app this process is, so the taskbar and toasts tie it
/// to the installed shortcut. Must run before any window exists.
#[cfg(windows)]
pub fn declare_app_id() {
    let wide: Vec<u16> = APP_USER_MODEL_ID
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: a valid, NUL-terminated UTF-16 string that outlives the call.
    let result = unsafe {
        windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(wide.as_ptr())
    };
    if result < 0 {
        tracing::warn!(hresult = result, "Could not set the AppUserModelID");
    }
}

#[cfg(not(windows))]
pub fn declare_app_id() {}
