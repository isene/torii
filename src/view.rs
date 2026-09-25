//! What torii shows when it starts inside a terminal, as the fe2o3
//! launcher starts it: whether the listener runs, the network's way out
//! right now, and the last login page it opened. `q` or `Esc` leaves.

use crate::{conn_name, handle_portal, read_connectivity, CONN_FULL, CONN_PORTAL, NM_IFACE, NM_PATH, NM_SERVICE};
use crust::{seq, style, Crust, Cursor, Input};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};
use zbus::blocking::{Connection, Proxy};

const RUST_RGB: (u8, u8, u8) = (247, 76, 0);
const TEXT_RGB: (u8, u8, u8) = (225, 225, 230);
const DIM_RGB: (u8, u8, u8) = (140, 140, 150);
const LIVE_RGB: (u8, u8, u8) = (120, 230, 140);
const WARN_RGB: (u8, u8, u8) = (230, 180, 90);
const BAR_BG: (u8, u8, u8) = (38, 38, 38);

pub fn run() {
    crate::QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
    let conn = Connection::system().ok();
    Crust::init();
    Crust::set_app_identity("torii");
    let mut note = String::new();
    loop {
        draw(conn.as_ref(), &note);
        note.clear();
        let Some(key) = Input::getchr_ms(600_000) else { continue };
        let Some(c) = conn.as_ref() else {
            if matches!(key.as_str(), "q" | "ESC") { break }
            continue;
        };
        match key.as_str() {
            "q" | "ESC" => break,
            "c" => {
                draw(conn.as_ref(), "asking NetworkManager to check…");
                let got: Option<u32> = Proxy::new(c, NM_SERVICE, NM_PATH, NM_IFACE).ok().and_then(|p| p.call("CheckConnectivity", &()).ok());
                note = match got {
                    Some(v) => format!("checked: the way out is {}", conn_name(v)),
                    None => "the check did not answer".into(),
                };
            }
            "o" => {
                note = match read_connectivity(c) {
                    Some(CONN_PORTAL) => {
                        handle_portal(c);
                        "opening the login page".into()
                    }
                    Some(v) => format!("no login page to open: the way out is {}", conn_name(v)),
                    None => "NetworkManager did not answer".into(),
                };
            }
            _ => {}
        }
    }
    Crust::cleanup();
}

fn draw(conn: Option<&Connection>, note: &str) {
    let (cols, rows) = Crust::terminal_size();
    let w = cols as usize;
    let mut out = String::new();

    // The bar across the top: is the listener running.
    let state = match listener() {
        Some((pid, kb)) => style::rgb(&format!("listening · pid {pid} · {:.1} MB", kb as f64 / 1024.0), Some(LIVE_RGB), Some(BAR_BG), ""),
        None => style::rgb("not listening · start `torii` with your session", Some(WARN_RGB), Some(BAR_BG), ""),
    };
    let head = format!(" torii   {}", crust::strip_ansi(&state));
    out.push_str(&format!(
        "{}{}{}{}{}{}",
        Cursor::at(1, 1),
        style::rgb(" ", None, Some(BAR_BG), ""),
        style::rgb("torii", Some(RUST_RGB), Some(BAR_BG), "b"),
        style::rgb("   ", None, Some(BAR_BG), ""),
        state,
        style::rgb(&" ".repeat(w.saturating_sub(crust::display_width(&head))), None, Some(BAR_BG), "")
    ));

    // The network and its way out now, and the last login page.
    let label = |s: &str| style::rgb(&format!("   {s:<14}"), Some(DIM_RGB), None, "");
    let (network, way) = match conn {
        Some(c) => (primary(c).unwrap_or_else(|| "none".into()), read_connectivity(c)),
        None => ("NetworkManager is not on the system bus".into(), None),
    };
    let way = match way {
        Some(CONN_FULL) => style::rgb("the internet", Some(LIVE_RGB), None, ""),
        Some(CONN_PORTAL) => style::rgb("a login page (o opens it)", Some(WARN_RGB), None, "b"),
        Some(v) => style::rgb(conn_name(v), Some(WARN_RGB), None, ""),
        None => style::rgb("unknown", Some(DIM_RGB), None, ""),
    };
    let lines = [
        format!("{}{}", label("Network"), style::rgb(&network, Some(TEXT_RGB), None, "b")),
        format!("{}{}", label("Way out"), way),
        format!("{}{}", label("Last portal"), style::rgb(&last_portal(), Some(TEXT_RGB), None, "")),
        String::new(),
        style::rgb("   When a network wants you to log in first, torii opens its page in the browser by itself.", Some(DIM_RGB), None, "i"),
    ];
    for r in 0..(rows as usize).saturating_sub(4) {
        let line = lines.get(r).cloned().unwrap_or_default();
        out.push_str(&format!("{}{}{}", Cursor::at(1, 3 + r as u16), line, seq::ERASE_EOL));
    }

    // The bar along the bottom, the version at the far right.
    let foot = if note.is_empty() { "o open the login page · c check now · q back".to_string() } else { note.to_string() };
    let version = format!("v{} ", env!("CARGO_PKG_VERSION"));
    let foot = format!(" {}", foot.chars().take(w.saturating_sub(version.len() + 3)).collect::<String>());
    let pad = w.saturating_sub(crust::display_width(&foot) + version.len());
    out.push_str(&format!(
        "{}{}{}",
        Cursor::at(1, rows),
        style::rgb(&format!("{foot}{}", " ".repeat(pad)), Some((200, 200, 205)), Some(BAR_BG), ""),
        style::rgb(&version, Some(DIM_RGB), Some(BAR_BG), "")
    ));
    print!("{out}");
    std::io::stdout().flush().ok();
}

/// The running listener: its pid and memory in kB, found in /proc.
fn listener() -> Option<(u32, u64)> {
    let me = std::process::id();
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        if pid == me || std::fs::read_to_string(format!("/proc/{pid}/comm")).map(|c| c.trim() != "torii").unwrap_or(true) {
            continue;
        }
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        let kb = status.lines().find_map(|l| l.strip_prefix("VmRSS:")).and_then(|v| v.split_whitespace().next()?.parse().ok());
        return Some((pid, kb.unwrap_or(0)));
    }
    None
}

/// The name of the connection the machine is on.
fn primary(c: &Connection) -> Option<String> {
    let nm = Proxy::new(c, NM_SERVICE, NM_PATH, NM_IFACE).ok()?;
    let p: zbus::zvariant::OwnedObjectPath = nm.get_property("PrimaryConnection").ok()?;
    if p.as_str() == "/" {
        return None;
    }
    let a = Proxy::new(c, NM_SERVICE, p.as_str(), "org.freedesktop.NetworkManager.Connection.Active").ok()?;
    let id: String = a.get_property("Id").ok()?;
    Some(id)
}

/// When the last login page was opened, and for which address, from
/// the capture torii keeps in ~/.torii/last-portal.txt.
fn last_portal() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let Ok(text) = std::fs::read_to_string(format!("{home}/.torii/last-portal.txt")) else { return "none yet".into() };
    let at: u64 = text.split_whitespace().nth(3).and_then(|t| t.parse().ok()).unwrap_or(0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let secs = now.saturating_sub(at);
    let ago = match secs {
        0..=59 => format!("{secs} s"),
        60..=3599 => format!("{} min", secs / 60),
        3600..=86399 => format!("{} h", secs / 3600),
        _ => format!("{} d", secs / 86400),
    };
    let probe = text.lines().find_map(|l| l.strip_prefix("===== ")).unwrap_or("");
    format!("{ago} ago · {probe}")
}
