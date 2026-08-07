//! `nginx_vhost` — install/refresh one or many vhosts, provisioning TLS as needed.
//!
//! Config shape (single vhost):
//! ```yaml
//! config:
//!   conf: nginx/site.conf          # ssl: true and provider: certbot by default
//! ```
//!
//! Multiple vhosts, each with its own domains:
//! ```yaml
//! config:
//!   provider: certbot              # default; `none` disables cert provisioning
//!   certbot: {webroot: /var/universal/letsencrypt, email: ops@example.com}
//!   vhosts:
//!     - conf: nginx/site.conf
//!     - conf: nginx/api.conf
//!       domains: [api.example.com] # override what the conf declares
//!       cert_name: api.example.com
//!     - conf: nginx/internal.conf
//!       ssl: false                 # opt this one out of certs
//! ```
//!
//! `managed` (the default strategy) owns the whole problem, so a repo needs no
//! bootstrap script:
//!
//!   1. Parse each vhost *per `server { }` block*, pairing every referenced
//!      certificate with the domains that actually use it (a conf can reference
//!      several; conflating them would check the wrong path and re-issue the
//!      wrong cert).
//!   2. For each cert missing on the target, break certbot's chicken-and-egg —
//!      an HTTPS vhost can't pass `nginx -t` without a cert, and certbot can't
//!      issue one without nginx serving the HTTP-01 challenge. So: temporary
//!      HTTP-only ACME vhost → reload → issue → remove. Skipped once the cert
//!      exists, so it's a no-op on later deploys. Certs shared by several vhosts
//!      are provisioned once.
//!   3. Install each vhost *staged*: back up, copy, symlink, `nginx -t`, reload —
//!      **restoring the backup if either fails**, so a bad config can't leave
//!      nginx broken (it's a shared host).
//!
//! `script` remains for repos that genuinely want to own the lifecycle.
//!
//! Vhosts are reinstalled on *every* deploy on purpose: otherwise edits to
//! `*.conf` silently never reach the server.
//!
//! A vhost that needs a value it can't hold in git — a CSP origin, a shared
//! header token — gets it at install time:
//!
//! ```yaml
//! config:
//!   conf: nginx/site.conf
//!   render:
//!     __SENTRY_CONNECT_ORIGIN__: "{origin:SENTRY_DSN}"
//!   snippets:
//!     - path: /etc/nginx/snippets/admin-access.conf
//!       mode: "0600"
//!       content: 'proxy_set_header X-Admin-Access "{secret:ADMIN_TOKEN}";'
//! ```

use super::{cfg_bool, cfg_str, PlanContext, PlannedStep};
use anyhow::{bail, Context, Result};
use serde_yaml::Value;
use std::path::Path;

/// The scheme-host-port of a URL-shaped secret, and nothing else.
///
/// A Sentry DSN carries its public key in the userinfo. A CSP `connect-src` only
/// needs the origin, and every visitor can read that header — so publishing the
/// whole DSN there would leak the key to anyone who views source.
fn origin_of(url: &str, name: &str) -> Result<String> {
    if url.is_empty() {
        return Ok(String::new());
    }
    if url.chars().any(|c| (c as u32) < 32 || c as u32 == 127) {
        bail!("nginx-vhost: {name} contains a control character");
    }
    let (scheme, rest) = url
        .split_once("://")
        .with_context(|| format!("nginx-vhost: {name} is not a URL, so it has no origin"))?;
    if scheme != "https" {
        bail!("nginx-vhost: {name} must be https to be used as an origin (got '{scheme}')");
    }
    // Drop userinfo (the part an origin must never carry), then path/query.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h, Some(p)),
        _ => (hostport, None),
    };
    if host.is_empty()
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        bail!("nginx-vhost: {name} has an invalid hostname");
    }
    Ok(match port {
        Some(p) => format!("https://{host}:{p}"),
        None => format!("https://{host}"),
    })
}

/// Expand `{secret:NAME}` and `{origin:NAME}` against the resolver.
fn expand_secrets(template: &str, ctx: &PlanContext) -> Result<String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let Some(end) = rest[start..].find('}').map(|e| start + e) else {
            break;
        };
        let inner = &rest[start + 1..end];
        let Some((kind, name)) = inner.split_once(':') else {
            out.push_str(&rest[..=end]);
            rest = &rest[end + 1..];
            continue;
        };
        if kind != "secret" && kind != "origin" {
            out.push_str(&rest[..=end]);
            rest = &rest[end + 1..];
            continue;
        }
        let value = ctx
            .resolver()
            .get(name)
            .map(|f| f.value)
            .or_else(|| {
                // Optional-and-absent is a legitimate empty value, not a failure.
                ctx.resolver()
                    .definition(name)
                    .filter(|d| !d.required)
                    .map(|_| String::new())
            })
            .with_context(|| {
                format!(
                    "nginx-vhost: {{{kind}:{name}}} needs secret '{name}', which no provider has ({})",
                    ctx.resolver().describe()
                )
            })?;
        out.push_str(&rest[..start]);
        out.push_str(&if kind == "origin" {
            origin_of(&value, name)?
        } else {
            value
        });
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// A certificate a vhost depends on, with the domains of the server block(s)
/// that use it.
#[derive(Debug, Clone, PartialEq)]
pub struct CertNeed {
    pub cert_name: String,
    pub domains: Vec<String>,
}

#[derive(Debug, Default, PartialEq)]
pub struct VhostFacts {
    /// Every domain mentioned by any server block.
    pub domains: Vec<String>,
    /// One entry per distinct certificate referenced.
    pub certs: Vec<CertNeed>,
}

/// One vhost to install, after config is resolved.
#[derive(Debug, Clone)]
struct VhostSpec {
    conf: String,
    site_name: String,
    ssl: bool,
    domains: Option<Vec<String>>,
    cert_name: Option<String>,
}

fn push_unique(list: &mut Vec<String>, value: &str) {
    if !value.is_empty() && !list.iter().any(|v| v == value) {
        list.push(value.to_string());
    }
}

/// Parse a vhost, tracking `server { }` blocks so each certificate is paired
/// with the domains that actually use it.
pub fn parse_vhost(text: &str) -> VhostFacts {
    let mut facts = VhostFacts::default();
    let mut depth: i32 = 0;
    let mut in_server = false;
    let mut block_domains: Vec<String> = Vec::new();
    let mut block_cert: Option<String> = None;

    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim().to_string();
        if line.is_empty() {
            continue;
        }

        if depth == 0 && line.starts_with("server") && line.contains('{') {
            in_server = true;
            block_domains.clear();
            block_cert = None;
        }

        if in_server {
            if let Some(rest) = line.strip_prefix("server_name") {
                for token in rest.trim_end_matches(';').split_whitespace() {
                    let d = token.trim().trim_end_matches(';');
                    // `_` is nginx's catch-all; wildcards can't use HTTP-01.
                    if d == "_" || d.starts_with('*') {
                        continue;
                    }
                    push_unique(&mut block_domains, d);
                    push_unique(&mut facts.domains, d);
                }
            }
            if line.starts_with("ssl_certificate") {
                if let Some(idx) = line.find("/live/") {
                    let tail = &line[idx + "/live/".len()..];
                    if let Some((name, _)) = tail.split_once('/') {
                        block_cert = Some(name.to_string());
                    }
                }
            }
        }

        depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;

        if in_server && depth <= 0 {
            if let Some(cert) = block_cert.take() {
                match facts.certs.iter_mut().find(|c| c.cert_name == cert) {
                    Some(existing) => {
                        for d in &block_domains {
                            push_unique(&mut existing.domains, d);
                        }
                    }
                    None => facts.certs.push(CertNeed {
                        cert_name: cert,
                        domains: block_domains.clone(),
                    }),
                }
            }
            in_server = false;
            depth = 0;
        }
    }
    facts
}

fn shell_lines(lines: &[String]) -> String {
    // printf keeps this safe to pass as a single ssh argument (no heredocs).
    let args = lines
        .iter()
        .map(|l| format!("'{}'", l.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ");
    format!("printf '%s\\n' {args}")
}

/// The directory a conf lives in, relative to the repo — which is also where it
/// lands on the target, since the whole directory is shipped.
fn conf_dir(conf: &str) -> String {
    conf.rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_else(|| ".".into())
}

fn default_site_name(conf: &str) -> String {
    Path::new(conf)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| conf.to_string())
}

fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    value?.as_sequence().map(|seq| {
        seq.iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    })
}

/// Resolve `conf:` (single) or `vhosts:` (list) into specs. `ssl` defaults to
/// true at both levels; a per-vhost value wins.
fn resolve_vhosts(cfg: &Value) -> Result<Vec<VhostSpec>> {
    let ssl_default = cfg_bool(cfg, "ssl", true);
    let single = cfg_str(cfg, "conf");
    let list = cfg.get("vhosts").and_then(|v| v.as_sequence());

    match (&single, list) {
        (Some(_), Some(_)) => {
            bail!("nginx-vhost: set either `conf:` (one vhost) or `vhosts:` (many), not both")
        }
        (None, None) => bail!("nginx-vhost: `conf:` or `vhosts:` is required"),
        (Some(conf), None) => Ok(vec![VhostSpec {
            conf: conf.clone(),
            site_name: cfg_str(cfg, "site_name").unwrap_or_else(|| default_site_name(conf)),
            ssl: ssl_default,
            domains: string_list(cfg.get("domains")),
            cert_name: cfg_str(cfg, "cert_name"),
        }]),
        (None, Some(items)) => {
            let mut specs = Vec::new();
            for (i, item) in items.iter().enumerate() {
                let conf = cfg_str(item, "conf")
                    .with_context(|| format!("nginx-vhost: vhosts[{i}] is missing `conf`"))?;
                specs.push(VhostSpec {
                    site_name: cfg_str(item, "site_name")
                        .unwrap_or_else(|| default_site_name(&conf)),
                    ssl: cfg_bool(item, "ssl", ssl_default),
                    domains: string_list(item.get("domains")),
                    cert_name: cfg_str(item, "cert_name"),
                    conf,
                });
            }
            Ok(specs)
        }
    }
}

/// What certs a vhost needs, honoring per-vhost overrides.
fn cert_needs_for(spec: &VhostSpec, ctx: &PlanContext) -> Result<Vec<CertNeed>> {
    if !spec.ssl {
        return Ok(Vec::new());
    }
    let local = ctx.repo_root.join(&spec.conf);
    let text = std::fs::read_to_string(&local)
        .with_context(|| format!("nginx-vhost: cannot read {}", local.display()))?;
    let facts = parse_vhost(&text);

    // An explicit cert_name forces provisioning even if the conf pulls its TLS
    // config in from a snippet we can't see.
    if let Some(name) = &spec.cert_name {
        let domains = spec
            .domains
            .clone()
            .unwrap_or_else(|| facts.domains.clone());
        if domains.is_empty() {
            bail!(
                "nginx-vhost: {} sets cert_name but no domains were found",
                spec.conf
            );
        }
        return Ok(vec![CertNeed {
            cert_name: name.clone(),
            domains,
        }]);
    }

    let mut certs = facts.certs;
    if let Some(overridden) = &spec.domains {
        match certs.len() {
            0 => {} // http-only vhost: nothing to provision
            1 => certs[0].domains = overridden.clone(),
            n => bail!(
                "nginx-vhost: {} references {n} certificates, so a `domains` override is \
                 ambiguous — drop it (the server blocks already declare them) or split the \
                 vhost and set `cert_name` per entry",
                spec.conf
            ),
        }
    }
    for cert in &certs {
        if cert.domains.is_empty() {
            bail!(
                "nginx-vhost: {} references cert '{}' but its server block has no server_name; \
                 add one, or set `domains:`",
                spec.conf,
                cert.cert_name
            );
        }
    }
    Ok(certs)
}

/// One-time, idempotent certbot setup: the tool itself, the ACME webroot, a
/// deploy hook that reloads nginx after each renewal, and the renewal timer.
/// Re-running changes nothing.
fn certbot_setup_step(webroot: &str, ctx: &PlanContext) -> PlannedStep {
    let sudo = ctx.sudo_prefix();
    let hook = "/etc/letsencrypt/renewal-hooks/deploy/00-deliver-reload-nginx.sh";
    PlannedStep::ssh(
        "certbot: ensure installed, webroot, renewal hook and timer".to_string(),
        format!(
            "set -e; \
             if ! command -v certbot >/dev/null; then \
               echo 'installing certbot'; \
               if command -v apt-get >/dev/null; then \
                 {sudo}apt-get update -qq && {sudo}DEBIAN_FRONTEND=noninteractive apt-get install -y -qq certbot; \
               else \
                 echo 'certbot missing and no apt-get — install it and re-run' >&2; exit 1; \
               fi; \
             fi; \
             {sudo}mkdir -p {webroot}; \
             {sudo}mkdir -p \"$(dirname {hook})\"; \
             printf '%s\\n' '#!/bin/sh' '# installed by deliver — reload nginx after a renewal' \
               'nginx -t && systemctl reload nginx' | {sudo}tee {hook} >/dev/null; \
             {sudo}chmod 755 {hook}; \
             if systemctl list-unit-files 2>/dev/null | grep -q '^certbot.timer'; then \
               {sudo}systemctl enable --now certbot.timer >/dev/null 2>&1 || true; \
             fi; \
             echo \"certbot $(certbot --version 2>&1 | awk '{{print $2}}') ready · webroot {webroot}\""
        ),
    )
}

/// Issue **or expand** a certificate, idempotently.
///
/// Checking only "does the file exist?" is not enough: if a domain is added to
/// the vhost later, the cert exists but no longer covers everything, and nginx
/// would serve a name the cert doesn't match. So this compares the cert's actual
/// SANs against the wanted domains and only acts when they differ.
///
/// Two paths, because of certbot's chicken-and-egg: with no cert the HTTPS vhost
/// can't load, so the challenge is served from a temporary HTTP-only vhost.
/// When a cert already exists the live vhost is already serving :80, so certbot
/// runs directly.
fn certbot_ensure_step(cert: &CertNeed, cfg: &Value, ctx: &PlanContext) -> PlannedStep {
    let sudo = ctx.sudo_prefix();
    let cert_name = &cert.cert_name;
    let certbot = cfg.get("certbot");
    let webroot = certbot
        .and_then(|c| c.get("webroot"))
        .and_then(|v| v.as_str())
        .unwrap_or("/var/universal/letsencrypt");
    // Renew with room to spare: certbot's own timer starts trying at 30 days, so
    // anything inside that window is already relying on a mechanism that may be
    // failing silently.
    let renew_days = certbot
        .and_then(|c| c.get("renew_before_days"))
        .and_then(|v| v.as_u64())
        .unwrap_or(30);
    let renew_window = renew_days * 86_400;
    let email = certbot
        .and_then(|c| c.get("email"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let acme_site = format!("deliver-acme-{cert_name}");
    let d_flags = cert
        .domains
        .iter()
        .map(|d| format!("-d {d}"))
        .collect::<Vec<_>>()
        .join(" ");
    let wanted = {
        let mut sorted = cert.domains.clone();
        sorted.sort();
        sorted.join(" ")
    };

    let temp_vhost = shell_lines(&[
        "server {".into(),
        "    listen 80;".into(),
        "    listen [::]:80;".into(),
        format!("    server_name {};", cert.domains.join(" ")),
        format!("    location /.well-known/acme-challenge/ {{ root {webroot}; }}"),
        "    location / { return 404; }".into(),
        "}".into(),
    ]);

    let script = format!(
        "set -e; \
         LIVE=/etc/letsencrypt/live/{cert_name}/fullchain.pem; \
         RENEWAL=/etc/letsencrypt/renewal/{cert_name}.conf; \
         WANTED='{wanted}'; \
         HAVE=''; \
         if [ -f \"$LIVE\" ]; then \
           HAVE=$({sudo}openssl x509 -in \"$LIVE\" -noout -text 2>/dev/null \
             | tr ',' '\\n' | sed -n 's/.*DNS://p' | tr -d ' ' | sort | tr '\\n' ' ' \
             | sed 's/ $//'); \
         fi; \
         FRESH=0; \
         if [ -f \"$LIVE\" ] && {sudo}openssl x509 -in \"$LIVE\" -noout -checkend {renew_window} >/dev/null 2>&1; then \
           FRESH=1; \
         fi; \
         WEBROOT_OK=1; \
         if [ -f \"$RENEWAL\" ] && ! {sudo}grep -qE '^webroot_path = {webroot},?$' \"$RENEWAL\"; then \
           WEBROOT_OK=0; \
         fi; \
         if [ \"$HAVE\" = \"$WANTED\" ] && [ \"$FRESH\" = 1 ] && [ \"$WEBROOT_OK\" = 1 ]; then \
           echo \"cert {cert_name} already covers: $WANTED\"; exit 0; \
         fi; \
         if [ \"$HAVE\" = \"$WANTED\" ] && [ \"$FRESH\" != 1 ]; then \
           echo \"cert {cert_name} covers the right names but expires within {renew_days}d — renewing\"; \
         fi; \
         if [ \"$WEBROOT_OK\" != 1 ]; then \
           echo \"cert {cert_name} renews from the wrong webroot — reissuing against {webroot}\"; \
           echo \"  (a stale webroot is why a cert renews cleanly in dry runs and still expires:\"; \
           echo \"   certbot writes the challenge where nginx does not serve it)\"; \
         fi; \
         if [ -z \"{email}\" ]; then \
           echo 'nginx-vhost: a certificate must be issued or expanded but certbot.email is not set' >&2; exit 1; \
         fi; \
         if [ -n \"$HAVE\" ]; then \
           echo \"expanding {cert_name}: have [$HAVE] want [$WANTED]\"; \
           {sudo}certbot certonly --webroot -w {webroot} {d_flags} --cert-name {cert_name} \
             --email {email} --agree-tos --non-interactive --expand --keep-until-expiring; \
         else \
           echo 'no cert for {cert_name} — bootstrapping HTTP-01'; \
           {temp_vhost} | {sudo}tee /etc/nginx/sites-available/{acme_site} >/dev/null; \
           {sudo}ln -sf /etc/nginx/sites-available/{acme_site} /etc/nginx/sites-enabled/{acme_site}; \
           {sudo}nginx -t && {sudo}systemctl reload nginx; \
           set +e; \
           {sudo}certbot certonly --webroot -w {webroot} {d_flags} --cert-name {cert_name} \
             --email {email} --agree-tos --non-interactive --keep-until-expiring; \
           RC=$?; \
           set -e; \
           {sudo}rm -f /etc/nginx/sites-enabled/{acme_site} /etc/nginx/sites-available/{acme_site}; \
           {sudo}nginx -t && {sudo}systemctl reload nginx; \
           [ $RC -eq 0 ] || {{ echo 'nginx-vhost: certbot failed to issue {cert_name}' >&2; exit $RC; }}; \
         fi; \
         {sudo}test -f \"$LIVE\"; echo \"cert {cert_name} ready\""
    );

    PlannedStep::ssh(
        format!(
            "certbot: ensure {cert_name} covers [{}]",
            cert.domains.join(", ")
        ),
        script,
    )
}

fn install_step(spec: &VhostSpec, ctx: &PlanContext) -> PlannedStep {
    let sudo = ctx.sudo_prefix();
    let root = ctx.target.dir.trim_end_matches('/');
    let site = &spec.site_name;
    let available = format!("/etc/nginx/sites-available/{site}");
    let enabled = format!("/etc/nginx/sites-enabled/{site}");
    let script = format!(
        "set -e; \
         SRC={root}/{}; DEST={available}; LINK={enabled}; \
         BAK=$(mktemp); HAD=0; \
         if [ -f \"$DEST\" ]; then {sudo}cp \"$DEST\" \"$BAK\"; HAD=1; fi; \
         {sudo}cp \"$SRC\" \"$DEST\"; {sudo}ln -sf \"$DEST\" \"$LINK\"; \
         if ! {sudo}nginx -t; then \
           echo 'nginx -t failed — restoring previous vhost' >&2; \
           if [ \"$HAD\" = 1 ]; then {sudo}cp \"$BAK\" \"$DEST\"; else {sudo}rm -f \"$DEST\" \"$LINK\"; fi; \
           {sudo}nginx -t >/dev/null 2>&1 || true; rm -f \"$BAK\"; exit 1; \
         fi; \
         if ! {sudo}systemctl reload nginx; then \
           echo 'nginx reload failed — restoring previous vhost' >&2; \
           if [ \"$HAD\" = 1 ]; then {sudo}cp \"$BAK\" \"$DEST\"; {sudo}nginx -t && {sudo}systemctl reload nginx || true; fi; \
           rm -f \"$BAK\"; exit 1; \
         fi; \
         rm -f \"$BAK\"; echo \"installed {site}\"",
        spec.conf
    );
    PlannedStep::ssh(
        format!("install vhost {site} (validate + rollback on failure)"),
        script,
    )
}

pub fn compile(cfg: &Value, ctx: &PlanContext) -> Result<Vec<PlannedStep>> {
    let root = ctx.target.dir.trim_end_matches('/').to_string();
    let sudo = ctx.sudo_prefix();
    let strategy = cfg_str(cfg, "strategy").unwrap_or_else(|| "managed".into());
    let specs = resolve_vhosts(cfg)?;

    // Ship every directory the vhosts live in (usually just one).
    let mut dirs: Vec<String> = Vec::new();
    for spec in &specs {
        let dir = spec
            .conf
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_else(|| ".".into());
        push_unique(&mut dirs, &dir);
    }
    let mut steps: Vec<PlannedStep> = dirs
        .iter()
        .map(|dir| {
            PlannedStep::command(
                format!("ship {dir}/ → {}:{root}/", ctx.dest_label()),
                ctx.copy_dir(dir, &root),
            )
        })
        .collect();

    // --- render placeholders --------------------------------------------------
    // Substituted here rather than on the target: the value never crosses the
    // wire as an argument, and a missing secret fails before anything installs.
    if let Some(map) = cfg.get("render").and_then(|v| v.as_mapping()) {
        let mut subs = Vec::new();
        for (key, template) in map {
            let key = key
                .as_str()
                .context("nginx-vhost: render keys must be strings")?;
            let template = template
                .as_str()
                .with_context(|| format!("nginx-vhost: render['{key}'] must be a string"))?;
            subs.push((key.to_string(), expand_secrets(template, ctx)?));
        }
        for spec in &specs {
            let local = ctx.repo_root.join(&spec.conf);
            let text = std::fs::read_to_string(&local)
                .with_context(|| format!("nginx-vhost: cannot read {}", local.display()))?;
            let mut rendered = text.clone();
            for (key, value) in &subs {
                // A placeholder that isn't there is a rename or a typo — and it
                // would ship a config that silently lost the value.
                if !rendered.contains(key.as_str()) {
                    bail!("nginx-vhost: {} contains no placeholder '{key}'", spec.conf);
                }
                rendered = rendered.replace(key.as_str(), value);
            }
            let staged = format!("{}/{}", ctx.work_dir(), default_site_name(&spec.conf));
            steps.push(PlannedStep::write_file(
                format!("render {} ({} placeholder(s))", spec.conf, subs.len()),
                staged.clone(),
                0o644,
                rendered,
            ));
            steps.push(PlannedStep::command(
                format!("ship rendered {}", spec.conf),
                ctx.copy(&staged, &format!("{root}/{}", conf_dir(&spec.conf))),
            ));
        }
    }

    // --- snippets -------------------------------------------------------------
    // Before the vhost install, because a conf that `include`s a missing snippet
    // fails `nginx -t` — which would roll the vhost back for the wrong reason.
    if let Some(list) = cfg.get("snippets").and_then(|v| v.as_sequence()) {
        for (i, snippet) in list.iter().enumerate() {
            let path = cfg_str(snippet, "path")
                .with_context(|| format!("nginx-vhost: snippets[{i}] is missing `path`"))?;
            if !path.starts_with('/') {
                bail!("nginx-vhost: snippets[{i}].path must be absolute (got '{path}')");
            }
            let content = cfg_str(snippet, "content")
                .with_context(|| format!("nginx-vhost: snippets[{i}] is missing `content`"))?;
            let content = expand_secrets(&content, ctx)?;
            let mode = cfg_str(snippet, "mode").unwrap_or_else(|| "0644".into());
            let owner = cfg_str(snippet, "owner").unwrap_or_else(|| "root:root".into());
            let name = path.rsplit('/').next().unwrap_or("snippet").to_string();
            let staged = format!("{}/{name}", ctx.work_dir());
            let landing = format!("{root}/{name}");

            // Written at 0600 locally and shipped with mode preserved, so a
            // token is never briefly world-readable in transit.
            steps.push(PlannedStep::write_file(
                format!("render snippet {name}"),
                staged.clone(),
                0o600,
                format!("{}\n", content.trim_end()),
            ));
            steps.push(PlannedStep::command(
                format!("ship snippet {name}"),
                ctx.copy(&staged, &root),
            ));
            steps.push(PlannedStep::ssh(
                format!("install snippet {path} (mode {mode}, owner {owner})"),
                format!(
                    "set -e; {sudo}install -d -m 755 -o root -g root {}; \
                     {sudo}install -m {mode} -o {} -g {} {landing} {path}; rm -f {landing}",
                    path.rsplit_once('/')
                        .map(|(d, _)| d)
                        .unwrap_or("/etc/nginx/snippets"),
                    owner.split(':').next().unwrap_or("root"),
                    owner.split(':').nth(1).unwrap_or("root"),
                ),
            ));
            steps.push(PlannedStep::command(
                format!("remove local {name}"),
                format!("rm -f {staged}"),
            ));
        }
    }

    if strategy == "script" {
        let script = cfg_str(cfg, "script")
            .context("nginx-vhost: `script` is required for strategy: script")?;
        steps.push(PlannedStep::ssh(
            format!("run {script}"),
            format!("{sudo}{root}/{script}"),
        ));
        return Ok(steps);
    }
    if strategy != "managed" {
        bail!("nginx-vhost: unknown strategy '{strategy}' (managed|script)");
    }

    // Provision certs first — an HTTPS vhost can't pass `nginx -t` without one.
    // Deduped across vhosts so a shared cert is issued once.
    let provider = cfg_str(cfg, "provider").unwrap_or_else(|| "certbot".into());
    let mut certs: Vec<CertNeed> = Vec::new();
    for spec in &specs {
        for need in cert_needs_for(spec, ctx)? {
            match certs.iter_mut().find(|c| c.cert_name == need.cert_name) {
                Some(existing) => {
                    for d in &need.domains {
                        push_unique(&mut existing.domains, d);
                    }
                }
                None => certs.push(need),
            }
        }
    }

    match provider.as_str() {
        "certbot" => {
            if !certs.is_empty() {
                let webroot = cfg
                    .get("certbot")
                    .and_then(|c| c.get("webroot"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("/var/universal/letsencrypt");
                steps.push(certbot_setup_step(webroot, ctx));
                steps.extend(certs.iter().map(|c| certbot_ensure_step(c, cfg, ctx)));
            }
        }
        "none" => {} // caller manages certs out of band
        other => bail!("nginx-vhost: unknown cert provider '{other}' (certbot|none)"),
    }

    steps.extend(specs.iter().map(|spec| install_step(spec, ctx)));

    // An nginx deploy is only "done" if the config parses and the running nginx
    // has picked it up. That's true of every vhost deploy, so it's implicit —
    // no need to write `verify: [remote_command: nginx -t]` in every config.
    // The reload is a no-op when the installs above already reloaded.
    if cfg_bool(cfg, "verify", true) {
        steps.push(PlannedStep::ssh(
            "verify: nginx -t, then reload".to_string(),
            format!("set -e; {sudo}nginx -t; {sudo}systemctl reload nginx; echo 'nginx config valid and reloaded'"),
        ));
    }

    Ok(steps)
}
