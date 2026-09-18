//! Console output: a banner, then one announced phase per stage of the run.
//!
//! Phases mirror docs/deploy-lifecycle.md, so what you see maps onto the
//! documented process: load → compile → preflight → execute → verify → done.
//!
//! Everything here writes to **stderr** except step/plan content, so `--json`
//! output on stdout stays machine-readable.
//!
//! Every message goes through [`crate::secrets::redact::scrub`] on the way out:
//! this is the console boundary, so a resolved secret that reached a message by
//! any route stops here rather than at each caller's discretion.

use crate::secrets::redact::scrub;
use std::time::Instant;

/// Once per process, not once per call: a `fleet` run drives the same per-repo
/// commands in a loop, and repeating the version line between repos would push
/// the thing the operator is reading further apart for no information.
pub fn banner() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        eprintln!("Delivery Boy CLI v{}", env!("CARGO_PKG_VERSION"));
    });
}

/// Announce what happens next.
pub fn phase(name: &str) {
    eprintln!("\n▸ {}", scrub(name));
}

/// A fact under the current phase.
pub fn detail(msg: impl AsRef<str>) {
    eprintln!("    {}", scrub(msg.as_ref()));
}

pub fn ok(msg: impl AsRef<str>) {
    eprintln!("    ✓ {}", scrub(msg.as_ref()));
}

pub fn fail(msg: impl AsRef<str>) {
    eprintln!("    ✗ {}", scrub(msg.as_ref()));
}

pub fn note(msg: impl AsRef<str>) {
    eprintln!("  {}", scrub(msg.as_ref()));
}

/// Wall-clock for the whole run, reported at the end.
pub struct Timer(Instant);

impl Timer {
    pub fn start() -> Self {
        Self(Instant::now())
    }

    pub fn elapsed(&self) -> String {
        let secs = self.0.elapsed().as_secs_f64();
        if secs < 60.0 {
            format!("{secs:.1}s")
        } else {
            format!("{}m{:02}s", (secs / 60.0) as u64, (secs % 60.0) as u64)
        }
    }
}
