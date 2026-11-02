//! Desktop notifications: Windows toasts.
//!
//! What a notification says comes from `waggle_core::notify`; this only puts
//! it on screen, with a sound per kind so intel can be told apart by ear
//! without looking.

use std::sync::OnceLock;

use anyhow::Result;
use waggle_core::notify::{Notification, NotificationSink};

/// What clicking a toast does: set once at startup to bring the window up.
/// Toasts report clicks on a thread of Windows' choosing, so this must be
/// callable from anywhere.
pub static ON_CLICK: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

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
    /// For "hide message text while locked". While the workstation is locked
    /// the secure desktop has the input, and switching to the user's input
    /// desktop fails; that is the usual test, and it needs no permissions.
    #[cfg(windows)]
    fn screen_locked(&self) -> bool {
        use windows_sys::Win32::System::StationsAndDesktops::{
            CloseDesktop, DESKTOP_SWITCHDESKTOP, OpenInputDesktop, SwitchDesktop,
        };
        // SAFETY: plain Win32 calls; the handle is closed before returning.
        unsafe {
            let desktop = OpenInputDesktop(0, 0, DESKTOP_SWITCHDESKTOP);
            if desktop.is_null() {
                return true;
            }
            let switched = SwitchDesktop(desktop) != 0;
            CloseDesktop(desktop);
            !switched
        }
    }

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
        toast = toast.on_activated(|_| {
            if let Some(on_click) = ON_CLICK.get() {
                on_click();
            }
            Ok(())
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
