//! Prune Juice CLI.
//!
//! House convention: stdout carries the payload, stderr carries human progress,
//! so `prune-juice --json | jq` composes. Dry-run is the default; `--apply` has
//! to be asked for.

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use prune_juice_core::disk::{self, CompactionCapability};
use prune_juice_core::docker::bollard_client::BollardClient;
use prune_juice_core::docker::{context, DockerClient};
use prune_juice_core::event::{Cancel, JsonlSink, NullSink};
use prune_juice_core::execute::{ExecuteOptions, Executor, ItemOutcome, Mode, Receipt};
use prune_juice_core::index::Index;
use prune_juice_core::model::{Bytes, Confidence, Liveness, ResourceKind};
use prune_juice_core::plan::tier::{Reversibility, Tier};
use prune_juice_core::plan::{Plan, Planner};
use prune_juice_core::scan::{ScanOptions, ScanReport, Scanner};
use prune_juice_core::vault::Vault;
use prune_juice_core::waiver::Waivers;
use prune_juice_core::Error;
use prune_juice_tui::TuiOptions;

const USAGE: &str = "\
prune-juice — reclaim Docker disk with provenance

USAGE:
    prune-juice [OPTIONS]

OPTIONS:
    --apply             Actually delete. Without this, nothing is touched.
    --tiers <LIST>      What to act on. Default `free`, which needs no
                        review. In rising order of what it costs to be wrong:
                          free         nothing here can be lost
                          orphan       owning project is gone; volumes vaulted
                          repullable   pulled again on next use (bandwidth)
                          stale        dormant, project still exists; vaulted
                          rebuildable  rebuilt from a context that still
                                       exists — costs time, and an old build
                                       with network-install steps may no
                                       longer reproduce
    --only-label <K=V>  Refuse to touch anything without this label
    --json              NDJSON on stdout, progress on stderr
    --no-sizes          Skip volume sizing (the expensive call)
    --roots <PATHS>     Colon-separated dirs to search for projects
    --context <NAME>    Scan only this context
    --no-tui            Force the one-shot report even on a terminal
    --deadline SECS     Give up after this long and report what was gathered
                        (default 120, 0 to wait indefinitely)
    --no-probe          Skip reading volume contents. No volume can then be
                        proven safe, so this only ever shrinks what is offered.
    --container-probe   Always read volume contents through a container, even
                        where the host could read them directly. This is the
                        only path available on Docker Desktop.
    --no-vault          Do not preserve a copy before an irreversible delete.
                        Irreversible items are then REFUSED, not deleted —
                        declining a backup is not consent to lose data.

    --waivers           List resources you have told the tool to leave alone
    --waive SELECTOR    Leave a resource alone. Needs --reason.
    --unwaive SELECTOR  Remove a waiver
    --reason TEXT       Why. Required with --waive, and it has to say something.

    --vault             List preserved copies and exit
    --vault-dump VOL    Preserve a volume now, without deleting anything
    --vault-verify      Re-read every preserved copy and confirm it is intact
    --vault-restore ID  Recreate a volume from a preserved copy
    --vault-forget ID   Delete a preserved copy. Irreversible, so it needs
                        --reason, same as a waiver.
    -h, --help          Show this help

With no arguments on a terminal, `prune-juice` opens an interactive
interface. Piped, redirected, or under CI it prints a one-shot report
instead, so it composes in a script without special-casing.

Tier `free` is the only one safe without review. Every other tier is an
explicit choice about a cost you are accepting — run without --apply first
and read what it says it would do.

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
    force_container_probe: bool,
    deadline: Option<std::time::Duration>,
    no_vault: bool,
    vault_cmd: Option<VaultCmd>,
    waiver_cmd: Option<WaiverCmd>,
    reason: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
enum WaiverCmd {
    List,
    Add(String),
    Remove(String),
}

#[derive(Clone, PartialEq, Eq)]
enum VaultCmd {
    List,
    Verify,
    Dump(String),
    Restore(String),
    Forget(String),
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
        force_container_probe: false,
        deadline: Some(std::time::Duration::from_secs(120)),
        no_vault: false,
        vault_cmd: None,
        waiver_cmd: None,
        reason: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--json" => a.json = true,
            "--no-sizes" => a.with_sizes = false,
            "--no-tui" => a.no_tui = true,
            "--deadline" => {
                let v = it.next().ok_or("--deadline needs seconds (0 to disable)")?;
                let secs: u64 = v
                    .parse()
                    .map_err(|_| "--deadline needs a number of seconds")?;
                a.deadline = (secs > 0).then(|| std::time::Duration::from_secs(secs));
            }
            "--no-probe" => a.no_probe = true,
            "--container-probe" => a.force_container_probe = true,
            "--no-vault" => a.no_vault = true,
            "--waivers" => a.waiver_cmd = Some(WaiverCmd::List),
            "--waive" => {
                a.waiver_cmd = Some(WaiverCmd::Add(
                    it.next()
                        .ok_or("--waive needs a selector, e.g. volume:oak_mysql")?,
                ))
            }
            "--unwaive" => {
                a.waiver_cmd = Some(WaiverCmd::Remove(
                    it.next().ok_or("--unwaive needs a selector")?,
                ))
            }
            "--reason" => a.reason = Some(it.next().ok_or("--reason needs text")?),
            "--vault" => a.vault_cmd = Some(VaultCmd::List),
            "--vault-verify" => a.vault_cmd = Some(VaultCmd::Verify),
            "--vault-dump" => {
                a.vault_cmd = Some(VaultCmd::Dump(
                    it.next().ok_or("--vault-dump needs a volume name")?,
                ))
            }
            "--vault-forget" => {
                a.vault_cmd = Some(VaultCmd::Forget(
                    it.next().ok_or("--vault-forget needs an entry id")?,
                ))
            }
            "--vault-restore" => {
                a.vault_cmd = Some(VaultCmd::Restore(
                    it.next().ok_or("--vault-restore needs an entry id")?,
                ))
            }
            "--apply" => a.apply = true,
            "--tiers" => {
                let v = it.next().ok_or("--tiers needs a value")?;
                a.tiers = v
                    .split(',')
                    .map(|s| match s.trim() {
                        "free" => Ok(Tier::Free),
                        "orphan" => Ok(Tier::Orphan),
                        "repullable" => Ok(Tier::Repullable),
                        "rebuildable" => Ok(Tier::Rebuildable),
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

/// A scanner with everything available on this machine: a container probe for
/// runtimes that hide their data root, and the index for memory across runs.
fn scanner<'a>(client: &'a BollardClient, index: Option<&'a Index>) -> Scanner<'a> {
    let s = Scanner::with_probe(client, client);
    match index {
        Some(i) => s.with_index(i),
        None => s,
    }
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

/// The vault subcommands. Read-only except `restore`, which creates a volume
/// and refuses to overwrite an existing one.
fn run_vault(cmd: &VaultCmd, reason: Option<&str>) -> Result<i32, Error> {
    let vault = Vault::open()?;
    let entries = vault.entries()?;

    match cmd {
        VaultCmd::List => {
            println!();
            println!("  vault: {}", vault.root().display());
            if entries.is_empty() {
                println!("  empty — nothing has been preserved yet");
                println!();
                return Ok(0);
            }
            println!(
                "  {} entries, {} on disk",
                entries.len(),
                vault.total_bytes().human()
            );
            println!();
            for e in &entries {
                println!(
                    "    {:<34} {:<24} {:>9}  {}",
                    e.id,
                    e.volume,
                    e.archive_bytes.human(),
                    e.engine.map(|x| x.as_str()).unwrap_or("—")
                );
            }
            println!();
            println!("  restore with:  prune-juice --vault-restore <id>");
            println!();
            Ok(0)
        }
        VaultCmd::Verify => {
            let mut bad = 0;
            println!();
            for e in &entries {
                match vault.verify(e) {
                    Ok(()) => println!("    ok       {}", e.id),
                    Err(err) => {
                        bad += 1;
                        println!("    CORRUPT  {}  {err}", e.id);
                    }
                }
            }
            if entries.is_empty() {
                println!("  nothing to verify");
            }
            println!();
            Ok(if bad > 0 { 5 } else { 0 })
        }
        VaultCmd::Dump(volume) => {
            // Useful on its own — "preserve this before I touch it" — and the
            // only way to exercise the dump path without deleting anything.
            let ctxs: Vec<_> = context::discover()
                .into_iter()
                .filter(|c| c.is_local())
                .collect();
            let ctx = ctxs.first().ok_or(Error::NoContext)?;
            let client = BollardClient::connect(&ctx.endpoint)?;

            let existing = client.list_volumes()?;
            let Some(v) = existing.iter().find(|v| &v.name == volume) else {
                return Err(Error::Config(format!("no volume named {volume}")));
            };

            eprintln!("preserving {volume}…");
            let entry = vault.store(&client, volume, &v.labels, None, v.size, now_unix())?;
            println!(
                "  preserved {} as {} ({} on disk, {} entries, verified)",
                entry.volume,
                entry.id,
                entry.archive_bytes.human(),
                entry.entry_count
            );
            println!("  nothing was deleted.");
            Ok(0)
        }
        VaultCmd::Forget(id) => {
            // Removing the last copy of something already deleted is the most
            // irreversible act this tool can perform — more so than the
            // deletion it backed up, which was recoverable precisely because
            // this file existed. So it takes a reason, like a waiver.
            let Some(entry) = entries.iter().find(|e| e.id == *id) else {
                return Err(Error::Config(format!(
                    "no vault entry with id {id}; run --vault to list them"
                )));
            };
            let Some(reason) = reason else {
                return Err(Error::Config(format!(
                    "--vault-forget needs --reason. This is the last copy of {} \
                     ({}), and deleting it cannot be undone.",
                    entry.volume,
                    entry.archive_bytes.human()
                )));
            };
            if reason.trim().chars().count() < 12 {
                return Err(Error::Config(
                    "--reason needs to say something (12 characters or more)".into(),
                ));
            }
            let others = entries
                .iter()
                .filter(|e| e.volume == entry.volume && e.id != entry.id)
                .count();
            vault.forget(entry)?;
            println!("  forgot {} ({})", entry.id, reason.trim());
            if others == 0 {
                println!(
                    "  that was the only copy of {} — it is now unrecoverable",
                    entry.volume
                );
            } else {
                println!("  {others} other cop(y/ies) of {} remain", entry.volume);
            }
            Ok(0)
        }
        VaultCmd::Restore(id) => {
            let Some(entry) = entries.iter().find(|e| e.id == *id) else {
                return Err(Error::Config(format!(
                    "no vault entry with id {id}; run --vault to list them"
                )));
            };
            let contexts: Vec<_> = context::discover()
                .into_iter()
                .filter(|c| c.is_local())
                .collect();
            let ctx = contexts.first().ok_or(Error::NoContext)?;
            let client = BollardClient::connect(&ctx.endpoint)?;

            eprintln!("restoring {} into volume {}…", entry.id, entry.volume);
            vault.restore(&client, entry)?;
            println!("  restored {} from {}", entry.volume, entry.id);
            println!("  the vault copy was kept; remove it yourself when you are sure");
            Ok(0)
        }
    }
}

fn run_waivers(cmd: &WaiverCmd, reason: Option<&str>) -> Result<i32, Error> {
    let mut w = Waivers::open()?;
    match cmd {
        WaiverCmd::List => {
            println!();
            println!("  waivers: {}", w.path().display());
            if w.is_empty() {
                println!("  none — nothing is being held back by hand");
            }
            for x in w.all() {
                println!("    {:<32} {}", x.selector, x.reason);
            }
            println!();
            Ok(0)
        }
        WaiverCmd::Add(selector) => {
            let Some(reason) = reason else {
                return Err(Error::Config(
                    "--waive needs --reason: an exclusion nobody can explain later is worse \
                     than none"
                        .into(),
                ));
            };
            w.add(selector, reason, None, now_unix())?;
            println!("  {selector} will be left alone: {reason}");
            Ok(0)
        }
        WaiverCmd::Remove(selector) => {
            if w.remove(selector)? {
                println!("  {selector} is no longer waived");
                Ok(0)
            } else {
                Err(Error::Config(format!("no waiver matching {selector}")))
            }
        }
    }
}

fn run(args: &Args) -> Result<i32, Error> {
    if let Some(cmd) = &args.waiver_cmd {
        return run_waivers(cmd, args.reason.as_deref());
    }
    if let Some(cmd) = &args.vault_cmd {
        return run_vault(cmd, args.reason.as_deref());
    }
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
        // Two minutes is generous for a healthy machine and short enough that
        // a wedged filesystem read degrades the report instead of looking like
        // a hang. `--deadline 0` disables it.
        deadline: args.deadline,
        force_container_probe: args.force_container_probe,
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

    // One index for the whole run. A failure to open it degrades the scan
    // rather than stopping it: attribution gets worse, nothing gets unsafe.
    let index = match Index::open() {
        Ok(i) => Some(i),
        Err(e) => {
            eprintln!("  ! the provenance index is unavailable ({e}); attribution will not improve across runs");
            None
        }
    };

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
        let report =
            scanner(&client, index.as_ref()).scan(&ctx.name, &opts, sink.clone(), &cancel)?;
        if !args.json {
            eprintln!("{} ms", report.duration_ms);
        }

        let waivers = Waivers::open().ok();
        let plan = Planner::plan_with_waivers(&report, now_unix(), waivers.as_ref());
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
        let fresh =
            scanner(&client, index.as_ref()).scan(&ctx.name, &opts, sink.clone(), &cancel)?;
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
            only_names: None,
            vault: !args.no_vault,
        };
        let vault = Vault::open()?;
        let store = disk::detect(ident.runtime, ident.data_root.as_deref(), &ctx.endpoint);
        let receipt = if args.apply {
            Executor::applying(&client)
                .with_vault(&client, &vault)
                .with_disk(&store)
                .run(plan, &fresh, now_unix(), &exec_opts, sink, &cancel)?
        } else {
            // No mutating client at all: dry-run is unable to delete, not
            // merely disinclined to.
            Executor::dry_run().with_disk(&store).run(
                plan,
                &fresh,
                now_unix(),
                &exec_opts,
                sink,
                &cancel,
            )?
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

    // The ladder. Everything above `free` costs something to be wrong about,
    // so each rung states its price and none of it happens without --tiers.
    println!();
    println!(
        "  IF YOU KNOW YOU CAN REBUILD — opt in with --tiers <name>
    (image sizes are summed, so shared layers count twice — the real figure is
     lower, and a run reports what it actually freed)"
    );
    for t in Tier::OPT_IN {
        let items: Vec<_> = plan.of_tier(t).collect();
        if items.is_empty() {
            continue;
        }
        let bytes: Bytes = items.iter().filter_map(|i| i.size).sum();
        println!(
            "    {:<12} {:>4} items {:>9}   {}",
            t.as_str(),
            items.len(),
            bytes.human(),
            t.caveat()
        );
        if t == Tier::Rebuildable {
            println!(
                "                                        an old build with network-install steps"
            );
            println!(
                "                                        may no longer reproduce; run without"
            );
            println!("                                        --apply first and read the list");
        }
    }
    if Tier::OPT_IN
        .iter()
        .all(|t| plan.of_tier(*t).next().is_none())
    {
        println!("    nothing — every remaining resource is either in use or unrecoverable");
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
        // `Preserved` means removed *and* copied to the vault first, so it
        // belongs in the count. Omitting it made a preserved volume disappear
        // from the breakdown while still being counted in the total.
        if i.outcome.removed() || i.outcome == ItemOutcome::WouldDelete {
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

    if let Some((want, got)) = r.shortfall() {
        println!(
            "  ! expected about {} but freed {} — something in that chain is not working",
            want.human(),
            got.human()
        );
    }
    for i in &r.items {
        match &i.outcome {
            ItemOutcome::Skipped(s) => println!("    skipped  {:<36} {s}", truncate(&i.name, 36)),
            ItemOutcome::Refused(m) => println!("    refused  {:<36} {m}", truncate(&i.name, 36)),
            ItemOutcome::Failed(m) => println!("    FAILED   {:<36} {m}", truncate(&i.name, 36)),
            _ => {}
        }
    }
    // Docker-reported and host-measured are different questions, so they are
    // shown as different lines and never added together.
    if let Some(rec) = &r.reclamation {
        println!();
        if r.simulated {
            // A dry run moved nothing, so there is nothing to have measured.
            // Saying "gave back 0 B" here would read as a failure.
            println!(
                "  host disk would be measured before and after a real run ({})",
                match &rec.compaction {
                    CompactionCapability::NotNeeded => "native filesystem — reclaims immediately",
                    CompactionCapability::Automatic(_) => "sparse image — reclaims on its own",
                    CompactionCapability::Triggerable { .. } =>
                        "sparse image — may need compacting",
                    CompactionCapability::ManualOnly { .. } =>
                        "virtual disk — compaction is manual",
                    CompactionCapability::Unavailable(_) => "not measurable on this setup",
                }
            );
        } else if let Some(m) = rec.host_measured {
            println!(
                "  host actually gave back {}  (confidence: {:?})",
                m.human(),
                rec.confidence
            );
        } else {
            println!("  host reclamation could not be measured on this setup");
        }
        if !r.simulated && rec.shortfall_worth_mentioning() {
            println!("  The host gave back much less than Docker reported. That is normal on a");
            println!("  VM-backed runtime: the guest has to discard the blocks first.");
            match &rec.compaction {
                CompactionCapability::Automatic(note) => println!("    {note}"),
                CompactionCapability::Triggerable { how, warning } => {
                    println!("    to reclaim it now:  {how}");
                    println!("    ({warning})");
                }
                CompactionCapability::ManualOnly { instructions } => {
                    for i in instructions {
                        println!("    {i}");
                    }
                }
                _ => {}
            }
        }
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
