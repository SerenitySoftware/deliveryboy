//! Ctrl-C while a deploy is executing.
//!
//! Left to the default handler, an interrupt kills `deliver` together with the
//! step it was waiting on, and the undo stack in `exec` is never walked: a
//! symlink already swapped stays swapped. While steps run, this module turns
//! the interrupt into a count instead. The terminal delivers it to the running
//! child too, so the child finishes or dies on its own, and `exec` reads the
//! count after each step and unwinds through the ordinary failure path.
//!
//! The handler is installed only while armed. Everywhere else — prompts,
//! preflight, read-only commands — Ctrl-C keeps its default meaning. No crate:
//! `signal(2)` and `SetConsoleCtrlHandler` are already linked by `std`, and the
//! handler only bumps two atomics, which is async-signal-safe.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Interrupts since the deploy was armed.
static COUNT: AtomicUsize = AtomicUsize::new(0);
/// Whether any armed run in this process was interrupted. Never reset, so a
/// `fleet deploy` stops after the repo the operator interrupted.
static SEEN: AtomicBool = AtomicBool::new(false);

fn record() {
    COUNT.fetch_add(1, Ordering::SeqCst);
    SEEN.store(true, Ordering::SeqCst);
}

/// How many times Ctrl-C has been pressed since `arm`.
pub fn count() -> usize {
    COUNT.load(Ordering::SeqCst)
}

/// Whether an armed run in this process was ever interrupted.
pub fn seen() -> bool {
    SEEN.load(Ordering::SeqCst)
}

/// Sleep for `wait`, returning early (with `false`) once `interrupted` is true.
pub fn nap(wait: Duration, interrupted: impl Fn() -> bool) -> bool {
    let until = Instant::now() + wait;
    loop {
        if interrupted() {
            return false;
        }
        let Some(left) = until.checked_duration_since(Instant::now()) else {
            return true;
        };
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
}

/// Catch Ctrl-C until the returned guard is dropped.
pub fn arm() -> Armed {
    COUNT.store(0, Ordering::SeqCst);
    sys::install();
    Armed(())
}

/// Ctrl-C is counted, not fatal, while this lives.
pub struct Armed(());

impl Drop for Armed {
    fn drop(&mut self) {
        sys::restore();
    }
}

/// Stand-in for the keypress in tests.
#[cfg(all(test, unix))]
pub fn raise() {
    record();
}

#[cfg(unix)]
mod sys {
    const SIGINT: i32 = 2;
    const SIG_DFL: usize = 0;

    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }

    extern "C" fn on_interrupt(_: i32) {
        super::record();
    }

    pub fn install() {
        let handler: extern "C" fn(i32) = on_interrupt;
        // SAFETY: the handler touches only atomics.
        unsafe { signal(SIGINT, handler as usize) };
    }

    pub fn restore() {
        // SAFETY: restoring the default disposition.
        unsafe { signal(SIGINT, SIG_DFL) };
    }
}

#[cfg(windows)]
mod sys {
    const CTRL_C_EVENT: u32 = 0;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }

    unsafe extern "system" fn on_interrupt(event: u32) -> i32 {
        if event == CTRL_C_EVENT {
            super::record();
            1
        } else {
            0
        }
    }

    pub fn install() {
        // SAFETY: registering a handler that touches only atomics.
        unsafe { SetConsoleCtrlHandler(Some(on_interrupt), 1) };
    }

    pub fn restore() {
        // SAFETY: removing the handler `install` added.
        unsafe { SetConsoleCtrlHandler(Some(on_interrupt), 0) };
    }
}

#[cfg(not(any(unix, windows)))]
mod sys {
    pub fn install() {}
    pub fn restore() {}
}
