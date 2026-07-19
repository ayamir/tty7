//! One place for the Windows subprocess flag every helper shell-out needs.
//!
//! tty7 is a GUI process with no console of its own, so launching a console
//! subsystem program (`git.exe`, `wsl.exe`, …) makes Windows allocate a fresh
//! console for it — a black window that pops up and vanishes. That is invisible
//! on a one-off invocation and very visible on the git-status probe, which runs
//! four `git` calls every time a pane's cwd changes or a command ends.
//!
//! `CREATE_NO_WINDOW` suppresses the console entirely; stdout/stderr pipes are
//! unaffected, so output capture keeps working. Every non-PTY `Command` in the
//! app should go through [`hide_console`] (or [`hide_console_tokio`] for the
//! async flavor) before it runs. PTY children are not in scope — the daemon
//! owns those and passes its own flags (see [`crate::daemon::spawn`]).

use std::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Suppress the console window Windows would otherwise allocate for a console
/// subsystem child. No-op on Unix, so callers stay `cfg`-free.
pub fn hide_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// [`hide_console`] for `tokio::process::Command`. Separate because tokio's
/// builder is a distinct type with its own `creation_flags`, not a `Deref` to
/// the std one.
pub fn hide_console_tokio(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}
