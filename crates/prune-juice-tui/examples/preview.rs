//! Render every screen to a fixed-size buffer and print it.
//!
//! Lets the interface be reviewed without a terminal — useful in CI, in a
//! screenshot for the README, and when iterating on layout.
//!
//!     cargo run -p prune-juice-tui --example preview

use prune_juice_core::docker::DaemonIdentity;
use prune_juice_core::model::{
    Bytes, DaemonId, ResourceKind, ResourceSummary, RuntimeFlavor, Totals,
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
    }
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
            volumes: 259,
            volume_bytes: Bytes(35_800_000_000),
            networks: 24,
            build_cache_records: 274,
            build_cache_bytes: Bytes(12_100_000_000),
        },
        resources: vec![
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
        stale: false,
        warnings: vec![],
    };

    let plan = Planner::plan(&report, NOW);
    let mut app = App::new();
    app.ready(report, plan);

    let (w, h) = (98u16, 26u16);
    for (label, screen) in [
        ("MAIN", Screen::Main),
        ("REVIEW", Screen::Review),
        ("REVIEW (evidence expanded)", Screen::Review),
    ] {
        app.screen = screen;
        if label.contains("expanded") {
            app.on_key(Key::Char('e'));
        }
        println!("\n=== {label} ===");
        print!("{}", render(&app, w, h));
    }

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
