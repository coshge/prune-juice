//! Render every screen to a fixed-size buffer and print it.
//!
//! Lets the interface be reviewed without a terminal — useful in CI, in a
//! screenshot for the README, and when iterating on layout.
//!
//!     cargo run -p prune-juice-tui --example preview

use prune_juice_core::docker::DaemonIdentity;
use prune_juice_core::event::ApplyStage;
use prune_juice_core::model::{
    Bytes, DaemonId, Recovery, ResourceKind, ResourceSummary, RuntimeFlavor, Totals,
};
use prune_juice_core::plan::Planner;
use prune_juice_core::scan::{Attributed, ScanReport};
use prune_juice_tui::app::{App, Key, Screen};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

const NOW: i64 = 1_800_000_000;

fn res(kind: ResourceKind, name: &str, size: u64, age_days: i64) -> ResourceSummary {
    let mut r = ResourceSummary::new(kind, name, name);
    r.created_unix = Some(NOW - age_days * 86_400);
    r.size = Some(Bytes(size));
    r
}

fn attr(r: ResourceSummary, orphan: bool, owner: Option<&str>) -> Attributed {
    Attributed {
        resource: r,
        claims: vec![],
        owner: owner.map(str::to_string),
        confidence: None,
        liveness: None,
        orphan_candidate: orphan,
        unattributed: false,
        content: None,
        recovery: None,
    }
}

fn recoverable(name: &str, size: u64, recovery: Recovery) -> Attributed {
    let mut a = attr(res(ResourceKind::Image, name, size, 90), false, None);
    a.recovery = Some(recovery);
    a
}

fn main() {
    let report = ScanReport {
        daemon: DaemonIdentity {
            id: DaemonId("aeb90dba".into()),
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
            images: 144,
            image_bytes: Bytes(82_500_000_000),
            image_unique_bytes: Some(Bytes(59_300_000_000)),
            volumes: 259,
            volume_bytes: Bytes(35_800_000_000),
            networks: 24,
            build_cache_records: 274,
            build_cache_bytes: Bytes(12_100_000_000),
        },
        resources: vec![
            recoverable(
                "wordpress:latest",
                31_500_000_000,
                Recovery::Pull("wordpress@sha256:example".into()),
            ),
            recoverable(
                "local-project:latest",
                49_200_000_000,
                Recovery::Build {
                    command: "docker compose build app".into(),
                    dir: "/Users/example/project".into(),
                },
            ),
            attr(
                res(ResourceKind::Network, "redkite_default", 0, 90),
                false,
                None,
            ),
            attr(
                res(ResourceKind::Network, "fen_default", 0, 90),
                false,
                None,
            ),
            attr(
                res(ResourceKind::Volume, "nbk_mysql", 379_000_000, 400),
                true,
                Some("nbk"),
            ),
            attr(
                res(ResourceKind::Volume, "nbk_tmp", 10_500_000, 400),
                true,
                Some("nbk"),
            ),
            attr(
                res(
                    ResourceKind::Volume,
                    "saffronfields-mariadb",
                    240_500_000,
                    359,
                ),
                false,
                Some("saffronfields"),
            ),
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

    let (w, h) = (98u16, 26u16);

    app.screen = Screen::Main;
    println!("\n=== MAIN ===");
    print!("{}", render(&app, w, h));

    app.menu_index = 1;
    app.on_key(Key::Enter);
    println!("\n=== REVIEW (pullable / rebuildable) ===");
    print!("{}", render(&app, w, 12));

    app.on_key(Key::Char('a'));
    app.on_key(Key::Char('d'));
    println!("\n=== CONFIRM (pullable / rebuildable) ===");
    print!("{}", render(&app, w, 18));

    app.on_key(Key::Esc);
    app.on_key(Key::Esc);
    app.menu_index = 2;
    app.on_key(Key::Enter);
    println!("\n=== REVIEW (stale / orphaned) ===");
    print!("{}", render(&app, w, 12));

    app.on_key(Key::Char('e'));
    println!("\n=== REVIEW (evidence expanded) ===");
    print!("{}", render(&app, w, 12));

    app.on_key(Key::Char('e'));
    app.on_key(Key::Char('a'));
    println!("\n=== REVIEW (all ticked) ===");
    print!("{}", render(&app, w, 10));

    app.screen = Screen::Confirm;
    println!("\n=== CONFIRM ===");
    print!("{}", render(&app, w, 20));

    app.begin_apply("safety scan complete; beginning cleanup…");
    app.note_activity("re-checking Docker state before deleting anything…");
    app.note_apply_progress(
        ApplyStage::Removed,
        Some(ResourceKind::Container),
        Some("redkite-wordpress-1".into()),
        12,
        153,
    );
    app.note_apply_progress(
        ApplyStage::Removed,
        Some(ResourceKind::Container),
        Some("fen-wordpress-1".into()),
        13,
        153,
    );
    app.note_apply_progress(
        ApplyStage::Removing,
        Some(ResourceKind::Container),
        Some("saffronfields-wordpress-1".into()),
        13,
        153,
    );
    println!("\n=== APPLYING ===");
    print!("{}", render(&app, w, 18));

    // And the scanning screen, mid-flight.
    let mut scanning = App::new();
    scanning.screen = Screen::Scanning;
    scanning.status = "measuring volume sizes (this is the slow part)…".into();
    scanning.seen_containers = 255;
    scanning.seen_images = 144;
    scanning.seen_volumes = 259;
    scanning.seen_networks = 24;
    println!("\n=== SCANNING ===");
    print!("{}", render(&scanning, w, 12));
}

fn render(app: &App, w: u16, h: u16) -> String {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| prune_juice_tui::draw_for_preview(f, app))
        .unwrap();
    let buf = term.backend().buffer().clone();
    (0..buf.area.height)
        .map(|y| {
            let line: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            format!("|{}|\n", line.trim_end())
        })
        .collect()
}
