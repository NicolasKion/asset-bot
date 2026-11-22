//! Turns ESI's asset tree into flat rows: how much of each type sits at which
//! station or structure, in which corporation hangar and inside which containers.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::esi::{Asset, LocationType};

/// Type id of a corporation office. Corporation hangars hang off it.
const OFFICE_TYPE: i64 = 27;

/// Asset trees are a few levels deep. This only stops a broken tree from looping.
const MAX_DEPTH: usize = 16;

/// Separates container names in [`Holding::containers`]. Item names cannot contain it.
const SEPARATOR: char = '\u{1f}';

/// Ids of NPC stations and of solar systems, whose names anyone can look up.
const STATIONS: Range<i64> = 60_000_000..64_000_000;
const SOLAR_SYSTEMS: Range<i64> = 30_000_000..40_000_000;

/// A stack of one type, with where it is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Holding {
    /// The station or structure, or the solar system for things in space.
    pub location_id: i64,
    /// Corporation hangar 1 to 7 the stack is in, directly or in a container.
    pub division: Option<u8>,
    /// Lowercase names of the containers and ships around the stack, see [`container_key`].
    pub containers: String,
    /// Fitted to a ship or loaded in one, so already in use.
    pub in_ship: bool,
    pub type_id: i64,
    pub quantity: i64,
}

/// An assembled ship with something fitted to it or loaded in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ship {
    pub item_id: i64,
    pub type_id: i64,
    /// Where the ship is docked. See [`Holding`] for the fields.
    pub location_id: i64,
    pub division: Option<u8>,
    pub containers: String,
    pub contents: Vec<ShipItem>,
}

/// A stack of one type on a ship.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ShipItem {
    pub type_id: i64,
    /// Sits in a module, rig or subsystem slot. Loaded charges do too.
    pub fitted: bool,
    pub quantity: i64,
}

/// The asset tree, flattened.
#[derive(Debug, Clone, Default)]
pub struct Flattened {
    pub holdings: Vec<Holding>,
    pub ships: Vec<Ship>,
}

/// Whether a location id is one whose name `/universe/names` knows.
pub fn has_public_name(location_id: i64) -> bool {
    STATIONS.contains(&location_id) || SOLAR_SYSTEMS.contains(&location_id)
}

/// Whether a location id is a solar system, so the things there float in space.
pub fn in_space(location_id: i64) -> bool {
    SOLAR_SYSTEMS.contains(&location_id)
}

/// The text to look for in [`Holding::containers`] to find one container.
pub fn container_key(name: &str) -> String {
    format!("{SEPARATOR}{}{SEPARATOR}", name.trim().to_lowercase())
}

/// The items worth asking ESI for a name: everything that holds other items,
/// which are containers, ships and structures.
pub fn nameable(assets: &[Asset]) -> Vec<i64> {
    let parents: HashSet<i64> = assets.iter().map(|asset| asset.location_id).collect();
    assets
        .iter()
        .filter(|asset| {
            asset.is_singleton && asset.type_id != OFFICE_TYPE && parents.contains(&asset.item_id)
        })
        .map(|asset| asset.item_id)
        .collect()
}

/// Flattens the asset tree. `names` are the player-given names of items.
///
/// Fitted modules and ship cargo are at the station the ship is in, marked
/// as [`Holding::in_ship`]. Things in asset safety are left out, since they are not usable there.
pub fn flatten(assets: &[Asset], names: &HashMap<i64, String>) -> Flattened {
    let by_id: HashMap<i64, &Asset> = assets.iter().map(|asset| (asset.item_id, asset)).collect();
    let mut totals: HashMap<Holding, i64> = HashMap::new();
    // Ship item id to what is on it.
    let mut aboard: HashMap<i64, HashMap<(i64, bool), i64>> = HashMap::new();

    for asset in assets {
        if asset.type_id == OFFICE_TYPE {
            continue;
        }
        let Some(place) = locate(asset, &by_id, names) else {
            continue;
        };
        if let Some(ship) = place.ship {
            *aboard
                .entry(ship)
                .or_default()
                .entry((asset.type_id, place.fitted))
                .or_default() += asset.quantity;
        }
        let key = Holding {
            location_id: place.location_id,
            division: place.division,
            containers: place.containers,
            in_ship: place.in_ship,
            type_id: asset.type_id,
            quantity: 0,
        };
        *totals.entry(key).or_default() += asset.quantity;
    }

    let holdings = totals
        .into_iter()
        .map(|(key, quantity)| Holding { quantity, ..key })
        .collect();
    let ships = aboard
        .into_iter()
        .filter_map(|(item_id, contents)| {
            let hull = by_id.get(&item_id)?;
            let place = locate(hull, &by_id, names)?;
            Some(Ship {
                item_id,
                type_id: hull.type_id,
                location_id: place.location_id,
                division: place.division,
                containers: place.containers,
                contents: contents
                    .into_iter()
                    .map(|((type_id, fitted), quantity)| ShipItem {
                        type_id,
                        fitted,
                        quantity,
                    })
                    .collect(),
            })
        })
        .collect();
    Flattened { holdings, ships }
}

struct Place {
    location_id: i64,
    division: Option<u8>,
    containers: String,
    in_ship: bool,
    /// The ship the asset is on, if any: the nearest one around it.
    ship: Option<i64>,
    /// The asset itself sits in a fitting slot of that ship.
    fitted: bool,
}

/// Walks from an asset up to the station, structure or solar system it is in.
fn locate(
    asset: &Asset,
    by_id: &HashMap<i64, &Asset>,
    names: &HashMap<i64, String>,
) -> Option<Place> {
    let mut current = asset;
    let mut division = None;
    let mut containers = String::new();
    let mut in_ship = false;
    let mut ship = None;

    for _ in 0..MAX_DEPTH {
        if current.location_flag == "AssetSafety" {
            return None;
        }
        division = division.or_else(|| corporation_hangar(&current.location_flag));

        let parent = by_id.get(&current.location_id);
        // Inside another item, anything that is not plain storage is a slot,
        // bay or hold of a ship.
        if let Some(parent) = parent
            && !is_storage(&current.location_flag)
        {
            in_ship = true;
            ship = ship.or(Some(parent.item_id));
        }

        let location_id = match parent {
            // Something in space that holds items is a structure the owner
            // owns or a ship. Its item id is the location others see too.
            Some(parent) if parent.location_type == LocationType::SolarSystem => parent.item_id,
            Some(parent) => {
                if let Some(name) = names.get(&parent.item_id) {
                    containers.push_str(&name.trim().to_lowercase());
                    containers.push(SEPARATOR);
                }
                current = parent;
                continue;
            }
            None => current.location_id,
        };
        if !containers.is_empty() {
            containers.insert(0, SEPARATOR);
        }
        return Some(Place {
            location_id,
            division,
            containers,
            in_ship,
            ship,
            fitted: ship == Some(asset.location_id) && is_slot(&asset.location_flag),
        });
    }
    None
}

/// Whether a location flag is a module, rig or subsystem slot.
fn is_slot(flag: &str) -> bool {
    ["HiSlot", "MedSlot", "LoSlot", "RigSlot", "SubSystemSlot"]
        .iter()
        .any(|slot| flag.starts_with(slot))
}

/// Whether a location flag means lying in a hangar, container or office,
/// as opposed to being fitted to or loaded in a ship.
fn is_storage(flag: &str) -> bool {
    matches!(
        flag,
        "Hangar"
            | "HangarAll"
            | "Unlocked"
            | "Locked"
            | "OfficeFolder"
            | "Impounded"
            | "Deliveries"
            | "CorpDeliveries"
            | "CapsuleerDeliveries"
            | "CorporationGoalDeliveries"
            | "AutoFit"
    ) || corporation_hangar(flag).is_some()
}

/// The hangar number of a `CorpSAG1` to `CorpSAG7` location flag.
fn corporation_hangar(flag: &str) -> Option<u8> {
    flag.strip_prefix("CorpSAG")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const JITA: i64 = 60_003_760;
    const STRUCTURE: i64 = 1_030_000_000_001;

    fn asset(item_id: i64, type_id: i64, quantity: i64, location_id: i64, flag: &str) -> Asset {
        let location_type = if STATIONS.contains(&location_id) {
            LocationType::Station
        } else if SOLAR_SYSTEMS.contains(&location_id) {
            LocationType::SolarSystem
        } else {
            LocationType::Item
        };
        Asset {
            item_id,
            type_id,
            quantity,
            location_id,
            location_flag: flag.to_owned(),
            location_type,
            is_singleton: quantity == 1,
        }
    }

    fn total(holdings: &[Holding], location_id: i64, type_id: i64) -> i64 {
        holdings
            .iter()
            .filter(|h| h.location_id == location_id && h.type_id == type_id)
            .map(|h| h.quantity)
            .sum()
    }

    #[test]
    fn items_in_containers_and_ships_count_for_the_station() {
        let assets = [
            asset(1, 34, 500, JITA, "Hangar"),
            asset(2, 3293, 1, JITA, "Hangar"),
            asset(3, 34, 200, 2, "Unlocked"),
            asset(4, 16227, 1, JITA, "Hangar"),
            asset(5, 2488, 1, 4, "DroneBay"),
            asset(6, 34, 50, STRUCTURE, "Hangar"),
        ];
        let names = HashMap::from([(2, "Ammo Box".to_owned())]);
        let holdings = flatten(&assets, &names).holdings;

        assert_eq!(total(&holdings, JITA, 34), 700);
        assert_eq!(total(&holdings, JITA, 2488), 1);
        assert_eq!(total(&holdings, STRUCTURE, 34), 50);

        let boxed: i64 = holdings
            .iter()
            .filter(|h| h.containers.contains(&container_key("ammo box")))
            .map(|h| h.quantity)
            .sum();
        assert_eq!(boxed, 200, "only the stack inside the named container");
    }

    #[test]
    fn what_is_in_a_ship_is_marked() {
        let assets = [
            asset(1, 16227, 1, JITA, "Hangar"),
            asset(2, 3082, 1, 1, "HiSlot0"),
            asset(3, 2488, 5, 1, "DroneBay"),
            asset(4, 3293, 1, 1, "Cargo"),
            asset(5, 34, 10, 4, "Unlocked"),
            asset(6, 3293, 1, JITA, "Hangar"),
            asset(7, 3082, 4, 6, "Unlocked"),
            asset(10, OFFICE_TYPE, 1, JITA, "OfficeFolder"),
            asset(11, 3082, 2, 10, "CorpSAG1"),
        ];
        let holdings = flatten(&assets, &HashMap::new()).holdings;
        let in_ship = |type_id| -> i64 {
            holdings
                .iter()
                .filter(|h| h.type_id == type_id && h.in_ship)
                .map(|h| h.quantity)
                .sum()
        };

        assert_eq!(in_ship(16227), 0, "the hull itself lies in the hangar");
        assert_eq!(
            in_ship(3082),
            1,
            "fitted, not the ones in a box or the corp hangar"
        );
        assert_eq!(total(&holdings, JITA, 3082), 7);
        assert_eq!(in_ship(2488), 5);
        assert_eq!(in_ship(34), 10, "in a container in the cargo hold");

        let ships = flatten(&assets, &HashMap::new()).ships;
        assert_eq!(ships.len(), 1);
        let ferox = &ships[0];
        assert_eq!(
            (ferox.item_id, ferox.type_id, ferox.location_id),
            (1, 16227, JITA)
        );
        let aboard = |type_id, fitted| -> i64 {
            ferox
                .contents
                .iter()
                .filter(|item| item.type_id == type_id && item.fitted == fitted)
                .map(|item| item.quantity)
                .sum()
        };
        assert_eq!(aboard(3082, true), 1);
        assert_eq!(aboard(2488, false), 5, "drones are aboard but not fitted");
        assert_eq!(aboard(34, false), 10);
    }

    #[test]
    fn corporation_hangars_are_found_through_the_office() {
        let assets = [
            asset(10, OFFICE_TYPE, 1, STRUCTURE, "OfficeFolder"),
            asset(11, 34, 100, 10, "CorpSAG2"),
            asset(12, 3293, 1, 10, "CorpSAG3"),
            asset(13, 34, 7, 12, "Unlocked"),
        ];
        let holdings = flatten(&assets, &HashMap::new()).holdings;

        assert_eq!(total(&holdings, STRUCTURE, 34), 107);
        assert_eq!(total(&holdings, STRUCTURE, OFFICE_TYPE), 0);
        let in_hangar_3: Vec<&Holding> =
            holdings.iter().filter(|h| h.division == Some(3)).collect();
        assert_eq!(in_hangar_3.len(), 2, "the container and the stack in it");
    }

    #[test]
    fn things_in_an_owned_structure_are_at_that_structure() {
        let system = 30_000_142;
        let assets = [
            asset(STRUCTURE, 35832, 1, system, "AutoFit"),
            asset(20, 4247, 300, STRUCTURE, "StructureFuel"),
            asset(21, OFFICE_TYPE, 1, STRUCTURE, "OfficeFolder"),
            asset(22, 34, 9, 21, "CorpSAG1"),
        ];
        let holdings = flatten(&assets, &HashMap::new()).holdings;

        assert_eq!(total(&holdings, STRUCTURE, 4247), 300);
        assert_eq!(total(&holdings, STRUCTURE, 34), 9);
        assert_eq!(total(&holdings, system, 35832), 1);
    }

    #[test]
    fn asset_safety_is_left_out() {
        let assets = [
            asset(30, 60, 1, JITA, "AssetSafety"),
            asset(31, 34, 5, 30, "Hangar"),
        ];
        assert!(flatten(&assets, &HashMap::new()).holdings.is_empty());
    }

    #[test]
    fn only_holders_of_items_get_their_names_asked() {
        let assets = [
            asset(2, 3293, 1, JITA, "Hangar"),
            asset(3, 34, 200, 2, "Unlocked"),
            asset(4, 16227, 1, JITA, "Hangar"),
            asset(10, OFFICE_TYPE, 1, JITA, "OfficeFolder"),
            asset(11, 34, 100, 10, "CorpSAG2"),
        ];
        assert_eq!(nameable(&assets), vec![2]);
    }
}
