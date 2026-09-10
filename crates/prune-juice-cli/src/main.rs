//! Prune Juice CLI.
//!
//! M1 is read-only by construction: nothing here can delete anything, because
//! the only destructive trait (`DockerMutate`) has no implementation in the
//! workspace yet.
//!
//! House convention: stdout carries the payload, stderr carries human progress,
//! so `prune-juice --json | jq` composes.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use prune_juice_core::docker::bollard_client::BollardClient;
use prune_juice_core::docker::{context, DockerClient};
use prune_juice_core::event::{Cancel, JsonlSink, NullSink};
use prune_juice_core::model::{Bytes, Confidence, Liveness, ResourceKind};
use prune_juice_core::scan::{ScanOptions, ScanReport, Scanner};
use prune_juice_core::Error;

const USAGE: &str = "\
prune-juice — reclaim Docker disk with provenance

USAGE:
    prune-juice [OPTIONS]

OPTIONS:
    --json              NDJSON on stdout, progress on stderr
    --no-sizes          Skip volume sizing (the expensive call)
    --roots <PATHS>     Colon-separated dirs to search for projects
    --context <NAME>    Scan only this context
    -h, --help          Show this help

M1 is read-only. No flag in this build can remove anything.

EXIT CODES:
    0  nothing needing attention
    1  reclaimable space or unattributed resources found
    2  usage error
    3  daemon unreachable
    4  permission denied
  130  cancelled
";

struct Args {
    json: bool,
    with_sizes: bool,
    roots: Vec<PathBuf>,
    only_context: Option<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        json: false,
        with_sizes: true,
        roots: default_roots(),
        only_context: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => a.json = true,
            "--no-sizes" => a.with_sizes = false,
            "--roots" => {
                let v = it.next().ok_or("--roots needs a value")?;
                a.roots = v
                    .split(':')
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from)
                    .collect();
            }
            "--context" => {
                a.only_context = Some(it.next().ok_or("--context needs a value")?);
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(a)
}

/// Where projects usually live. Only directories that exist are kept — a
/// configured root that is missing is a scan-blocking condition, not a licence
/// to call everything under it an orphan.
fn default_roots() -> Vec<PathBuf> {
    let home = std::env::var("HOME").map(PathBuf::from).ok();
    let mut out = Vec::new();
    if let Some(h) = home {
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

fn run(args: &Args) -> Result<i32, Error> {
    let cancel = Cancel::new();

    // Contexts are candidate endpoints, not identities. Two of them can be the
    // same engine — `/var/run/docker.sock` is frequently a symlink — so we
    // connect, ask each daemon for its own `/info` ID, and drop duplicates.
    let contexts: Vec<_> = context::discover()
        .into_iter()
        .filter(|c| args.only_context.as_deref().is_none_or(|n| c.name == n))
        .filter(|c| c.is_local())
        .collect();

    if contexts.is_empty() {
        return Err(Error::NoContext);
    }

    let mut seen_daemons: Vec<String> = Vec::new();
    let mut reports: Vec<ScanReport> = Vec::new();
    let mut last_err: Option<Error> = None;

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
        if seen_daemons.iter().any(|d| d == ident.id.as_str()) {
            if !args.json {
                eprintln!(
                    "  context {:<12} is the same engine as one already scanned — skipping",
                    ctx.name
                );
            }
            continue;
        }
        seen_daemons.push(ident.id.0.clone());

        let sink: Arc<dyn prune_juice_core::EventSink> = if args.json {
            Arc::new(JsonlSink::new(io::stdout()))
        } else {
            Arc::new(NullSink)
        };

        let opts = ScanOptions {
            project_roots: args.roots.clone(),
            with_sizes: args.with_sizes,
        };

        if !args.json {
            eprint!("scanning {}… ", ctx.name);
            io::stderr().flush().ok();
        }
        let report = Scanner::new(&client).scan(&ctx.name, &opts, sink, &cancel)?;
        if !args.json {
            eprintln!("{} ms", report.duration_ms);
        }
        reports.push(report);
    }

    if reports.is_empty() {
        return Err(last_err.unwrap_or(Error::NoContext));
    }

    if !args.json {
        for r in &reports {
            render(r);
        }
    }

    // Exit 1 when there is something worth a human's attention.
    let needs_attention = reports.iter().any(|r| {
        r.totals.build_cache_bytes > Bytes::ZERO
            || r.orphan_candidates().next().is_some()
            || r.unattributed().next().is_some()
    });
    Ok(if needs_attention { 1 } else { 0 })
}

fn render(r: &ScanReport) {
    let d = &r.daemon;
    println!();
    println!(
        "  {}  ·  {:?}  ·  engine {} (API {})",
        r.context, d.runtime, d.server_version, d.api_version
    );
    println!("  daemon {}", d.id);
    if d.swarm_active {
        println!(
            "  ! swarm is active — resources may be referenced by something not modelled here"
        );
    }
    println!();

    let t = &r.totals;
    println!(
        "  {:>5} containers   {:>5} images ({})",
        t.containers,
        t.images,
        t.image_bytes.human()
    );
    println!(
        "  {:>5} volumes ({}) {:>5} networks",
        t.volumes,
        t.volume_bytes.human(),
        t.networks
    );
    println!(
        "  {:>5} build cache records, {} reclaimable",
        t.build_cache_records,
        t.build_cache_bytes.human()
    );
    println!("  {:>5} projects known", r.projects_known);
    if r.stale {
        println!("  ! sizes are incomplete — figures below understate the total");
    }
    for w in &r.warnings {
        println!("  ! {w}");
    }

    // --- orphan candidates -----------------------------------------------
    let mut orphans: Vec<_> = r.orphan_candidates().collect();
    orphans.sort_by_key(|a| std::cmp::Reverse(a.resource.size.unwrap_or(Bytes::ZERO).get()));
    println!();
    println!(
        "  ORPHAN CANDIDATES ({}) — a confident claim on a project that is gone",
        orphans.len()
    );
    if orphans.is_empty() {
        println!("    none");
    }
    for a in orphans.iter().take(25) {
        println!(
            "    {:<44} {:>9}  {}",
            truncate(&a.resource.name, 44),
            a.resource
                .size
                .map(|b| b.human())
                .unwrap_or_else(|| "—".into()),
            a.owner.as_deref().unwrap_or("?")
        );
        for e in a.claims.iter().flat_map(|c| &c.evidence) {
            println!("        [{}] {}", e.source.tag(), e.detail);
        }
    }

    // --- unattributed ------------------------------------------------------
    let unattributed: Vec<_> = r
        .unattributed()
        .filter(|a| a.resource.kind == ResourceKind::Volume)
        .collect();
    let unattributed_bytes: Bytes = unattributed.iter().filter_map(|a| a.resource.size).sum();
    let anon = unattributed
        .iter()
        .filter(|a| a.resource.is_anonymous_volume())
        .count();

    println!();
    println!(
        "  UNATTRIBUTED VOLUMES ({}, {}) — never offered for deletion",
        unattributed.len(),
        unattributed_bytes.human()
    );
    println!("    {anon} are anonymous (64-hex). Their owning containers are gone, so the");
    println!("    mapping is unrecoverable from Docker. Recorded now so it is not lost again.");

    // --- attribution quality ------------------------------------------------
    let vols: Vec<_> = r.of_kind(ResourceKind::Volume).collect();
    let proven = vols
        .iter()
        .filter(|a| a.confidence == Some(Confidence::Proven))
        .count();
    let strong = vols
        .iter()
        .filter(|a| a.confidence == Some(Confidence::Strong))
        .count();
    let weak = vols
        .iter()
        .filter(|a| a.confidence == Some(Confidence::Weak))
        .count();
    let guess = vols
        .iter()
        .filter(|a| a.confidence == Some(Confidence::Guess))
        .count();
    let none = vols.iter().filter(|a| a.confidence.is_none()).count();

    println!();
    println!("  VOLUME ATTRIBUTION");
    println!("    proven {proven}   strong {strong}   weak {weak}   guess {guess}   none {none}");

    let unverifiable = vols
        .iter()
        .filter(|a| matches!(a.liveness, Some(Liveness::Unverifiable { .. })))
        .count();
    if unverifiable > 0 {
        println!(
            "    {unverifiable} volumes have an unverifiable project — a root may be unmounted."
        );
        println!("    These are held back deliberately rather than called orphans.");
    }
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

/// Reserved for M3. Interactive mode is the default only on a TTY; piped or
/// CI invocations always take the one-shot path above.
#[allow(dead_code)]
fn is_interactive() -> bool {
    io::stdout().is_terminal() && std::env::var("CI").is_err()
}
