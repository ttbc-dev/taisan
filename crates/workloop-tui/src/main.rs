//! `workloop-tui` binary — the interactive terminal driver for the TUI
//! state machine in `lib.rs`. It owns only I/O: raw-mode termios, ANSI
//! rendering, and byte→`Key` translation. All contract behaviour lives
//! in the library; all execution truth lives in the store, so quitting
//! here changes nothing.

use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use workloop_core::domain::{Actor, ActorKind, Origin};
use workloop_core::work::WorkloopConfig;
use workloop_store::sqlite::SqliteStore;
use workloop_tui::{Key, SqliteContracts, Tui};

#[derive(Parser)]
#[command(
    name = "workloop-tui",
    about = "management TUI over the workloop store"
)]
struct Cli {
    /// Path to the SQLite store; must be the same file the CLI uses.
    #[arg(long, default_value = "workloop.db")]
    db: PathBuf,
    /// Installation config path; defaults to <db>.config.json like the CLI.
    #[arg(long)]
    config: Option<PathBuf>,
}

/// Installation config — same file the CLI reads; only the fields the
/// TUI needs are decoded.
#[derive(serde::Deserialize)]
struct Installation {
    actor_id: String,
    actor_kind: String,
    #[serde(default = "default_repository")]
    repository: String,
    #[serde(default = "default_capacity")]
    capacity: u32,
    #[serde(default)]
    allowed_effects: Option<workloop_core::domain::AllowedEffects>,
    #[serde(default)]
    budget: Option<workloop_core::domain::ExecutionBudget>,
}

fn default_repository() -> String {
    "local/repo".into()
}
fn default_capacity() -> u32 {
    1
}

fn load_installation(path: &Path) -> Result<Installation, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("config read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("config: {e}"))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Raw mode: canonical input and echo off; restored on drop — the
/// terminal returns to its prior state no matter how the loop exits.
struct RawMode {
    fd: i32,
    saved: libc::termios,
}

impl RawMode {
    fn enable(fd: i32) -> Result<Self, String> {
        unsafe {
            let mut saved = std::mem::zeroed::<libc::termios>();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return Err("tcgetattr: not a terminal".into());
            }
            let mut raw = saved;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            raw.c_iflag &= !(libc::ICRNL | libc::IXON);
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
                return Err("tcsetattr failed".into());
            }
            Ok(Self { fd, saved })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved);
        }
    }
}

/// Wait up to `timeout` for input on `fd`; returns the bytes read.
fn read_input(fd: i32, timeout: Duration) -> Option<Vec<u8>> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis() as libc::c_int;
    let ready = unsafe { libc::poll(&mut pfd, 1, millis) };
    if ready <= 0 {
        return None;
    }
    let mut buf = [0u8; 64];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
    if n <= 0 {
        None
    } else {
        Some(buf[..n as usize].to_vec())
    }
}

fn bytes_to_keys(bytes: &[u8]) -> Vec<Key> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => {
                if bytes.len() >= i + 3 && bytes[i + 1] == b'[' {
                    keys.push(match bytes[i + 2] {
                        b'A' => Key::Up,
                        b'B' => Key::Down,
                        _ => Key::Esc,
                    });
                    i += 3;
                } else {
                    keys.push(Key::Esc);
                    i += 1;
                }
            }
            b'\r' | b'\n' => {
                keys.push(Key::Enter);
                i += 1;
            }
            c if c.is_ascii() => {
                keys.push(Key::Char(c as char));
                i += 1;
            }
            _ => i += 1,
        }
    }
    keys
}

fn terminal_width() -> usize {
    unsafe {
        let mut ws = std::mem::zeroed::<libc::winsize>();
        if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
            ws.ws_col as usize
        } else {
            120
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("workloop-tui: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| cli.db.with_extension("config.json"));
    let installation = load_installation(&config_path)?;
    let actor = Actor {
        kind: match installation.actor_kind.as_str() {
            "automation" => ActorKind::Automation,
            _ => ActorKind::Human,
        },
        external_id: installation.actor_id.clone(),
    };
    let config = WorkloopConfig {
        default_repository: installation.repository.clone(),
        allowed_effects: installation.allowed_effects.clone().unwrap_or(
            workloop_core::domain::AllowedEffects {
                workspace_write: true,
                github_write: false,
                network: false,
            },
        ),
        budget: installation.budget.clone().unwrap_or_default(),
    };
    let store =
        SqliteStore::open(&cli.db, installation.capacity).map_err(|e| format!("store: {e}"))?;
    let contracts = SqliteContracts {
        store: &store,
        config: &config,
        now_unix_seconds: now(),
    };
    let mut tui = Tui::new(contracts, actor, Origin("tui".into()));
    tui.refresh_board();

    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();
    let _raw = RawMode::enable(fd)?;
    let mut stdout = std::io::stdout();
    write!(stdout, "\x1b[?25l").map_err(|e| e.to_string())?;
    let result = event_loop(&mut tui, fd, &mut stdout);
    write!(stdout, "\x1b[?25h\x1b[2J\x1b[H").map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())?;
    result
}

fn event_loop<C: workloop_tui::Contracts>(
    tui: &mut Tui<C>,
    fd: i32,
    out: &mut impl Write,
) -> Result<(), String> {
    loop {
        write!(out, "\x1b[2J\x1b[H").map_err(|e| e.to_string())?;
        for line in tui.render(terminal_width()) {
            write!(out, "{line}\r\n").map_err(|e| e.to_string())?;
        }
        out.flush().map_err(|e| e.to_string())?;
        if tui.quit_requested() {
            return Ok(());
        }
        let keys = match read_input(fd, Duration::from_millis(1000)) {
            Some(bytes) => bytes_to_keys(&bytes),
            None => vec![Key::Tick],
        };
        for key in keys {
            tui.handle(key);
            if tui.quit_requested() {
                return Ok(());
            }
        }
    }
}
