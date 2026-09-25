//! torii — captive-portal listener for NetworkManager.
//!
//! Replaces Firefox's removed "Open network login page" banner. Subscribes
//! to NetworkManager's PropertiesChanged signal on the system bus, watches
//! the Connectivity property, and on a `* → portal` transition opens
//! the portal's login page in gaze. On `portal → full` it sends a
//! "Connected" notification.
//!
//! Signal-driven only — no polling. Idle cost is one process parked in
//! epoll_wait on the D-Bus socket. CPU when idle = 0.

mod view;

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::collections::HashMap;

use zbus::blocking::{Connection, Proxy, MessageIterator};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::message::Type as MessageType;
use zbus::MatchRule;

const NM_SERVICE: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_IFACE: &str = "org.freedesktop.NetworkManager";
const NM_AC_IFACE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const NM_IP4_IFACE: &str = "org.freedesktop.NetworkManager.IP4Config";
const DBUS_PROPS: &str = "org.freedesktop.DBus.Properties";

const NOTIFY_SERVICE: &str = "org.freedesktop.Notifications";
const NOTIFY_PATH: &str = "/org/freedesktop/Notifications";
const NOTIFY_IFACE: &str = "org.freedesktop.Notifications";

// NetworkManager's State: 50 and up means connected (local, site, global).
const NM_STATE_CONNECTED: u32 = 50;

// Connectivity values from NetworkManager.
const CONN_NONE: u32 = 1;
const CONN_PORTAL: u32 = 2;
const CONN_LIMITED: u32 = 3;
const CONN_FULL: u32 = 4;

fn conn_name(v: u32) -> &'static str {
    match v {
        0 => "unknown",
        1 => "none",
        2 => "portal",
        3 => "limited",
        4 => "full",
        _ => "?",
    }
}

/// Set by the terminal view, where a log line would write over the screen.
static QUIET: AtomicBool = AtomicBool::new(false);

fn log(msg: &str) {
    if !QUIET.load(Ordering::Relaxed) {
        eprintln!("[torii] {}", msg);
    }
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    if arg == "-v" || arg == "--version" {
        println!("torii {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if arg == "-h" || arg == "--help" {
        println!("torii — opens a network's login page when NetworkManager finds one");
        println!();
        println!("  torii            in a terminal: the listener's state, the way out, the last login page");
        println!("                   without one: the listener");
        println!("  torii --daemon   the listener, even from a terminal");
        println!("  torii --once     check once, open the login page if there is one, and exit");
        return;
    }
    // Inside a terminal (the fe2o3 launcher, a shell) torii shows what it
    // knows and lets go on q; without one it is the listener.
    use std::io::IsTerminal;
    if arg.is_empty() && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        view::run();
        return;
    }
    let once = arg == "--once";

    if once {
        match Connection::system() {
            Ok(conn) => {
                let v = read_connectivity(&conn).unwrap_or(0);
                log(&format!("connectivity = {}", conn_name(v)));
                if v == CONN_PORTAL {
                    handle_portal(&conn);
                }
            }
            Err(e) => log(&format!("dbus connect failed: {}", e)),
        }
        return;
    }

    // Reconnect loop with capped exponential backoff. NM may be late on boot.
    let mut delay_secs: u64 = 5;
    loop {
        match Connection::system() {
            Ok(conn) => {
                delay_secs = 5;
                run_loop(&conn);
                // run_loop returns only on signal-stream end / fatal stream
                // error. Reconnect.
                log("d-bus stream closed, reconnecting…");
            }
            Err(e) => {
                log(&format!("dbus connect failed: {} — retrying in {}s", e, delay_secs));
            }
        }
        std::thread::sleep(Duration::from_secs(delay_secs));
        delay_secs = (delay_secs * 2).min(60);
    }
}

fn run_loop(conn: &Connection) {
    log("connected to NetworkManager");
    let initial = read_connectivity(conn).unwrap_or(0);
    log(&format!("connectivity = {}", conn_name(initial)));

    // Subscribe to PropertiesChanged on the NM root object.
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(NM_SERVICE).unwrap()
        .interface(DBUS_PROPS).unwrap()
        .member("PropertiesChanged").unwrap()
        .path(NM_PATH).unwrap()
        .build();

    let iter = match MessageIterator::for_match_rule(rule, conn, None) {
        Ok(it) => it,
        Err(e) => { log(&format!("subscribe failed: {}", e)); return; }
    };

    let mut prev = initial;
    let mut prev_state = read_state(conn).unwrap_or(0);
    for msg in iter {
        let msg = match msg {
            Ok(m) => m,
            Err(_) => continue,
        };
        // Body: (iface: String, changed: a{sv}, invalidated: as)
        let body = msg.body();
        let parsed: Result<(String, HashMap<String, OwnedValue>, Vec<String>), _> =
            body.deserialize();
        let (iface, changed, _) = match parsed {
            Ok(t) => t,
            Err(_) => continue,
        };
        if iface != NM_IFACE { continue; }
        // Just connected: have NetworkManager look for a portal now,
        // not at its next check up to five minutes away.
        if let Some(state) = changed.get("State").and_then(|v| u32::try_from(v).ok()) {
            if state >= NM_STATE_CONNECTED && prev_state < NM_STATE_CONNECTED {
                check_soon(conn.clone());
            }
            prev_state = state;
        }
        let new_v = match changed.get("Connectivity") {
            Some(v) => match u32::try_from(v) {
                Ok(n) => n,
                Err(_) => continue,
            },
            None => continue,
        };
        if new_v == prev { continue; }
        log(&format!("connectivity: {} → {}", conn_name(prev), conn_name(new_v)));
        match (prev, new_v) {
            (_, CONN_PORTAL) => handle_portal(conn),
            (CONN_PORTAL, CONN_FULL) | (CONN_PORTAL, CONN_LIMITED) | (CONN_PORTAL, CONN_NONE) => {
                if new_v == CONN_FULL { handle_cleared(conn); }
            }
            _ => {}
        }
        prev = new_v;
    }
}

/// Ask NetworkManager for a connectivity check 3 s after connecting,
/// and once more 12 s later if the network was still settling. A
/// portal it finds arrives as the usual Connectivity change. Runs on
/// its own short-lived thread, only when a connection comes up.
fn check_soon(conn: Connection) {
    std::thread::spawn(move || {
        for wait in [3, 12] {
            std::thread::sleep(Duration::from_secs(wait));
            let Ok(nm) = Proxy::new(&conn, NM_SERVICE, NM_PATH, NM_IFACE) else { return };
            let got: Result<u32, _> = nm.call("CheckConnectivity", &());
            match got {
                Ok(v) => {
                    log(&format!("check after connect: {}", conn_name(v)));
                    if v == CONN_PORTAL || v == CONN_FULL { return; }
                }
                Err(e) => { log(&format!("check failed: {}", e)); return; }
            }
        }
    });
}

fn read_state(conn: &Connection) -> Option<u32> {
    let proxy = Proxy::new(conn, NM_SERVICE, NM_PATH, DBUS_PROPS).ok()?;
    let v: OwnedValue = proxy.call("Get", &(NM_IFACE, "State")).ok()?;
    u32::try_from(&v).ok()
}

fn handle_portal(conn: &Connection) {
    // The network may have gone in the meantime (a portal reported just
    // as the Wi-Fi went off); a page opened then only says unreachable.
    if read_state(conn).unwrap_or(0) < NM_STATE_CONNECTED {
        log("portal reported, but no longer connected; not opening");
        return;
    }
    notify(conn, "Captive portal", "Opening login page…", 2);
    // The login page is where the portal sends the connectivity check:
    // it answers that plain-HTTP request with a redirect to its own page
    // (a UniFi guest portal on 192.168.1.1:8880, say), which need not be
    // the gateway at all. No redirect: open the check address itself and
    // let the portal take the browser there. The gateway is the last
    // resort, for when even the check cannot be sent.
    let probe = check_uri(conn);
    let raw = split_http(&probe).map(|(h, port, path)| http_get(&h, port, &path));
    save_capture(&probe, raw.as_deref());
    let url = match raw.as_deref().map(|r| (r, redirect_target(r, &probe))) {
        Some((_, Some(to))) => to,
        Some((r, None)) if r.starts_with("HTTP/") => probe,
        _ => match gateway_ip(conn) {
            Some(gw) => format!("http://{}/", gw),
            None => probe,
        },
    };
    // gaze is the browser here, and a portal page opens as a tab in the
    // window that is already up. Firefox stands behind it, for a machine
    // that has no gaze.
    for (browser, args) in [("gaze", &[][..]), ("firefox", &["--new-tab"][..])] {
        log(&format!("opening {}: {}", browser, url));
        let res = Command::new(browser)
            .args(args)
            .arg(&url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match res {
            // gaze hands the page to the window that is up and ends at
            // once. Wait for it on a short thread, or every login page
            // leaves a dead process behind for as long as torii runs.
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
                return;
            }
            Err(e) => log(&format!("{} would not start: {}", browser, e)),
        }
    }
}

/// NetworkManager's connectivity check address, the one a portal
/// intercepts.
fn check_uri(conn: &Connection) -> String {
    Proxy::new(conn, NM_SERVICE, NM_PATH, DBUS_PROPS).ok()
        .and_then(|p| p.call::<_, _, OwnedValue>("Get", &(NM_IFACE, "ConnectivityCheckUri")).ok())
        .and_then(|v| String::try_from(v).ok())
        .filter(|u| u.starts_with("http://"))
        .unwrap_or_else(|| "http://connectivity-check.ubuntu.com/".into())
}

/// `http://host[:port]/path` as (host, port, path); None for anything
/// else, since only plain HTTP gets redirected by a portal.
fn split_http(uri: &str) -> Option<(String, u16, String)> {
    let rest = uri.strip_prefix("http://")?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (hostport, 80),
    };
    Some((host.to_string(), port, path.to_string()))
}

/// One plain-HTTP GET the way a browser sends it: the raw answer, at
/// most 8 KB, or a line saying why there is none.
fn http_get(host: &str, port: u16, path: &str) -> String {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};
    // IPv4 first: a portal network rarely routes IPv6.
    let addrs: Vec<_> = (host, port).to_socket_addrs().map(|a| a.collect()).unwrap_or_default();
    let addr = match addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).copied() {
        Some(a) => a,
        None => return format!("cannot resolve {host}\n"),
    };
    let mut s = match TcpStream::connect_timeout(&addr, Duration::from_secs(4)) {
        Ok(s) => s,
        Err(e) => return format!("cannot connect to {host}: {e}\n"),
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(4)));
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: Mozilla/5.0 (X11; Linux x86_64) torii\r\n\
         Accept: text/html,*/*\r\nConnection: close\r\n\r\n");
    if let Err(e) = s.write_all(req.as_bytes()) {
        return format!("cannot send to {host}: {e}\n");
    }
    let mut buf = Vec::new();
    let _ = s.take(8192).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// The Location of a 3xx answer, made absolute against `base`.
fn redirect_target(raw: &str, base: &str) -> Option<String> {
    let status = raw.lines().next()?.split_whitespace().nth(1)?;
    if !status.starts_with('3') {
        return None;
    }
    let loc = raw.lines()
        .take_while(|l| !l.trim().is_empty())
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.trim().eq_ignore_ascii_case("location")).map(|(_, v)| v.trim().to_string()))?;
    if loc.starts_with('/') {
        let (h, port, _) = split_http(base)?;
        let hp = if port == 80 { h } else { format!("{h}:{port}") };
        return Some(format!("http://{hp}{loc}"));
    }
    Some(loc)
}

/// The portal's answer to the check, kept in ~/.torii/last-portal.txt
/// for when a login page still fails to show.
fn save_capture(probe: &str, raw: Option<&str>) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let out = format!("portal seen at {now} (unix time)\n\n===== {probe}\n{}", raw.unwrap_or("not a plain-HTTP address\n"));
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let dir = std::path::Path::new(&home).join(".torii");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("last-portal.txt"), out);
}

fn handle_cleared(conn: &Connection) {
    notify(conn, "Connected", "Internet reachable", 0);
}

/// Read the NM Connectivity property.
fn read_connectivity(conn: &Connection) -> Option<u32> {
    let proxy = Proxy::new(conn, NM_SERVICE, NM_PATH, DBUS_PROPS).ok()?;
    let v: OwnedValue = proxy.call("Get", &(NM_IFACE, "Connectivity")).ok()?;
    u32::try_from(&v).ok()
}

/// Resolve gateway IP for the primary active connection.
fn gateway_ip(conn: &Connection) -> Option<String> {
    let nm = Proxy::new(conn, NM_SERVICE, NM_PATH, DBUS_PROPS).ok()?;
    let primary: OwnedValue = nm.call("Get", &(NM_IFACE, "PrimaryConnection")).ok()?;
    let primary_path: OwnedObjectPath = primary.try_into().ok()?;
    let p_str = primary_path.as_str();
    if p_str == "/" || p_str.is_empty() { return None; }

    let ac = Proxy::new(conn, NM_SERVICE, p_str, DBUS_PROPS).ok()?;
    let ip4: OwnedValue = ac.call("Get", &(NM_AC_IFACE, "Ip4Config")).ok()?;
    let ip4_path: OwnedObjectPath = ip4.try_into().ok()?;
    let ip4_str = ip4_path.as_str();
    if ip4_str == "/" || ip4_str.is_empty() { return None; }

    let ip = Proxy::new(conn, NM_SERVICE, ip4_str, DBUS_PROPS).ok()?;
    let gw: OwnedValue = ip.call("Get", &(NM_IP4_IFACE, "Gateway")).ok()?;
    let s: String = String::try_from(gw).ok()?;
    if s.is_empty() { None } else { Some(s) }
}

/// Best-effort notification via org.freedesktop.Notifications (dunst).
/// Lives on the SESSION bus, not the system bus we use for NM.
/// urgency: 0 = low, 1 = normal, 2 = critical.
fn notify(_system_conn: &Connection, summary: &str, body: &str, urgency: u8) {
    let session = match Connection::session() {
        Ok(c) => c,
        Err(_) => return,
    };
    let proxy = match Proxy::new(&session, NOTIFY_SERVICE, NOTIFY_PATH, NOTIFY_IFACE) {
        Ok(p) => p,
        Err(_) => return,
    };
    let mut hints: HashMap<&str, Value> = HashMap::new();
    hints.insert("urgency", Value::U8(urgency));
    // Critical urgency = persist until dismissed (timeout 0).
    let timeout: i32 = if urgency == 2 { 0 } else { 4000 };
    let actions: Vec<&str> = Vec::new();
    let _: Result<u32, _> = proxy.call(
        "Notify",
        &("torii", 0u32, "", summary, body, actions, hints, timeout),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_portal_redirect_is_followed() {
        let raw = "HTTP/1.1 302 Moved Temporarily\r\nConnection: close\r\nLocation: http://192.168.1.1:8880/guest/s/default/?ap=x\r\n\r\n";
        assert_eq!(redirect_target(raw, "http://connectivity-check.ubuntu.com/").as_deref(), Some("http://192.168.1.1:8880/guest/s/default/?ap=x"));
        let rel = "HTTP/1.1 302 Found\r\nlocation: /login\r\n\r\n";
        assert_eq!(redirect_target(rel, "http://10.0.0.1:8080/").as_deref(), Some("http://10.0.0.1:8080/login"));
        assert_eq!(redirect_target("HTTP/1.1 204 No Content\r\n\r\n", "http://x/"), None);
    }

    #[test]
    fn plain_http_addresses_split() {
        assert_eq!(split_http("http://connectivity-check.ubuntu.com/"), Some(("connectivity-check.ubuntu.com".into(), 80, "/".into())));
        assert_eq!(split_http("http://1.2.3.4:8880/a?b"), Some(("1.2.3.4".into(), 8880, "/a?b".into())));
        assert_eq!(split_http("https://x/"), None);
    }
}
