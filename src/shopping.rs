//! Compares what a stockpile wants with what is there.

use std::collections::HashMap;
use std::fmt::Write as _;

use crate::store::Item;

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
