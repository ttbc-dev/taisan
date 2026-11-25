//! `taisan-executor`: the durable remote executor helper.
//!
//! Two subcommands, matching the two halves of the reference
//! transport:
//!
//! - `serve` — the durable helper. Binds its configured unix socket,
//!   mints a fresh incarnation, and answers authenticated sessions
//!   with the six fixed executor operations. All Invocation state
//!   lives in the configured state root, so killing this process
//!   loses nothing durable: bound requests keep their recorded
//!   incarnation and read back as typed evidence on the next boot.
//! - `connect` — the transport bridge. Copies frames verbatim between
//!   stdio and the helper's socket. Under restricted SSH this is the
//!   forced command: the SSH session authenticates the operator
//!   principal, the session-auth handshake inside the channel
//!   authenticates the coordinator, and no shell is ever reachable.
//!
//! Neither subcommand exposes an RPC beyond the fixed envelope set.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use workloop_executor::config::EndpointConfig;
use workloop_executor::service::ExecutorService;

const USAGE: &str = "taisan-executor <serve|connect> --config <endpoint.toml>";

fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    let mut args = std::env::args().skip(1);
    let subcommand = args.next().unwrap_or_default();
    let mut config_path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config_path = args.next(),
            _ => {
                eprintln!("taisan-executor: unknown argument {arg:?}");
                eprintln!("{USAGE}");
                return 2;
            }
        }
    }
    let Some(config_path) = config_path else {
        eprintln!("{USAGE}");
        return 2;
    };
    let config = match EndpointConfig::load(std::path::Path::new(&config_path)) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("taisan-executor: config: {e}");
            return 1;
        }
    };
    match subcommand.as_str() {
        "serve" => serve(&config),
        "connect" => connect(&config),
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}

/// One shared stop flag; SIGTERM/SIGINT flip it so the accept loop
/// drains and exits instead of dying mid-write of a durable record.
/// (The record layer is atomic, so even an uncaught kill loses
/// nothing committed — this is cleanliness, not correctness.)
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop(_signal: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn serve(config: &EndpointConfig) -> i32 {
    let service = match ExecutorService::boot(config.clone()) {
        Ok(service) => Arc::new(service),
        Err(e) => {
            eprintln!("taisan-executor serve: {e}");
            return 1;
        }
    };
    unsafe {
        libc::signal(libc::SIGTERM, on_stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_stop as *const () as libc::sighandler_t);
    }
    let stop = Arc::new(AtomicBool::new(false));
    // Bridge the process-global stop flag to the serve loop's flag.
    {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !STOP.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            stop.store(true, Ordering::SeqCst);
        });
    }
    eprintln!(
        "taisan-executor serve: incarnation {} on {}",
        service.incarnation(),
        config.socket_path.display()
    );
    match service.serve_unix(&config.socket_path, stop) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("taisan-executor serve: {e}");
            1
        }
    }
}

/// Copy frames verbatim between stdio and the helper's socket. Both
/// directions shut down on their peer's EOF; no byte is interpreted
/// here — the helper's session-auth handshake owns the channel's
/// trust.
fn connect(config: &EndpointConfig) -> i32 {
    let mut reader = match UnixStream::connect(&config.socket_path) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!(
                "taisan-executor connect: {}: {e}",
                config.socket_path.display()
            );
            return 1;
        }
    };
    let mut writer = match reader.try_clone() {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("taisan-executor connect: {e}");
            return 1;
        }
    };
    let upstream = std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if writer.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = writer.shutdown(Shutdown::Write);
    });
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
        let _ = stdout.flush();
    }
    let _ = upstream.join();
    0
}
