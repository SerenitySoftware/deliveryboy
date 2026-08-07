//! Verification checks compiled to steps. A failed check fails the deploy.

use crate::deployers::{PlanContext, PlannedStep, StepKind};
use anyhow::{bail, Result};
use serde_yaml::Value;

pub fn compile(check: &Value, ctx: &PlanContext) -> Result<PlannedStep> {
    if let Some(http) = check.get("http") {
        let url = http
            .get("url")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("verify http: 'url' is required"))?;
        return Ok(PlannedStep {
            label: format!("verify http {url}"),
            kind: StepKind::Http {
                url,
                expect_status: http
                    .get("expect_status")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(200) as u16,
                retries: http.get("retries").and_then(|v| v.as_u64()).unwrap_or(1) as u32,
                interval: http.get("interval").and_then(|v| v.as_u64()).unwrap_or(5),
            },
            rollback: None,
            cleanup: false,
            secret: false,
        });
    }
    if let Some(cmd) = check.get("remote_command").and_then(|v| v.as_str()) {
        return Ok(PlannedStep::ssh(format!("verify: {cmd}"), cmd));
    }
    if let Some(file) = check.get("remote_file").and_then(|v| v.as_str()) {
        let dest = format!("{}/{}", ctx.target.dir.trim_end_matches('/'), file);
        return Ok(PlannedStep::ssh(
            format!("verify file {file}"),
            format!("test -s '{dest}'"),
        ));
    }
    if let Some(c) = check.get("contains") {
        let file = c
            .get("remote_file")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("verify contains: 'remote_file' is required"))?;
        let text = c
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("verify contains: 'text' is required"))?;
        let dest = format!("{}/{}", ctx.target.dir.trim_end_matches('/'), file);
        return Ok(PlannedStep::ssh(
            format!("verify {file} contains {text:?}"),
            format!("grep -qF '{text}' '{dest}'"),
        ));
    }
    bail!("unknown verify check: {check:?}")
}
