//! The systemd notification protocol (`sd_notify`, spec D14) for
//! `Type=notify` units: `READY=1` once every vault is open, `RELOADING=1`
//! around a SIGHUP reload, `WATCHDOG=1` while the runtime is alive, and
//! `STOPPING=1` at shutdown.
//!
//! Outside systemd (no `NOTIFY_SOCKET`) every call is a no-op. A failed send
//! is logged and otherwise ignored: the service never stops because its
//! supervisor cannot be told something.

use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, warn};

/// Where the messages go.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Path(PathBuf),
    /// A Linux abstract socket (`@name` in `NOTIFY_SOCKET`).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Abstract(Vec<u8>),
}

/// The service manager's notification socket.
pub struct Notifier {
    target: Target,
    sock: UnixDatagram,
}

impl Notifier {
    /// From `NOTIFY_SOCKET`, when the process runs under a manager that
    /// wants notifications.
    pub fn from_env() -> Option<Notifier> {
        let v = std::env::var_os("NOTIFY_SOCKET")?;
        let v = v.to_string_lossy();
        match Notifier::new(&v) {
            Ok(n) => n,
            Err(e) => {
                warn!("NOTIFY_SOCKET={v}: {e}; the service manager is not notified");
                None
            }
        }
    }

    /// `Ok(None)` for an address this build does not speak (`vsock:`, or
    /// an abstract socket off Linux).
    pub fn new(addr: &str) -> std::io::Result<Option<Notifier>> {
        let target = if let Some(name) = addr.strip_prefix('@') {
            if !cfg!(target_os = "linux") {
                debug!("NOTIFY_SOCKET {addr}: abstract sockets exist only on Linux");
                return Ok(None);
            }
            Target::Abstract(name.as_bytes().to_vec())
        } else if addr.starts_with('/') {
            Target::Path(PathBuf::from(addr))
        } else {
            debug!("NOTIFY_SOCKET {addr}: not a path or an abstract socket; not notifying");
            return Ok(None);
        };
        let sock = UnixDatagram::unbound()?;
        Ok(Some(Notifier { target, sock }))
    }

    /// Send one message: `KEY=VALUE` lines.
    pub fn send(&self, msg: &str) -> std::io::Result<()> {
        match &self.target {
            Target::Path(p) => self.sock.send_to(msg.as_bytes(), p).map(|_| ()),
            #[cfg(target_os = "linux")]
            Target::Abstract(name) => {
                use std::os::linux::net::SocketAddrExt;
                let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
                self.sock.send_to_addr(msg.as_bytes(), &addr).map(|_| ())
            }
            #[cfg(not(target_os = "linux"))]
            Target::Abstract(_) => Ok(()),
        }
    }

    fn tell(&self, msg: &str) {
        if let Err(e) = self.send(msg) {
            warn!(
                "notifying the service manager ({}): {e}",
                msg.lines().next().unwrap_or_default()
            );
        }
    }

    pub fn ready(&self, status: &str) {
        self.tell(&ready_message(status));
    }

    pub fn reloading(&self) {
        self.tell(&reloading_message(monotonic_usec()));
    }

    pub fn watchdog(&self) {
        self.tell("WATCHDOG=1");
    }

    pub fn stopping(&self) {
        self.tell("STOPPING=1\nSTATUS=stopping");
    }
}

/// A status is one line in the protocol.
fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

pub fn ready_message(status: &str) -> String {
    format!("READY=1\nSTATUS={}", one_line(status))
}

/// `RELOADING=1` carries the monotonic time it was sent at, as systemd
/// wants since 253 (older versions ignore it).
pub fn reloading_message(monotonic_usec: u64) -> String {
    format!("RELOADING=1\nMONOTONIC_USEC={monotonic_usec}\nSTATUS=reloading the config")
}

/// `CLOCK_MONOTONIC` in microseconds, the clock systemd compares with.
pub fn monotonic_usec() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: ts is a valid timespec to write to; CLOCK_MONOTONIC exists everywhere we build.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1_000
}

/// How often to send `WATCHDOG=1`: half of `WATCHDOG_USEC`, when the
/// watchdog is on and meant for this process (`WATCHDOG_PID`, when set).
pub fn watchdog_interval() -> Option<Duration> {
    watchdog_interval_from(
        std::env::var("WATCHDOG_USEC").ok().as_deref(),
        std::env::var("WATCHDOG_PID").ok().as_deref(),
        std::process::id(),
    )
}

pub fn watchdog_interval_from(usec: Option<&str>, pid: Option<&str>, me: u32) -> Option<Duration> {
    if let Some(p) = pid.filter(|p| !p.is_empty()) {
        if p.trim().parse::<u32>().ok() != Some(me) {
            return None;
        }
    }
    let usec: u64 = usec?.trim().parse().ok().filter(|&u| u > 0)?;
    Some(Duration::from_micros(usec / 2).max(Duration::from_millis(10)))
}

/// Send `WATCHDOG=1` every `every`, each time only after `alive` has
/// returned. `alive` takes the locks the service works under, so a vault
/// stuck holding one stops the pings and systemd restarts the service, as
/// it would for a runtime that stopped scheduling tasks. It runs on a
/// blocking thread: a lock that never comes free stalls this task, not a
/// runtime worker.
pub fn spawn_watchdog(
    n: std::sync::Arc<Notifier>,
    every: Duration,
    alive: std::sync::Arc<dyn Fn() + Send + Sync>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let a = alive.clone();
            if tokio::task::spawn_blocking(move || a()).await.is_err() {
                warn!("watchdog: the liveness check panicked; no more pings");
                return;
            }
            n.watchdog();
        }
    })
}

/// SIGHUP reloads in flight: `RELOADING=1` when the first one starts,
/// `READY=1` only when the last one has ended, so two signals close
/// together never say READY while a reload still runs.
#[derive(Debug, Default)]
pub struct Reloads(std::sync::atomic::AtomicUsize);

impl Reloads {
    /// A reload starts: whether to send `RELOADING=1`.
    pub fn begin(&self) -> bool {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0
    }

    /// A reload ended: whether to send `READY=1`.
    pub fn end(&self) -> bool {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_reloads_say_ready_once_the_last_one_ends() {
        let r = Reloads::default();
        assert!(r.begin());
        assert!(!r.begin(), "already reloading");
        assert!(!r.end(), "the other one still runs");
        assert!(r.end());
        assert!(r.begin(), "a new reload later");
        assert!(r.end());
    }

    #[test]
    fn the_watchdog_stops_pinging_while_the_service_is_stuck() {
        use std::sync::{Arc, Mutex};
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("notify");
        let rx = UnixDatagram::bind(&path).unwrap();
        rx.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let n = Arc::new(Notifier::new(path.to_str().unwrap()).unwrap().unwrap());
        // The service's lock, as a vault would hold it forever.
        let lock = Arc::new(Mutex::new(()));
        let l = lock.clone();
        let alive: Arc<dyn Fn() + Send + Sync> = Arc::new(move || drop(l.lock().unwrap()));
        let task = {
            let _in = rt.enter();
            spawn_watchdog(n, Duration::from_millis(20), alive)
        };
        let recv = || {
            let mut buf = [0u8; 64];
            rx.recv(&mut buf).ok().map(|n| buf[..n].to_vec())
        };
        assert_eq!(recv().as_deref(), Some(&b"WATCHDOG=1"[..]));
        let held = lock.lock().unwrap();
        // The pings already sent (or past the lock) arrive; then silence.
        let mut queued = 0;
        while recv().is_some() {
            queued += 1;
            assert!(queued < 5, "still pinging while the lock is held");
        }
        for _ in 0..2 {
            assert_eq!(recv(), None, "no ping while the lock is held");
        }
        drop(held);
        assert_eq!(recv().as_deref(), Some(&b"WATCHDOG=1"[..]));
        task.abort();
    }

    #[test]
    fn messages_are_as_systemd_reads_them() {
        assert_eq!(
            ready_message("2 of 2 vaults\nrunning"),
            "READY=1\nSTATUS=2 of 2 vaults running"
        );
        assert_eq!(
            reloading_message(42),
            "RELOADING=1\nMONOTONIC_USEC=42\nSTATUS=reloading the config"
        );
        let a = monotonic_usec();
        let b = monotonic_usec();
        assert!(a > 0 && b >= a);
    }

    #[test]
    fn the_watchdog_runs_at_half_its_period_for_this_process_only() {
        assert_eq!(
            watchdog_interval_from(Some("60000000"), None, 7),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            watchdog_interval_from(Some("60000000"), Some("7"), 7),
            Some(Duration::from_secs(30))
        );
        assert_eq!(watchdog_interval_from(Some("60000000"), Some("8"), 7), None);
        assert_eq!(watchdog_interval_from(None, None, 7), None);
        assert_eq!(watchdog_interval_from(Some("0"), None, 7), None);
        assert_eq!(watchdog_interval_from(Some("x"), None, 7), None);
    }

    #[test]
    fn messages_reach_a_path_socket() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("notify");
        let rx = UnixDatagram::bind(&path).unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let n = Notifier::new(path.to_str().unwrap()).unwrap().unwrap();
        n.ready("ok");
        n.watchdog();
        n.stopping();
        let mut buf = [0u8; 512];
        let mut got = vec![];
        for _ in 0..3 {
            let len = rx.recv(&mut buf).unwrap();
            got.push(String::from_utf8_lossy(&buf[..len]).into_owned());
        }
        assert_eq!(got, ["READY=1\nSTATUS=ok", "WATCHDOG=1", "STOPPING=1\nSTATUS=stopping"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn messages_reach_an_abstract_socket() {
        use std::os::linux::net::SocketAddrExt;
        let name = format!("lsync-test-notify-{}", std::process::id());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let rx = UnixDatagram::bind_addr(&addr).unwrap();
        rx.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let n = Notifier::new(&format!("@{name}")).unwrap().unwrap();
        n.watchdog();
        let mut buf = [0u8; 64];
        let len = rx.recv(&mut buf).unwrap();
        assert_eq!(&buf[..len], b"WATCHDOG=1");
    }

    #[test]
    fn other_addresses_are_not_spoken() {
        assert!(Notifier::new("vsock:2:1234").unwrap().is_none());
        assert!(Notifier::new("relative").unwrap().is_none());
    }
}
