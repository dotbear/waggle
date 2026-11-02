use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::Deserialize;
use waggle_core::universe::{CapitalClass, ManufacturingProductKind};
use zip::ZipArchive;

const SHIP_CATEGORY_ID: u32 = 6;
// Cargo, secure cargo, audit-log secure, and freight containers. These are
// inventory containers, not NPC loot objects or container blueprints.
const CONTAINER_GROUP_IDS: [u32; 4] = [12, 340, 448, 649];
// The rigSlot dogma effect identifies ship and structure rigs alike.
const RIG_SLOT_EFFECT_ID: u32 = 2663;

/// The capital a ship group belongs to, by SDE group ID.
///
/// Lancer dreadnoughts count as dreadnoughts and command carriers as carriers:
/// a home-defence umbrella treats them the way it treats the hulls they are
/// variants of. Freighters and jump freighters are deliberately absent.
fn capital_class(group_id: u32) -> Option<CapitalClass> {
    match group_id {
        30 => Some(CapitalClass::Titan),
        485 | 4594 => Some(CapitalClass::Dreadnought),
        547 | 5120 => Some(CapitalClass::Carrier),
        659 => Some(CapitalClass::Supercarrier),
        883 => Some(CapitalClass::Rorqual),
        1538 => Some(CapitalClass::ForceAuxiliary),
        _ => None,
    }
}

#[derive(Debug, Parser)]
#[command(about = "Generate EVEmarchy's minimal universe data from an official EVE SDE")]
struct Args {
    #[arg(long)]
    archive: PathBuf,
    #[arg(long)]
    build_number: u64,
    #[arg(long, default_value = "crates/waggle-core/assets/universe-v1.tsv")]
    output: PathBuf,
}

#[derive(Debug, Deserialize)]
struct LocalizedName {
    en: String,
}

#[derive(Debug, Deserialize)]
struct RawRegion {
    #[serde(rename = "_key")]
    id: u32,
    name: LocalizedName,
}

#[derive(Debug, Deserialize)]
struct RawSystem {
    #[serde(rename = "_key")]
    id: u32,
    #[serde(rename = "regionID")]
    region_id: u32,
    name: LocalizedName,
}

#[derive(Debug, Deserialize)]
struct RawGate {
    #[serde(rename = "solarSystemID")]
    solar_system_id: u32,
    destination: RawGateDestination,
}

#[derive(Debug, Deserialize)]
struct RawGateDestination {
    #[serde(rename = "solarSystemID")]
    solar_system_id: u32,
}

#[derive(Debug, Deserialize)]
struct RawGroup {
    #[serde(rename = "_key")]
    id: u32,
    #[serde(rename = "categoryID")]
    category_id: u32,
}

#[derive(Debug, Deserialize)]
struct RawType {
    #[serde(rename = "_key")]
    id: u32,
    #[serde(rename = "groupID")]
    group_id: u32,
    name: LocalizedName,
    published: bool,
}

#[derive(Debug, Deserialize)]
struct RawBlueprint {
    #[serde(rename = "_key")]
    id: u32,
    activities: RawActivities,
}

#[derive(Debug, Default, Deserialize)]
struct RawActivities {
    manufacturing: Option<RawActivity>,
    invention: Option<RawActivity>,
}

#[derive(Debug, Deserialize)]
struct RawActivity {
    #[serde(default)]
    products: Vec<RawProduct>,
}

#[derive(Debug, Deserialize)]
struct RawProduct {
    #[serde(rename = "typeID")]
    type_id: u32,
}

#[derive(Debug, Deserialize)]
struct RawTypeDogma {
    #[serde(rename = "_key")]
    id: u32,
    #[serde(rename = "dogmaEffects", default)]
    effects: Vec<RawEffect>,
}

#[derive(Debug, Deserialize)]
struct RawEffect {
    #[serde(rename = "effectID")]
    id: u32,
}

fn manufacturing_products(
    blueprints: &[RawBlueprint],
    dogma: &[RawTypeDogma],
) -> Result<Vec<(u32, ManufacturingProductKind)>> {
    let invented: HashSet<_> = blueprints
        .iter()
        .filter_map(|blueprint| blueprint.activities.invention.as_ref())
        .flat_map(|activity| activity.products.iter().map(|product| product.type_id))
        .collect();
    let rigs: HashSet<_> = dogma
        .iter()
        .filter(|item| {
            item.effects
                .iter()
                .any(|effect| effect.id == RIG_SLOT_EFFECT_ID)
        })
        .map(|item| item.id)
        .collect();
    let mut products = Vec::new();
    for blueprint in blueprints {
        let Some(manufacturing) = &blueprint.activities.manufacturing else {
            continue;
        };
        // Retired SDE entries can retain materials and time without a product.
        // They have no manufactured item to classify.
        if manufacturing.products.is_empty() {
            continue;
        }
        let [product] = manufacturing.products.as_slice() else {
            bail!(
                "blueprint {} must have exactly one manufacturing product",
                blueprint.id
            );
        };
        let kind = if rigs.contains(&product.type_id) {
            ManufacturingProductKind::Rig
        } else if invented.contains(&blueprint.id) {
            ManufacturingProductKind::Invented
        } else {
            ManufacturingProductKind::Other
        };
        products.push((blueprint.id, kind));
    }
    products.sort_by_key(|(id, _)| *id);
    Ok(products)
}

fn main() -> Result<()> {
    let args = Args::parse();
    let source = File::open(&args.archive)
        .with_context(|| format!("could not open SDE archive {}", args.archive.display()))?;
    let mut archive = ZipArchive::new(source).context("could not read SDE zip archive")?;

    let mut regions = read_json_lines::<RawRegion>(&mut archive, "mapRegions.jsonl")?
        .into_iter()
        .map(|region| (region.id, region.name.en))
        .collect::<Vec<_>>();
    regions.sort_by_key(|(id, _)| *id);

    let mut systems = read_json_lines::<RawSystem>(&mut archive, "mapSolarSystems.jsonl")?
        .into_iter()
        .map(|system| (system.id, system.region_id, system.name.en))
        .collect::<Vec<_>>();
    systems.sort_by_key(|(id, _, _)| *id);

    let mut gates = read_json_lines::<RawGate>(&mut archive, "mapStargates.jsonl")?
        .into_iter()
        .filter_map(|gate| {
            let first = gate.solar_system_id;
            let second = gate.destination.solar_system_id;
            (first != second).then_some(if first < second {
                (first, second)
            } else {
                (second, first)
            })
        })
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    gates.sort_unstable();

    let ship_groups = read_json_lines::<RawGroup>(&mut archive, "groups.jsonl")?
        .into_iter()
        .filter(|group| group.category_id == SHIP_CATEGORY_ID)
        .map(|group| group.id)
        .collect::<HashSet<_>>();
    let published_types = read_json_lines::<RawType>(&mut archive, "types.jsonl")?
        .into_iter()
        .filter(|item| item.published)
        .collect::<Vec<_>>();
    let mut ships = published_types
        .iter()
        .filter(|item| ship_groups.contains(&item.group_id))
        .map(|item| (item.id, item.name.en.clone()))
        .collect::<Vec<_>>();
    ships.sort_by_key(|(id, _)| *id);
    let mut capitals = published_types
        .iter()
        .filter_map(|item| capital_class(item.group_id).map(|class| (item.id, class)))
        .collect::<Vec<_>>();
    capitals.sort_by_key(|(id, _)| *id);
    let mut container_types = published_types
        .iter()
        .filter(|item| CONTAINER_GROUP_IDS.contains(&item.group_id))
        .map(|item| item.id)
        .collect::<Vec<_>>();
    container_types.sort_unstable();
    let mut item_types = published_types
        .into_iter()
        .map(|item| (item.id, item.name.en))
        .collect::<Vec<_>>();
    item_types.sort_by_key(|(id, _)| *id);

    let products = manufacturing_products(
        &read_json_lines::<RawBlueprint>(&mut archive, "blueprints.jsonl")?,
        &read_json_lines::<RawTypeDogma>(&mut archive, "typeDogma.jsonl")?,
    )?;

    validate_names(regions.iter().map(|(_, name)| name))?;
    validate_names(systems.iter().map(|(_, _, name)| name))?;
    validate_names(ships.iter().map(|(_, name)| name))?;
    validate_names(item_types.iter().map(|(_, name)| name))?;

    let known_regions = regions.iter().map(|(id, _)| *id).collect::<HashSet<_>>();
    for (id, region_id, _) in &systems {
        if !known_regions.contains(region_id) {
            bail!("system {id} references missing region {region_id}");
        }
    }

    write_atomically(
        &args.output,
        args.build_number,
        &GeneratedUniverse {
            regions: &regions,
            systems: &systems,
            gates: &gates,
            ships: &ships,
            item_types: &item_types,
            products: &products,
            container_types: &container_types,
            capitals: &capitals,
        },
    )?;
    println!(
        "Wrote {} regions, {} systems, {} gates, {} ship types, and {} published item types to {}",
        regions.len(),
        systems.len(),
        gates.len(),
        ships.len(),
        item_types.len(),
        args.output.display()
    );

    Ok(())
}

fn read_json_lines<T>(archive: &mut ZipArchive<File>, filename: &str) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let source = archive
        .by_name(filename)
        .with_context(|| format!("SDE archive does not contain {filename}"))?;
    BufReader::new(source)
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let line =
                line.with_context(|| format!("could not read {filename} line {}", index + 1))?;
            serde_json::from_str(&line)
                .with_context(|| format!("could not parse {filename} line {}", index + 1))
        })
        .collect()
}

fn validate_names<'a>(names: impl Iterator<Item = &'a String>) -> Result<()> {
    for name in names {
        if name.contains(['\t', '\n', '\r']) {
            bail!("name {name:?} cannot be represented in the universe data format");
        }
    }
    Ok(())
}

struct GeneratedUniverse<'a> {
    regions: &'a [(u32, String)],
    systems: &'a [(u32, u32, String)],
    gates: &'a [(u32, u32)],
    ships: &'a [(u32, String)],
    item_types: &'a [(u32, String)],
    products: &'a [(u32, ManufacturingProductKind)],
    container_types: &'a [u32],
    capitals: &'a [(u32, CapitalClass)],
}

fn write_atomically(output: &Path, build_number: u64, data: &GeneratedUniverse<'_>) -> Result<()> {
    let parent = output
        .parent()
        .context("universe output path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = output.with_extension("tsv.tmp");
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    let mut writer = BufWriter::new(file);

    writeln!(writer, "B\t{build_number}")?;
    for (id, name) in data.regions {
        writeln!(writer, "R\t{id}\t{name}")?;
    }
    for (id, region_id, name) in data.systems {
        writeln!(writer, "S\t{id}\t{region_id}\t{name}")?;
    }
    for (first, second) in data.gates {
        writeln!(writer, "G\t{first}\t{second}")?;
    }
    for (id, name) in data.ships {
        writeln!(writer, "T\t{id}\t{name}")?;
    }
    for (id, name) in data.item_types {
        writeln!(writer, "I\t{id}\t{name}")?;
    }
    for (id, kind) in data.products {
        writeln!(writer, "P\t{id}\t{}", kind.label())?;
    }
    for id in data.container_types {
        writeln!(writer, "C\t{id}")?;
    }
    for (id, class) in data.capitals {
        writeln!(writer, "K\t{id}\t{}", class.label())?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    fs::rename(&temporary, output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_follows_invention_outputs_and_the_products_rig_slot() {
        let blueprints: Vec<RawBlueprint> = serde_json::from_value(serde_json::json!([
            {"_key": 1, "activities": {
                "manufacturing": {"products": [{"typeID": 101}]},
                "invention": {"products": [{"typeID": 2}, {"typeID": 3}]}
            }},
            {"_key": 2, "activities": {"manufacturing": {"products": [{"typeID": 102}]}}},
            {"_key": 3, "activities": {"manufacturing": {"products": [{"typeID": 103}]}}},
            {"_key": 4, "activities": {"manufacturing": {"products": [{"typeID": 104}]}}},
            {"_key": 5, "activities": {"manufacturing": {}}},
            {"_key": 6, "activities": {}}
        ]))
        .unwrap();
        let dogma: Vec<RawTypeDogma> = serde_json::from_value(serde_json::json!([
            {"_key": 103, "dogmaEffects": [{"effectID": 2663}]},
            {"_key": 104, "dogmaEffects": [{"effectID": 2663}]}
        ]))
        .unwrap();
        assert_eq!(
            manufacturing_products(&blueprints, &dogma).unwrap(),
            vec![
                (1, ManufacturingProductKind::Other),
                (2, ManufacturingProductKind::Invented),
                (3, ManufacturingProductKind::Rig),
                (4, ManufacturingProductKind::Rig),
            ]
        );
    }
}
