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
use prune_juice_core::update::{self, Decision};
use prune_juice_core::vault::Vault;
use prune_juice_core::waiver::Waivers;
use prune_juice_core::Error;
use prune_juice_tui::TuiOptions;

mod banner;
mod interrupt;

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
    --no-sizes          Skip volume sizing (the expensive call). One call
                        reports volume sizes AND build cache, so this also
                        hides the build cache — the safe tier then reads 0 B
                        and --apply reclaims none of it. Use it to scan fast,
                        not before an --apply.
    --roots <PATHS>     Colon-separated dirs to search for projects
    --context <NAME>    Scan only this context
    --remote            Allow a daemon that is not a local socket (tcp://,
                        ssh://). Needs --reason. Off by default: a remote
                        engine's disk is not this machine's disk, its projects
                        are not these projects, and the provenance recorded
                        here would be about someone else's resources.
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

    --check-update      Ask now whether a newer release exists
    --update            Install the newest release. Refuses when a package
                        manager owns this binary, and names the command that
                        manager wants instead.
    --update-check on|off
                        Turn the automatic once-a-day check on or off and
                        remember the answer
    --no-update-check   Skip the automatic check for this run only
                        (PRUNE_JUICE_NO_UPDATE_CHECK=1 does the same)
    -V, --version       Print the version
    -h, --help          Show this help

With no arguments on a terminal, `prune-juice` opens an interactive
interface. Piped, redirected, or under CI it prints a one-shot report
instead, so it composes in a script without special-casing.

Tier `free` is the only one safe without review. Every other tier is an
explicit choice about a cost you are accepting — run without --apply first
and read what it says it would do.

An automatic check runs at most once a day, in the background, on a terminal
only. It never delays a scan and never fails one: if the release server cannot
be reached the run is unaffected and nothing is said. Notices go to stderr, so
they stay out of --json and out of anything you pipe.

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
    /// Consent to talk to a daemon that is not a local socket. Paired with a
    /// written reason, like every other exception this tool makes.
    remote: bool,
    no_tui: bool,
    no_probe: bool,
    force_container_probe: bool,
    deadline: Option<std::time::Duration>,
    no_vault: bool,
    vault_cmd: Option<VaultCmd>,
    waiver_cmd: Option<WaiverCmd>,
    reason: Option<String>,
    update_cmd: Option<UpdateCmd>,
    no_update_check: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UpdateCmd {
    /// Ask now, and say what the answer is either way.
    Check,
    /// Ask, and install if there is something newer.
    Install,
    /// Remember whether the automatic check should run.
    Automatic(bool),
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
        remote: false,
        no_tui: false,
        no_probe: false,
        force_container_probe: false,
        deadline: Some(std::time::Duration::from_secs(120)),
        no_vault: false,
        vault_cmd: None,
        waiver_cmd: None,
        reason: None,
        update_cmd: None,
        no_update_check: false,
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
            "--remote" => a.remote = true,
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
            "--check-update" => a.update_cmd = Some(UpdateCmd::Check),
            "--update" => a.update_cmd = Some(UpdateCmd::Install),
            "--update-check" => {
                let v = it.next().ok_or("--update-check needs `on` or `off`")?;
                a.update_cmd = Some(match v.as_str() {
                    "on" => UpdateCmd::Automatic(true),
                    "off" => UpdateCmd::Automatic(false),
                    other => {
                        return Err(format!("--update-check takes `on` or `off`, not {other}"))
                    }
                });
            }
            "--no-update-check" => a.no_update_check = true,
            "-V" | "--version" => {
                // Two words, version last. `--update` parses this from the
                // binary it just downloaded before it will install it, so the
                // shape is load-bearing rather than cosmetic.
                println!("prune-juice {}", update::current_version());
                std::process::exit(0);
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

/// Whether this run may talk to a daemon that is not a local socket, and why.
///
/// `--remote` on its own is not enough, for the same reason a waiver needs a
/// sentence: an exception nobody can explain later is worse than none. A
/// remote engine's disk is not this machine's disk, so nothing it reports can
/// be cross-checked against the filesystem here, host reclamation cannot be
/// measured at all, and the projects the scan looks for are local
/// directories that have nothing to do with the resources being judged.
fn remote_authorisation(args: &Args) -> Result<Option<String>, Error> {
    if !args.remote {
        return Ok(None);
    }
    let reason = args.reason.as_deref().unwrap_or("").trim().to_string();
    if reason.chars().count() < 12 {
        return Err(Error::Config(
            "--remote needs --reason \"…\" saying why this run should judge a daemon whose \
             disk and projects are not this machine's (12 characters or more)"
                .into(),
        ));
    }
    Ok(Some(reason))
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
        // A Ctrl-C is not a fault to report back to the person who pressed it.
        Err(Error::Cancelled) if interrupt::interrupted() => {
            eprintln!("interrupted — the scan stopped and nothing was changed");
            std::process::exit(Error::Cancelled.exit_code());
        }
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

/// The explicit update commands.
///
/// These are foreground and synchronous, unlike the automatic check: the user
/// asked, so waiting a moment and hearing the answer — including a failure —
/// is the point. The automatic path is the one that must never be felt.
fn run_update(cmd: UpdateCmd, from_offer: bool) -> Result<i32, Error> {
    if let UpdateCmd::Automatic(on) = cmd {
        let path = update::set_automatic_checks(on)?;
        println!(
            "  automatic update checks are {}  ({})",
            if on { "on" } else { "off" },
            path.display()
        );
        return Ok(0);
    }

    if update::verify::release_key().is_none() {
        return Err(Error::Config(
            "this build has no release-signing key, so it cannot verify an update. \
             Install from a release build, or upgrade the way you installed it."
                .into(),
        ));
    }
    if !update::net::Curl::available() {
        return Err(Error::Config(
            "curl was not found, and it is how updates are fetched".into(),
        ));
    }

    let exe = std::env::current_exe().map_err(Error::Io)?;
    let origin = update::Origin::detect(&exe);
    let curl = update::net::Curl;
    // A forced refresh, not the cached answer: someone who typed the command
    // wants today's answer, not yesterday's.
    let checker = update::Checker::new(&curl, now_unix()).with_origin(origin.clone());

    eprint!("checking for a newer release… ");
    io::stderr().flush().ok();
    let manifest = match checker.fetch_manifest() {
        Ok(m) => {
            eprintln!("ok");
            m
        }
        Err(e) => {
            eprintln!("failed");
            // Exit 5, not 1: nothing is wrong with this machine, the check
            // did not complete. A script can tell those apart.
            eprintln!("error: {e}");
            return Ok(5);
        }
    };

    if !update::is_newer(update::current_version(), &manifest.version)? {
        println!(
            "  prune-juice {} is the newest release.",
            update::current_version()
        );
        return Ok(0);
    }

    // Skipped when this came from the offer: the notice is what was just
    // answered, and printing it again under the answer reads as a second
    // release rather than the same one.
    if !from_offer {
        let notice = update::Notice {
            current: update::current_version().to_string(),
            latest: manifest.version.clone(),
            notes_url: manifest.notes_url.clone(),
            origin: origin.clone(),
        };
        println!();
        for line in notice.lines() {
            println!("  {line}");
        }
        if let Some(url) = &notice.notes_url {
            println!("  {url}");
        }
        println!();
    }

    if cmd == UpdateCmd::Check {
        // A finding, in the same sense as reclaimable space: something is
        // there for you to act on.
        return Ok(1);
    }

    eprintln!("downloading and verifying…");
    let done = update::install::install(&curl, &manifest, &origin, &exe)?;
    println!(
        "  installed prune-juice {} at {} ({})",
        done.version,
        done.path.display(),
        Bytes(done.bytes).human()
    );
    // True of `--update`, and *not* true when the offer is about to re-exec
    // into what was just installed — saying it there would be a promise the
    // very next line breaks.
    if !from_offer {
        println!("  the copy already running is unchanged; the next invocation is the new one.");
    }
    Ok(0)
}

/// Should this run mention an update at the end?
///
/// Deliberately as conservative as `wants_tui`. A notice is for a person
/// reading a terminal: not for a pipe, not for a log, not for CI, and never
/// mixed into machine output.
fn wants_update_notice(args: &Args) -> bool {
    !args.json
        && !args.no_update_check
        && args.update_cmd.is_none()
        && io::stdout().is_terminal()
        && io::stderr().is_terminal()
        && std::env::var("CI").is_err()
        && update::automatic_checks().is_ok()
}

/// Print whatever the background check found, if it found anything in time.
///
/// The 250 ms grace exists because the alternative is worse in the common
/// case: a cached answer is ready almost immediately, and a bare `try_recv`
/// would lose it to a race for no benefit. It is not a wait on the network —
/// the check has had the whole scan to finish, and if it has not, the answer
/// is already in the cache for next time.
fn report_update(rx: Option<&std::sync::mpsc::Receiver<Decision>>, announced: bool) {
    let Some(rx) = rx else { return };
    // Already said before the scan. Saying it again at the bottom would be
    // the same news twice in one run.
    if announced {
        return;
    }
    let Ok(decision) = rx.recv_timeout(std::time::Duration::from_millis(250)) else {
        return;
    };
    // Anything other than "there is a newer version" is silence. A failed
    // check is not news, and telling someone their update check failed while
    // they were reclaiming disk is noise.
    if let Decision::Available(notice) = decision {
        print_notice(&notice);
    }
}

/// Set on the process that replaces this one, so an update can be offered at
/// most once per invocation chain. Without it a binary that somehow still
/// reads as older than the feed would offer, install and re-exec for ever.
const REEXEC_GUARD: &str = "PRUNE_JUICE_UPDATED";

/// May this run *ask*, rather than merely mention?
///
/// Stricter than [`wants_update_notice`] by two things. Stdin has to be a
/// terminal — a question needs somewhere to read the answer from, and a run
/// whose stdin is a pipe would consume that pipe to answer it. And the copy
/// has to be one this tool may replace: offering to install over a Homebrew
/// binary is offering to break someone's installation.
fn may_offer_update(notice: &update::Notice) -> bool {
    may_offer(
        &notice.origin,
        io::stdin().is_terminal(),
        std::env::var(REEXEC_GUARD).is_ok(),
    )
}

/// The decision, separated from the environment so it can be tested without
/// a terminal and without setting a variable the rest of this binary's tests
/// would see.
fn may_offer(origin: &update::Origin, stdin_is_terminal: bool, already_updated: bool) -> bool {
    stdin_is_terminal && !already_updated && origin.self_replace_allowed()
}

/// Ask, with yes as the default.
///
/// Pressing return takes the update, because that is what someone who has
/// just been told a newer version exists almost always wants, and because
/// what is on the other side of the question is a signed artifact this tool
/// verifies before it moves anything into place.
///
/// EOF is the one silence that is *not* a yes. A read returning zero bytes
/// means the answer was never given — Ctrl-D, or a stdin that went away —
/// and replacing the binary someone just ran on the strength of a question
/// nobody answered is not a default, it is an assumption.
fn ask_to_update() -> bool {
    eprint!("  Update now? [Y/n] ");
    io::stderr().flush().ok();
    let mut line = String::new();
    let read = io::stdin().read_line(&mut line);
    accepted(read.ok(), &line)
}

/// The decision, separated from stdin so it can be tested.
///
/// `read` is the byte count the read reported, or `None` if it failed at all.
/// `Some(0)` is EOF and is the only empty answer that declines; an empty line
/// is a return keypress, which is the default being taken.
fn accepted(read: Option<usize>, line: &str) -> bool {
    match read {
        None | Some(0) => false,
        Some(_) => matches!(line.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes"),
    }
}

/// Replace this process with the binary that was just installed.
///
/// `--update` alone cannot do this: it says "the copy already running is
/// unchanged", which is true and is the honest thing to say when someone
/// asked only to update. Here they asked for a scan, so the run continues —
/// on the new version, with the same arguments, which is what puts them on
/// the start screen rather than back at a shell prompt.
///
/// Only returns on failure; on success this process no longer exists.
#[cfg(unix)]
fn reexec_into_new_binary() -> Error {
    use std::os::unix::process::CommandExt;
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return Error::Io(e),
    };
    let err = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env(REEXEC_GUARD, "1")
        .exec();
    Error::Io(err)
}

#[cfg(not(unix))]
fn reexec_into_new_binary() -> Error {
    Error::Config("this platform cannot restart into the new binary".into())
}

/// The notice itself, wherever in the run it is reached from. stderr, because
/// it is not part of the payload.
fn print_notice(notice: &update::Notice) {
    eprintln!();
    for line in notice.lines() {
        eprintln!("  {line}");
    }
}

fn run(args: &Args) -> Result<i32, Error> {
    if let Some(cmd) = &args.waiver_cmd {
        return run_waivers(cmd, args.reason.as_deref());
    }
    if let Some(cmd) = &args.vault_cmd {
        return run_vault(cmd, args.reason.as_deref());
    }
    if let Some(cmd) = args.update_cmd {
        return run_update(cmd, false);
    }
    let cancel = Cancel::new();
    // From here on Ctrl-C stops the run at its next checkpoint and reports
    // what it did, rather than killing the process between two deletions.
    interrupt::install(&cancel);

    // A header is decoration, so it is shown to a person and to nobody else:
    // never under --json, never into a pipe, and not before the interface,
    // which paints over the screen and would lose it anyway.
    if !args.json && !wants_tui(args) && io::stdout().is_terminal() {
        print!("{}", banner::header());
    }

    // Contexts are candidate endpoints, not identities. Two can be the same
    // engine, so we connect, ask each daemon for its own /info ID, and dedupe.
    let named: Vec<_> = context::discover()
        .into_iter()
        .filter(|c| args.only_context.as_deref().is_none_or(|n| c.name == n))
        .collect();
    let remote_allowed = remote_authorisation(args)?;
    let (contexts, refused): (Vec<_>, Vec<_>) = named
        .into_iter()
        .partition(|c| c.is_local() || remote_allowed.is_some());

    // Said out loud, per context. A remote daemon dropped in silence looks
    // exactly like a daemon that is not there.
    for c in &refused {
        eprintln!(
            "  skipping {} ({}) — not a local socket. Re-run with \
             --remote --reason \"…\" to include it.",
            c.name, c.endpoint
        );
    }
    if let Some(reason) = &remote_allowed {
        if contexts.iter().any(|c| !c.is_local()) {
            eprintln!("  remote daemons included by request: {reason}");
        }
    }
    if contexts.is_empty() {
        return Err(Error::NoContext);
    }

    // Above the interface dispatch, because both paths need the answer and
    // because an offer to replace this binary belongs before either of them
    // has painted anything. One check per run, whichever way the run goes.
    let update = wants_update_notice(args).then(|| update::spawn_check(now_unix()));
    let news = update
        .as_ref()
        .and_then(|rx| update::await_notice(rx, update::NOTICE_WAIT));
    // Whether it has been said. A `None` here is not "no update" — it is also
    // a check that has not answered yet, and that one may still arrive in
    // time to be a footnote at the end of the run.
    let announced = news.is_some();

    if let Some(notice) = &news {
        // Being asked and being told how to do it by hand is the same
        // sentence twice, so the question replaces the second line rather
        // than following it.
        if may_offer_update(notice) {
            eprintln!();
            eprintln!("  {}", notice.lines()[0]);
        } else {
            print_notice(notice);
        }
        if may_offer_update(notice) && ask_to_update() {
            // The same path `--update` takes, and it prints what it did. A
            // non-zero code means it did not happen, and is the run's code:
            // carrying on into a scan would bury the reason.
            let code = run_update(UpdateCmd::Install, true)?;
            if code != 0 {
                return Ok(code);
            }
            return Err(reexec_into_new_binary());
        }
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
            // Carried, not re-fetched: the check ran above, where it could
            // still be acted on. This is only so the news survives the
            // interface painting over the terminal that showed it.
            update: news,
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
                    exclusive_size: i.reclaimable_size(),
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
        if receipt.cancelled {
            // The record of what was done has been printed; now leave with the
            // documented cancellation code rather than pretending the run
            // finished. Remaining contexts are not scanned.
            if !args.json {
                eprintln!("  interrupted — stopped at an item boundary, nothing was half-done");
            }
            report_update(update.as_ref(), announced);
            return Ok(130);
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
    // Last, after the report and the receipt: an update notice is the least
    // important thing on the screen and should read as a footnote.
    report_update(update.as_ref(), announced);
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
    // Two image figures, never one and never summed: the stack total is what
    // `docker images` adds up to, and the layer figure is what the images
    // actually occupy once a shared base layer is counted once.
    let image_size = match t.image_unique_bytes {
        Some(unique) if unique < t.image_bytes => {
            format!(
                "{} on disk, {} of stacks",
                unique.human(),
                t.image_bytes.human()
            )
        }
        _ => t.image_bytes.human(),
    };
    println!(
        "  {:>5} containers   {:>5} images ({})   {:>5} volumes ({})",
        t.containers,
        t.images,
        image_size,
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
        let b = i.reclaimable_size().unwrap_or(Bytes::ZERO);
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
    (an image counts only the layers no other image holds, so these figures are
     what removing the whole group frees, not what `docker images` adds up to)"
    );
    for t in Tier::OPT_IN {
        let items: Vec<_> = plan.of_tier(t).collect();
        if items.is_empty() {
            continue;
        }
        let bytes: Bytes = items.iter().filter_map(|i| i.reclaimable_size()).sum();
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
    orphans.sort_by_key(|i| std::cmp::Reverse(i.reclaimable_size().unwrap_or(Bytes::ZERO).get()));

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
            i.reclaimable_size()
                .map(|b| b.human())
                .unwrap_or_else(|| "—".into()),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Who may be *asked* to update, as opposed to merely told.
    ///
    /// The managed origins are the point. Offering to overwrite a Homebrew or
    /// Cargo binary in place is offering to break someone's installation, and
    /// the executor would refuse it anyway — so the question is never put.
    #[test]
    fn only_a_replaceable_copy_with_a_human_at_the_keyboard_is_asked() {
        use update::Origin;
        assert!(may_offer(&Origin::Standalone, true, false));

        // No terminal to read an answer from: a run whose stdin is a pipe
        // would eat that pipe to answer the question.
        assert!(!may_offer(&Origin::Standalone, false, false));

        // Already the product of an update this run. Without this an offer
        // could install and re-exec in a loop.
        assert!(!may_offer(&Origin::Standalone, true, true));

        for managed in [
            Origin::Homebrew,
            Origin::Cargo,
            Origin::MacPorts,
            Origin::NixStore,
            Origin::AppBundle,
        ] {
            assert!(
                !may_offer(&managed, true, false),
                "{managed:?} is not ours to replace"
            );
            // …and every one of them still has something to tell the user.
            assert!(managed.why_not().is_some() || managed == Origin::AppBundle);
        }
    }

    /// Return takes the update; EOF does not.
    #[test]
    fn return_accepts_and_only_eof_declines_without_saying_so() {
        // The default, taken by pressing return: one byte read, nothing in it.
        assert!(accepted(Some(1), "\n"));
        assert!(accepted(Some(3), " \n"));
        assert!(accepted(Some(2), "y\n"));
        assert!(accepted(Some(4), "YES\n"));

        assert!(!accepted(Some(2), "n\n"));
        assert!(!accepted(Some(3), "no\n"));
        // Not an answer to the question that was asked.
        assert!(!accepted(Some(6), "later\n"));

        // EOF: zero bytes, so the question was never answered at all. This is
        // the one empty answer that is not the default being taken.
        assert!(!accepted(Some(0), ""));
        // And a read that failed outright.
        assert!(!accepted(None, ""));
    }

    /// A plain scan, as `parse_args` would produce with no flags at all.
    fn args() -> Args {
        Args {
            json: false,
            with_sizes: true,
            apply: false,
            tiers: vec![Tier::Free],
            only_label: None,
            roots: Vec::new(),
            only_context: None,
            remote: false,
            no_tui: true,
            no_probe: false,
            force_container_probe: false,
            deadline: None,
            no_vault: false,
            vault_cmd: None,
            waiver_cmd: None,
            reason: None,
            update_cmd: None,
            no_update_check: true,
        }
    }

    #[test]
    fn a_remote_daemon_is_refused_by_default() {
        assert!(remote_authorisation(&args()).unwrap().is_none());
    }

    #[test]
    fn remote_without_a_reason_is_a_usage_error() {
        let a = Args {
            remote: true,
            ..args()
        };
        let err = remote_authorisation(&a).unwrap_err();
        // Exit 2, the house code for "you asked for this wrongly", not 3.
        assert_eq!(err.exit_code(), 2);

        // A gesture is not a reason, and neither is a blank one.
        for weak in ["", "   ", "because", "yes please"] {
            let a = Args {
                remote: true,
                reason: Some(weak.into()),
                ..args()
            };
            assert!(
                remote_authorisation(&a).is_err(),
                "{weak:?} must not authorise a remote daemon"
            );
        }
    }

    #[test]
    fn remote_with_a_written_reason_is_allowed_and_kept() {
        let a = Args {
            remote: true,
            reason: Some("  staging box, disk full, approved by ops  ".into()),
            ..args()
        };
        assert_eq!(
            remote_authorisation(&a).unwrap().as_deref(),
            Some("staging box, disk full, approved by ops"),
            "the reason is kept, trimmed, so the run can state it"
        );
    }
}
