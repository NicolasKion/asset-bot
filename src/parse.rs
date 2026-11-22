//! Reads pasted item lists: multibuy lines, copies from the inventory and EFT fits.

/// One line of a pasted list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub name: String,
    pub quantity: i64,
    /// The whole line, when a number was split off it. Some type names end in
    /// a number, so this is tried as a name if `name` is not one.
    pub whole: Option<String>,
}

impl Line {
    fn one(name: &str) -> Self {
        Self {
            name: name.trim().to_owned(),
            quantity: 1,
            whole: None,
        }
    }
}

/// Parses pasted text into names and quantities. Names are not checked here.
///
/// Understood per line: `Name 500`, `Name x500`, `500 Name`, `500x Name`, a
/// tab separated copy from the game (`Name<tab>500<tab>...`), and EFT fits,
/// where `[Hull, Fit name]` counts the hull and `Module, Charge` counts both.
pub fn parse(text: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut in_fit = false;

    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some(inner) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            // `[Empty High slot]` and the like carry nothing.
            if let Some((hull, _fit_name)) = inner.split_once(',') {
                in_fit = true;
                lines.push(Line::one(hull));
            }
            continue;
        }

        if line.contains('\t') {
            lines.push(tabbed(line));
            continue;
        }

        let line = line.strip_suffix("/offline").map_or(line, str::trim_end);
        if let Some((name, quantity)) = counted(line) {
            lines.push(Line {
                name: name.to_owned(),
                quantity,
                whole: Some(line.to_owned()),
            });
        } else if let Some((module, charge)) = line.split_once(", ").filter(|_| in_fit) {
            lines.push(Line::one(module));
            lines.push(Line::one(charge));
        } else {
            lines.push(Line::one(line));
        }
    }
    lines
}

/// A line copied from the game: the name, then usually the quantity, then
/// columns such as group and volume. Assembled ships have an empty quantity.
fn tabbed(line: &str) -> Line {
    let mut fields = line.split('\t').map(str::trim);
    let first = fields.next().unwrap_or_default();
    let second = fields.next().unwrap_or_default();
    let (name, quantity) = match (quantity(first), quantity(second)) {
        (_, Some(quantity)) => (first, quantity),
        (Some(quantity), None) if !second.is_empty() => (second, quantity),
        _ => (first, 1),
    };
    Line {
        name: name.to_owned(),
        quantity,
        whole: None,
    }
}

/// Splits a quantity off the end (`Name 5`, `Name x5`) or the start
/// (`5 Name`, `5x Name`, `5 x Name`) of a line.
fn counted(line: &str) -> Option<(&str, i64)> {
    if let Some((name, last)) = line.rsplit_once(char::is_whitespace) {
        let number = last.strip_prefix(['x', 'X']).unwrap_or(last);
        if let Some(quantity) = quantity(number) {
            return Some((name.trim_end(), quantity));
        }
    }
    let (first, name) = line.split_once(char::is_whitespace)?;
    let number = first.strip_suffix(['x', 'X']).unwrap_or(first);
    let quantity = quantity(number)?;
    let name = name.trim_start();
    let name = name.strip_prefix("x ").map_or(name, str::trim_start);
    Some((name, quantity))
}

/// Reads a positive whole number, with or without thousands separators.
fn quantity(text: &str) -> Option<i64> {
    let starts_and_ends_with_digit = text.starts_with(|c: char| c.is_ascii_digit())
        && text.ends_with(|c: char| c.is_ascii_digit());
    if !starts_and_ends_with_digit {
        return None;
    }
    let digits: String = text
        .chars()
        .filter(|c| !matches!(c, ',' | '.' | '\'' | '\u{a0}' | '\u{202f}'))
        .collect();
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|&quantity| quantity > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(text: &str) -> Vec<(String, i64)> {
        parse(text)
            .into_iter()
            .map(|line| (line.name, line.quantity))
            .collect()
    }

    fn expect(text: &str, want: &[(&str, i64)]) {
        let want: Vec<(String, i64)> = want.iter().map(|&(n, q)| (n.to_owned(), q)).collect();
        assert_eq!(pairs(text), want, "input {text:?}");
    }

    #[test]
    fn multibuy_lines() {
        expect(
            "Nanite Repair Paste 500\nTritanium x1,000\n\n  Warp Scrambler II  \n",
            &[
                ("Nanite Repair Paste", 500),
                ("Tritanium", 1000),
                ("Warp Scrambler II", 1),
            ],
        );
    }

    #[test]
    fn quantity_in_front() {
        expect(
            "5 Hobgoblin II\n5x Hammerhead II\n3 x Ogre II\n10MN Afterburner II",
            &[
                ("Hobgoblin II", 5),
                ("Hammerhead II", 5),
                ("Ogre II", 3),
                ("10MN Afterburner II", 1),
            ],
        );
    }

    #[test]
    fn copies_from_the_game() {
        expect(
            "Tritanium\t1.000\tMineral\t\t\t10 m3\t5.000,00 ISK\nFerox\t\tCombat Battlecruiser\n20\tScourge Fury Heavy Missile",
            &[
                ("Tritanium", 1000),
                ("Ferox", 1),
                ("Scourge Fury Heavy Missile", 20),
            ],
        );
    }

    #[test]
    fn eft_fit() {
        let fit = "[Ferox, Fleet Ferox]\n\
                   Magnetic Field Stabilizer II\n\
                   250mm Railgun II, Caldari Navy Antimatter Charge M\n\
                   [Empty Med slot]\n\
                   Sensor Booster II /offline\n\
                   \n\
                   Hornet EC-300 x5\n\
                   Caldari Navy Antimatter Charge M x2000";
        expect(
            fit,
            &[
                ("Ferox", 1),
                ("Magnetic Field Stabilizer II", 1),
                ("250mm Railgun II", 1),
                ("Caldari Navy Antimatter Charge M", 1),
                ("Sensor Booster II", 1),
                ("Hornet EC-300", 5),
                ("Caldari Navy Antimatter Charge M", 2000),
            ],
        );
    }

    #[test]
    fn a_split_off_number_keeps_the_whole_line() {
        let lines = parse("Some Implant 10");
        assert_eq!(lines[0].whole.as_deref(), Some("Some Implant 10"));
        assert_eq!(parse("Tritanium")[0].whole, None);
    }

    #[test]
    fn commas_outside_a_fit_stay_in_the_name() {
        expect("Odd, Name", &[("Odd, Name", 1)]);
    }
}
