//! The mark, at the resolution a terminal has.
//!
//! The droplet is rasterised from the same curve as the app icon — the Bézier
//! in `app/PruneJuice/scripts/make-icon.swift` — so the Dock and the terminal
//! show one droplet at two resolutions rather than two drawings of one idea.
//! Half-block cells carry two pixels of vertical resolution each, which is
//! what lets the shoulders curve instead of step.
//!
//! Not ASCII, deliberately. Block elements are in every monospace font that can
//! already render the `·` and the `…` this tool has always printed, and the
//! ASCII outline it replaces could not hold a curve at this size. What it does
//! not depend on is colour: the empty half is drawn in a lighter glyph than the
//! full half, so the droplet reads as half full on a monochrome terminal and a
//! consumer that *can* colour only makes that clearer.
//!
//! Data, not output: `core` still never prints. Whoever holds these strings
//! decides where they go.

/// Eight rows, every one of them [`DROP_WIDTH`] columns wide, so text set
/// beside the droplet keeps a straight left edge. There is a test.
pub const DROP: [&str; 8] = [
    "     ░     ",
    "   ▄░░░▄   ",
    "  ▄░░░░░▄  ",
    " ░░░░░░░░░ ",
    "███████████",
    "███████████",
    "▀█████████▀",
    "  ▀█████▀  ",
];

/// The width every row of [`DROP`] is padded to.
pub const DROP_WIDTH: usize = 11;

/// The first row below the fill line. Rows before it are the empty half and
/// rows from it down are what has been reclaimed, which is the whole point of
/// the mark being *half* full. A consumer with colour tints the two halves
/// differently; one without still has two different glyphs.
pub const DROP_FILL_ROW: usize = 4;

/// The row the first line of accompanying text belongs beside: the droplet's
/// waist, so the two blocks share a middle instead of the text reading as a
/// caption stuck to the top of a drawing.
pub const DROP_WAIST: usize = 3;

/// What the tool does, in one line, for whatever sits under the name.
pub const TAGLINE: &str = "reclaim Docker disk with provenance";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_droplet_is_rectangular() {
        // A ragged right edge would bend the text column beside it, and in the
        // interface it would leave the panel to its right ragged too.
        for row in DROP {
            assert_eq!(
                row.chars().count(),
                DROP_WIDTH,
                "{row:?} is the wrong width"
            );
        }
    }

    #[test]
    fn it_is_drawn_from_one_small_set_of_glyphs() {
        // Every one of these is a block element, which is the set that renders
        // in a terminal font. A stray glyph from anywhere else is how a mark
        // starts arriving as a replacement box on somebody's machine.
        for row in DROP {
            for ch in row.chars() {
                assert!(
                    matches!(ch, ' ' | '░' | '▀' | '▄' | '█'),
                    "{ch:?} in {row:?} is not one of the block glyphs"
                );
            }
        }
    }

    #[test]
    fn the_halves_are_told_apart_without_colour() {
        // The contract the doc comment makes: light glyphs above the fill line,
        // solid ones below it. Colour may reinforce that; it may not be what
        // carries it.
        for (i, row) in DROP.iter().enumerate() {
            if i < DROP_FILL_ROW {
                assert!(
                    !row.contains('█'),
                    "row {i} is above the fill line: {row:?}"
                );
            } else {
                assert!(
                    !row.contains('░'),
                    "row {i} is below the fill line: {row:?}"
                );
            }
        }
    }

    #[test]
    fn there_is_room_for_two_lines_beside_the_waist() {
        assert!(DROP_WAIST + 2 <= DROP.len());
        assert!(DROP_FILL_ROW < DROP.len());
    }
}
