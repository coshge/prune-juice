//! Prune Juice CLI.
//!
//! House convention: stdout carries the payload, stderr carries human progress,
//! so `prune-juice --json | jq` composes. Dry-run is the default; `--apply` has
//! to be asked for.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use prune_juice_core::docker::bollard_client::BollardClient;
use prune_juice_core::docker::{context, DockerClient};
use prune_juice_core::event::{Cancel, JsonlSink, NullSink};
use prune_juice_core::execute::{ExecuteOptions, Executor, ItemOutcome, Mode, Receipt};
use prune_juice_core::model::{Bytes, Confidence, Liveness, ResourceKind};
use prune_juice_core::plan::tier::{Reversibility, Tier};
use prune_juice_core::plan::{Plan, Planner};
use prune_juice_core::scan::{ScanOptions, ScanReport, Scanner};
use prune_juice_core::Error;
use prune_juice_tui::TuiOptions;

const USAGE: &str = "\
prune-juice — reclaim Docker disk with provenance

USAGE:
    prune-juice [OPTIONS]

OPTIONS:
    --apply             Actually delete. Without this, nothing is touched.
    --tiers <LIST>      Comma-separated: free,orphan,stale  (default: free)
    --only-label <K=V>  Refuse to touch anything without this label
    --json              NDJSON on stdout, progress on stderr
    --no-sizes          Skip volume sizing (the expensive call)
    --roots <PATHS>     Colon-separated dirs to search for projects
    --context <NAME>    Scan only this context
    --no-tui            Force the one-shot report even on a terminal
    --no-probe          Skip reading volume contents. No volume can then be
                        proven safe, so this only ever shrinks what is offered.
    -h, --help          Show this help

With no arguments on a terminal, `prune-juice` opens an interactive
interface. Piped, redirected, or under CI it prints a one-shot report
instead, so it composes in a script without special-casing.

Tier `free` is the only one safe without review. Asking for `orphan` or
`stale` on the command line skips a review step that exists for a reason.

EXIT CODES:
    0  nothing needing attention
    1  reclaimable space or unattributed resources found
    2  usage error
    3  daemon unreachable
    4  permission denied
    5  partial failure — something was refused or failed
  130  cancelled
";

struct Args {
    json: bool,
    with_sizes: bool,
    apply: bool,
    tiers: Vec<Tier>,
    only_label: Option<(String, String)>,
    roots: Vec<PathBuf>,
    only_context: Option<String>,
    no_tui: bool,
    no_probe: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        json: false,
        with_sizes: true,
        apply: false,
        tiers: vec![Tier::Free],
        only_label: None,
        roots: default_roots(),
        only_context: None,
        no_tui: false,
        no_probe: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => a.json = true,
            "--no-sizes" => a.with_sizes = false,
            "--no-tui" => a.no_tui = true,
            "--no-probe" => a.no_probe = true,
            "--apply" => a.apply = true,
            "--tiers" => {
                let v = it.next().ok_or("--tiers needs a value")?;
                a.tiers = v
                    .split(',')
                    .map(|s| match s.trim() {
                        "free" => Ok(Tier::Free),
                        "orphan" => Ok(Tier::Orphan),
                        "stale" => Ok(Tier::Stale),
                        other => Err(format!("unknown tier: {other}")),
                    })
                    .collect::<Result<_, _>>()?;
            }
            "--only-label" => {
                let v = it.next().ok_or("--only-label needs K=V")?;
                let (k, val) = v.split_once('=').ok_or("--only-label must be K=V")?;
                a.only_label = Some((k.to_string(), val.to_string()));
            }
            "--roots" => {
                let v = it.next().ok_or("--roots needs a value")?;
                a.roots = v
                    .split(':')
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .collect();
            }
            "--context" => a.only_context = Some(it.next().ok_or("--context needs a value")?),
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(a)
}

fn default_roots() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(h) = std::env::var("HOME") {
        let h = PathBuf::from(h);
        for c in [
            "Documents/Repos",
            "Repos",
            "repos",
            "dev",
            "code",
            "src",
            "Sites",
            "Projects",
            "work",
        ] {
            let p = h.join(c);
            if p.is_dir() {
                out.push(p);
            }
        }
    }
    out
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    match run(&args) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e}");
            if let Some(h) = e.hint() {
                eprintln!("  {h}");
            }
            std::process::exit(e.exit_code());
        }
    }
}

/// Interactive by default, but only when there is a human on the other end and
/// no flag has already asked for a specific machine-readable behaviour.
fn wants_tui(args: &Args) -> bool {
    !args.no_tui
        && !args.json
        && !args.apply
        && io::stdout().is_terminal()
        && io::stdin().is_terminal()
        && std::env::var("CI").is_err()
}

fn run(args: &Args) -> Result<i32, Error> {
    let cancel = Cancel::new();

    // Contexts are candidate endpoints, not identities. Two can be the same
    // engine, so we connect, ask each daemon for its own /info ID, and dedupe.
    let contexts: Vec<_> = context::discover()
        .into_iter()
        .filter(|c| args.only_context.as_deref().is_none_or(|n| c.name == n))
        .filter(|c| c.is_local())
        .collect();
    if contexts.is_empty() {
        return Err(Error::NoContext);
    }

    let opts = ScanOptions {
        project_roots: args.roots.clone(),
        with_sizes: args.with_sizes,
        probe_volumes: !args.no_probe,
    };

    if wants_tui(args) {
        // The interface scans on its own thread, so it takes an endpoint rather
        // than a connected client. The first local context wins; multi-daemon
        // selection lives in the settings screen, not the launch path.
        let ctx = contexts.first().expect("checked non-empty above");
        return prune_juice_tui::run(TuiOptions {
            endpoint: ctx.endpoint.clone(),
            context: ctx.name.clone(),
            scan: opts,
        });
    }

    let mut seen: Vec<String> = Vec::new();
    let mut exit = 0;
    let mut any = false;
    let mut last_err = None;

    for ctx in &contexts {
        let client = match BollardClient::connect(&ctx.endpoint) {
            Ok(c) => c,
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        };
        let ident = match client.identity() {
            Ok(i) => i,
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        };
        if seen.iter().any(|d| d == ident.id.as_str()) {
            if !args.json {
                eprintln!("  {} is the same engine as one already scanned", ctx.name);
            }
            continue;
        }
        seen.push(ident.id.0.clone());
        any = true;

        let sink: Arc<dyn prune_juice_core::EventSink> = if args.json {
            Arc::new(JsonlSink::new(io::stdout()))
        } else {
            Arc::new(NullSink)
        };

        if !args.json {
            eprint!("scanning {}… ", ctx.name);
            io::stderr().flush().ok();
        }
        let report = Scanner::new(&client).scan(&ctx.name, &opts, sink.clone(), &cancel)?;
        if !args.json {
            eprintln!("{} ms", report.duration_ms);
        }

        let plan = Planner::plan(&report, now_unix());
        if args.json {
            for i in &plan.items {
                sink.emit(prune_juice_core::Event::Classified {
                    kind: i.kind.as_str().to_string(),
                    name: i.name.clone(),
                    tier: i.verdict.tier.as_str().to_string(),
                    because: i.verdict.because.clone(),
                    size: i.size,
                    owner: i.owner.clone(),
                    provenance: i.provenance.clone(),
                });
            }
        } else {
            render(&report, &plan);
        }

        // The executor ALWAYS runs. Without --apply it runs in dry-run mode,
        // which exercises the identical code path — including per-item
        // revalidation — and reports exactly what would have happened. That is
        // what makes dry-run an oracle for the apply path rather than a
        // separate, less-tested branch.
        if args.apply && !args.json {
            eprint!("re-checking before applying… ");
            io::stderr().flush().ok();
        }
        // Re-scan so witnesses are revalidated against a freshly taken world.
        let fresh = Scanner::new(&client).scan(&ctx.name, &opts, sink.clone(), &cancel)?;
        if args.apply && !args.json {
            eprintln!("ok");
        }

        let exec_opts = ExecuteOptions {
            mode: if args.apply {
                Mode::Apply
            } else {
                Mode::DryRun
            },
            tiers: args.tiers.clone(),
            only_label: args.only_label.clone(),
        };
        let receipt = if args.apply {
            Executor::applying(&client).run(plan, &fresh, now_unix(), &exec_opts, sink, &cancel)?
        } else {
            // No mutating client at all: dry-run is unable to delete, not
            // merely disinclined to.
            Executor::dry_run().run(plan, &fresh, now_unix(), &exec_opts, sink, &cancel)?
        };

        if !args.json {
            render_receipt(&receipt);
        }
        if receipt.problems() > 0 {
            exit = exit.max(5);
        }
        if !args.apply && (receipt.docker_reported > Bytes::ZERO || receipt.skipped() > 0) {
            exit = exit.max(1);
        }
    }

    if !any {
        return Err(last_err.unwrap_or(Error::NoContext));
    }
    Ok(exit)
}

fn render(r: &ScanReport, plan: &Plan) {
    let d = &r.daemon;
    println!();
    println!(
        "  {}  ·  {:?}  ·  engine {} (API {})",
        r.context, d.runtime, d.server_version, d.api_version
    );
    if d.swarm_active {
        println!("  ! swarm is active — some resources cannot be proven unreferenced");
    }
    for w in &r.warnings {
        println!("  ! {w}");
    }

    let t = &r.totals;
    println!();
    println!(
        "  {:>5} containers   {:>5} images ({})   {:>5} volumes ({})",
        t.containers,
        t.images,
        t.image_bytes.human(),
        t.volumes,
        t.volume_bytes.human()
    );
    println!(
        "  {:>5} networks     {:>5} build cache records ({} reclaimable)",
        t.networks,
        t.build_cache_records,
        t.build_cache_bytes.human()
    );

    // The reversibility breakdown is stated at run level, not per item. For
    // Tier 1 the last line is always true, and the word "irreversible" only
    // ever appears beside a non-zero number.
    let free = plan.free_bytes();
    let irreversible = plan.irreversible_free_bytes();
    let free_items = plan.of_tier(Tier::Free).count();

    println!();
    println!("  SAFE TO RECLAIM — {}", free.human());
    let (mut rebuildable, mut restorable) = (Bytes::ZERO, Bytes::ZERO);
    for i in plan.of_tier(Tier::Free) {
        let b = i.size.unwrap_or(Bytes::ZERO);
        match i.reversibility() {
            Reversibility::Rebuildable(_) => rebuildable = rebuildable + b,
            Reversibility::Restorable(_) => restorable = restorable + b,
            Reversibility::Gone => {}
        }
    }
    rebuildable = rebuildable + plan.build_cache_reclaimable;
    println!(
        "    {:>10}  rebuildable   ({free_items} items + build cache)",
        rebuildable.human()
    );
    if restorable > Bytes::ZERO {
        println!("    {:>10}  restorable", restorable.human());
    }
    println!("    {:>10}  irreversible", irreversible.human());
    if irreversible == Bytes::ZERO {
        println!("    Nothing here can be lost.");
    }

    let mut orphans: Vec<_> = plan.of_tier(Tier::Orphan).collect();
    orphans.sort_by_key(|i| std::cmp::Reverse(i.size.unwrap_or(Bytes::ZERO).get()));

    println!();
    println!(
        "  NEEDS REVIEW — {} orphaned ({}), {} stale",
        orphans.len(),
        plan.bytes_of_tier(Tier::Orphan).human(),
        plan.of_tier(Tier::Stale).count()
    );
    for i in orphans.iter().take(12) {
        println!(
            "    {:<40} {:>9}  {}",
            truncate(&i.name, 40),
            i.size.map(|b| b.human()).unwrap_or_else(|| "—".into()),
            i.verdict.because
        );
    }
    if orphans.len() > 12 {
        println!("    … and {} more", orphans.len() - 12);
    }

    let unattributed: Vec<_> = plan
        .of_tier(Tier::Unattributed)
        .filter(|i| i.kind == ResourceKind::Volume)
        .collect();
    let ub: Bytes = unattributed.iter().filter_map(|i| i.size).sum();
    println!();
    println!(
        "  UNATTRIBUTED — {} volumes ({}), never offered for deletion",
        unattributed.len(),
        ub.human()
    );

    let vols: Vec<_> = r.of_kind(ResourceKind::Volume).collect();
    let count = |c: Confidence| vols.iter().filter(|a| a.confidence == Some(c)).count();
    println!(
        "  attribution: proven {}  strong {}  weak {}  guess {}  none {}",
        count(Confidence::Proven),
        count(Confidence::Strong),
        count(Confidence::Weak),
        count(Confidence::Guess),
        vols.iter().filter(|a| a.confidence.is_none()).count()
    );
    let unverifiable = vols
        .iter()
        .filter(|a| matches!(a.liveness, Some(Liveness::Unverifiable { .. })))
        .count();
    if unverifiable > 0 {
        println!("  {unverifiable} have an unverifiable project — a root may be unmounted, so they are held back");
    }

    println!();
    println!("  Nothing was changed. Re-run with --apply to reclaim the safe tier.");
    println!();
}

fn render_receipt(r: &Receipt) {
    println!();
    if r.simulated {
        println!("  DRY RUN — nothing was touched");
    }
    let would = r
        .items
        .iter()
        .filter(|i| i.outcome == ItemOutcome::WouldDelete)
        .count();
    // Break the count down by kind. "109 items" tells a user nothing about
    // whether they are comfortable; "20 networks, 89 containers" does.
    let mut by_kind: std::collections::BTreeMap<&str, usize> = Default::default();
    for i in &r.items {
        if matches!(i.outcome, ItemOutcome::WouldDelete | ItemOutcome::Deleted) {
            *by_kind.entry(i.kind.as_str()).or_default() += 1;
        }
    }
    let breakdown = by_kind
        .iter()
        .map(|(k, n)| format!("{n} {k}s"))
        .collect::<Vec<_>>()
        .join(", ");

    if r.simulated {
        println!(
            "  would delete {would} ({breakdown})   skipped {}   problems {}",
            r.skipped(),
            r.problems()
        );
        println!(
            "  would free {} (as Docker counts it)",
            r.docker_reported.human()
        );
    } else {
        println!(
            "  deleted {} ({breakdown})   skipped {}   problems {}",
            r.deleted(),
            r.skipped(),
            r.problems()
        );
        println!("  docker reports {} freed", r.docker_reported.human());
    }
    if r.build_cache_reclaimed > Bytes::ZERO {
        println!(
            "  build cache: {} reclaimed",
            r.build_cache_reclaimed.human()
        );
    }
    // In a dry run, name what would go. "109 items" is not reviewable; a list is.
    if r.simulated {
        let names: Vec<&str> = r
            .items
            .iter()
            .filter(|i| i.outcome == ItemOutcome::WouldDelete)
            .map(|i| i.name.as_str())
            .collect();
        for chunk in names.chunks(3).take(6) {
            println!("    {}", chunk.join("  ·  "));
        }
        if names.len() > 18 {
            println!(
                "    … and {} more (--json for the full list)",
                names.len() - 18
            );
        }
    }

    for i in &r.items {
        match &i.outcome {
            ItemOutcome::Skipped(s) => println!("    skipped  {:<36} {s}", truncate(&i.name, 36)),
            ItemOutcome::Refused(m) => println!("    refused  {:<36} {m}", truncate(&i.name, 36)),
            ItemOutcome::Failed(m) => println!("    FAILED   {:<36} {m}", truncate(&i.name, 36)),
            _ => {}
        }
    }
    println!();
    println!("  Note: Docker's figure is logical. Actual host reclamation can differ,");
    println!("  especially on a VM-backed runtime. Host measurement is a later milestone.");
    println!();
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let head: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{head}…")
    }
}
