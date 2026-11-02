// No console window behind the app in release builds; logs go to a file.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    cell::RefCell,
    fs::File,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use slint::{CloseRequestResponse, ComponentHandle, Timer, TimerMode};
use tracing::{error, info, warn};
use waggle_core::notify::{Notification, NotificationSink, Urgency};

use waggle::{
    AppWindow,
    config::Config,
    engine::{self, Command},
    paths::AppPaths,
    toast,
    tray::{Tray, TrayAction},
    view,
};

fn main() {
    if let Err(error) = run() {
        error!(error = %format!("{error:#}"), "Waggle could not start");
        eprintln!("Waggle could not start: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let paths = AppPaths::discover()?;
    paths.ensure()?;
    let _log_guard = init_logging(&paths);
    info!(version = env!("CARGO_PKG_VERSION"), "Waggle starting");

    // One Waggle per user: a second copy would log into Jabber again and
    // announce every alert twice. The lock is released when the process ends,
    // however it ends.
    let lock = File::create(&paths.lock_file).context("could not create the lock file")?;
    if lock.try_lock().is_err() {
        info!("Waggle is already running");
        let _ = toast::Desktop::detect().deliver(&status_notification(
            "Waggle is already running",
            "Look for the honeycomb in the system tray.",
        ));
        return Ok(());
    }

    let config = Config::load(&paths.config_file)?;
    if !paths.config_file.exists() {
        // Written once, so there is a file to find and edit.
        if let Err(error) = config.save(&paths.config_file) {
            warn!(%error, "Could not write the default settings");
        }
    }

    let window = AppWindow::new().context("could not create the window")?;
    // Closing hides to the tray; Quit in the tray menu ends the program.
    window
        .window()
        .on_close_requested(|| CloseRequestResponse::HideWindow);

    let tooltip = Arc::new(Mutex::new(String::from("Waggle")));
    let commands = {
        let weak = window.as_weak();
        let tooltip = Arc::clone(&tooltip);
        engine::spawn(paths.clone(), config, move |view| {
            if let Ok(mut text) = tooltip.lock() {
                *text = view.tooltip();
            }
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    view::apply(&window, &view);
                }
            });
        })
    };

    {
        let commands = commands.clone();
        window.on_refresh_paps(move || {
            let _ = commands.send(Command::RefreshPaps);
        });
    }

    window.show().context("could not show the window")?;

    // The tray is created once the event loop runs, on this thread, and its
    // clicks are read by a timer the program keeps alive.
    thread_local! {
        static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
        static TRAY_POLL: Timer = Timer::default();
    }
    let weak = window.as_weak();
    Timer::single_shot(Duration::ZERO, move || {
        match Tray::create() {
            Ok(tray) => TRAY.with(|slot| *slot.borrow_mut() = Some(tray)),
            Err(error) => warn!(error = %format!("{error:#}"), "No tray icon"),
        }
        let mut shown_tooltip = String::new();
        TRAY_POLL.with(|timer| {
            timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
                let actions = TRAY.with(|slot| {
                    let slot = slot.borrow();
                    let Some(tray) = slot.as_ref() else {
                        return Vec::new();
                    };
                    if let Ok(text) = tooltip.lock()
                        && *text != shown_tooltip
                    {
                        tray.set_tooltip(&text);
                        shown_tooltip = text.clone();
                    }
                    tray.actions()
                });
                for action in actions {
                    match action {
                        TrayAction::Open => {
                            if let Some(window) = weak.upgrade() {
                                show_again(&window);
                            }
                        }
                        TrayAction::RefreshPaps => {
                            let _ = commands.send(Command::RefreshPaps);
                        }
                        TrayAction::Quit => {
                            info!("Quit from the tray");
                            let _ = slint::quit_event_loop();
                        }
                    }
                }
            });
        });
    });

    slint::run_event_loop_until_quit().context("the window's event loop failed")?;
    info!("Waggle stopped");
    Ok(())
}

/// Shows the window after it was hidden to the tray. The software renderer
/// only repaints what changed since it was hidden, so the whole window is
/// marked dirty by flipping a property the background depends on
/// (docs/windows-notes.md).
fn show_again(window: &AppWindow) {
    if let Err(error) = window.show() {
        warn!(%error, "Could not show the window");
    }
    window.set_nudge(!window.get_nudge());
}

/// A plain notice from Waggle itself, outside the intel and ping wording.
fn status_notification(title: &str, body: &str) -> Notification {
    Notification {
        title: title.to_owned(),
        body: body.to_owned(),
        category: "status",
        urgency: Urgency::Low,
        glyph: "",
        icon: "",
        image: None,
        replace_tag: None,
        click: None,
        actions: Vec::new(),
        redacted: false,
    }
}

/// Logs to a daily file in the data folder, keeping a week, and to stderr.
fn init_logging(paths: &AppPaths) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let filter = EnvFilter::try_from_env("WAGGLE_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let file = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("waggle")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&paths.log_dir)
        .ok();
    let (file_layer, guard) = match file {
        Some(file) => {
            let (writer, guard) = tracing_appender::non_blocking(file);
            (
                Some(fmt::layer().with_writer(writer).with_ansi(false)),
                Some(guard),
            )
        }
        None => (None, None),
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();
    guard
}
