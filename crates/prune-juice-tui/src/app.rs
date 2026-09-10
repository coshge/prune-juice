//! Application state and key handling.
//!
//! Deliberately free of `ratatui` and `crossterm`: this module knows nothing
//! about terminals, so the whole state machine is testable without one. The
//! renderer reads this; it never owns state.

use std::collections::BTreeSet;

use prune_juice_core::execute::Receipt;
use prune_juice_core::model::{Bytes, ResourceKind};
use prune_juice_core::plan::tier::{Reversibility, Tier};
use prune_juice_core::plan::Plan;
use prune_juice_core::scan::ScanReport;

/// What the user is looking at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Scanning,
    Main,
    Review,
    /// Shown before acting on reviewed rows. The last chance to look at what
    /// is about to happen, stated in full.
    Confirm,
    Applying,
    Finished,
}

/// What the event loop should do next. Returned by key handling so the state
/// machine stays pure and the side effects live in one place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Quit,
    Rescan,
    /// Reclaim the safe tier. No confirmation dialog — pressing the key on a
    /// row that says "nothing here can be lost" *is* the confirmation.
    ReclaimFree,
    /// Act on the rows the user ticked. Only reachable from the confirm screen,
    /// because these are irreversible without the vault.
    ApplySelected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuItem {
    Reclaim,
    Review,
    Rescan,
    Quit,
}

impl MenuItem {
    pub const ALL: [MenuItem; 4] = [
        MenuItem::Reclaim,
        MenuItem::Review,
        MenuItem::Rescan,
        MenuItem::Quit,
    ];
}

/// One row in the review table.
#[derive(Clone, Debug)]
pub struct ReviewRow {
    pub kind: ResourceKind,
    pub name: String,
    pub size: Option<Bytes>,
    pub tier: Tier,
    pub owner: Option<String>,
    pub because: String,
    pub reversibility: Reversibility,
    /// Citable provenance — where this claim came from. Shown first, because
    /// "the label says nbk and that directory is gone" is what justifies the
    /// verdict to a person.
    pub provenance: Vec<String>,
    /// The raw facts the classifier consulted. Secondary, and shown only so a
    /// curious user can see the machine's working.
    pub evidence: Vec<String>,
}

pub struct App {
    pub screen: Screen,
    pub status: String,
    pub report: Option<ScanReport>,
    pub plan: Option<Plan>,
    pub receipt: Option<Receipt>,

    pub menu_index: usize,
    pub review_index: usize,
    pub review_rows: Vec<ReviewRow>,
    /// Rows whose evidence is expanded inline.
    pub expanded: BTreeSet<usize>,
    /// Rows the user has ticked for action.
    pub selected: BTreeSet<usize>,

    /// Progressive counts, so the scan screen fills in rather than hanging.
    pub seen_containers: u32,
    pub seen_images: u32,
    pub seen_volumes: u32,
    pub seen_networks: u32,

    pub should_quit: bool,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            screen: Screen::Scanning,
            status: "starting…".into(),
            report: None,
            plan: None,
            receipt: None,
            menu_index: 0,
            review_index: 0,
            review_rows: Vec::new(),
            expanded: BTreeSet::new(),
            selected: BTreeSet::new(),
            seen_containers: 0,
            seen_images: 0,
            seen_volumes: 0,
            seen_networks: 0,
            should_quit: false,
        }
    }

    pub fn note_resource(&mut self, kind: ResourceKind) {
        match kind {
            ResourceKind::Container => self.seen_containers += 1,
            ResourceKind::Image => self.seen_images += 1,
            ResourceKind::Volume => self.seen_volumes += 1,
            ResourceKind::Network => self.seen_networks += 1,
            ResourceKind::BuildCache => {}
        }
    }

    /// Hand the app a finished scan and plan.
    pub fn ready(&mut self, report: ScanReport, plan: Plan) {
        self.review_rows = build_review_rows(&plan);
        self.report = Some(report);
        self.plan = Some(plan);
        self.screen = Screen::Main;
        self.menu_index = 0;
        self.review_index = 0;
        self.expanded.clear();
        self.selected.clear();
        self.status.clear();
    }

    pub fn finished(&mut self, receipt: Receipt) {
        self.receipt = Some(receipt);
        self.screen = Screen::Finished;
    }

    pub fn free_bytes(&self) -> Bytes {
        self.plan
            .as_ref()
            .map(|p| p.free_bytes())
            .unwrap_or(Bytes::ZERO)
    }

    pub fn free_count(&self) -> usize {
        self.plan
            .as_ref()
            .map(|p| p.of_tier(Tier::Free).count())
            .unwrap_or(0)
    }

    /// Bytes in the safe tier that could not be undone.
    ///
    /// The Tier 1 Theorem says this is always zero. It is displayed rather than
    /// assumed so the claim is checked against real data on every run — and if
    /// it is ever non-zero the UI says so instead of promising safety.
    pub fn irreversible_bytes(&self) -> Bytes {
        self.plan
            .as_ref()
            .map(|p| p.irreversible_free_bytes())
            .unwrap_or(Bytes::ZERO)
    }

    pub fn menu_label(&self, item: MenuItem) -> String {
        match item {
            MenuItem::Reclaim => {
                if self.free_count() == 0 && self.free_bytes() == Bytes::ZERO {
                    "Nothing to reclaim".into()
                } else {
                    format!("Reclaim {}", self.free_bytes().human())
                }
            }
            MenuItem::Review => format!("Review {} items", self.review_rows.len()),
            MenuItem::Rescan => "Scan again".into(),
            MenuItem::Quit => "Quit".into(),
        }
    }

    pub fn menu_enabled(&self, item: MenuItem) -> bool {
        match item {
            MenuItem::Reclaim => self.free_count() > 0 || self.free_bytes() > Bytes::ZERO,
            MenuItem::Review => !self.review_rows.is_empty(),
            _ => true,
        }
    }

    /// Handle one key. Returns what the event loop should do.
    ///
    /// `key` is a plain character or a named key, so this is callable from a
    /// test without constructing a crossterm event.
    pub fn on_key(&mut self, key: Key) -> Action {
        match self.screen {
            Screen::Scanning | Screen::Applying => match key {
                // Escape hatch while work is in flight. The worker sees the
                // cancel flag and stops between items.
                Key::Char('q') | Key::Esc => {
                    self.should_quit = true;
                    Action::Quit
                }
                _ => Action::None,
            },
            Screen::Main => self.on_key_main(key),
            Screen::Review => self.on_key_review(key),
            Screen::Confirm => match key {
                // A single deliberate key, on a screen that has just spelled
                // out what will happen and what will be preserved first.
                Key::Char('y') | Key::Char('Y') => Action::ApplySelected,
                _ => {
                    self.screen = Screen::Review;
                    Action::None
                }
            },
            Screen::Finished => match key {
                Key::Char('r') => Action::Rescan,
                _ => {
                    self.should_quit = true;
                    Action::Quit
                }
            },
        }
    }

    fn on_key_main(&mut self, key: Key) -> Action {
        match key {
            Key::Up | Key::Char('k') => {
                self.menu_index = self.menu_index.saturating_sub(1);
                Action::None
            }
            Key::Down | Key::Char('j') => {
                if self.menu_index + 1 < MenuItem::ALL.len() {
                    self.menu_index += 1;
                }
                Action::None
            }
            Key::Enter => {
                let item = MenuItem::ALL[self.menu_index];
                if !self.menu_enabled(item) {
                    return Action::None;
                }
                match item {
                    MenuItem::Reclaim => Action::ReclaimFree,
                    MenuItem::Review => {
                        self.screen = Screen::Review;
                        Action::None
                    }
                    MenuItem::Rescan => Action::Rescan,
                    MenuItem::Quit => {
                        self.should_quit = true;
                        Action::Quit
                    }
                }
            }
            Key::Char('q') | Key::Esc => {
                self.should_quit = true;
                Action::Quit
            }
            _ => Action::None,
        }
    }

    fn on_key_review(&mut self, key: Key) -> Action {
        match key {
            Key::Up | Key::Char('k') => {
                self.review_index = self.review_index.saturating_sub(1);
                Action::None
            }
            Key::Down | Key::Char('j') => {
                if self.review_index + 1 < self.review_rows.len() {
                    self.review_index += 1;
                }
                Action::None
            }
            Key::Char('e') | Key::Enter => {
                if !self.expanded.remove(&self.review_index) {
                    self.expanded.insert(self.review_index);
                }
                Action::None
            }
            Key::Char(' ') => {
                if !self.selected.remove(&self.review_index) {
                    self.selected.insert(self.review_index);
                }
                Action::None
            }
            Key::Char('a') => {
                if self.selected.len() == self.review_rows.len() {
                    self.selected.clear();
                } else {
                    self.selected = (0..self.review_rows.len()).collect();
                }
                Action::None
            }
            Key::Char('d') | Key::Char('D') => {
                if self.selected.is_empty() {
                    self.status = "nothing ticked — press space to choose rows".into();
                    return Action::None;
                }
                self.screen = Screen::Confirm;
                Action::None
            }
            Key::Char('q') | Key::Esc | Key::Left => {
                self.screen = Screen::Main;
                Action::None
            }
            _ => Action::None,
        }
    }

    pub fn current_row(&self) -> Option<&ReviewRow> {
        self.review_rows.get(self.review_index)
    }

    pub fn selected_rows(&self) -> Vec<&ReviewRow> {
        self.selected
            .iter()
            .filter_map(|i| self.review_rows.get(*i))
            .collect()
    }

    pub fn selected_names(&self) -> Vec<String> {
        self.selected_rows()
            .iter()
            .map(|r| r.name.clone())
            .collect()
    }

    pub fn selected_bytes(&self) -> Bytes {
        self.selected_rows().iter().filter_map(|r| r.size).sum()
    }

    /// How many of the ticked rows need preserving before they can go.
    pub fn selected_needing_vault(&self) -> usize {
        self.selected_rows()
            .iter()
            .filter(|r| r.reversibility.is_gone() && r.kind == ResourceKind::Volume)
            .count()
    }

    /// Ticked rows that are irreversible and cannot be preserved either, so
    /// acting on them would simply lose data.
    pub fn selected_unpreservable(&self) -> Vec<&ReviewRow> {
        self.selected_rows()
            .into_iter()
            .filter(|r| r.reversibility.is_gone() && r.kind != ResourceKind::Volume)
            .collect()
    }
}

/// Keys the app understands. A tiny enum rather than crossterm's, so the state
/// machine has no terminal dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Esc,
    Char(char),
}

/// Everything needing a human decision, biggest first.
///
/// Tier 1 is deliberately absent: it is actioned from the main screen and needs
/// no review. What lands here is orphaned or stale, and in this build it is
/// **inspect-only** — deleting an orphaned volume is irreversible until the
/// vault exists, and offering that behind two keystrokes would be exactly the
/// risk this tool is supposed to remove.
fn build_review_rows(plan: &Plan) -> Vec<ReviewRow> {
    let mut rows: Vec<ReviewRow> = plan
        .items
        .iter()
        .filter(|i| matches!(i.verdict.tier, Tier::Orphan | Tier::Stale))
        .map(|i| ReviewRow {
            kind: i.kind,
            name: i.name.clone(),
            size: i.size,
            tier: i.verdict.tier,
            owner: i.owner.clone(),
            because: i.verdict.because.clone(),
            reversibility: i.verdict.reversibility.clone(),
            provenance: i.provenance.clone(),
            evidence: i
                .evidence
                .facts()
                .map(|f| format!("{} = {}", f.key, f.value))
                .collect(),
        })
        .collect();

    // Orphans first — they are the actionable finding — then by size.
    rows.sort_by_key(|r| {
        (
            r.tier != Tier::Orphan,
            std::cmp::Reverse(r.size.unwrap_or(Bytes::ZERO).get()),
        )
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use prune_juice_core::docker::DaemonIdentity;
    use prune_juice_core::model::{DaemonId, ResourceSummary, RuntimeFlavor, Totals};
    use prune_juice_core::plan::Planner;
    use prune_juice_core::scan::Attributed;

    const NOW: i64 = 1_800_000_000;
    const OLD: i64 = NOW - 90 * 24 * 3600;

    fn attributed(r: ResourceSummary, orphan: bool) -> Attributed {
        Attributed {
            resource: r,
            claims: vec![],
            owner: if orphan { Some("gone".into()) } else { None },
            confidence: None,
            liveness: None,
            orphan_candidate: orphan,
            unattributed: false,
            content: None,
            recovery: None,
        }
    }

    fn network(name: &str) -> ResourceSummary {
        let mut r = ResourceSummary::new(ResourceKind::Network, name, name);
        r.created_unix = Some(OLD);
        r.size = Some(Bytes(1000));
        r
    }

    fn report(resources: Vec<Attributed>) -> ScanReport {
        ScanReport {
            daemon: DaemonIdentity {
                id: DaemonId("D".into()),
                api_version: "1.54".into(),
                server_version: "29".into(),
                runtime: RuntimeFlavor::OrbStack,
                data_root: None,
                local: true,
                swarm_active: false,
            },
            context: "t".into(),
            totals: Totals {
                build_cache_bytes: Bytes(12_100_000_000),
                ..Default::default()
            },
            resources,
            projects_known: 0,
            duration_ms: 10,
            stale: false,
            warnings: vec![],
        }
    }

    fn ready_app(resources: Vec<Attributed>) -> App {
        let rep = report(resources);
        let plan = Planner::plan(&rep, NOW);
        let mut app = App::new();
        app.ready(rep, plan);
        app
    }

    #[test]
    fn starts_on_the_scanning_screen() {
        let app = App::new();
        assert_eq!(app.screen, Screen::Scanning);
        assert!(!app.should_quit);
    }

    #[test]
    fn quitting_works_from_every_screen() {
        for screen in [
            Screen::Scanning,
            Screen::Main,
            Screen::Review,
            Screen::Applying,
        ] {
            let mut app = ready_app(vec![]);
            app.screen = screen;
            let action = app.on_key(Key::Char('q'));
            // Review treats q as "back", which is the conventional shape.
            if screen == Screen::Review {
                assert_eq!(app.screen, Screen::Main);
            } else {
                assert_eq!(action, Action::Quit, "{screen:?}");
                assert!(app.should_quit, "{screen:?}");
            }
        }
    }

    #[test]
    fn a_scan_moves_us_to_the_main_screen() {
        let app = ready_app(vec![attributed(network("proj_default"), false)]);
        assert_eq!(app.screen, Screen::Main);
        assert!(app.free_count() > 0);
    }

    #[test]
    fn reclaim_is_disabled_when_there_is_nothing_to_reclaim() {
        let rep = ScanReport {
            totals: Totals::default(), // no build cache either
            ..report(vec![])
        };
        let plan = Planner::plan(&rep, NOW);
        let mut app = App::new();
        app.ready(rep, plan);

        assert!(!app.menu_enabled(MenuItem::Reclaim));
        assert_eq!(app.menu_label(MenuItem::Reclaim), "Nothing to reclaim");

        // Pressing enter on a disabled item must do nothing at all.
        app.menu_index = 0;
        assert_eq!(app.on_key(Key::Enter), Action::None);
    }

    #[test]
    fn enter_on_reclaim_asks_for_no_confirmation() {
        // Tier 1's whole promise is that this needs no dialog. If a
        // confirmation step ever creeps in, this test should be the thing that
        // forces the conversation.
        let mut app = ready_app(vec![attributed(network("proj_default"), false)]);
        app.menu_index = 0;
        assert_eq!(app.on_key(Key::Enter), Action::ReclaimFree);
    }

    #[test]
    fn the_safe_tier_reports_zero_irreversible_bytes() {
        let app = ready_app(vec![attributed(network("proj_default"), false)]);
        assert_eq!(app.irreversible_bytes(), Bytes::ZERO);
    }

    #[test]
    fn review_lists_orphans_before_stale_items() {
        let mut orphan = network("orphan_default");
        orphan.size = Some(Bytes(1));
        let app = ready_app(vec![
            attributed(network("keeps_default"), false),
            attributed(orphan, true),
        ]);
        assert!(!app.review_rows.is_empty());
        assert_eq!(app.review_rows[0].tier, Tier::Orphan);
    }

    #[test]
    fn navigation_cannot_run_off_either_end() {
        let mut app = ready_app(vec![attributed(network("proj_default"), false)]);

        // Up at the top stays at the top.
        app.on_key(Key::Up);
        assert_eq!(app.menu_index, 0);

        // Down past the end stays at the end.
        for _ in 0..20 {
            app.on_key(Key::Down);
        }
        assert_eq!(app.menu_index, MenuItem::ALL.len() - 1);
    }

    #[test]
    fn evidence_expands_and_collapses() {
        let mut orphan = network("orphan_default");
        orphan.size = Some(Bytes(5));
        let mut app = ready_app(vec![attributed(orphan, true)]);
        app.screen = Screen::Review;

        assert!(app.expanded.is_empty());
        app.on_key(Key::Char('e'));
        assert!(app.expanded.contains(&0));
        app.on_key(Key::Char('e'));
        assert!(app.expanded.is_empty());
    }

    #[test]
    fn review_rows_carry_their_provenance() {
        // A row a user is asked to judge must show where the claim came from,
        // not only the classifier's internal working.
        let mut orphan = network("orphan_default");
        orphan.size = Some(Bytes(5));
        let mut a = attributed(orphan, true);
        a.claims = vec![prune_juice_core::model::Claim {
            project: prune_juice_core::model::ProjectId("/gone".into()),
            project_name: "gone".into(),
            provider: prune_juice_core::model::ProviderKind::Compose,
            confidence: prune_juice_core::model::Confidence::Strong,
            root: None,
            liveness: prune_juice_core::model::Liveness::Absent {
                since_unix: None,
                scans: 3,
            },
            evidence: vec![prune_juice_core::model::Evidence::new(
                prune_juice_core::model::EvidenceSource::Label,
                "com.docker.compose.project = gone",
            )],
        }];
        let app = ready_app(vec![a]);
        assert!(app.review_rows[0]
            .provenance
            .iter()
            .any(|p| p.starts_with("[label]")));
    }

    #[test]
    fn review_rows_carry_their_evidence() {
        let mut orphan = network("orphan_default");
        orphan.size = Some(Bytes(5));
        let app = ready_app(vec![attributed(orphan, true)]);
        assert!(
            !app.review_rows[0].evidence.is_empty(),
            "a reviewable row with no evidence is not reviewable"
        );
    }

    #[test]
    fn escape_from_review_returns_to_the_menu() {
        let mut app = ready_app(vec![attributed(network("proj_default"), false)]);
        app.screen = Screen::Review;
        app.on_key(Key::Esc);
        assert_eq!(app.screen, Screen::Main);
        assert!(!app.should_quit, "back is not quit");
    }

    fn reviewable_app() -> App {
        let mut orphan = network("nbk_mysql");
        orphan.kind = ResourceKind::Volume;
        orphan.size = Some(Bytes(379_000_000));
        let mut second = network("nbk_tmp");
        second.kind = ResourceKind::Volume;
        second.size = Some(Bytes(10_500_000));
        ready_app(vec![attributed(orphan, true), attributed(second, true)])
    }

    #[test]
    fn space_ticks_a_row_and_ticking_again_unticks_it() {
        let mut app = reviewable_app();
        app.screen = Screen::Review;
        assert!(app.selected.is_empty());
        app.on_key(Key::Char(' '));
        assert_eq!(app.selected.len(), 1);
        app.on_key(Key::Char(' '));
        assert!(app.selected.is_empty());
    }

    #[test]
    fn a_selects_all_then_clears() {
        let mut app = reviewable_app();
        app.screen = Screen::Review;
        app.on_key(Key::Char('a'));
        assert_eq!(app.selected.len(), app.review_rows.len());
        app.on_key(Key::Char('a'));
        assert!(app.selected.is_empty());
    }

    #[test]
    fn acting_with_nothing_ticked_does_nothing_and_says_why() {
        let mut app = reviewable_app();
        app.screen = Screen::Review;
        assert_eq!(app.on_key(Key::Char('d')), Action::None);
        assert_eq!(
            app.screen,
            Screen::Review,
            "must not reach a confirm screen"
        );
        assert!(app.status.contains("nothing ticked"), "{}", app.status);
    }

    #[test]
    fn acting_on_ticked_rows_goes_via_a_confirm_screen() {
        // Never straight from a list keypress to a deletion.
        let mut app = reviewable_app();
        app.screen = Screen::Review;
        app.on_key(Key::Char(' '));
        assert_eq!(app.on_key(Key::Char('d')), Action::None);
        assert_eq!(app.screen, Screen::Confirm);
    }

    #[test]
    fn only_y_confirms_and_anything_else_backs_out() {
        let mut app = reviewable_app();
        app.screen = Screen::Review;
        app.on_key(Key::Char(' '));
        app.on_key(Key::Char('d'));
        assert_eq!(app.on_key(Key::Char('y')), Action::ApplySelected);

        // Any other key must cancel, including a stray return.
        for k in [
            Key::Enter,
            Key::Esc,
            Key::Char('n'),
            Key::Char('x'),
            Key::Down,
        ] {
            let mut app = reviewable_app();
            app.screen = Screen::Review;
            app.on_key(Key::Char(' '));
            app.on_key(Key::Char('d'));
            assert_eq!(app.on_key(k), Action::None, "{k:?} must not confirm");
            assert_eq!(app.screen, Screen::Review, "{k:?} must go back");
        }
    }

    #[test]
    fn the_confirm_screen_knows_what_needs_preserving() {
        let mut app = reviewable_app();
        app.screen = Screen::Review;
        app.on_key(Key::Char('a'));
        assert_eq!(app.selected_names().len(), 2);
        assert_eq!(app.selected_bytes(), Bytes(389_500_000));
        assert_eq!(
            app.selected_needing_vault(),
            2,
            "both are irreversible volumes, so both get copied first"
        );
        assert!(app.selected_unpreservable().is_empty());
    }

    #[test]
    fn a_rescan_clears_any_ticks() {
        // Selections index into a list that a rescan replaces. Carrying them
        // over would act on whatever happened to land at those positions.
        let mut app = reviewable_app();
        app.screen = Screen::Review;
        app.on_key(Key::Char('a'));
        assert!(!app.selected.is_empty());

        let rep = report(vec![]);
        let plan = Planner::plan(&rep, NOW);
        app.ready(rep, plan);
        assert!(app.selected.is_empty());
    }

    #[test]
    fn progressive_counts_accumulate_during_a_scan() {
        let mut app = App::new();
        app.note_resource(ResourceKind::Volume);
        app.note_resource(ResourceKind::Volume);
        app.note_resource(ResourceKind::Container);
        assert_eq!(app.seen_volumes, 2);
        assert_eq!(app.seen_containers, 1);
    }
}
