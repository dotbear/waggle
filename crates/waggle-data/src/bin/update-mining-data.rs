//! Reproducible, exact-name mining volumes and mining-hold sizes from CCP's
//! official JSONL SDE.
use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
};
#[derive(Parser)]
struct Args {
    #[arg(long)]
    archive: PathBuf,
    #[arg(long)]
    build_number: u64,
    #[arg(
        long,
        default_value = "crates/waggle-core/assets/mining-volumes-v1.tsv"
    )]
    output: PathBuf,
    #[arg(long, default_value = "crates/waggle-core/assets/mining-holds-v1.tsv")]
    holds_output: PathBuf,
}
/// The dogma attribute EVE stores a ship's mining (formerly ore) hold in.
const MINING_HOLD: u64 = 1556;
/// Dogma's post-percent: `value * (1 + bonus / 100)`.
const POST_PERCENT: u64 = 6;
fn jsonl(zip: &mut zip::ZipArchive<File>, name: &str) -> Result<Vec<Value>> {
    BufReader::new(zip.by_name(name)?)
        .lines()
        .map(|l| Ok(serde_json::from_str::<Value>(&l?)?))
        .collect()
}
fn text(value: &Value) -> String {
    let raw = value["bonusText"]["en"].as_str().unwrap_or_default();
    let mut plain = String::new();
    let mut in_tag = false;
    for c in raw.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => plain.push(c),
            _ => {}
        }
    }
    plain.to_lowercase()
}
/// Every published ship with a mining hold, its base size, and the bonuses
/// that grow it.
///
/// Dogma says which attribute grows the hold but not which skill multiplies
/// it; that lives only in the ship's traits. So each dogma bonus is matched to
/// the trait line with the same value that talks about hold capacity, and the
/// run fails rather than guess when a ship's bonus has no such line — a new
/// hull with an unexplained bonus would otherwise get a silently wrong hold.
fn holds(zip: &mut zip::ZipArchive<File>, ships: &HashSet<u64>) -> Result<Vec<String>> {
    let effects: HashMap<u64, u64> = jsonl(zip, "dogmaEffects.jsonl")?
        .iter()
        .flat_map(|e| {
            let id = e["_key"].as_u64().unwrap_or(0);
            e["modifierInfo"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| {
                    m["modifiedAttributeID"] == MINING_HOLD
                        && m["domain"] == "shipID"
                        && m["func"] == "ItemModifier"
                })
                .map(move |m| (id, m.clone()))
                .collect::<Vec<_>>()
        })
        .map(|(id, m)| {
            ensure!(
                m["operation"] == POST_PERCENT,
                "mining-hold effect {id} is not a percentage bonus"
            );
            Ok((id, m["modifyingAttributeID"].as_u64().context("modifier")?))
        })
        .collect::<Result<_>>()?;
    let traits: HashMap<u64, Value> = jsonl(zip, "typeBonus.jsonl")?
        .into_iter()
        .map(|t| (t["_key"].as_u64().unwrap_or(0), t))
        .collect();
    let mut rows = BTreeMap::new();
    for dogma in jsonl(zip, "typeDogma.jsonl")? {
        let id = dogma["_key"].as_u64().unwrap_or(0);
        if !ships.contains(&id) {
            continue;
        }
        let attributes: HashMap<u64, f64> = dogma["dogmaAttributes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| Some((a["attributeID"].as_u64()?, a["value"].as_f64()?)))
            .collect();
        let Some(base) = attributes.get(&MINING_HOLD).copied().filter(|v| *v > 0.0) else {
            continue;
        };
        let mut bonuses = vec![];
        for effect in dogma["dogmaEffects"].as_array().into_iter().flatten() {
            let Some(attribute) = effects.get(&effect["effectID"].as_u64().unwrap_or(0)) else {
                continue;
            };
            let Some(bonus) = attributes.get(attribute).copied().filter(|v| *v != 0.0) else {
                continue;
            };
            let about_hold = |b: &&Value| {
                b["bonus"].as_f64() == Some(bonus) && text(b).contains("hold capacity")
            };
            let ship_traits = &traits[&id];
            let skill = ship_traits["types"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|s| {
                    s["_value"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|b| about_hold(&b))
                })
                .and_then(|s| s["_key"].as_u64());
            let role = ship_traits["roleBonuses"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|b| about_hold(&b));
            match (skill, role) {
                (Some(skill), _) => bonuses.push(format!("{skill}:{bonus}")),
                (None, true) => bonuses.push(format!("0:{bonus}")),
                (None, false) => {
                    bail!("ship {id} has a {bonus}% mining-hold bonus no trait explains")
                }
            }
        }
        rows.insert(id, (base, bonuses));
    }
    Ok(rows
        .into_iter()
        .map(|(id, (base, bonuses))| {
            let bonuses = if bonuses.is_empty() {
                "-".to_owned()
            } else {
                bonuses.join(";")
            };
            format!("{id}\t{}\t{bonuses}", (base * 1_000_000.0).round() as i64)
        })
        .collect())
}
fn main() -> Result<()> {
    let args = Args::parse();
    let mut zip = zip::ZipArchive::new(File::open(args.archive)?)?;
    let groups = BufReader::new(zip.by_name("groups.jsonl")?)
        .lines()
        .map(|l| Ok(serde_json::from_str::<Value>(&l?)?))
        .collect::<Result<Vec<_>>>()?;
    // Category 25: asteroids/ice. Group 711: harvestable gas clouds.
    // Category 6: ships.
    let ship_groups = groups
        .iter()
        .filter(|g| g["categoryID"] == 6)
        .map(|g| g["_key"].as_u64().unwrap())
        .collect::<HashSet<_>>();
    let groups = groups
        .into_iter()
        .filter(|g| g["categoryID"] == 25 || g["_key"] == 711)
        .map(|g| g["_key"].as_u64().unwrap())
        .collect::<HashSet<_>>();
    let mut ships = HashMap::new();
    let mut rows = vec![];
    for line in BufReader::new(zip.by_name("types.jsonl")?).lines() {
        let v: Value = serde_json::from_str(&line?)?;
        if v["published"] == true && ship_groups.contains(&v["groupID"].as_u64().unwrap_or(0)) {
            let name = v["name"]["en"].as_str().unwrap_or_default();
            ensure!(!name.contains(['\t', '\n', '\r']), "invalid ship name");
            ships.insert(v["_key"].as_u64().unwrap(), name.to_owned());
        }
        if v["published"] != true || !groups.contains(&v["groupID"].as_u64().unwrap_or(0)) {
            continue;
        }
        let name = v["name"]["en"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing English ore name"))?;
        let volume = v["volume"]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("missing volume for {name}"))?;
        let scaled = volume * 1_000_000.0;
        ensure!(
            scaled.is_finite()
                && scaled > 0.0
                && scaled < i64::MAX as f64
                && (scaled - scaled.round()).abs() < 1e-6,
            "volume precision for {name}"
        );
        ensure!(!name.contains(['\t', '\n', '\r']), "invalid ore name");
        rows.push((
            v["_key"].as_u64().unwrap(),
            scaled.round() as i64,
            name.to_owned(),
        ));
    }
    rows.sort();
    let temp = args.output.with_extension("tsv.tmp");
    let mut f = File::create(&temp)?;
    writeln!(
        f,
        "# CCP SDE build {}; volume in millionths of m3; exact English names",
        args.build_number
    )?;
    for (id, volume, name) in &rows {
        writeln!(f, "{id}\t{volume}\t{name}")?;
    }
    f.sync_all()?;
    std::fs::rename(temp, args.output)?;
    println!("Wrote {} mining volumes", rows.len());
    let holds = holds(&mut zip, &ships.keys().copied().collect())?;
    let temp = args.holds_output.with_extension("tsv.tmp");
    let mut f = File::create(&temp)?;
    writeln!(
        f,
        "# CCP SDE build {}; base mining hold in millionths of m3; \
         bonuses are skill:percent per level (skill 0 is a flat role bonus)",
        args.build_number
    )?;
    for row in &holds {
        let id: u64 = row.split('\t').next().unwrap().parse()?;
        writeln!(f, "{row}\t{}", ships[&id])?;
    }
    f.sync_all()?;
    std::fs::rename(temp, args.holds_output)?;
    println!("Wrote {} mining holds", holds.len());
    Ok(())
}
