//! Rendering. Reads [`App`]; never mutates it.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use prune_juice_core::model::{Bytes, ResourceKind};
use prune_juice_core::plan::tier::{Reversibility, Tier};

use crate::app::{App, MenuItem, Screen};

const ACCENT: Color = Color::Magenta;
const DIM: Color = Color::DarkGray;
const GOOD: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let rows = Layout::vertical([
        Constraint::Length(1), // title
        Constraint::Min(0),    // body
        Constraint::Length(1), // key hints
    ])
    .split(area);

    title(f, rows[0], app);
    match app.screen {
        Screen::Scanning => scanning(f, rows[1], app),
        Screen::Main => main_screen(f, rows[1], app),
        Screen::Review => review(f, rows[1], app),
        Screen::Confirm => confirm(f, rows[1], app),
        Screen::Applying => applying(f, rows[1], app),
        Screen::Finished => finished(f, rows[1], app),
    }
    hints(f, rows[2], app);
}

fn title(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![Span::styled(
        " Prune Juice ",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )];
    if let Some(r) = &app.report {
        spans.push(Span::styled(
            format!("· {} · {:?} ", r.context, r.daemon.runtime),
            Style::default().fg(DIM),
        ));
        if r.daemon.swarm_active {
            spans.push(Span::styled("· swarm active ", Style::default().fg(WARN)));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn scanning(f: &mut Frame, area: Rect, app: &App) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", app.status),
            Style::default().fg(WARN),
        )),
        Line::from(""),
        Line::from(format!(
            "  {:>5} containers   {:>5} images",
            app.seen_containers, app.seen_images
        )),
        Line::from(format!(
            "  {:>5} volumes      {:>5} networks",
            app.seen_volumes, app.seen_networks
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  Nothing is being changed. This is a read-only scan.",
            Style::default().fg(DIM),
        )),
    ];
    f.render_widget(Paragraph::new(lines), area);
}

fn main_screen(f: &mut Frame, area: Rect, app: &App) {
    let cols = Layout::vertical([
        Constraint::Length(9), // headline
        Constraint::Length(7), // menu
        Constraint::Min(0),    // totals
    ])
    .split(area);

    headline(f, cols[0], app);
    menu(f, cols[1], app);
    totals(f, cols[2], app);
}

/// The claim, and the arithmetic behind it.
///
/// The reversibility breakdown is stated at run level rather than per item, and
/// the word "irreversible" only ever appears beside a number. For the safe tier
/// that number is zero — which is the Tier 1 Theorem, restated for a human.
fn headline(f: &mut Frame, area: Rect, app: &App) {
    let free = app.free_bytes();
    let irreversible = app.irreversible_bytes();

    let (mut rebuildable, mut restorable) = (Bytes::ZERO, Bytes::ZERO);
    if let Some(p) = &app.plan {
        for i in p.of_tier(Tier::Free) {
            let b = i.reclaimable_size().unwrap_or(Bytes::ZERO);
            match i.reversibility() {
                Reversibility::Rebuildable(_) => rebuildable = rebuildable + b,
                Reversibility::Restorable(_) => restorable = restorable + b,
                Reversibility::Gone => {}
            }
        }
        rebuildable = rebuildable + p.build_cache_reclaimable;
    }

    let mut lines = vec![
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                free.human(),
                Style::default()
                    .fg(if free > Bytes::ZERO { GOOD } else { DIM })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  safe to reclaim", Style::default().fg(DIM)),
        ]),
        Line::from(""),
        Line::from(format!(
            "    {:>10}  rebuildable   ({} items + build cache)",
            rebuildable.human(),
            app.free_count()
        )),
    ];
    if restorable > Bytes::ZERO {
        lines.push(Line::from(format!(
            "    {:>10}  restorable",
            restorable.human()
        )));
    }
    lines.push(Line::from(vec![Span::raw(format!(
        "    {:>10}  irreversible",
        irreversible.human()
    ))]));
    lines.push(Line::from(""));
    if irreversible == Bytes::ZERO {
        lines.push(Line::from(Span::styled(
            "    Nothing here can be lost.",
            Style::default().fg(GOOD),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "    Some of this cannot be undone — review before reclaiming.",
            Style::default().fg(BAD),
        )));
    }

    f.render_widget(Paragraph::new(lines), area);
}

fn menu(f: &mut Frame, area: Rect, app: &App) {
    let mut lines = Vec::new();
    for (i, item) in MenuItem::ALL.iter().enumerate() {
        let selected = i == app.menu_index;
        let enabled = app.menu_enabled(*item);
        let marker = if selected { "❱ " } else { "  " };
        let style = match (selected, enabled) {
            (_, false) => Style::default().fg(DIM),
            (true, true) => Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            (false, true) => Style::default(),
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{marker}{}", app.menu_label(*item)), style),
        ]));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn totals(f: &mut Frame, area: Rect, app: &App) {
    let Some(r) = &app.report else { return };
    let t = &r.totals;
    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                "  {} containers · {} images ({}) · {} volumes ({}) · {} networks",
                t.containers,
                t.images,
                // The layer figure where the daemon computed it: an image's
                // stacks overlap, so a sum over stack sizes is not disk.
                t.image_unique_bytes.unwrap_or(t.image_bytes).human(),
                t.volumes,
                t.volume_bytes.human(),
                t.networks
            ),
            Style::default().fg(DIM),
        )),
        Line::from(Span::styled(
            format!(
                "  {} build cache records ({} reclaimable) · {} projects known",
                t.build_cache_records,
                t.build_cache_bytes.human(),
                r.projects_known
            ),
            Style::default().fg(DIM),
        )),
    ];
    if r.stale {
        lines.push(Line::from(Span::styled(
            "  ! sizes are incomplete — the figures above are a floor",
            Style::default().fg(WARN),
        )));
    }
    for w in &r.warnings {
        lines.push(Line::from(Span::styled(
            format!("  ! {w}"),
            Style::default().fg(WARN),
        )));
    }
    // Below the warnings, and dimmer than them: a new release is the least
    // urgent thing on this screen. Clipped to the actual width like every
    // other line here — a notice that wraps a narrow terminal would push the
    // real content off it.
    if let Some(u) = &app.update {
        let room = area.width.saturating_sub(4) as usize;
        for line in u.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {}", clip(&line, room)),
                Style::default().fg(ACCENT),
            )));
        }
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn review(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::TOP).title(Span::styled(
        format!(
            " {} — {} items ",
            app.review_group.title(),
            app.review_rows.len()
        ),
        Style::default().fg(DIM),
    ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.review_rows.is_empty() {
        f.render_widget(
            Paragraph::new("  nothing needs a decision").style(Style::default().fg(DIM)),
            inner,
        );
        return;
    }

    // Column widths are derived from the actual area rather than fixed, so the
    // row does not silently clip in a split pane. Name and reason share
    // whatever is left after the fixed-width size and tier columns.
    let total = inner.width.max(30) as usize;
    let fixed = 2 + 11 + 13; // marker + size + tier
    let flexible = total.saturating_sub(fixed).max(12);
    let name_w = (flexible * 45 / 100).clamp(10, 44);
    let why_w = flexible.saturating_sub(name_w).max(8);

    // Keep the cursor on screen without a full scrollbar implementation.
    let height = inner.height as usize;
    let mut lines: Vec<Line> = Vec::new();
    let start = app.review_index.saturating_sub(height / 3);

    for (i, row) in app.review_rows.iter().enumerate().skip(start) {
        let selected = i == app.review_index;
        let ticked = app.selected.contains(&i);
        let marker = match (selected, ticked) {
            (true, true) => "❱▪",
            (true, false) => "❱ ",
            (false, true) => " ▪",
            (false, false) => "  ",
        };
        let tier_style = match row.tier {
            Tier::Orphan => Style::default().fg(WARN),
            Tier::Repullable => Style::default().fg(GOOD),
            Tier::Rebuildable | Tier::Stale => Style::default().fg(WARN),
            _ => Style::default().fg(DIM),
        };
        let name_style = if selected {
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        lines.push(Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("{:<name_w$}", clip(&row.name, name_w)), name_style),
            Span::styled(
                format!(
                    "{:>9}  ",
                    row.size.map(|b| b.human()).unwrap_or_else(|| "—".into())
                ),
                Style::default().fg(DIM),
            ),
            Span::styled(format!("{:<13}", row.tier.as_str()), tier_style),
            Span::styled(clip(&row.because, why_w), Style::default().fg(DIM)),
        ]));

        if app.expanded.contains(&i) {
            if let Reversibility::Gone = row.reversibility {
                lines.push(Line::from(Span::styled(
                    "      irreversible — deleting this cannot be undone until the vault exists",
                    Style::default().fg(BAD),
                )));
            }
            // Provenance first — it is what justifies the verdict to a person.
            for p in &row.provenance {
                lines.push(Line::from(Span::styled(
                    format!("      {}", clip(p, total.saturating_sub(8))),
                    Style::default().fg(WARN),
                )));
            }
            for e in &row.evidence {
                lines.push(Line::from(Span::styled(
                    format!("      {}", clip(e, total.saturating_sub(8))),
                    Style::default().fg(DIM),
                )));
            }
        }

        if lines.len() >= height {
            break;
        }
    }

    f.render_widget(Paragraph::new(lines), inner);
}

/// The last look before accepting any non-free cost.
///
/// States, in full: how many rows, how much, how many will be copied to the
/// vault first, and — if any cannot be preserved — that acting would lose them.
/// Nothing here is a surprise by the time the user presses a key.
fn confirm(f: &mut Frame, area: Rect, app: &App) {
    let rows = app.selected_rows();
    let vaulted = app.selected_needing_vault();
    let unpreservable = app.selected_unpreservable();

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  about to act on "),
            Span::styled(
                format!("{} items", rows.len()),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(" · {}", app.selected_bytes().human())),
        ]),
        Line::from(""),
    ];

    for r in rows.iter().take(10) {
        let tag = if r.reversibility.is_gone() && r.kind == ResourceKind::Volume {
            Span::styled("  copied to the vault first", Style::default().fg(GOOD))
        } else if r.reversibility.is_gone() {
            Span::styled("  CANNOT be preserved", Style::default().fg(BAD))
        } else if r.tier == Tier::Repullable {
            Span::styled("  must be pulled again", Style::default().fg(GOOD))
        } else if r.tier == Tier::Rebuildable {
            Span::styled("  must be rebuilt", Style::default().fg(WARN))
        } else {
            Span::styled("  recreated on next up", Style::default().fg(DIM))
        };
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::raw(format!("{:<30}", clip(&r.name, 30))),
            Span::styled(
                format!(
                    "{:>10}",
                    r.size.map(|b| b.human()).unwrap_or_else(|| "—".into())
                ),
                Style::default().fg(DIM),
            ),
            tag,
        ]));
    }
    if rows.len() > 10 {
        lines.push(Line::from(format!("    … and {} more", rows.len() - 10)));
    }

    lines.push(Line::from(""));
    if vaulted > 0 {
        lines.push(Line::from(Span::styled(
            format!("  {vaulted} will be copied to the vault and verified before removal."),
            Style::default().fg(GOOD),
        )));
        lines.push(Line::from(Span::styled(
            "  If a copy cannot be made, that item is left exactly where it is.",
            Style::default().fg(DIM),
        )));
    }
    if !unpreservable.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                "  {} cannot be preserved — acting on those loses them for good.",
                unpreservable.len()
            ),
            Style::default().fg(BAD),
        )));
    }
    if rows.iter().any(|r| r.tier == Tier::Repullable) {
        lines.push(Line::from(Span::styled(
            "  Re-pulling costs bandwidth and time on the next use.",
            Style::default().fg(DIM),
        )));
    }
    if rows.iter().any(|r| r.tier == Tier::Rebuildable) {
        lines.push(Line::from(Span::styled(
            "  Rebuilding costs time; an old network-based build may no longer reproduce.",
            Style::default().fg(WARN),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  press y to go ahead, anything else to go back",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn applying(f: &mut Frame, area: Rect, app: &App) {
    let progress = if app.apply_total > 0 {
        format!("  Applying cleanup  {}/{}", app.apply_done, app.apply_total)
    } else {
        "  Preparing cleanup".into()
    };
    let mut lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            progress,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("  {}", app.status),
            Style::default().fg(WARN),
        )),
        Line::from(""),
        Line::from(Span::styled("  Recent activity", Style::default().fg(DIM))),
    ];

    let max_width = usize::from(area.width).saturating_sub(4);
    for entry in app.apply_log() {
        let colour = if entry.starts_with('✓') {
            GOOD
        } else if entry.starts_with('!') {
            BAD
        } else {
            DIM
        };
        lines.push(Line::from(Span::styled(
            format!("  {}", clip(entry, max_width)),
            Style::default().fg(colour),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Each item is re-checked immediately before removal. q cancels after the current item.",
        Style::default().fg(DIM),
    )));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn finished(f: &mut Frame, area: Rect, app: &App) {
    let Some(r) = &app.receipt else {
        return f.render_widget(
            Paragraph::new(format!("  {}", app.status)).style(Style::default().fg(BAD)),
            area,
        );
    };

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                r.docker_reported.human(),
                Style::default().fg(GOOD).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" reclaimed, as Docker counts it", Style::default().fg(DIM)),
        ]),
        Line::from(""),
        Line::from(format!(
            "  deleted {}   skipped {}   problems {}",
            r.deleted(),
            r.skipped(),
            r.problems()
        )),
    ];

    if r.skipped() > 0 {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Skipped items changed between planning and applying, so they were left alone.",
            Style::default().fg(DIM),
        )));
    }

    lines.push(Line::from(""));
    let unlocked = app.reclaim_follow_up_images();
    if unlocked > 0 {
        let noun = if unlocked == 1 { "image" } else { "images" };
        lines.push(Line::from(Span::styled(
            format!(
                "  Next: scan again to review up to {unlocked} {noun} this Reclaim may have unlocked."
            ),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "  Next: scan again to refresh what remains and continue cleanup.",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(Span::styled(
        "  Press Enter to continue.",
        Style::default().fg(ACCENT),
    )));

    for i in &r.items {
        use prune_juice_core::execute::ItemOutcome;
        match &i.outcome {
            ItemOutcome::Refused(m) => lines.push(Line::from(Span::styled(
                format!("    refused  {}  {m}", clip(&i.name, 30)),
                Style::default().fg(WARN),
            ))),
            ItemOutcome::Failed(m) => lines.push(Line::from(Span::styled(
                format!("    FAILED   {}  {m}", clip(&i.name, 30)),
                Style::default().fg(BAD),
            ))),
            _ => {}
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Docker's figure is logical. Actual host reclamation can differ on a",
        Style::default().fg(DIM),
    )));
    lines.push(Line::from(Span::styled(
        "  VM-backed runtime; host measurement arrives in a later milestone.",
        Style::default().fg(DIM),
    )));

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

fn hints(f: &mut Frame, area: Rect, app: &App) {
    let text = match app.screen {
        Screen::Scanning => "q quit",
        Screen::Main => "↑↓ move   ↵ select   q quit",
        Screen::Review => "↑↓ move   space tick   a all   e evidence   d act on ticked   esc back",
        Screen::Confirm => "y go ahead   any other key cancels",
        Screen::Applying => "working…   q cancel safely",
        Screen::Finished => "↵ continue (scan again)   r scan again   q quit",
    };
    f.render_widget(
        Paragraph::new(Span::styled(format!("  {text}"), Style::default().fg(DIM))),
        area,
    );
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let head: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Key, Screen};
    use prune_juice_core::docker::DaemonIdentity;
    use prune_juice_core::event::ApplyStage;
    use prune_juice_core::execute::Receipt;
    use prune_juice_core::model::{
        DaemonId, Recovery, ResourceKind, ResourceSummary, RuntimeFlavor, Totals,
    };
    use prune_juice_core::plan::Planner;
    use prune_juice_core::scan::{Attributed, ScanReport};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const NOW: i64 = 1_800_000_000;

    fn demo_app() -> App {
        let mut net = ResourceSummary::new(ResourceKind::Network, "n1", "proj_default");
        net.created_unix = Some(NOW - 90 * 24 * 3600);
        net.size = Some(Bytes(1024));

        let mut orphan = ResourceSummary::new(ResourceKind::Volume, "v1", "nbk_mysql");
        orphan.created_unix = Some(NOW - 400 * 24 * 3600);
        orphan.size = Some(Bytes(379_000_000));

        let mut image = ResourceSummary::new(ResourceKind::Image, "sha256:i1", "wordpress:latest");
        image.created_unix = Some(NOW - 90 * 24 * 3600);
        image.size = Some(Bytes(2_000_000_000));

        let report = ScanReport {
            daemon: DaemonIdentity {
                id: DaemonId("D".into()),
                api_version: "1.54".into(),
                server_version: "29.4.0".into(),
                runtime: RuntimeFlavor::OrbStack,
                data_root: None,
                local: true,
                swarm_active: false,
            },
            context: "orbstack".into(),
            totals: Totals {
                containers: 255,
                volumes: 259,
                build_cache_bytes: Bytes(12_100_000_000),
                ..Default::default()
            },
            resources: vec![
                Attributed {
                    resource: net,
                    claims: vec![],
                    owner: None,
                    confidence: None,
                    liveness: None,
                    orphan_candidate: false,
                    unattributed: false,
                    content: None,
                    recovery: None,
                },
                Attributed {
                    resource: orphan,
                    claims: vec![],
                    owner: Some("nbk".into()),
                    confidence: None,
                    liveness: None,
                    orphan_candidate: true,
                    unattributed: false,
                    content: None,
                    recovery: None,
                },
                Attributed {
                    resource: image,
                    claims: vec![],
                    owner: None,
                    confidence: None,
                    liveness: None,
                    orphan_candidate: false,
                    unattributed: false,
                    content: None,
                    recovery: Some(Recovery::Pull("wordpress@sha256:digest".into())),
                },
            ],
            projects_known: 88,
            duration_ms: 3300,
            provenance_checkpointed: false,
            stale: false,
            warnings: vec![],
        };
        let plan = Planner::plan(&report, NOW);
        let mut app = App::new();
        app.ready(report, plan);
        app
    }

    /// Render at a given size and return the flattened text.
    fn render_at(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, app)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn every_screen_renders_without_panicking() {
        let mut app = demo_app();
        for screen in [
            Screen::Scanning,
            Screen::Main,
            Screen::Review,
            Screen::Applying,
            Screen::Finished,
        ] {
            app.screen = screen;
            let _ = render_at(&app, 100, 30);
        }
    }

    #[test]
    fn finished_screen_makes_continuation_the_obvious_next_step() {
        let mut app = demo_app();
        app.receipt = Some(Receipt {
            daemon: DaemonId("D".into()),
            started_unix: NOW,
            simulated: false,
            items: vec![],
            build_cache_reclaimed: Bytes::ZERO,
            docker_reported: Bytes::ZERO,
            predicted: Bytes::ZERO,
            reclamation: None,
            cancelled: false,
        });
        app.screen = Screen::Finished;

        let out = render_at(&app, 110, 30);
        assert!(out.contains("Next: scan again"), "{out}");
        assert!(out.contains("Press Enter to continue"), "{out}");
        assert!(out.contains("q quit"), "{out}");
        assert!(!out.contains("any other key quits"), "{out}");
    }

    #[test]
    fn applying_screen_shows_live_progress_and_recent_activity() {
        let mut app = demo_app();
        app.begin_apply("re-checking Docker state…");
        app.note_activity("safety scan complete");
        app.note_apply_progress(
            ApplyStage::Removed,
            Some(ResourceKind::Container),
            Some("site-web-1".into()),
            1,
            3,
        );
        app.note_apply_progress(
            ApplyStage::Removing,
            Some(ResourceKind::Image),
            Some("site-wordpress:latest".into()),
            1,
            3,
        );

        let out = render_at(&app, 110, 30);
        assert!(out.contains("Applying cleanup  1/3"), "{out}");
        assert!(
            out.contains("removing image site-wordpress:latest"),
            "{out}"
        );
        assert!(out.contains("Recent activity"), "{out}");
        assert!(out.contains("removed  container site-web-1"), "{out}");
    }

    #[test]
    fn renders_in_a_cramped_terminal() {
        // Layout constraints that assume room can panic or silently clip. A
        // user with a split pane is not an edge case.
        let mut app = demo_app();
        for (w, h) in [(20, 5), (40, 10), (80, 3), (200, 60)] {
            for screen in [Screen::Scanning, Screen::Main, Screen::Review] {
                app.screen = screen;
                let _ = render_at(&app, w, h);
            }
        }
    }

    #[test]
    fn the_headline_states_the_safe_figure_and_that_nothing_can_be_lost() {
        let app = demo_app();
        let out = render_at(&app, 100, 30);
        assert!(out.contains("safe to reclaim"), "{out}");
        assert!(out.contains("irreversible"), "{out}");
        assert!(
            out.contains("Nothing here can be lost"),
            "the Tier 1 claim must be stated when it holds:\n{out}"
        );
    }

    #[test]
    fn the_main_screen_exposes_recoverable_cleanup_without_flags() {
        let app = demo_app();
        let out = render_at(&app, 110, 30);
        assert!(out.contains("Review pullable / rebuildable"), "{out}");
        assert!(out.contains("up to 2.0 GB"), "{out}");
    }

    #[test]
    fn recoverable_confirmation_states_the_repull_cost() {
        let mut app = demo_app();
        app.menu_index = 1;
        app.on_key(Key::Enter);
        app.on_key(Key::Char('a'));
        app.on_key(Key::Char('d'));
        let out = render_at(&app, 110, 24);
        assert!(out.contains("must be pulled again"), "{out}");
        assert!(out.contains("costs bandwidth"), "{out}");
        assert!(out.contains("press y"), "{out}");
    }

    #[test]
    fn review_rows_never_exceed_the_available_width() {
        // A fixed-width row silently clips in a split pane. Every rendered line
        // must fit the terminal it was drawn for.
        let mut app = demo_app();
        app.screen = Screen::Review;
        for w in [40u16, 60, 80, 98, 140] {
            let out = render_at(&app, w, 20);
            for line in out.lines() {
                assert!(
                    line.chars().count() <= w as usize,
                    "line of {} chars at width {w}: {line:?}",
                    line.chars().count()
                );
            }
        }
    }

    #[test]
    fn an_available_release_is_shown_without_crowding_out_the_report() {
        let mut app = demo_app();
        app.note_update(prune_juice_core::update::Notice {
            current: "0.1.0".into(),
            latest: "0.2.0".into(),
            notes_url: None,
            origin: prune_juice_core::update::Origin::Standalone,
        });
        let out = render_at(&app, 100, 30);
        assert!(out.contains("Prune Juice 0.2.0 is available"), "{out}");
        assert!(out.contains("--update"), "{out}");
        // The figures it sits under are still there.
        assert!(out.contains("safe to reclaim"), "{out}");
    }

    #[test]
    fn the_update_notice_fits_every_width_like_every_other_line() {
        let mut app = demo_app();
        app.note_update(prune_juice_core::update::Notice {
            current: "0.1.0".into(),
            latest: "0.2.0".into(),
            notes_url: None,
            origin: prune_juice_core::update::Origin::Standalone,
        });
        for w in [20u16, 40, 60, 80, 140] {
            let out = render_at(&app, w, 30);
            for line in out.lines() {
                assert!(
                    line.chars().count() <= w as usize,
                    "line of {} chars at width {w}: {line:?}",
                    line.chars().count()
                );
            }
        }
    }

    #[test]
    fn review_shows_the_orphan_with_its_reason() {
        let mut app = demo_app();
        app.screen = Screen::Review;
        let out = render_at(&app, 120, 30);
        assert!(out.contains("nbk_mysql"), "{out}");
        assert!(out.contains("orphan"), "{out}");
    }

    #[test]
    fn expanding_a_row_reveals_its_evidence_and_irreversibility() {
        let mut app = demo_app();
        app.screen = Screen::Review;
        let before = render_at(&app, 120, 30);
        app.on_key(Key::Char('e'));
        let after = render_at(&app, 120, 30);
        assert_ne!(before, after, "expanding must change what is drawn");
        assert!(
            after.contains("irreversible"),
            "an orphaned volume cannot be undone yet, and must say so:\n{after}"
        );
    }

    #[test]
    fn the_confirm_screen_spells_out_what_will_happen() {
        let mut app = demo_app();
        app.screen = Screen::Review;
        app.on_key(Key::Char('a'));
        app.screen = Screen::Confirm;
        let out = render_at(&app, 100, 26);
        assert!(out.contains("about to act on"), "{out}");
        assert!(
            out.contains("vault"),
            "the user must be told a copy is taken first:\n{out}"
        );
        assert!(out.contains("press y"), "{out}");
    }

    #[test]
    fn ticked_rows_are_visibly_marked() {
        let mut app = demo_app();
        app.screen = Screen::Review;
        let before = render_at(&app, 100, 26);
        app.on_key(Key::Char(' '));
        let after = render_at(&app, 100, 26);
        assert_ne!(before, after, "a tick must be visible");
    }

    #[test]
    fn the_scanning_screen_says_nothing_is_being_changed() {
        let mut app = App::new();
        app.screen = Screen::Scanning;
        app.seen_volumes = 42;
        let out = render_at(&app, 100, 20);
        assert!(out.contains("read-only"), "{out}");
        assert!(out.contains("42"), "progressive counts must show:\n{out}");
    }
}
