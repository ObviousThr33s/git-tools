//! A mark for every project, derived from its name.
//!
//! Nothing is authored and nothing is fetched: the same name always yields
//! the same figure, so a project becomes recognisable by shape before you
//! have read its label.
//!
//! SHA-256 is used rather than a cheaper hash for one reason only -- the
//! Python implementation of this tool uses it, and a project should carry
//! the same mark whichever binary you open.

use ratatui::style::Color;
use sha2::{Digest, Sha256};

pub const SIZE: usize = 7; // odd, so there is a true centre column
pub const FILL: u8 = 44; // percent lit; below ~35 reads sparse, above ~55 solid
pub const WIDTH: usize = SIZE * 2; // display columns: two per cell, for aspect

/// A mirrored bit grid, seeded by the project name.
///
/// The mirroring is the whole trick. Symmetry is what the eye reads as
/// deliberate; without it the same bits are noise.
pub fn grid(name: &str) -> [[bool; SIZE]; SIZE] {
    let digest = Sha256::digest(name.as_bytes());
    let half = SIZE.div_ceil(2);
    let mut grid = [[false; SIZE]; SIZE];
    for y in 0..SIZE {
        for x in 0..half {
            let lit = digest[(y * half + x) % digest.len()] % 100 < FILL;
            grid[y][x] = lit;
            grid[y][SIZE - 1 - x] = lit;
        }
    }
    if grid.iter().all(|r| r.iter().all(|&c| !c)) {
        grid[SIZE / 2][SIZE / 2] = true; // a blank mark represents nothing
    }
    grid
}

/// Render as terminal rows. Half-height blocks pack two grid rows into one
/// terminal row, so a 7x7 figure occupies 4 lines and keeps its square
/// aspect against a tall terminal cell.
pub fn lines(name: &str) -> Vec<String> {
    let g = grid(name);
    let mut out = Vec::new();
    for y in (0..SIZE).step_by(2) {
        let top = g[y];
        let bottom = if y + 1 < SIZE { g[y + 1] } else { [false; SIZE] };
        let row: String = (0..SIZE)
            .map(|x| match (top[x], bottom[x]) {
                (false, false) => "  ",
                (true, false) => "▀▀",
                (false, true) => "▄▄",
                (true, true) => "██",
            })
            .collect();
        out.push(row);
    }
    out
}

/// Tint by language where a language is known, by hash where it isn't --
/// so the colour carries information where information exists, and stays
/// stable where it doesn't.
const LANGUAGE_TINT: &[(&str, u8)] = &[
    ("rust", 209), ("go", 80), ("python", 220), ("javascript", 227),
    ("typescript", 75), ("c", 111), ("c++", 176), ("c#", 141), ("java", 173),
    ("kotlin", 141), ("swift", 209), ("ruby", 203), ("php", 104),
    ("shell", 149), ("powershell", 69), ("lua", 63), ("haskell", 140),
    ("scala", 203), ("elixir", 141), ("zig", 215), ("dart", 45),
    ("html", 209), ("css", 105), ("vue", 79), ("nix", 75), ("ocaml", 214),
    ("perl", 111),
];

const NEUTRAL_TINTS: [u8; 8] = [39, 79, 141, 209, 214, 105, 45, 168];

/// Commits get their own spread of hues. A project's mark carries one colour
/// for the whole list, so its commits need a wider palette than the eight
/// neutrals -- otherwise neighbouring commits land on the same tint often
/// enough to read as related when they are not.
const COMMIT_TINTS: [u8; 16] = [
    39, 45, 51, 79, 84, 114, 141, 147, 168, 175, 203, 209, 214, 220, 105, 116,
];

pub fn tint(name: &str, language: &str) -> Color {
    let lang = language.trim().to_lowercase();
    if let Some((_, code)) = LANGUAGE_TINT.iter().find(|(l, _)| *l == lang) {
        return Color::Indexed(*code);
    }
    let digest = Sha256::digest(format!("tint:{name}").as_bytes());
    Color::Indexed(NEUTRAL_TINTS[digest[0] as usize % NEUTRAL_TINTS.len()])
}

/// A commit's own colour, from its sha. Language means nothing here and the
/// sha is already uniform, so the tint is drawn straight from the palette.
pub fn commit_tint(sha: &str) -> Color {
    let digest = Sha256::digest(format!("commit:{sha}").as_bytes());
    Color::Indexed(COMMIT_TINTS[digest[0] as usize % COMMIT_TINTS.len()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        assert_eq!(lines("NEXUS"), lines("NEXUS"));
    }

    #[test]
    fn name_sensitive() {
        assert_ne!(lines("NEXUS"), lines("NEXUs"));
    }

    #[test]
    fn mirrored() {
        let g = grid("game_projects");
        for row in g.iter() {
            for x in 0..SIZE {
                assert_eq!(row[x], row[SIZE - 1 - x]);
            }
        }
    }

    #[test]
    fn never_blank() {
        // No name should produce an empty figure.
        for name in ["", "a", "zzz", "Archive/db_example"] {
            assert!(grid(name).iter().any(|r| r.iter().any(|&c| c)));
        }
    }

    /// The point of using SHA-256 rather than a cheaper hash: a project must
    /// carry the same mark whichever implementation you open. These strings
    /// were produced by the Python tool in this repository.
    #[test]
    fn matches_the_python_implementation() {
        let cases: &[(&str, &str)] = &[
            ("NEXUS", "▄▄▄▄▄▄  ▄▄▄▄▄▄/▄▄▀▀▄▄▄▄▄▄▀▀▄▄/▀▀▀▀██  ██▀▀▀▀/  ▀▀▀▀▀▀▀▀▀▀  "),
            ("game_projects", "  ▄▄▄▄  ▄▄▄▄  /      ▄▄      /████  ▀▀  ████/▀▀    ▀▀    ▀▀"),
            ("Archive/db_example", "▀▀▀▀      ▀▀▀▀/▀▀          ▀▀/▄▄▀▀██████▀▀▄▄/      ▀▀      "),
        ];
        for (name, expected) in cases {
            assert_eq!(lines(name).join("/"), *expected, "mark drifted for {name}");
        }
    }

    #[test]
    fn commit_tints_are_stable_and_spread() {
        assert_eq!(commit_tint("521d273"), commit_tint("521d273"));
        let shas = [
            "521d273", "9f7e237", "e990e30", "aaee4b1", "35d71ce", "0f1e2d3",
            "c0ffee1", "deadbe2",
        ];
        let distinct: std::collections::HashSet<_> =
            shas.iter().map(|s| commit_tint(s)).collect();
        assert!(distinct.len() >= shas.len() - 2, "commit tints clump together");
    }

    #[test]
    fn a_commit_carries_its_own_mark() {
        assert_ne!(lines("git-history-rs"), lines("521d2733a"));
    }

    #[test]
    fn renders_four_lines_of_fixed_width() {
        let l = lines("anything");
        assert_eq!(l.len(), SIZE.div_ceil(2));
        assert!(l.iter().all(|s| s.chars().count() == WIDTH));
    }
}
