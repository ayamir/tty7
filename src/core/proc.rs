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
//! app should go through [`hide_console`] before it runs. PTY children are not
//! in scope — the daemon owns those and passes its own flags (see
//! [`crate::daemon::spawn`]).

use std::process::Command;

/// Suppress the console window Windows would otherwise allocate for a console
/// subsystem child. No-op on Unix, so callers stay `cfg`-free.
pub fn hide_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}
