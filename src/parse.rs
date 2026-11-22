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

/// A fit from a pasted list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastedFit {
    pub hull: String,
    pub name: String,
    /// How many ships of the fit, from `[Hull, Name] x10`. 1 without it.
    pub count: i64,
    pub lines: Vec<FitLine>,
}

/// One line under a fit's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FitLine {
    pub line: Line,
    pub part: Part,
}

/// What a line of a fit is on the ship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// A module, rig or subsystem in a slot: a line without a quantity.
    Fitted,
    /// Drones and cargo: a line with a quantity.
    Carried,
    /// The charge loaded in a module: what follows the comma.
    Loaded,
}

/// A pasted list, with the fits in it kept apart.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pasted {
    /// The lines before the first fit.
    pub loose: Vec<Line>,
    pub fits: Vec<PastedFit>,
}

impl Pasted {
    /// Everything as plain lines: per fit its hull and all its lines, times
    /// the fit's count.
    pub fn into_lines(self) -> Vec<Line> {
        let mut lines = self.loose;
        for fit in self.fits {
            lines.push(Line {
                name: fit.hull,
                quantity: fit.count,
                whole: None,
            });
            lines.extend(fit.lines.into_iter().map(|fit_line| Line {
                quantity: fit_line.line.quantity.saturating_mul(fit.count),
                ..fit_line.line
            }));
        }
        lines
    }
}

/// Parses pasted text into names and quantities. Names are not checked here.
///
/// Understood per line: `Name 500`, `Name x500`, `500 Name`, `500x Name`, a
/// tab separated copy from the game (`Name<tab>500<tab>...`), and EFT fits,
/// where `[Hull, Fit name]` starts a fit and `Module, Charge` counts both.
///
/// Each EFT fit is kept together. Everything after a
/// `[Hull, Fit name]` header belongs to that fit, up to the next header.
/// `[Hull, Fit name] x10` wants ten of them.
pub fn parse_fits(text: &str) -> Pasted {
    let mut pasted = Pasted::default();

    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if let Some((inner, after)) = line.strip_prefix('[').and_then(|l| l.split_once(']')) {
            // `[Empty High slot]` and the like carry nothing.
            if let Some((hull, name)) = inner.split_once(',') {
                let after = after.trim();
                let count = quantity(after.strip_prefix(['x', 'X']).unwrap_or(after));
                pasted.fits.push(PastedFit {
                    hull: hull.trim().to_owned(),
                    name: name.trim().to_owned(),
                    count: count.unwrap_or(1),
                    lines: Vec::new(),
                });
            }
            continue;
        }

        let Some(fit) = pasted.fits.last_mut() else {
            pasted.loose.push(if line.contains('\t') {
                tabbed(line)
            } else {
                plain(line)
            });
            continue;
        };
        if line.contains('\t') {
            fit.lines.push(FitLine {
                line: tabbed(line),
                part: Part::Carried,
            });
            continue;
        }
        let line = line.strip_suffix("/offline").map_or(line, str::trim_end);
        if counted(line).is_some() {
            fit.lines.push(FitLine {
                line: plain(line),
                part: Part::Carried,
            });
        } else if let Some((module, charge)) = line.split_once(", ") {
            fit.lines.push(FitLine {
                line: Line::one(module),
                part: Part::Fitted,
            });
            fit.lines.push(FitLine {
                line: Line::one(charge),
                part: Part::Loaded,
            });
        } else {
            fit.lines.push(FitLine {
                line: Line::one(line),
                part: Part::Fitted,
            });
        }
    }
    pasted
}

/// A line outside the tab separated format: a name with or without a quantity.
fn plain(line: &str) -> Line {
    match counted(line) {
        Some((name, quantity)) => Line {
            name: name.to_owned(),
            quantity,
            whole: Some(line.to_owned()),
        },
        None => Line::one(line),
    }
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

    fn parse(text: &str) -> Vec<Line> {
        parse_fits(text).into_lines()
    }

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
    fn fits_are_kept_apart() {
        let pasted = parse_fits(
            "Nanite Repair Paste 500\n\
             [Ferox, Fleet Ferox] x10\n\
             Magnetic Field Stabilizer II\n\
             250mm Railgun II, Caldari Navy Antimatter Charge M\n\
             [Empty Med slot]\n\
             Hornet EC-300 x5\n\
             [Scimitar, Logi]\n\
             Large Remote Shield Booster II",
        );
        assert_eq!(pasted.loose.len(), 1);
        assert_eq!(pasted.fits.len(), 2);

        let ferox = &pasted.fits[0];
        assert_eq!(
            (ferox.hull.as_str(), ferox.name.as_str(), ferox.count),
            ("Ferox", "Fleet Ferox", 10)
        );
        let fitted: Vec<&str> = ferox
            .lines
            .iter()
            .filter(|l| l.part == Part::Fitted)
            .map(|l| l.line.name.as_str())
            .collect();
        assert_eq!(fitted, ["Magnetic Field Stabilizer II", "250mm Railgun II"]);
        assert_eq!(
            ferox.lines.len(),
            4,
            "the loaded charge and the drones come on top"
        );
        let part_of = |name: &str| {
            ferox
                .lines
                .iter()
                .find(|l| l.line.name == name)
                .map(|l| l.part)
        };
        assert_eq!(
            part_of("Caldari Navy Antimatter Charge M"),
            Some(Part::Loaded)
        );
        assert_eq!(part_of("Hornet EC-300"), Some(Part::Carried));
        assert_eq!(pasted.fits[1].count, 1);

        let flat = pasted.into_lines();
        let hornets = flat
            .iter()
            .find(|l| l.name == "Hornet EC-300")
            .map(|l| l.quantity);
        assert_eq!(hornets, Some(50), "five per ship, ten ships");
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
