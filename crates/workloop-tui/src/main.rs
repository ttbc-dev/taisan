//! `taisan-tui` — the management console: a kanban board over the
//! built-in queue plus typed Command actions. It is a line-driven
//! console: every verb either re-queries the projection or arms,
//! previews, and submits one typed Command — the identical contract
//! surface the CLI uses. Disconnecting it changes no execution truth.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use clap::Parser;
use workloop_config::load_or_create;
use workloop_core::command::{CommandDecision, CommandId, FindingChoice};
use workloop_core::domain::WorkId;
use workloop_core::store::Store;
use workloop_store::sqlite::SqliteStore;
use workloop_tui::render;
use workloop_tui::session::{Action, ORIGIN, SubmitOutcome, TuiSession};

#[derive(Parser)]
#[command(
    name = "taisan-tui",
    bin_name = "taisan-tui",
    about = "taisan management console (TUI)"
)]
struct Args {
    /// SQLite store path — the same store the CLI drives.
    #[arg(long, global = true, default_value = "workloop.db")]
    db: PathBuf,
    /// Installation config path (shared schema with the CLI).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn usage() -> &'static str {
    "verbs:
  refresh            re-query work_list/capacity/attention and redraw the board
  open <work|n>      open a Work by id or board row number; shows detail+findings
  judge              start a judgment over the open work's head-scoped findings
  pick <n> <choice> [rationale]  decide one finding: fix | dismiss | accept | wait
                     (dismiss/accept require a rationale)
  cancel|retry|resume|release   arm that Command on the open work
  ack <attention_id> arm an acknowledge Command
  preview            advisory preflight of the armed request (work::preview)
  submit             submit the armed request (work::handle_command)
  pending            show the request whose result is unknown
  recover [cmd_id]   settle a pending request: query command_result by its
                     replay key (a given cmd_id must match); with no cmd_id,
                     replay the identical key and payload if nothing landed
  detail | board     reprint the open work or the board
  quit               exit — disconnecting changes no execution truth"
}

pub(crate) fn main() {
    let args = Args::parse();
    if let Err(e) = run(args) {
        eprintln!("taisan-tui: {e}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> Result<(), String> {
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(|| args.db.with_extension("config.json"));
    let installation = load_or_create(&config_path)?;
    let store = SqliteStore::open(&args.db, installation.capacity).map_err(|e| e.to_string())?;
    let mut session = TuiSession::new(
        &store,
        installation.workloop_config()?,
        installation.actor(),
    );
    session.refresh(now())?;
    let mut selected: Option<WorkId> = None;

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    println!(
        "{}",
        render::board_frame(session.board().expect("refreshed"), None)
    );
    println!("{usage}", usage = usage());
    loop {
        print!("taisan-tui> ");
        let _ = std::io::stdout().flush();
        let Some(line) = lines.next() else {
            // EOF: disconnecting is a no-op on execution truth.
            return Ok(());
        };
        let line = line.map_err(|e| e.to_string())?.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let Some(verb) = it.next() else { continue };
        let rest: Vec<String> = it.map(str::to_string).collect();
        match dispatch(&mut session, &mut selected, verb, &rest) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            // A verb's failure is local: print it and keep the console —
            // a typo must never kill the session or reach `work`.
            Err(e) => println!("error: {e}"),
        }
    }
}

/// One console verb. Returns `Ok(false)` to quit; `Err` prints locally.
fn dispatch<S: Store>(
    session: &mut TuiSession<'_, S>,
    selected: &mut Option<WorkId>,
    verb: &str,
    rest: &[String],
) -> Result<bool, String> {
    match verb {
        "quit" | "q" | "exit" => return Ok(false),
        "help" | "h" | "?" => println!("{}", usage()),
        "refresh" | "r" => {
            session.refresh(now())?;
            println!(
                "{}",
                render::board_frame(session.board().expect("refreshed"), selected.as_ref())
            );
        }
        "board" | "b" => {
            let board = session.board().ok_or("run refresh first")?;
            println!("{}", render::board_frame(board, selected.as_ref()));
        }
        "open" | "o" => {
            let target = rest.first().ok_or("open <work_id|n>")?;
            let work_id = resolve_target(session, target)?;
            session.open(&work_id, now())?;
            *selected = Some(work_id);
            println!(
                "{}",
                render::detail_frame(session.detail().expect("opened"))
            );
            print!("{}", render::sources_frame(session.detail_sources()));
        }
        "detail" | "d" => {
            let detail = session.detail().ok_or("open a work first")?;
            println!("{}", render::detail_frame(detail));
            print!("{}", render::sources_frame(session.detail_sources()));
        }
        "judge" => {
            let draft = session.begin_judgment()?;
            println!("{}", render::draft_frame(draft));
        }
        "pick" => {
            let n: usize = rest
                .first()
                .ok_or("pick <n> <choice> [rationale]")?
                .parse()
                .map_err(|_| "pick <n>: n must be a number".to_string())?;
            let choice = parse_choice(rest.get(1).ok_or("pick <n> <choice> [rationale]")?)?;
            let rationale = if rest.len() > 2 {
                Some(rest[2..].join(" "))
            } else {
                None
            };
            let occurrence_id = {
                let draft = session.draft().ok_or("run judge first")?;
                draft
                    .findings
                    .get(n.wrapping_sub(1))
                    .ok_or_else(|| format!("no finding #{n} in the displayed set"))?
                    .occurrence_id
                    .clone()
            };
            session.choose(&occurrence_id, choice, rationale)?;
            println!("{}", render::draft_frame(session.draft().expect("draft")));
        }
        "cancel" | "retry" | "resume" | "release" => {
            let action = match verb {
                "cancel" => Action::Cancel,
                "retry" => Action::Retry,
                "resume" => Action::Resume,
                _ => Action::ReleaseJudgment,
            };
            let req = session.stage_action(&action)?;
            println!("armed {} (key {})", kind_of(req), req.idempotency_key);
        }
        "ack" => {
            let attention_id = rest.first().ok_or("ack <attention_id>")?.clone();
            let req = session.stage_action(&Action::Acknowledge { attention_id })?;
            println!("armed {} (key {})", kind_of(req), req.idempotency_key);
        }
        "preview" | "p" => {
            let preview = session.preview(now())?;
            println!("{}", render::preview_frame(&preview));
        }
        "submit" | "s" => match session.submit(now())? {
            SubmitOutcome::Decided(decision) => {
                print_decision(&decision);
                if let CommandDecision::Rejected { reason, .. } = &decision
                    && matches!(
                        reason,
                        workloop_core::command::RejectionCode::StaleHead
                            | workloop_core::command::RejectionCode::StaleWorkVersion
                    )
                {
                    println!("state moved — findings re-queried, old choices cleared");
                }
            }
            SubmitOutcome::Unknown => {
                println!("submit result: UNKNOWN — the response was lost; run recover");
            }
        },
        "pending" => match session.pending() {
            Some(req) => println!("{}", render::pending_frame(&req.idempotency_key, None)),
            None => println!("no pending request"),
        },
        "recover" => {
            let outcome = match rest.first() {
                Some(id) => session.recover_by_id(&CommandId(id.clone()), now())?,
                None => session.recover(now())?,
            };
            match outcome {
                workloop_tui::session::RecoverOutcome::Found(d) => {
                    println!("recovered recorded decision:");
                    print_decision(&d);
                }
                workloop_tui::session::RecoverOutcome::Replayed(d) => {
                    println!("replayed identical request:");
                    print_decision(&d);
                }
            }
        }
        _ => println!("unknown verb '{verb}' — try help"),
    }
    Ok(true)
}

fn print_decision(decision: &CommandDecision) {
    print!("{}", render::decision_frame(decision));
}

fn kind_of(req: &workloop_core::command::CommandRequest) -> String {
    serde_json::to_value(&req.body)
        .ok()
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
        .unwrap_or_else(|| ORIGIN.into())
}

fn parse_choice(s: &str) -> Result<FindingChoice, String> {
    match s {
        "fix" => Ok(FindingChoice::Fix),
        "dismiss" => Ok(FindingChoice::Dismiss),
        "accept" => Ok(FindingChoice::Accept),
        "wait" => Ok(FindingChoice::Wait),
        other => Err(format!(
            "unknown choice '{other}' — fix|dismiss|accept|wait"
        )),
    }
}

/// `open` accepts a Work id or the card number `board_frame` prints
/// (`Board::order()` — column-major).
fn resolve_target<S: Store>(session: &TuiSession<'_, S>, target: &str) -> Result<WorkId, String> {
    if let Ok(n) = target.parse::<usize>() {
        let order = session.board().ok_or("run refresh first")?.order();
        return order
            .get(n.wrapping_sub(1))
            .cloned()
            .ok_or_else(|| format!("no card #{n} on the board"));
    }
    Ok(WorkId(target.into()))
}
