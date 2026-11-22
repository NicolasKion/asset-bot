//! Compares what a stockpile wants with what is there.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use crate::store::{DockedShip, Fit, Item};

/// The items of which there is less than wanted, with the missing quantity.
/// `stock` is how much of each type id is there. Keeps the order of `wanted`.
pub fn missing(wanted: &[Item], stock: &HashMap<i64, i64>) -> Vec<Item> {
    wanted
        .iter()
        .filter_map(|item| {
            let there = stock.get(&item.type_id).copied().unwrap_or(0);
            let short = item.quantity.saturating_sub(there);
            (short > 0).then(|| Item {
                quantity: short,
                ..item.clone()
            })
        })
        .collect()
}

/// How one wanted fit compares with the ships that are there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FitStatus {
    pub name: String,
    pub wanted: i64,
    /// Ships that have everything the fit asks for.
    pub ready: i64,
    /// Ships of the right hull that lack something.
    pub incomplete: i64,
}

/// What matching the wanted fits against the docked ships came to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FitPlan {
    pub statuses: Vec<FitStatus>,
    /// What it takes to get every wanted ship ready: what incomplete
    /// ships lack, and for ships that are not there at all the hull and
    /// everything in the fit. Sorted by name.
    pub needed: Vec<Item>,
    /// How many hulls the matched ships take up, by type id. They must not
    /// count again as loose stock.
    pub used_hulls: HashMap<i64, i64>,
    /// Everything on the matched ships, by type id. It must not count again
    /// for stockpiles that count what is in ships.
    pub used_contents: HashMap<i64, i64>,
}

/// Decides which docked ship stands for which wanted fit.
///
/// A ship has a fit when it is the fit's hull, every module of the fit is
/// fitted to it and the fit's drones and cargo are aboard, whatever the
/// ship is called. What else is on it does not matter, and neither do
/// loaded charges. Fits that ask for more pick first, so a ship goes to
/// the most specific fit it has. Ships of the right hull that are
/// left over count as incomplete ones of a fit that is still short,
/// closest first.
pub fn plan_fits(fits: &[Fit], ships: &[DockedShip]) -> FitPlan {
    let mut order: Vec<usize> = (0..fits.len()).collect();
    order.sort_by_key(|&index| {
        let fit = &fits[index];
        std::cmp::Reverse(fit.fitted.len() + fit.carried.len())
    });

    let mut free: Vec<bool> = vec![true; ships.len()];
    let mut ready = vec![0_i64; fits.len()];
    let mut incomplete = vec![0_i64; fits.len()];
    // Type id to name and quantity.
    let mut needed: BTreeMap<i64, (String, i64)> = BTreeMap::new();
    let mut plan = FitPlan::default();
    let mut take = |ship: &DockedShip| {
        *plan.used_hulls.entry(ship.type_id).or_default() += 1;
        for (&type_id, &quantity) in &ship.contents {
            *plan.used_contents.entry(type_id).or_default() += quantity;
        }
    };
    let mut need = |item: &Item, quantity: i64| {
        let entry = needed
            .entry(item.type_id)
            .or_insert_with(|| (item.name.clone(), 0));
        entry.1 = entry.1.saturating_add(quantity);
    };

    for &index in &order {
        let fit = &fits[index];
        for (ship, free) in ships.iter().zip(&mut free) {
            if ready[index] >= fit.wanted {
                break;
            }
            if *free && ship.type_id == fit.hull_type_id && lacking(fit, ship).is_empty() {
                *free = false;
                ready[index] += 1;
                take(ship);
            }
        }
    }

    for &index in &order {
        let fit = &fits[index];
        let mut candidates: Vec<(i64, usize)> = ships
            .iter()
            .enumerate()
            .filter(|&(ship_index, ship)| free[ship_index] && ship.type_id == fit.hull_type_id)
            .map(|(ship_index, ship)| {
                let short: i64 = lacking(fit, ship)
                    .iter()
                    .map(|&(_, quantity)| quantity)
                    .sum();
                (short, ship_index)
            })
            .collect();
        candidates.sort_unstable();
        for (_, ship_index) in candidates {
            if ready[index] + incomplete[index] >= fit.wanted {
                break;
            }
            let ship = &ships[ship_index];
            free[ship_index] = false;
            incomplete[index] += 1;
            take(ship);
            for (item, quantity) in lacking(fit, ship) {
                need(item, quantity);
            }
        }

        let absent = (fit.wanted - ready[index] - incomplete[index]).max(0);
        if absent > 0 {
            let hull = Item {
                type_id: fit.hull_type_id,
                name: fit.hull_name.clone(),
                quantity: 1,
            };
            need(&hull, absent);
            for item in fit.fitted.iter().chain(&fit.carried).chain(&fit.loaded) {
                need(item, item.quantity.saturating_mul(absent));
            }
        }
    }

    plan.statuses = fits
        .iter()
        .enumerate()
        .map(|(index, fit)| FitStatus {
            name: fit.name.clone(),
            wanted: fit.wanted,
            ready: ready[index],
            incomplete: incomplete[index],
        })
        .collect();
    plan.needed = needed
        .into_iter()
        .map(|(type_id, (name, quantity))| Item {
            type_id,
            name,
            quantity,
        })
        .collect();
    plan.needed.sort_by(|a, b| a.name.cmp(&b.name));
    plan
}

/// What a ship lacks of a fit, with how many: modules that are not fitted,
/// and drones and cargo that are not aboard. Loaded charges are not looked at.
fn lacking<'a>(fit: &'a Fit, ship: &DockedShip) -> Vec<(&'a Item, i64)> {
    let in_slots = |type_id| ship.fitted.get(&type_id).copied().unwrap_or(0);
    // Aboard, but not in a slot: a charge loaded in a gun is no cargo.
    let aboard = |type_id| ship.contents.get(&type_id).copied().unwrap_or(0) - in_slots(type_id);
    let fitted = fit.fitted.iter().map(|item| (item, in_slots(item.type_id)));
    let carried = fit.carried.iter().map(|item| (item, aboard(item.type_id)));
    fitted
        .chain(carried)
        .filter_map(|(item, there)| {
            let short = item.quantity - there;
            (short > 0).then_some((item, short))
        })
        .collect()
}

/// Adds two lists of wanted items into one, sorted by name.
pub fn combine(first: &[Item], second: &[Item]) -> Vec<Item> {
    let mut totals: BTreeMap<i64, Item> = BTreeMap::new();
    for item in first.iter().chain(second) {
        totals
            .entry(item.type_id)
            .and_modify(|total| total.quantity = total.quantity.saturating_add(item.quantity))
            .or_insert_with(|| item.clone());
    }
    let mut items: Vec<Item> = totals.into_values().collect();
    items.sort_by(|a, b| a.name.cmp(&b.name));
    items
}

/// How a list of items is written out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `Name 5`, what the multibuy window of the game's market takes.
    Multibuy,
    /// `Name x5`, as in the cargo part of an EFT fit.
    Eft,
    /// Name, a tab, the quantity. Pastes into two spreadsheet columns.
    Spreadsheet,
}

impl Format {
    pub const ALL: [Self; 3] = [Self::Multibuy, Self::Eft, Self::Spreadsheet];

    /// What the format is called on its button.
    pub fn label(self) -> &'static str {
        match self {
            Self::Multibuy => "Multibuy",
            Self::Eft => "EFT (Name x5)",
            Self::Spreadsheet => "Spreadsheet",
        }
    }

    /// Short name used in button ids.
    pub fn key(self) -> &'static str {
        match self {
            Self::Multibuy => "multibuy",
            Self::Eft => "eft",
            Self::Spreadsheet => "sheet",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|format| format.key() == key)
    }

    /// One line per item.
    pub fn render(self, items: &[Item]) -> String {
        let mut text = String::new();
        for Item { name, quantity, .. } in items {
            // Writing to a String cannot fail.
            let _ = match self {
                Self::Multibuy => writeln!(text, "{name} {quantity}"),
                Self::Eft => writeln!(text, "{name} x{quantity}"),
                Self::Spreadsheet => writeln!(text, "{name}\t{quantity}"),
            };
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(type_id: i64, name: &str, quantity: i64) -> Item {
        Item {
            type_id,
            name: name.to_owned(),
            quantity,
        }
    }

    const FEROX: i64 = 16227;
    const RAILGUN: i64 = 3082;
    const STABILIZER: i64 = 10190;
    const HORNET: i64 = 2488;
    const AMMO: i64 = 230;

    fn fit(name: &str, wanted: i64, fitted: &[(i64, &str, i64)]) -> Fit {
        Fit {
            name: name.to_owned(),
            hull_type_id: FEROX,
            hull_name: "Ferox".to_owned(),
            wanted,
            fitted: fitted
                .iter()
                .map(|&(id, name, n)| item(id, name, n))
                .collect(),
            carried: vec![item(HORNET, "Hornet EC-300", 5)],
            loaded: vec![item(AMMO, "Antimatter Charge M", 7)],
        }
    }

    /// A Ferox with these modules fitted and these drones and cargo aboard.
    fn ship_with(fitted: &[(i64, i64)], carried: &[(i64, i64)]) -> DockedShip {
        let fitted: HashMap<i64, i64> = fitted.iter().copied().collect();
        let mut contents = fitted.clone();
        for &(type_id, quantity) in carried {
            *contents.entry(type_id).or_default() += quantity;
        }
        DockedShip {
            type_id: FEROX,
            contents,
            fitted,
        }
    }

    /// A Ferox with these modules fitted and its five drones aboard.
    fn ship(fitted: &[(i64, i64)]) -> DockedShip {
        ship_with(fitted, &[(HORNET, 5)])
    }

    fn needed(plan: &FitPlan) -> Vec<(&str, i64)> {
        plan.needed
            .iter()
            .map(|item| (item.name.as_str(), item.quantity))
            .collect()
    }

    #[test]
    fn ships_are_matched_by_what_is_fitted() {
        let fleet = fit(
            "Fleet Ferox",
            3,
            &[
                (RAILGUN, "250mm Railgun II", 7),
                (STABILIZER, "Magnetic Field Stabilizer II", 2),
            ],
        );
        let ships = [
            ship(&[(RAILGUN, 7), (STABILIZER, 2), (999, 1)]),
            ship(&[(RAILGUN, 5), (STABILIZER, 2)]),
        ];
        let plan = plan_fits(&[fleet], &ships);

        assert_eq!(
            plan.statuses,
            vec![FitStatus {
                name: "Fleet Ferox".to_owned(),
                wanted: 3,
                ready: 1,
                incomplete: 1,
            }]
        );
        // Two railguns for the incomplete ship, a whole fit for the third.
        assert_eq!(
            needed(&plan),
            vec![
                ("250mm Railgun II", 9),
                ("Antimatter Charge M", 7),
                ("Ferox", 1),
                ("Hornet EC-300", 5),
                ("Magnetic Field Stabilizer II", 2),
            ]
        );
        assert_eq!(plan.used_hulls.get(&FEROX), Some(&2));
        assert_eq!(plan.used_contents.get(&RAILGUN), Some(&12));
    }

    #[test]
    fn drones_and_cargo_have_to_be_aboard_but_loaded_charges_do_not() {
        let fleet = fit("Fleet Ferox", 3, &[(RAILGUN, "250mm Railgun II", 7)]);
        let ships = [
            // No charges loaded at all: still ready.
            ship_with(&[(RAILGUN, 7)], &[(HORNET, 5)]),
            // Two drones lost.
            ship_with(&[(RAILGUN, 7)], &[(HORNET, 3)]),
            // Hornets sitting in slots, as a loaded charge would, are no drones.
            ship_with(&[(RAILGUN, 7), (HORNET, 5)], &[]),
        ];
        let plan = plan_fits(&[fleet], &ships);

        assert_eq!(
            (plan.statuses[0].ready, plan.statuses[0].incomplete),
            (1, 2)
        );
        assert_eq!(needed(&plan), vec![("Hornet EC-300", 7)]);
    }

    #[test]
    fn a_ship_goes_to_the_most_specific_fit_and_spare_ships_stay_free() {
        let basic = fit("Basic", 1, &[(RAILGUN, "250mm Railgun II", 7)]);
        let full = fit(
            "Full",
            1,
            &[
                (RAILGUN, "250mm Railgun II", 7),
                (STABILIZER, "Magnetic Field Stabilizer II", 2),
            ],
        );
        let ships = [
            ship(&[(RAILGUN, 7), (STABILIZER, 2)]),
            ship(&[(RAILGUN, 7)]),
            ship(&[(RAILGUN, 7)]),
        ];
        let plan = plan_fits(&[basic, full], &ships);

        let ready: Vec<(&str, i64)> = plan
            .statuses
            .iter()
            .map(|status| (status.name.as_str(), status.ready))
            .collect();
        assert_eq!(ready, vec![("Basic", 1), ("Full", 1)]);
        assert!(plan.needed.is_empty());
        assert_eq!(
            plan.used_hulls.get(&FEROX),
            Some(&2),
            "the third ship is not taken"
        );
    }

    #[test]
    fn lists_are_combined_by_type() {
        let combined = combine(
            &[item(1, "Ferox", 2), item(2, "Nanite Repair Paste", 500)],
            &[item(1, "Ferox", 3), item(3, "Damage Control II", 1)],
        );
        assert_eq!(
            combined,
            vec![
                item(3, "Damage Control II", 1),
                item(1, "Ferox", 5),
                item(2, "Nanite Repair Paste", 500),
            ]
        );
    }

    #[test]
    fn only_what_is_short_is_listed() {
        let wanted = [
            item(1, "Ferox", 10),
            item(2, "Nanite Repair Paste", 500),
            item(3, "Warp Scrambler II", 20),
            item(4, "Damage Control II", 10),
        ];
        let stock = HashMap::from([(1, 4), (2, 500), (3, 900)]);

        let short = missing(&wanted, &stock);
        assert_eq!(
            short,
            vec![item(1, "Ferox", 6), item(4, "Damage Control II", 10)]
        );
        assert_eq!(
            Format::Multibuy.render(&short),
            "Ferox 6\nDamage Control II 10\n"
        );
        assert_eq!(
            Format::Eft.render(&short),
            "Ferox x6\nDamage Control II x10\n"
        );
        assert_eq!(
            Format::Spreadsheet.render(&short),
            "Ferox\t6\nDamage Control II\t10\n"
        );
        for format in Format::ALL {
            assert_eq!(Format::from_key(format.key()), Some(format));
        }
    }
}
