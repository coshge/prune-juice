//! The interactive terminal interface.
//!
//! `prune-juice` with no arguments lands here. Work happens on background
//! threads and reaches the UI as messages, so the interface stays responsive
//! while a `system df` takes its several seconds.

pub mod app;
mod ui;

/// Render one frame. Exposed so the `preview` example can show every screen
/// without a terminal — useful in CI and when iterating on layout.
pub fn draw_for_preview(f: &mut ratatui::Frame, app: &App) {
    ui::draw(f, app);
}

use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use prune_juice_core::docker::bollard_client::BollardClient;
use prune_juice_core::event::{Cancel, ChannelSink, Event};
use prune_juice_core::execute::{ExecuteOptions, Executor, Mode, Receipt};
use prune_juice_core::plan::tier::Tier;
use prune_juice_core::plan::{Plan, Planner};
use prune_juice_core::scan::{ScanOptions, ScanReport, Scanner};
use prune_juice_core::Error;

use app::{Action, App, Key, Screen};

/// What the worker threads send back.
enum Msg {
    // Boxed: `Event` carries a whole `ResourceSummary`, and an unboxed variant
    // would make every message that size.
    Scan(Box<Event>),
    Ready(Box<(ScanReport, Plan)>),
    Applied(Box<Receipt>),
    Failed(String),
}

pub struct TuiOptions {
    pub endpoint: String,
    pub context: String,
    pub scan: ScanOptions,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Build every TUI scanner through the same provenance-aware path. Planning
/// and pre-delete revalidation must not disagree merely because one forgot the
/// index.
fn scanner<'a>(
    client: &'a BollardClient,
    index: Option<&'a prune_juice_core::index::Index>,
) -> Scanner<'a> {
    let scanner = Scanner::with_probe(client, client);
    match index {
        Some(index) => scanner.with_index(index),
        None => scanner,
    }
}

/// Run the interface. Returns a process exit code.
pub fn run(opts: TuiOptions) -> Result<i32, Error> {
    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, opts);
    // Restore before propagating anything: a panic or an error must never
    // leave the user staring at a raw-mode terminal.
    restore_terminal(&mut terminal);
    result
}

type Term = Terminal<CrosstermBackend<io::Stdout>>;

fn setup_terminal() -> Result<Term, Error> {
    // If we panic mid-render the default hook prints into a raw alternate
    // screen and the shell is left unusable. Restore first, then panic.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));

    enable_raw_mode().map_err(Error::Io)?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(Error::Io)?;
    Terminal::new(CrosstermBackend::new(stdout)).map_err(Error::Io)
}

fn restore_terminal(terminal: &mut Term) {
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
}

fn event_loop(terminal: &mut Term, opts: TuiOptions) -> Result<i32, Error> {
    let mut app = App::new();
    let cancel = Cancel::new();

    let (tx, rx) = mpsc::channel::<Msg>();
    spawn_scan(&opts, tx.clone(), cancel.clone());

    loop {
        terminal.draw(|f| ui::draw(f, &app)).map_err(Error::Io)?;

        // Drain whatever the workers have produced without blocking the
        // redraw. A slow `df` must not freeze the interface.
        drain(&rx, &mut app);

        if event::poll(Duration::from_millis(80)).map_err(Error::Io)? {
            if let CtEvent::Key(k) = event::read().map_err(Error::Io)? {
                if k.kind != KeyEventKind::Release {
                    if is_hard_quit(&k) {
                        cancel.cancel();
                        break;
                    }
                    match app.on_key(translate(k)) {
                        Action::Quit => {
                            cancel.cancel();
                            break;
                        }
                        Action::Rescan => {
                            app = App::new();
                            spawn_scan(&opts, tx.clone(), cancel.clone());
                        }
                        Action::ReclaimFree => {
                            if let Some(plan) = app.plan.take() {
                                app.screen = Screen::Applying;
                                app.status = "reclaiming…".into();
                                spawn_apply(
                                    &opts,
                                    plan,
                                    vec![Tier::Free],
                                    None,
                                    tx.clone(),
                                    cancel.clone(),
                                );
                            }
                        }
                        Action::ApplySelected => {
                            let names: std::collections::BTreeSet<String> =
                                app.selected_names().into_iter().collect();
                            let tiers = app.selected_tiers();
                            if let Some(plan) = app.plan.take() {
                                app.screen = Screen::Applying;
                                app.status =
                                    "re-checking and removing the items you ticked…".into();
                                spawn_apply(
                                    &opts,
                                    plan,
                                    tiers,
                                    Some(names),
                                    tx.clone(),
                                    cancel.clone(),
                                );
                            }
                        }
                        Action::None => {}
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    // Exit code mirrors the CLI's contract.
    let code = match &app.receipt {
        Some(r) if r.problems() > 0 => 5,
        _ if app.free_bytes() > prune_juice_core::model::Bytes::ZERO => 1,
        _ => 0,
    };
    Ok(code)
}

/// Ctrl-C must always work, whatever screen we are on.
fn is_hard_quit(k: &KeyEvent) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL) && matches!(k.code, KeyCode::Char('c' | 'C'))
}

fn translate(k: KeyEvent) -> Key {
    match k.code {
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Char(c) => Key::Char(c),
        _ => Key::Char('\0'),
    }
}

fn drain(rx: &Receiver<Msg>, app: &mut App) {
    while let Ok(msg) = rx.try_recv() {
        match msg {
            Msg::Scan(e) => match *e {
                Event::ScanStarted { context, .. } => {
                    app.status = format!("scanning {context}…");
                }
                Event::ResourceFound { resource } => app.note_resource(resource.kind),
                Event::Phase { phase, .. } => {
                    // `Sizing` is the slow one; naming it stops the pause
                    // looking like a hang.
                    app.status = match phase {
                        prune_juice_core::event::Phase::Sizing => {
                            "measuring volume sizes (this is the slow part)…".into()
                        }
                        prune_juice_core::event::Phase::Listing => "listing resources…".into(),
                        prune_juice_core::event::Phase::Attributing => {
                            "working out who owns what…".into()
                        }
                        other => format!("{other:?}…"),
                    };
                }
                Event::Warning { message, .. } => app.status = format!("! {message}"),
                _ => {}
            },
            Msg::Ready(b) => {
                let (report, plan) = *b;
                app.ready(report, plan);
            }
            Msg::Applied(r) => app.finished(*r),
            Msg::Failed(e) => {
                app.status = format!("error: {e}");
                app.screen = Screen::Finished;
            }
        }
    }
}

fn spawn_scan(opts: &TuiOptions, tx: Sender<Msg>, cancel: Cancel) {
    let endpoint = opts.endpoint.clone();
    let context = opts.context.clone();
    let scan_opts = opts.scan.clone();

    std::thread::spawn(move || {
        let (etx, erx) = mpsc::channel::<Event>();
        // Forward core events to the UI channel on their own thread so a slow
        // consumer cannot block the scan.
        let fwd = tx.clone();
        std::thread::spawn(move || {
            while let Ok(e) = erx.recv() {
                if fwd.send(Msg::Scan(Box::new(e))).is_err() {
                    break;
                }
            }
        });

        let client = match BollardClient::connect(&endpoint) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Msg::Failed(e.to_string()));
                return;
            }
        };
        let sink = Arc::new(ChannelSink(etx));
        let index = prune_juice_core::index::Index::open().ok();
        match scanner(&client, index.as_ref()).scan(&context, &scan_opts, sink, &cancel) {
            Ok(report) => {
                let plan = Planner::plan(&report, now_unix());
                let _ = tx.send(Msg::Ready(Box::new((report, plan))));
            }
            Err(e) => {
                let _ = tx.send(Msg::Failed(e.to_string()));
            }
        }
    });
}

fn spawn_apply(
    opts: &TuiOptions,
    plan: Plan,
    tiers: Vec<Tier>,
    only_names: Option<std::collections::BTreeSet<String>>,
    tx: Sender<Msg>,
    cancel: Cancel,
) {
    let endpoint = opts.endpoint.clone();
    let context = opts.context.clone();
    let scan_opts = opts.scan.clone();

    std::thread::spawn(move || {
        let client = match BollardClient::connect(&endpoint) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Msg::Failed(e.to_string()));
                return;
            }
        };

        // Re-scan first. Every witness is revalidated against THIS report,
        // item by item, immediately before its own delete.
        let (etx, _erx) = mpsc::channel::<Event>();
        let sink = Arc::new(ChannelSink(etx));
        // The planning scan may have admitted stopped containers only because
        // their volume provenance was durably checkpointed. Revalidation must
        // use the same index or every such witness changes tier and is skipped.
        let index = prune_juice_core::index::Index::open().ok();
        let fresh = match scanner(&client, index.as_ref()).scan(
            &context,
            &scan_opts,
            sink.clone(),
            &cancel,
        ) {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Msg::Failed(e.to_string()));
                return;
            }
        };

        let exec_opts = ExecuteOptions {
            mode: Mode::Apply,
            tiers,
            only_label: None,
            // Reviewed rows are fenced by name: anything the user did not tick
            // is unreachable by this run, not merely skipped.
            only_names,
            // Always on. An irreversible item is copied and verified first, or
            // it is refused — the interface never offers bare deletion.
            vault: true,
        };
        let vault = match prune_juice_core::vault::Vault::open() {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(Msg::Failed(e.to_string()));
                return;
            }
        };
        let store = prune_juice_core::disk::detect(
            fresh.daemon.runtime,
            fresh.daemon.data_root.as_deref(),
            &endpoint,
        );
        match Executor::applying(&client)
            .with_vault(&client, &vault)
            .with_disk(&store)
            .run(plan, &fresh, now_unix(), &exec_opts, sink, &cancel)
        {
            Ok(r) => {
                let _ = tx.send(Msg::Applied(Box::new(r)));
            }
            Err(e) => {
                let _ = tx.send(Msg::Failed(e.to_string()));
            }
        }
    });
}
