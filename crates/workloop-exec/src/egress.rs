//! The taisan-owned CONNECT egress proxy and the in-sandbox relay
//! (`docs/v0.3.md` §14.4–§14.5).
//!
//! Worker and verification sandboxes run with `--unshare-net`: only
//! loopback exists inside. A [`ConnectProxy`] listens on a Unix socket
//! bind-mounted into the sandbox at a fixed path; an in-sandbox relay
//! (the `__sandbox-relay` CLI subcommand, [`relay_main`]) listens on
//! `127.0.0.1` and splices each connection to that socket, so the
//! runtime sees an ordinary `HTTPS_PROXY` endpoint.
//!
//! The proxy accepts `CONNECT host:443` only, for exact bare host
//! names in its allowlist — DNS is resolved and the TCP connection is
//! dialed here, outside the sandbox. Every other destination,
//! including the repository host, package registries, and arbitrary
//! addresses, is refused. TLS is never inspected. Each verdict is
//! appended to a bounded JSONL evidence file — destination and
//! verdict only, never payload bytes.
//!
//! Proxy failure is an environment repair: callers refuse dispatch
//! rather than degrade to `--share-net` or an unrestricted network.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Serialize;

/// The loopback endpoint the in-sandbox relay binds. Every sandbox
/// has its own network namespace, so one fixed port cannot collide.
pub const RELAY_LISTEN: &str = "127.0.0.1:18080";
/// The `HTTPS_PROXY` value the contained environment carries.
pub const RELAY_PROXY_URL: &str = "http://127.0.0.1:18080";

/// Bound on one CONNECT request head — a proxy request is a single
/// line plus headers; anything larger is malformed.
const MAX_REQUEST_HEAD: u64 = 8 * 1024;

/// The directory proxy sockets live in. `preferred` is used when a
/// socket name still fits `sun_path` (~104 bytes); a deeper state
/// root falls back to a short owner-only `/tmp` directory derived
/// from `preferred`'s digest — egress must never degrade to an
/// unavailable-proxy refusal because the state root is deep. The
/// fallback is `0700`: the socket is a CONNECT capability, never
/// world-connectable.
pub fn socket_dir(preferred: &Path) -> std::io::Result<PathBuf> {
    // sun_path is 104 bytes on Linux and macOS; socket names here are
    // at most ~32 bytes, so the directory must stay under ~68.
    if preferred.as_os_str().len() <= 68 {
        return Ok(preferred.to_path_buf());
    }
    let digest = &workloop_core::domain::sha256_hex(
        "egress-socket-dir",
        preferred.as_os_str().as_encoded_bytes(),
    )[..16];
    let dir = PathBuf::from("/tmp").join(format!("taisan-egress-{digest}"));
    std::fs::create_dir_all(&dir)?;
    // The fallback dir is a CONNECT capability: it must be a real
    // directory owned by the effective user. A pre-created symlink or
    // foreign-owned directory is refused before any chmod or bind —
    // `/tmp` is sticky, so once the owner check passes the entry
    // cannot be swapped out from under us.
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(&dir)?;
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "egress socket directory is not owned by the effective user",
            ));
        }
    }
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// Per-connection bound for reading the request head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Evidence file bound: destination+verdict lines only, and the file
/// stops growing past this size — evidence is bounded by contract.
const MAX_EVIDENCE_BYTES: u64 = 512 * 1024;

/// Dials `host:port` on behalf of an allowed CONNECT. Injectable so
/// tests prove allow/deny without real DNS or egress.
pub type Dialer = Arc<dyn Fn(&str, u16) -> std::io::Result<TcpStream> + Send + Sync>;

fn default_dialer(host: &str, port: u16) -> std::io::Result<TcpStream> {
    TcpStream::connect((host, port))
}

#[derive(Debug, Serialize)]
struct EvidenceLine {
    unix_seconds: i64,
    destination: String,
    verdict: &'static str,
}

struct ProxyState {
    allowlist: Vec<String>,
    evidence_path: PathBuf,
    dialer: Dialer,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
}

/// A running CONNECT proxy. [`ConnectProxy::shutdown`] closes the
/// listener; dropping without shutdown leaves the accept thread to
/// exit with the process — the socket file is unlinked either way on
/// the next `start`.
pub struct ConnectProxy {
    sock_path: PathBuf,
    evidence_path: PathBuf,
    allowlist: Vec<String>,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<std::thread::JoinHandle<()>>,
}

impl ConnectProxy {
    /// Bind `sock_path` and serve CONNECT requests for `allowlist`
    /// (bare host names, matched case-insensitively after lowering).
    /// `evidence_path` receives the bounded verdict log. A stale socket
    /// file is replaced — the run lease serializes supervisors, so a
    /// live peer cannot be unlinked.
    pub fn start(
        sock_path: &Path,
        allowlist: Vec<String>,
        evidence_path: &Path,
    ) -> std::io::Result<Self> {
        Self::start_with(sock_path, allowlist, evidence_path, None, None)
    }

    /// `start` with an injectable dialer and clock — the deterministic
    /// seam the suite uses to prove allow/deny without real egress.
    pub(crate) fn start_with(
        sock_path: &Path,
        allowlist: Vec<String>,
        evidence_path: &Path,
        dialer: Option<Dialer>,
        now: Option<Arc<dyn Fn() -> i64 + Send + Sync>>,
    ) -> std::io::Result<Self> {
        if let Some(parent) = sock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(parent) = evidence_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // A crashed earlier proxy leaves its socket file behind; the
        // run lease guarantees no live listener owns it.
        let _ = std::fs::remove_file(sock_path);
        let listener = UnixListener::bind(sock_path)?;
        listener.set_nonblocking(true)?;
        let allowlist: Vec<String> = allowlist
            .iter()
            .map(|h| h.trim().to_lowercase())
            .filter(|h| !h.is_empty())
            .collect();
        let state = Arc::new(ProxyState {
            allowlist: allowlist.clone(),
            evidence_path: evidence_path.to_path_buf(),
            dialer: dialer.unwrap_or_else(|| Arc::new(default_dialer)),
            now: now.unwrap_or_else(|| {
                Arc::new(|| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0)
                })
            }),
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let accept_shutdown = shutdown.clone();
        let accept_state = state.clone();
        let accept_thread = std::thread::spawn(move || {
            while !accept_shutdown.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let conn_state = accept_state.clone();
                        std::thread::spawn(move || handle_conn(stream, conn_state));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            sock_path: sock_path.to_path_buf(),
            evidence_path: evidence_path.to_path_buf(),
            allowlist,
            shutdown,
            accept_thread: Some(accept_thread),
        })
    }

    /// The Unix socket path the sandbox bind-mounts.
    pub fn sock_path(&self) -> &Path {
        &self.sock_path
    }

    /// The allowlist this proxy enforces — recorded for evidence and
    /// diagnostics, never secret.
    pub fn allowlist(&self) -> &[String] {
        &self.allowlist
    }

    /// The bounded verdict log path.
    pub fn evidence_path(&self) -> &Path {
        &self.evidence_path
    }

    /// Stop accepting and join the accept thread. In-flight
    /// connections finish with their sockets; callers shut down after
    /// the sandboxes they served have exited.
    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

impl Drop for ConnectProxy {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// Read one bounded request head from `stream`.
fn read_head(stream: &mut UnixStream) -> Option<String> {
    stream.set_read_timeout(Some(HEAD_TIMEOUT)).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if buf.len() as u64 > MAX_REQUEST_HEAD {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    String::from_utf8(buf).ok()
}

/// Parse `CONNECT host:port HTTP/1.x` → `(host, port)`. Anything else
/// — another method, a missing port, userinfo, a path — is malformed.
fn parse_connect(head: &str) -> Option<(String, u16)> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    if parts.next()? != "CONNECT" {
        return None;
    }
    let authority = parts.next()?;
    // Exactly `host:port` — no userinfo, path, or query.
    let (host, port) = authority.rsplit_once(':')?;
    if host.is_empty() || host.contains('@') || host.contains('/') {
        return None;
    }
    let port: u16 = port.parse().ok()?;
    Some((host.to_lowercase(), port))
}

/// Write an HTTP response head. A `200` opens the tunnel — no
/// `Connection: close`, the splice takes over; errors close.
fn respond(stream: &mut UnixStream, status: &str) {
    let head = if status.starts_with("200") {
        "HTTP/1.1 200 Connection Established\r\n\r\n".to_string()
    } else {
        format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    };
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.flush();
}

fn record(state: &ProxyState, destination: &str, verdict: &'static str) {
    let line = EvidenceLine {
        unix_seconds: (state.now)(),
        destination: destination.to_string(),
        verdict,
    };
    let Ok(mut text) = serde_json::to_string(&line) else {
        return;
    };
    text.push('\n');
    // Bounded evidence: past the cap the file stops growing — the
    // verdicts already recorded stay, and the proxy keeps enforcing.
    let size = std::fs::metadata(&state.evidence_path)
        .map(|m| m.len())
        .unwrap_or(0);
    if size + text.len() as u64 > MAX_EVIDENCE_BYTES {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&state.evidence_path)
    {
        let _ = file.write_all(text.as_bytes());
    }
}

fn handle_conn(stream: UnixStream, state: Arc<ProxyState>) {
    let mut stream = stream;
    let Some(head) = read_head(&mut stream) else {
        return;
    };
    let Some((host, port)) = parse_connect(&head) else {
        respond(&mut stream, "400 Bad Request");
        return;
    };
    let destination = format!("{host}:{port}");
    // Exact-host, fixed-port policy: only the configured bare host
    // names on 443 are reachable. Everything else — including the
    // repository host and the verification allowlist — is denied here.
    if port != 443 || !state.allowlist.contains(&host) {
        record(&state, &destination, "denied");
        respond(&mut stream, "403 Forbidden");
        return;
    }
    let upstream = match (state.dialer)(&host, port) {
        Ok(stream) => stream,
        Err(_) => {
            record(&state, &destination, "dial_failed");
            respond(&mut stream, "502 Bad Gateway");
            return;
        }
    };
    record(&state, &destination, "allowed");
    respond(&mut stream, "200 Connection Established");
    // Bidirectional splice: the sandbox side and the upstream socket
    // forward bytes verbatim — TLS is never inspected. One clone per
    // direction keeps each pump on its own handle; each pump
    // propagates its EOF as a write-side shutdown so a closed peer
    // cannot strand the other direction's threads.
    let Ok(sandbox_read) = stream.try_clone() else {
        return;
    };
    let Ok(upstream_read) = upstream.try_clone() else {
        return;
    };
    let _ = upstream.set_nodelay(true);
    std::thread::scope(|scope| {
        scope.spawn(|| pump(sandbox_read, upstream));
        scope.spawn(|| pump(upstream_read, stream));
    });
}

/// A writer whose write side can be shut down independently — the
/// splice propagates EOF so the peer's read ends instead of leaving
/// pump threads parked on a half-closed socket.
trait ShutdownWrite: Write {
    fn shutdown_write(&self);
}

impl ShutdownWrite for TcpStream {
    fn shutdown_write(&self) {
        let _ = self.shutdown(std::net::Shutdown::Write);
    }
}

impl ShutdownWrite for UnixStream {
    fn shutdown_write(&self) {
        let _ = self.shutdown(std::net::Shutdown::Write);
    }
}

/// Copy `reader` to `writer` until either side closes, then shut the
/// writer's send side down so the far end sees EOF and its own pump
/// can finish.
fn pump<R, W>(mut reader: R, writer: W)
where
    R: Read,
    W: ShutdownWrite,
{
    let mut writer = writer;
    let mut buf = [0u8; 16 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if writer.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = writer.flush();
    writer.shutdown_write();
}

/// The in-sandbox relay entry point (`taisan __sandbox-relay`). Binds
/// `listen` on loopback inside the sandbox's network namespace, then
/// spawns `command` as its child — the relay is the sandbox's init
/// and stays alive for the child's whole life so the PID namespace
/// boundary holds (§14.6). Every accepted connection is spliced to
/// `proxy_sock`; the child's `HTTPS_PROXY` environment (set by the
/// supervisor's contained env) points at `listen`.
///
/// Returns the child's exit code, or 127 when the child cannot start.
pub fn relay_main(proxy_sock: &Path, listen: &str, command: &[String]) -> i32 {
    let Some(program) = command.first() else {
        return 2;
    };
    let listener = match TcpListener::bind(listen) {
        Ok(listener) => listener,
        Err(_) => return 2,
    };
    let sock = proxy_sock.to_path_buf();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(tcp) = conn else { break };
            let sock = sock.clone();
            std::thread::spawn(move || relay_conn(tcp, &sock));
        }
    });
    match std::process::Command::new(program)
        .args(&command[1..])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
    {
        Ok(mut child) => match child.wait() {
            Ok(status) => status.code().unwrap_or(1),
            Err(_) => 1,
        },
        Err(_) => 127,
    }
}

/// Splice one relayed TCP connection to the proxy's Unix socket.
fn relay_conn(tcp: TcpStream, sock: &Path) {
    let _ = tcp.set_nodelay(true);
    let Ok(unix) = UnixStream::connect(sock) else {
        return;
    };
    let Ok(tcp_read) = tcp.try_clone() else {
        return;
    };
    let Ok(unix_read) = unix.try_clone() else {
        return;
    };
    std::thread::scope(|scope| {
        scope.spawn(|| pump(tcp_read, unix));
        scope.spawn(|| pump(unix_read, tcp));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("egress-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A dialer that maps every allowed host onto a local echo server.
    fn echo_dialer() -> Dialer {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let mut s = conn;
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"echo-back");
            }
        });
        Arc::new(move |_, _| TcpStream::connect(addr))
    }

    /// Send a CONNECT request and read only the response head — a 200
    /// opens a tunnel that never closes on its own.
    fn connect_request(stream: &mut UnixStream, authority: &str) -> String {
        stream
            .write_all(
                format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes(),
            )
            .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 256];
        // Read until the header terminator; bounded so a missing one
        // cannot hang the test.
        for _ in 0..64 {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[test]
    fn allowed_host_connects_and_bytes_flow() {
        let dir = tmp("allow");
        let proxy = ConnectProxy::start_with(
            &dir.join("p.sock"),
            vec!["model.example".into()],
            &dir.join("evidence.jsonl"),
            Some(echo_dialer()),
            None,
        )
        .unwrap();
        let mut conn = UnixStream::connect(proxy.sock_path()).unwrap();
        let response = connect_request(&mut conn, "model.example:443");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        // After the 200 head the splice carries payload both ways.
        conn.write_all(b"ping").unwrap();
        let mut buf = [0u8; 9];
        conn.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"echo-back");
        drop(conn);
        let evidence_path = dir.join("evidence.jsonl");
        proxy.shutdown();
        let evidence = std::fs::read_to_string(evidence_path).unwrap();
        assert!(evidence.contains("\"allowed\""), "{evidence}");
        assert!(evidence.contains("model.example:443"), "{evidence}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn denied_hosts_ports_and_methods_are_refused() {
        let dir = tmp("deny");
        let proxy = ConnectProxy::start_with(
            &dir.join("p.sock"),
            vec!["model.example".into()],
            &dir.join("evidence.jsonl"),
            Some(echo_dialer()),
            None,
        )
        .unwrap();
        for (authority, expect) in [
            ("other.example:443", "403"),
            ("model.example:80", "403"),
            ("model.example:444", "403"),
            ("evil-model.example:443", "403"),
            ("MODEL.EXAMPLE.:443", "403"),
        ] {
            let mut conn = UnixStream::connect(proxy.sock_path()).unwrap();
            let response = connect_request(&mut conn, authority);
            assert!(response.contains(expect), "{authority}: {response}");
        }
        // A non-CONNECT request is malformed, not a tunnel.
        let mut conn = UnixStream::connect(proxy.sock_path()).unwrap();
        conn.write_all(b"GET http://model.example/ HTTP/1.1\r\n\r\n")
            .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 256];
        for _ in 0..64 {
            match conn.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        let response = String::from_utf8_lossy(&buf);
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
        drop(conn);
        let evidence_path = dir.join("evidence.jsonl");
        proxy.shutdown();
        let evidence = std::fs::read_to_string(evidence_path).unwrap();
        let denied = evidence.matches("\"denied\"").count();
        assert_eq!(denied, 5, "{evidence}");
        // Evidence names destinations and verdicts only — never a
        // payload or credential.
        assert!(!evidence.contains("GET http"), "{evidence}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dial_failure_is_a_502_not_a_tunnel() {
        let dir = tmp("dialfail");
        let proxy = ConnectProxy::start_with(
            &dir.join("p.sock"),
            vec!["model.example".into()],
            &dir.join("evidence.jsonl"),
            Some(Arc::new(|_, _| Err(std::io::Error::other("no route")))),
            None,
        )
        .unwrap();
        let mut conn = UnixStream::connect(proxy.sock_path()).unwrap();
        let response = connect_request(&mut conn, "model.example:443");
        assert!(response.starts_with("HTTP/1.1 502"), "{response}");
        drop(conn);
        let evidence_path = dir.join("evidence.jsonl");
        proxy.shutdown();
        let evidence = std::fs::read_to_string(evidence_path).unwrap();
        assert!(evidence.contains("\"dial_failed\""), "{evidence}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relay_conn_splices_tcp_to_the_proxy_socket() {
        let dir = tmp("relay");
        let proxy = ConnectProxy::start_with(
            &dir.join("p.sock"),
            vec!["model.example".into()],
            &dir.join("evidence.jsonl"),
            Some(Arc::new(|_, _| Err(std::io::Error::other("no dial")))),
            None,
        )
        .unwrap();
        // A TCP listener stands in for the in-sandbox relay endpoint;
        // each accepted connection is spliced to the proxy socket.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let relay_addr = listener.local_addr().unwrap();
        let sock = proxy.sock_path().to_path_buf();
        std::thread::spawn(move || {
            if let Ok((tcp, _)) = listener.accept() {
                relay_conn(tcp, &sock);
            }
        });
        let mut client = TcpStream::connect(relay_addr).unwrap();
        client
            .write_all(b"CONNECT denied.example:443 HTTP/1.1\r\n\r\n")
            .unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 256];
        for _ in 0..64 {
            match client.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
            }
        }
        let response = String::from_utf8_lossy(&buf);
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        drop(client);
        proxy.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
