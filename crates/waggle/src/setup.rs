//! The setup view's behaviour: adding characters through EVE's login,
//! importing from RIFT, and saving settings.
//!
//! Runs on the UI thread except for the EVE login, which waits on the
//! browser and so runs on a thread of its own and reports back.

use std::{cell::RefCell, rc::Rc, time::Duration};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use secrecy::SecretString;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};
use tracing::{info, warn};
use waggle_core::{
    config::PrivacyMode,
    esi::{LoopbackCallback, SHIP_SCOPE, SKILL_QUEUE_SCOPE, SKILLS_SCOPE, SsoClient},
    import,
    store::{LinkedCharacter, Store},
};

use crate::{
    AppWindow, CharacterItem,
    config::{Config, DIRECTORBOT, JABBER_DOMAIN},
    engine::{self, APP},
    paths::AppPaths,
};

/// What the setup view needs from the rest of the app.
pub struct Hooks {
    /// Restart the engine with these settings.
    pub restart: Box<dyn Fn(Config)>,
    /// Tell the engine the linked characters changed.
    pub characters_changed: Box<dyn Fn()>,
}

struct Setup {
    paths: AppPaths,
    window: Weak<AppWindow>,
    draft: RefCell<Config>,
    hooks: Hooks,
}

thread_local! {
    /// The setup controller, reachable from closures the login thread posts
    /// back to the UI thread (which cannot carry an `Rc`).
    static SETUP: RefCell<Option<Rc<Setup>>> = const { RefCell::new(None) };
}

pub fn install(
    window: &AppWindow,
    paths: AppPaths,
    config: &Config,
    first_run: bool,
    hooks: Hooks,
) {
    let setup = Rc::new(Setup {
        paths,
        window: window.as_weak(),
        draft: RefCell::new(config.clone()),
        hooks,
    });
    SETUP.with(|slot| *slot.borrow_mut() = Some(Rc::clone(&setup)));
    window.set_jabber_domain(JABBER_DOMAIN.into());
    window.set_can_start_with_windows(cfg!(windows));
    window.set_data_folder(setup.paths.data_dir.display().to_string().into());
    window.set_first_run(first_run);
    setup.populate(window);
    window.set_setup_open(first_run);

    let s = Rc::clone(&setup);
    window.on_open_setup(move || {
        let Some(window) = s.window.upgrade() else {
            return;
        };
        if window.get_setup_open() {
            window.set_setup_open(false);
            return;
        }
        match Config::load(&s.paths.config_file) {
            Ok(config) => *s.draft.borrow_mut() = config,
            Err(error) => warn!(error = %format!("{error:#}"), "Could not reload the settings"),
        }
        window.set_character_status(SharedString::new());
        window.set_rift_status(SharedString::new());
        s.populate(&window);
        window.set_setup_open(true);
    });

    let s = Rc::clone(&setup);
    window.on_cancel_setup(move || {
        if let Some(window) = s.window.upgrade() {
            window.set_setup_open(false);
        }
    });

    let s = Rc::clone(&setup);
    window.on_add_character(move || s.add_character());

    let s = Rc::clone(&setup);
    window.on_remove_character(move |index| s.remove_character(index));

    let s = Rc::clone(&setup);
    window.on_import_rift(move || s.import_rift());

    let s = Rc::clone(&setup);
    window.on_add_channel(move |name| {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return;
        }
        s.draft.borrow_mut().intel.channels.entry(name).or_default();
        if let Some(window) = s.window.upgrade() {
            window.set_new_channel(SharedString::new());
            s.populate_channels(&window);
        }
    });

    let s = Rc::clone(&setup);
    window.on_remove_channel(move |index| {
        let name = s
            .draft
            .borrow()
            .intel
            .channels
            .keys()
            .nth(index as usize)
            .cloned();
        if let Some(name) = name {
            s.draft.borrow_mut().intel.channels.remove(&name);
        }
        if let Some(window) = s.window.upgrade() {
            s.populate_channels(&window);
        }
    });

    window.on_test_alert(|| {
        use waggle_core::notify::{Notification, NotificationSink, Urgency};
        // The real intel shape and sound, so this is what a hostile sounds like.
        let sample = Notification {
            title: "Test: hostile 1 jump from your pilot".to_owned(),
            body: "This is what an intel alert looks and sounds like.".to_owned(),
            category: "intel",
            urgency: Urgency::High,
            glyph: "",
            icon: "",
            image: None,
            replace_tag: None,
            click: None,
            actions: Vec::new(),
            redacted: false,
        };
        if let Err(error) = crate::toast::Desktop::detect().deliver(&sample) {
            warn!(error = %format!("{error:#}"), "Could not show the test alert");
        }
    });

    let s = Rc::clone(&setup);
    window.on_save_setup(move || {
        let Some(window) = s.window.upgrade() else {
            return;
        };
        match s.save(&window) {
            Ok(config) => {
                window.set_setup_open(false);
                window.set_first_run(false);
                (s.hooks.restart)(config);
            }
            Err(error) => {
                window.set_character_status(format!("Could not save: {error:#}").into());
            }
        }
    });
}

impl Setup {
    /// Fills the form from the draft settings.
    fn populate(&self, window: &AppWindow) {
        let draft = self.draft.borrow();
        let user = draft
            .jabber
            .jid
            .as_deref()
            .and_then(|jid| jid.split_once('@'))
            .map(|(local, _)| local.to_owned())
            .unwrap_or_default();
        let saved = draft.jabber.jid.as_deref().is_some_and(|jid| {
            matches!(engine::secrets().lookup_jabber_password(jid), Ok(Some(_)))
        });
        window.set_jabber_user(user.into());
        window.set_jabber_password(SharedString::new());
        window.set_jabber_password_saved(saved);
        window.set_max_jumps(i32::from(draft.intel.max_jumps));
        window.set_only_undocked(draft.intel.only_undocked);
        window.set_notifications(draft.notifications.enabled);
        window.set_privacy(match draft.notifications.privacy {
            PrivacyMode::Off => 0,
            PrivacyMode::Locked => 1,
            PrivacyMode::Always => 2,
        });
        window.set_start_with_windows(startup::enabled());
        window
            .set_rift_available(import::default_rift_settings().is_some_and(|path| path.exists()));
        drop(draft);
        self.populate_channels(window);
        self.populate_characters(window);
    }

    fn populate_channels(&self, window: &AppWindow) {
        let rows = self
            .draft
            .borrow()
            .intel
            .channels
            .iter()
            .map(|(name, regions)| CharacterItem {
                name: name.clone().into(),
                detail: regions.join(", ").into(),
            })
            .collect::<Vec<_>>();
        window.set_setup_channels(ModelRc::from(Rc::new(VecModel::from(rows))));
    }

    fn populate_characters(&self, window: &AppWindow) {
        let rows = self
            .linked()
            .unwrap_or_default()
            .into_iter()
            .map(|character| CharacterItem {
                name: character.name.into(),
                detail: format!("added {}", character.authorized_at.format("%Y-%m-%d")).into(),
            })
            .collect::<Vec<_>>();
        window.set_setup_characters(ModelRc::from(Rc::new(VecModel::from(rows))));
    }

    fn linked(&self) -> Result<Vec<LinkedCharacter>> {
        Store::open(&self.paths.database_file)?.linked_characters()
    }

    fn add_character(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        window.set_adding_character(true);
        window.set_character_status("Opening EVE's login in your browser…".into());
        let paths = self.paths.clone();
        let config = self.draft.borrow().clone();
        let weak = self.window.clone();
        std::thread::spawn(move || {
            let outcome = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("could not start the login")
                .and_then(|runtime| runtime.block_on(link_character(&paths, &config)));
            let message = match &outcome {
                Ok(name) => {
                    info!(character = name, "Linked a character");
                    format!("Added {name}.")
                }
                Err(error) => {
                    warn!(error = %format!("{error:#}"), "Could not link a character");
                    format!("Not added: {}", explain_login_failure(error))
                }
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak.upgrade() {
                    window.set_adding_character(false);
                    window.set_character_status(message.into());
                    SETUP.with(|slot| {
                        if let Some(setup) = slot.borrow().as_ref() {
                            setup.populate_characters(&window);
                            (setup.hooks.characters_changed)();
                        }
                    });
                }
            });
        });
    }

    fn remove_character(&self, index: i32) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let result = (|| -> Result<String> {
            let store = Store::open(&self.paths.database_file)?;
            let character = store
                .linked_characters()?
                .into_iter()
                .nth(index as usize)
                .context("that character is no longer linked")?;
            engine::secrets().delete(character.id)?;
            store.delete_character(character.id)?;
            Ok(character.name)
        })();
        match result {
            Ok(name) => window.set_character_status(format!("Removed {name}.").into()),
            Err(error) => {
                window.set_character_status(format!("Could not remove: {error:#}").into())
            }
        }
        self.populate_characters(&window);
        (self.hooks.characters_changed)();
    }

    fn import_rift(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let Some(settings) = import::default_rift_settings() else {
            return;
        };
        let secrets = engine::secrets();
        let result = import::from_rift(
            &settings,
            &self.paths.jump_bridges_file,
            Some(JABBER_DOMAIN),
            &*secrets,
            false,
        );
        match result {
            Ok(imported) => {
                let mut draft = self.draft.borrow_mut();
                let channels = imported.channels.len();
                for (name, regions) in imported.channels {
                    draft.intel.channels.insert(name, regions);
                }
                if let Some(jid) = imported.jabber_jid {
                    draft.jabber.jid = Some(jid);
                    draft
                        .jabber
                        .directorbot
                        .get_or_insert_with(|| DIRECTORBOT.to_owned());
                }
                drop(draft);
                let mut summary = format!(
                    "Imported {channels} intel channel{}, {} jump bridge{}{}.",
                    if channels == 1 { "" } else { "s" },
                    imported.bridges,
                    if imported.bridges == 1 { "" } else { "s" },
                    if imported.jabber_password_stored {
                        " and your Jabber login"
                    } else {
                        ""
                    },
                );
                for note in imported.notes {
                    summary.push(' ');
                    summary.push_str(&note);
                }
                summary.push_str(" Press Save to use it.");
                window.set_rift_status(summary.into());
                self.populate(&window);
            }
            Err(error) => window.set_rift_status(format!("Could not import: {error:#}").into()),
        }
    }

    /// Reads the form into the draft, stores the password, writes the file.
    fn save(&self, window: &AppWindow) -> Result<Config> {
        let mut draft = self.draft.borrow_mut();
        let user = window.get_jabber_user().trim().to_owned();
        if user.contains('@') || user.contains(' ') {
            bail!("the Jabber username is just the part before @{JABBER_DOMAIN}");
        }
        if user.is_empty() {
            draft.jabber.jid = None;
        } else {
            let jid = format!("{user}@{JABBER_DOMAIN}");
            let password = window.get_jabber_password().to_string();
            if !password.is_empty() {
                engine::secrets()
                    .store_jabber_password(&jid, &SecretString::from(password))
                    .context("could not keep the Jabber password")?;
            }
            draft.jabber.jid = Some(jid);
            draft
                .jabber
                .directorbot
                .get_or_insert_with(|| DIRECTORBOT.to_owned());
        }
        draft.intel.max_jumps = u8::try_from(window.get_max_jumps().clamp(0, 20)).unwrap_or(2);
        draft.intel.only_undocked = window.get_only_undocked();
        draft.notifications.enabled = window.get_notifications();
        draft.notifications.privacy = match window.get_privacy() {
            1 => PrivacyMode::Locked,
            2 => PrivacyMode::Always,
            _ => PrivacyMode::Off,
        };
        draft.validate()?;
        draft.save(&self.paths.config_file)?;
        if let Err(error) = startup::set(window.get_start_with_windows()) {
            warn!(error = %format!("{error:#}"), "Could not change starting with Windows");
        }
        info!("Saved settings");
        Ok(draft.clone())
    }
}

/// A login failure in words a pilot can act on, or the error itself when it
/// is not one of the expected ones.
fn explain_login_failure(error: &anyhow::Error) -> String {
    let detail = format!("{error:#}");
    if detail.contains("timed out waiting for EVE SSO") {
        "the EVE login wasn't finished within 5 minutes. Press Add character to try again."
            .to_owned()
    } else if detail.contains("could not listen for EVE SSO") {
        "another program is using port 17633, which EVE's login returns to. Close it and try again."
            .to_owned()
    } else if detail.contains("declined") {
        "the login was cancelled in the browser.".to_owned()
    } else {
        detail
    }
}

/// Runs EVE's login in the browser and links the character it returns.
async fn link_character(paths: &AppPaths, config: &Config) -> Result<String> {
    let sso = SsoClient::new(APP)?;
    let metadata = sso.metadata().await?;
    let callback = LoopbackCallback::bind(&config.esi.redirect_uri, APP.name).await?;
    // EVE replaces a character's scopes on every login, so always ask for
    // everything Waggle uses: location and online status (required), plus
    // skills, the queue and the ship for the pilots tab and mining hold.
    let authorization = sso.authorization_request(
        &metadata,
        &config.esi.client_id,
        &config.esi.redirect_uri,
        &[SKILL_QUEUE_SCOPE, SKILLS_SCOPE, SHIP_SCOPE],
    )?;
    open::that_detached(authorization.url.as_str())
        .context("could not open the browser; is one installed?")?;
    let code = callback
        .receive(&authorization.state, Duration::from_secs(5 * 60))
        .await?;
    let tokens = sso
        .exchange_code(
            &metadata,
            &config.esi.client_id,
            &code,
            &authorization.verifier,
        )
        .await?;
    let character = tokens.access.character;
    engine::secrets().store(character.id, &character.name, &tokens.refresh)?;
    Store::open(&paths.database_file)?.upsert_character(&LinkedCharacter {
        id: character.id,
        name: character.name.clone(),
        scopes: character.scopes,
        authorized_at: Utc::now(),
    })?;
    Ok(character.name)
}

/// Starting with Windows, through the per-user Run key.
mod startup {
    use anyhow::Result;

    #[cfg(windows)]
    const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

    #[cfg(windows)]
    fn reg(args: &[&str]) -> std::io::Result<std::process::ExitStatus> {
        use std::os::windows::process::CommandExt;
        // No console window flashing up for a moment.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("reg")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    }

    #[cfg(windows)]
    pub fn enabled() -> bool {
        reg(&["query", RUN_KEY, "/v", "Waggle"]).is_ok_and(|status| status.success())
    }

    #[cfg(windows)]
    pub fn set(on: bool) -> Result<()> {
        use anyhow::{Context, bail};
        if on == enabled() {
            return Ok(());
        }
        let status = if on {
            let exe = std::env::current_exe().context("could not find Waggle's own path")?;
            let value = format!("\"{}\"", exe.display());
            reg(&[
                "add", RUN_KEY, "/v", "Waggle", "/t", "REG_SZ", "/d", &value, "/f",
            ])?
        } else {
            reg(&["delete", RUN_KEY, "/v", "Waggle", "/f"])?
        };
        if !status.success() {
            bail!("reg.exe refused the change");
        }
        Ok(())
    }

    #[cfg(not(windows))]
    pub fn enabled() -> bool {
        false
    }

    #[cfg(not(windows))]
    pub fn set(_on: bool) -> Result<()> {
        Ok(())
    }
}
