//! What a pane is *running*: the process tree under its shell, and the TCP ports
//! that tree is listening on. Feeds the GUI's details panel (`QueryProcs`).
//!
//! Everything here is best-effort and read-only. A pid can exit between the
//! table walk and the name lookup, `lsof` may be missing, `/proc` may be
//! unreadable — each of those degrades to a shorter list, never an error. The
//! panel showing one fewer row is a non-event; a details query that can fail is
//! a support burden.
//!
//! Called on demand from the details panel, not on a timer — see the doc on
//! [`ClientMsg::QueryProcs`](crate::daemon::protocol::ClientMsg::QueryProcs) for
//! why this is pull-based when `Cwd` and `Agent` are pushed.

use std::collections::HashMap;

use crate::daemon::protocol::{PaneProcs, PortEntry, ProcEntry};

/// Depth cap on the process walk. Deep trees are real (a shell running `make`
/// running a compiler driver running the compiler), but past a handful of hops
/// the rows stop being information and start being noise in a 260px column.
const MAX_DEPTH: u8 = 6;

/// Hard cap on rows, so a pane that spawned a thousand workers can't turn a
/// details query into a wire-format stress test.
const MAX_PROCS: usize = 64;

/// The process tree under `shell_pid` plus its listening ports. `fg_pgid` is the
/// PTY's foreground process group, used to mark the row the user is looking at;
/// pass `None` when it isn't known.
pub fn snapshot(shell_pid: u32, fg_pgid: Option<i32>) -> PaneProcs {
    let table = process_table();
    let procs = walk(&table, shell_pid, fg_pgid);
    let ports = listening_ports(&procs);
    PaneProcs { procs, ports }
}

/// One row of the system process table, reduced to what the walk needs.
struct Row {
    ppid: u32,
    pgid: u32,
    name: String,
}

/// Depth-first from the shell, so the caller can render in order and indent by
/// `depth` without rebuilding a hierarchy.
fn walk(table: &HashMap<u32, Row>, shell_pid: u32, fg_pgid: Option<i32>) -> Vec<ProcEntry> {
    // Children by parent, so the descent is a lookup rather than a table scan
    // per node. Sorted by pid: the process table's own order is unspecified, and
    // a list that reshuffles between two refreshes reads as churn.
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, row) in table {
        children.entry(row.ppid).or_default().push(*pid);
    }
    for kids in children.values_mut() {
        kids.sort_unstable();
    }

    let mut out = Vec::new();
    let mut stack = vec![(shell_pid, 0u8)];
    while let Some((pid, depth)) = stack.pop() {
        let Some(row) = table.get(&pid) else { continue };
        if out.len() >= MAX_PROCS {
            break;
        }
        out.push(ProcEntry {
            pid,
            name: row.name.clone(),
            depth,
            foreground: fg_pgid.is_some_and(|g| g as u32 == row.pgid),
        });
        if depth + 1 > MAX_DEPTH {
            continue;
        }
        if let Some(kids) = children.get(&pid) {
            // Pushed in reverse so the pop order stays ascending by pid.
            for kid in kids.iter().rev() {
                stack.push((*kid, depth + 1));
            }
        }
    }
    out
}

// ── Platform: the process table ─────────────────────────────────────────────

/// macOS: one `proc_listallpids` sweep, then `PROC_PIDTBSDINFO` per pid for
/// parent/group. Cheaper than shelling out to `ps`, and it can't be defeated by
/// a user's `ps` alias or a locale-dependent column layout.
#[cfg(target_os = "macos")]
fn process_table() -> HashMap<u32, Row> {
    let mut table = HashMap::new();
    // Ask for the count first, then read into a buffer sized from it (plus slack,
    // since processes can appear between the two calls).
    // SAFETY: the documented "how big a buffer do I need" form — null buffer,
    // zero size — which only returns a byte count.
    let bytes = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return table;
    }
    let cap = (bytes as usize / std::mem::size_of::<libc::c_int>()) + 64;
    let mut pids = vec![0 as libc::c_int; cap];
    // SAFETY: buffer and its true byte length; the call writes at most that many
    // bytes and returns how many it wrote.
    let written = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr() as *mut libc::c_void,
            (cap * std::mem::size_of::<libc::c_int>()) as libc::c_int,
        )
    };
    if written <= 0 {
        return table;
    }
    let n = written as usize / std::mem::size_of::<libc::c_int>();
    for &pid in pids.iter().take(n.min(cap)) {
        if pid <= 0 {
            continue;
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: zeroed buffer of the expected type, real size passed; the
        // result is read back only when the kernel filled exactly that many
        // bytes (a short return means the pid died mid-walk).
        let ret = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        if ret != size {
            continue;
        }
        // `pbi_comm` is the kernel's truncated name (16 bytes). Prefer the full
        // executable basename, which is what the user typed.
        let name = proc_name(pid).unwrap_or_else(|| cstr_field(&info.pbi_comm));
        table.insert(
            pid as u32,
            Row {
                ppid: info.pbi_ppid,
                pgid: info.pbi_pgid,
                name,
            },
        );
    }
    table
}

/// Read a fixed-size, NUL-padded C char array into a `String`.
#[cfg(target_os = "macos")]
fn cstr_field(buf: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Linux: `/proc/<pid>/stat` carries ppid and pgid in fixed positions. The
/// comm field is parenthesized and may itself contain spaces and parens, so the
/// fields after it are located from the *last* `)`, not by splitting the line.
#[cfg(target_os = "linux")]
fn process_table() -> HashMap<u32, Row> {
    let mut table = HashMap::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return table;
    };
    for entry in dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let mut fields = stat[close + 1..].split_whitespace();
        // After `)`: state, ppid, pgrp, …
        let (Some(_state), Some(ppid), Some(pgid)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(ppid), Ok(pgid)) = (ppid.parse::<u32>(), pgid.parse::<u32>()) else {
            continue;
        };
        let name = proc_name(pid as i32).unwrap_or_else(|| {
            // Fall back to the parenthesized comm already in hand.
            stat[..close]
                .rfind('(')
                .map_or_else(|| String::new(), |open| stat[open + 1..close].to_string())
        });
        table.insert(pid, Row { ppid, pgid, name });
    }
    table
}

/// Windows: reuse the existing toolhelp snapshot. It carries no process-group
/// concept, so nothing is ever marked foreground — matching how `foreground_title`
/// already treats the platform.
#[cfg(windows)]
fn process_table() -> HashMap<u32, Row> {
    crate::daemon::winproc::snapshot()
        .into_iter()
        .map(|p| {
            (
                p.pid,
                Row {
                    // `winproc::Proc` names the parent link `parent`.
                    ppid: p.parent,
                    pgid: 0,
                    name: p.name,
                },
            )
        })
        .collect()
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn process_table() -> HashMap<u32, Row> {
    HashMap::new()
}

/// Executable basename of `pid` (macOS).
#[cfg(target_os = "macos")]
fn proc_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: valid, correctly-sized buffer; `proc_pidpath` writes at most
    // `buf.len()` bytes and returns the count (<=0 on failure).
    let ret =
        unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if ret <= 0 {
        return None;
    }
    let path = std::str::from_utf8(&buf[..ret as usize]).ok()?;
    Some(path.rsplit('/').next().unwrap_or(path).to_string())
}

/// Executable basename of `pid` via `/proc/<pid>/exe` (Linux). Unreadable for
/// processes we don't own, hence the caller's `comm` fallback.
#[cfg(target_os = "linux")]
fn proc_name(pid: i32) -> Option<String> {
    let path = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    let name = path.file_name()?.to_str()?;
    let name = name.strip_suffix(" (deleted)").unwrap_or(name);
    (!name.is_empty()).then(|| name.to_string())
}

// ── Platform: listening ports ───────────────────────────────────────────────

/// TCP listeners owned by any pid in `procs`, via `lsof`.
///
/// Shelling out rather than reading the socket tables directly: on macOS the
/// only supported route is a private `libproc` fd walk, and on Linux matching
/// `/proc/net/tcp` inodes against every pid's fds costs more syscalls than the
/// subprocess. `lsof` ships with macOS; where it's missing this returns empty,
/// which just hides the row.
#[cfg(unix)]
fn listening_ports(procs: &[ProcEntry]) -> Vec<PortEntry> {
    use std::process::{Command, Stdio};

    if procs.is_empty() {
        return Vec::new();
    }
    let pid_list = procs
        .iter()
        .map(|p| p.pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    // `-Fpn`: machine-readable output, pid (`p…`) and name (`n…`) fields only,
    // one per line. `-nP` skips DNS and /etc/services lookups — both can block.
    let out = Command::new("lsof")
        .args([
            "-nP",
            "-iTCP",
            "-sTCP:LISTEN",
            "-a",
            "-p",
            &pid_list,
            "-Fpn",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else { return Vec::new() };
    let text = String::from_utf8_lossy(&out.stdout);

    let by_pid: HashMap<u32, &str> = procs.iter().map(|p| (p.pid, p.name.as_str())).collect();
    let mut ports: Vec<PortEntry> = Vec::new();
    let mut current = 0u32;
    for line in text.lines() {
        let Some((tag, rest)) = line.split_at_checked(1) else {
            continue;
        };
        match tag {
            "p" => current = rest.parse().unwrap_or(0),
            "n" => {
                let Some(port) = parse_listen_port(rest) else {
                    continue;
                };
                // One listener commonly binds both v4 and v6, or several
                // addresses on the same port; the panel wants the port once.
                if ports.iter().any(|e| e.port == port && e.pid == current) {
                    continue;
                }
                ports.push(PortEntry {
                    port,
                    pid: current,
                    name: by_pid
                        .get(&current)
                        .copied()
                        .unwrap_or_default()
                        .to_string(),
                });
            }
            _ => {}
        }
    }
    ports.sort_by_key(|e| (e.port, e.pid));
    ports
}

#[cfg(not(unix))]
fn listening_ports(_procs: &[ProcEntry]) -> Vec<PortEntry> {
    Vec::new()
}

/// The port out of an `lsof -Fn` name field: `*:3000`, `127.0.0.1:8080`,
/// `[::1]:5173`, sometimes with a trailing ` (LISTEN)` despite `-F`.
fn parse_listen_port(name: &str) -> Option<u16> {
    let name = name.split_whitespace().next()?;
    // Split on the *last* colon: an IPv6 literal is full of them.
    let (_, port) = name.rsplit_once(':')?;
    port.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ppid: u32, name: &str) -> Row {
        Row {
            ppid,
            pgid: 0,
            name: name.to_string(),
        }
    }

    #[test]
    fn walk_is_depth_first_from_the_shell() {
        let table: HashMap<u32, Row> = [
            (100, row(1, "zsh")),
            (200, row(100, "make")),
            (300, row(200, "cc")),
            (400, row(100, "vim")),
            // A sibling process outside the shell's tree must not appear.
            (500, row(1, "Finder")),
        ]
        .into_iter()
        .collect();

        let got = walk(&table, 100, None);
        let names: Vec<_> = got.iter().map(|p| (p.name.as_str(), p.depth)).collect();
        assert_eq!(
            names,
            vec![("zsh", 0), ("make", 1), ("cc", 2), ("vim", 1)],
            "depth-first, ascending pid, shell's tree only"
        );
    }

    #[test]
    fn walk_marks_the_foreground_process_group() {
        let mut table: HashMap<u32, Row> = [(100, row(1, "zsh")), (200, row(100, "vim"))]
            .into_iter()
            .collect();
        table.get_mut(&100).unwrap().pgid = 100;
        table.get_mut(&200).unwrap().pgid = 200;

        let got = walk(&table, 100, Some(200));
        assert!(
            !got[0].foreground,
            "the shell is backgrounded while vim runs"
        );
        assert!(got[1].foreground, "vim's group owns the terminal");
    }

    #[test]
    fn walk_survives_a_cycle_in_the_table() {
        // Two processes claiming each other as parent — impossible on a live
        // kernel, but the table is a non-atomic sweep of pids that can be reused
        // mid-walk, so the descent must terminate regardless.
        let table: HashMap<u32, Row> = [(100, row(200, "a")), (200, row(100, "b"))]
            .into_iter()
            .collect();
        let got = walk(&table, 100, None);
        assert!(got.len() <= MAX_PROCS, "bounded, not infinite");
    }

    #[test]
    fn parses_lsof_listen_addresses() {
        assert_eq!(parse_listen_port("*:3000"), Some(3000));
        assert_eq!(parse_listen_port("127.0.0.1:8080"), Some(8080));
        assert_eq!(parse_listen_port("[::1]:5173"), Some(5173));
        assert_eq!(parse_listen_port("*:5432 (LISTEN)"), Some(5432));
        assert_eq!(parse_listen_port("/tmp/some.sock"), None);
    }
}
