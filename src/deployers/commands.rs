//! `commands` — run declared steps, nothing implied.
//!
//! The escape hatch as a first-class deployer, for work that isn't shaped like
//! any other deployer (ensuring a directory exists with the right owner/modes,
//! restarting a unit, running a one-off migration). It keeps such work *in the
//! config* instead of in a shell script that has to be shipped to the server.
//!
//! ```yaml
//! config:
//!   steps:
//!     - ssh: sudo install -d -m 755 -o www-data:www-data /srv/downloads
//!     - command: ./scripts/local-thing.sh
//! ```

use super::{compile_raw_step, PlanContext, PlannedStep};
use anyhow::{bail, Result};
use serde_yaml::Value;

pub fn compile(cfg: &Value, ctx: &PlanContext) -> Result<Vec<PlannedStep>> {
    let Some(steps) = cfg.get("steps").and_then(|v| v.as_sequence()) else {
        bail!("commands: `steps:` is required (a list of {{ssh: ...}} / {{command: ...}})");
    };
    if steps.is_empty() {
        bail!("commands: `steps:` is empty");
    }
    steps.iter().map(|raw| compile_raw_step(raw, ctx)).collect()
}
