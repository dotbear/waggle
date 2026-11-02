//! Draws Waggle's real window offscreen, one image per tab, filled with
//! sample data, and saves them side by side as a PNG. Nothing appears on the
//! screen, so the design can be checked without a VM round trip.
//!
//!     cargo run -p waggle --example screenshot -- out.png

use std::{rc::Rc, time::Duration};

use chrono::{TimeDelta, Utc};
use slint::{
    ComponentHandle, ModelRc, PhysicalSize, Rgb8Pixel, VecModel,
    platform::{
        Platform, WindowAdapter,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};
use waggle::{AppWindow, MiningInfo, OreRow, view};
use waggle_core::{
    directorbot::{PapSnapshot, PapStats},
    esi::{CharacterPresence, ShipPresence},
    skills::{SkillQueue, SkillQueueEntry},
    store::{IntelAlert, JabberPing},
};

struct Offscreen(Rc<MinimalSoftwareWindow>);

impl Platform for Offscreen {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

const WIDTH: u32 = 380;
const HEIGHT: u32 = 600;

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "waggle-screens.png".to_owned());
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(Offscreen(window.clone()))).unwrap();
    let ui = AppWindow::new().unwrap();
    view::apply(&ui, &sample());
    ui.set_mining(sample_mining());
    ui.show().unwrap();
    window.set_size(PhysicalSize::new(WIDTH, HEIGHT));

    let mut frames = Vec::new();
    for tab in 0..4 {
        ui.set_tab(tab);
        std::thread::sleep(Duration::from_millis(10));
        slint::platform::update_timers_and_animations();
        let mut buffer = vec![Rgb8Pixel::default(); (WIDTH * HEIGHT) as usize];
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut buffer, WIDTH as usize);
        });
        frames.push(buffer);
    }

    let gap = 12;
    let total_width = WIDTH * 4 + gap * 3;
    let mut image = vec![40u8; (total_width * HEIGHT * 3) as usize];
    for (index, frame) in frames.iter().enumerate() {
        let x0 = index as u32 * (WIDTH + gap);
        for y in 0..HEIGHT {
            for x in 0..WIDTH {
                let pixel = frame[(y * WIDTH + x) as usize];
                let at = ((y * total_width + x0 + x) * 3) as usize;
                image[at..at + 3].copy_from_slice(&[pixel.r, pixel.g, pixel.b]);
            }
        }
    }
    let file = std::fs::File::create(&out).unwrap();
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), total_width, HEIGHT);
    encoder.set_color(png::ColorType::Rgb);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&image)
        .unwrap();
    println!("{out}");
}

fn sample() -> view::ViewModel {
    let now = Utc::now();
    let ago = |seconds: i64| now - TimeDelta::seconds(seconds);
    view::ViewModel {
        now,
        paps: Some(PapSnapshot {
            updated_at: now,
            month: PapStats {
                strategic: 12,
                peacetime: 13,
            },
            last_30_days: PapStats {
                strategic: 20,
                peacetime: 24,
            },
            last_90_days: PapStats {
                strategic: 55,
                peacetime: 108,
            },
            last_strat_at: None,
            last_strat_character: None,
        }),
        jabber_text: "Directorbot connected".to_owned(),
        jabber_ok: true,
        setup_hint: String::new(),
        status: "3 of 4 intel channels · 2 pilots".to_owned(),
        intel: vec![
            alert(
                1,
                "1DQ1-A",
                1,
                "1DQ1-A Sabre + 3 on gate",
                "Scout One",
                "Example Pilot",
                ago(25),
                false,
            ),
            alert(
                2,
                "T5ZI-S",
                2,
                "T5ZI-S 5x red, ishtars",
                "Scout Two",
                "Example Pilot",
                ago(140),
                true,
            ),
            alert(
                3,
                "Q-JQSG",
                4,
                "q-jqsg loki nv",
                "Scout Three",
                "Second Pilot",
                ago(600),
                false,
            ),
        ],
        pings: vec![
            ping(
                1,
                Some("Home Defence"),
                "Synthetic Boss",
                "1DQ1-A",
                "Strategic",
                "Op 1",
                ago(90),
                false,
            ),
            ping(
                2,
                Some("Mining Op"),
                "Another Boss",
                "1DQ1-A",
                "Peacetime",
                "",
                ago(3600),
                false,
            ),
            ping(3, None, "", "", "", "", ago(7200), true),
        ],
        pilots: vec![
            pilot(
                "Example Pilot",
                Some(true),
                "1DQ1-A",
                Some(false),
                "Hulk",
                Some(("Exhumers", 5, 135)),
            ),
            pilot(
                "Second Pilot",
                Some(false),
                "Jita",
                Some(true),
                "Orca",
                None,
            ),
        ],
        mining: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn alert(
    id: u64,
    system: &str,
    distance: u16,
    message: &str,
    author: &str,
    pilot: &str,
    at: chrono::DateTime<Utc>,
    via_bridge: bool,
) -> IntelAlert {
    IntelAlert {
        id,
        occurred_at: at,
        channel: "intel.example".to_owned(),
        author: author.to_owned(),
        message: message.to_owned(),
        system_id: 0,
        system_name: system.to_owned(),
        distance,
        character_id: 1,
        character_name: pilot.to_owned(),
        stale_presence: false,
        via_bridge,
    }
}

#[allow(clippy::too_many_arguments)]
fn ping(
    id: u64,
    fleet: Option<&str>,
    fc: &str,
    formup: &str,
    pap: &str,
    comms: &str,
    at: chrono::DateTime<Utc>,
    muted: bool,
) -> JabberPing {
    let some = |value: &str| (!value.is_empty()).then(|| value.to_owned());
    JabberPing {
        id,
        occurred_at: at,
        received_at: at,
        from: "directorbot@example.invalid".to_owned(),
        source: None,
        sender: None,
        target: muted.then(|| "newsletter".to_owned()),
        summary: "Recruitment newsletter for the opt-in group".to_owned(),
        description: String::new(),
        fc: some(fc),
        fleet: fleet.map(str::to_owned),
        formup: some(formup),
        pap_type: some(pap),
        comms: some(comms),
        comms_link: None,
        comms_target: None,
        doctrine: None,
        body: String::new(),
        fc_portrait: None,
        muted,
        fleet_index: 1,
        fleet_count: 1,
    }
}

fn pilot(
    name: &str,
    online: Option<bool>,
    system: &str,
    docked: Option<bool>,
    ship: &str,
    training: Option<(&str, u8, i64)>,
) -> CharacterPresence {
    let now = Utc::now();
    CharacterPresence {
        character_id: 1,
        character_name: name.to_owned(),
        online,
        solar_system_id: None,
        solar_system_name: Some(system.to_owned()),
        region_name: None,
        portrait: None,
        docked,
        observed_at: Some(now),
        error: None,
        skill_queue: Some(SkillQueue {
            training: training.map(|(skill, level, minutes)| SkillQueueEntry {
                character_id: 1,
                character_name: name.to_owned(),
                skill_id: 1,
                skill_name: skill.to_owned(),
                level,
                position: 0,
                start_at: Some(now),
                finish_at: Some(now + TimeDelta::minutes(minutes)),
                level_start_sp: None,
                level_end_sp: None,
                training_start_sp: None,
            }),
            next: Vec::new(),
            remaining: 0,
            ends_at: None,
            paused: false,
        }),
        skill_queue_error: None,
        ship: Some(ShipPresence {
            type_id: 0,
            item_id: 0,
            name: ship.to_owned(),
            type_name: Some(ship.to_owned()),
            mining_hold: None,
        }),
        ship_error: None,
        industry_slots: None,
    }
}

fn sample_mining() -> MiningInfo {
    MiningInfo {
        active: true,
        title: "SESSION".into(),
        total: "48,210 m³".into(),
        rate: "21,400 m³".into(),
        hold_percent: 0.72,
        hold_text: "Hold 21,600 / 30,000 m³".into(),
        ores: ModelRc::from(Rc::new(VecModel::from(vec![
            OreRow {
                name: "Bitumens".into(),
                amount: "31,050 m³".into(),
            },
            OreRow {
                name: "Zeolites".into(),
                amount: "12,400 m³".into(),
            },
            OreRow {
                name: "Compressed Bitumens".into(),
                amount: "4,760 m³".into(),
            },
        ]))),
    }
}
