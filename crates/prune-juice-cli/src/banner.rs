//! The header a one-shot run opens with.
//!
//! The droplet itself lives in `core::brand`, because the interface draws the
//! same one and two copies would eventually be two different droplets.

use std::io::{self, IsTerminal};

use prune_juice_core::brand::{DROP, DROP_FILL_ROW, DROP_WAIST, TAGLINE};
use prune_juice_core::update;

/// xterm-256 rather than 24-bit colour. Terminal.app has no truecolor at all,
/// and a mark that comes out the wrong colour on the Mac's own terminal is
/// worse than one that is a shade coarser everywhere. 98 is within a few
/// points of the app's `plum`; 141 is that colour lightened.
const FULL: &str = "\x1b[38;5;98m";
const EMPTY: &str = "\x1b[38;5;141m";
const RESET: &str = "\x1b[0m";

/// Whether to tint the droplet at all.
///
/// `NO_COLOR` is the user saying no and `TERM=dumb` is the terminal saying it
/// cannot. Either way the glyphs still read as half full, which is why colour
/// is allowed to be absent rather than having to be substituted for.
fn tint() -> bool {
    io::stdout().is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
}

/// The header, blank line above and below, ready to print as one write.
///
/// Decoration, not payload: the caller prints this only when a person is
/// watching. Piped or redirected output starts at the report.
pub fn header() -> String {
    render(tint())
}

fn render(tint: bool) -> String {
    let beside = [
        format!("prune-juice {}", update::current_version()),
        TAGLINE.to_string(),
    ];

    let mut out = String::from("\n");
    for (row, art) in DROP.iter().enumerate() {
        out.push_str("  ");
        if tint {
            out.push_str(if row < DROP_FILL_ROW { EMPTY } else { FULL });
            out.push_str(art);
            out.push_str(RESET);
        } else {
            out.push_str(art);
        }
        match row.checked_sub(DROP_WAIST).and_then(|i| beside.get(i)) {
            Some(text) => {
                out.push_str("    ");
                out.push_str(text);
            }
            // Only an untinted row can be trimmed: a reset sequence has to
            // survive to the end of the line or the colour runs on.
            None if !tint => {
                while out.ends_with(' ') {
                    out.pop();
                }
            }
            None => {}
        }
        out.push('\n');
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_fits_a_narrow_terminal() {
        for line in render(false).lines() {
            // Well inside 80, which is the narrowest terminal the report
            // itself is readable in.
            assert!(
                line.chars().count() <= 60,
                "{line:?} is {} wide",
                line.chars().count()
            );
            assert_eq!(line.trim_end(), line, "{line:?} has trailing whitespace");
        }
    }

    #[test]
    fn it_names_the_version_it_was_built_from() {
        // The number beside the droplet has to be this binary's, or a bug
        // report quoting the header sends someone to the wrong source.
        let h = render(false);
        assert!(
            h.contains(&format!("prune-juice {}", update::current_version())),
            "{h}"
        );
    }

    #[test]
    fn the_text_sits_where_the_droplet_is_widest() {
        // Beside the top row it reads as a caption; the point of the offset is
        // that the two blocks share a middle.
        let h = render(false);
        let lines: Vec<_> = h.lines().collect();
        // One leading blank line, then the art.
        assert!(lines[1 + DROP_WAIST].contains("prune-juice"), "{lines:?}");
        assert!(!lines[1].contains("prune-juice"), "{lines:?}");
    }

    #[test]
    fn untinted_output_carries_no_escape_sequences() {
        // `NO_COLOR` has to mean no colour, not less colour.
        assert!(!render(false).contains('\x1b'));
    }

    #[test]
    fn every_tinted_line_closes_the_colour_it_opened() {
        // A line that sets a colour and does not reset it bleeds into whatever
        // the shell prints next, which outlives the process.
        for line in render(true).lines().filter(|l| l.contains('\x1b')) {
            assert!(line.ends_with(RESET) || line.contains(RESET), "{line:?}");
            assert_eq!(
                line.matches("\x1b[38;5;").count(),
                line.matches(RESET).count(),
                "unbalanced colour in {line:?}"
            );
        }
    }

    #[test]
    fn tinting_changes_nothing_but_the_colour() {
        let strip = |s: String| {
            let mut out = String::new();
            let mut chars = s.chars();
            while let Some(c) = chars.next() {
                if c == '\x1b' {
                    for c in chars.by_ref() {
                        if c == 'm' {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        };
        // Trailing spaces are the one difference: an untinted row is trimmed
        // and a tinted one cannot be.
        let tinted: Vec<String> = strip(render(true))
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect();
        let plain: Vec<String> = render(false).lines().map(|l| l.to_string()).collect();
        assert_eq!(tinted, plain);
    }

    #[test]
    fn it_is_padded_top_and_bottom() {
        let h = render(false);
        assert!(h.starts_with('\n') && h.ends_with("\n\n"), "{h:?}");
    }
}
