# ThankTank integration handoff

ThankTank remained read-only while this feature was built. The sample is based on
its `api-completion` worktree, root Taskfile, CI/release/docs/production pipelines,
release bundle/source/versions/deploy scripts, infrastructure target guards and
internal release/docs guides. It is not evidence of a ThankTank release.

Use [the release contract](azure-releases.md) and
[the sample config](../examples/thanktank.release.yml). New `delivery:*` tasks below
are required integration work; Delivery Boy does not claim that they exist.
Keep Azure Repos. Do not emulate `BUILD_BUILDID`, `SOURCE_CI_RUN_ID`, `TF_BUILD`,
`SYSTEM_JOBID`, pipeline lock ownership or a successful main-build record.

## 1. Add a local producer contract

Add `scripts/release/deliveryboy.py` (or an equivalent small adapter) and root
Taskfile entries for these operations. All commands must read the paths and target
from Delivery Boy's environment/context, validate them, and keep secrets out of
arguments and output. Add the root `.deliver-release.yml` from the example and
keep `.local/deliveryboy-bundle` ignored. Configure the actual active Dev target;
if it is `dev-managed`, use its group/registry and preserve DNS/cutover guards.

| New task | Exact responsibility |
| --- | --- |
| `delivery:compose-check` | Run existing `up` and its runtime smoke checks; always run `down`, including failure. Preserve either failure. |
| `delivery:export` | Verify the already sealed `.local/release-bundle`; copy it to `.local/deliveryboy-bundle/application`, and copy existing internal/external site outputs into `docs/internal` and `docs/external`. Emit `images.json` using each component's `build.json.images[component].id` and `application/COMPONENT.tar.gz`; preserve its `tag` as `local_ref` and derive `config_digest` from the saved archive config bytes. Verify the mapping using `image_identity.py`'s bounded metadata checks; never rewrite tested IDs for another Docker store. Copy required test/schema/coverage/security/rehearsal/dependency reports. Never rebuild images or docs. |
| `delivery:check-bundle` | Read `DELIVER_ARTIFACTS`; rerun exact file/source/schema/image-security/rehearsal guards, verify all six components, coverage/check evidence and hosted-browser contract/tooling. Check both docs audiences and policies. Never rebuild or rewrite the retained payload. |
| `delivery:check-windows` | On native Windows, run locked `dotnet restore Gathering.sln` and Release `dotnet build --no-restore` in `src/backend`, matching `.azure/pipelines/ci.yml`; run `windows_inventory.py`. Retain inventory/results under `DELIVER_EVIDENCE`; fail on any command/SDK mismatch. Add any required print-agent runtime tests explicitly. |

Keep `task test` as the sole full Linux suite. It already invokes tooling,
infrastructure, schema, security, docs, backend, Angular, portal and image checks.
`test:images` already builds, rehearses, exports and scans. Do not run a second
release build after it. Tests inside Linux containers can run on macOS; the
separate Windows check cannot be marked passed from that host. Make all platform
paths portable and execute the Windows receipt path on a real Windows host.

Adjust `scripts/release/bundle.py` to accept a typed local receipt alongside the
existing CI producer path. Preserve source, clean tree, exact file set, image ID,
schema, scan, SBOM and successful rehearsal checks. Use an explicit producer kind
such as `deliveryboy-local`, commit and manifest SHA-256. A missing build ID alone
must not grant acceptance. Delivery Boy downloads and pins the enclosing manifest;
the adapter validates its application evidence. Remove CI-only wording from the
local branch while retaining the existing CI validation branch if still used.

## 2. Split application deployment into hooks

Refactor reusable parts of `scripts/release/run.py` and `production.py`; do not
call their current all-in-one `deploy` functions unchanged. They still select CI
producers or assume a pipeline lock, and they also publish images/run migration.
Delivery Boy now owns those phases. Implement:

| New task | Exact responsibility |
| --- | --- |
| `delivery:infrastructure` | Validate the typed context and real lease ownership. Use existing Bicep/Task flows for the selected foundation and pinned-image job definitions. Preserve target/output/resource ID, non-destructive what-if, private identity and hostname guards. Do not migrate or start application writers. |
| `delivery:writer-guard` | Independently verify all four writer services have no active revisions or replicas; cover scheduled/manual writer jobs, the legacy source stack and any external writers. Refuse migration if any writer remains or the target/route is stale. |
| `delivery:migration-guard` | Preserve schema-version/destructive-change checks, backup/recovery prerequisites, data-mode and credential preservation, and environment isolation. Write safe pre-migration evidence. Prepare bootstrap inputs/jobs as needed while writers stay stopped. Do not run the migration job itself; `migration_jobs` owns that exact execution. |
| `delivery:rollout` | Consume only `context.images` digest references. Apply `release-apps` with existing Bicep and explicitly select the intended revisions. Do not rebuild, retag or repush. Check all six apps and exactly one intended worker revision. Keep previous writers stopped until schema compatibility allows activation. |
| `delivery:verify` | Reuse existing hosted runtime/HTTPS/operations acceptance, production data/SMTP/license rules, and stopped-writer recovery safeguards. The native verification job runs first. Save sanitized result/URL/schema records under `DELIVER_EVIDENCE`. Return failure for prepared-only production endpoints. |
| `delivery:browser` | Run the existing hosted Playwright contract/tooling against the exact deployed portal/API URLs from this release; do not accept old producer tooling or skip unavailable checks. |

`DELIVER_LOCKS` contains the private lease IDs for the acquired blobs. Renew them
using Azure login before each application-side mutation; a context boolean alone
is insufficient. Use `DELIVER_SUBSCRIPTION` explicitly on all Azure calls and
match `DELIVER_RESOURCE_GROUP` against `scripts/infra/targets.py` output validation.
Preserve the shared `thanktank-pipeline` identity where available; keep runtime,
migration and frontend identities separate. Do not create a personal automation
identity or require Cloudflare credentials.

Do not weaken `scripts/infra/deploy.py::require_active_source`. It reads the
current Azure Repos main routing decision to stop a retired Dev foundation from
being recreated. Keep that read through Azure Repos credentials or a checked
remote-main Git read; it need not consume hosted build minutes. Preserve
`preflight.py::checked_route/check_dns` and source-retention/cutover rules.

Refactor `production.py::accepted` and `versions.py` producer selection to validate
the local receipt/tag path. Replace the `TF_BUILD/SYSTEM_JOBID` guard with actual
lease ownership. Keep stable remote tags, main ancestry, target isolation,
production secrets/data/license/operations checks and saved recovery records.
Use `release_accept.py`/release notes only as consumers of the final accepted
marker; they must not require a made-up CI run ID or accept before docs/browser.
Delivery Boy retains basic Git notes and ticket URLs; richer Azure Boards/PR
notes can remain an optional project hook.

The v1 CLI requires existing registry, sites and app resources for preflight and
revision snapshots. Production first-boot provisioning, private secret upload,
DNS/TLS setup and data import remain explicit application setup work. Preserve
`production_secrets.py`'s private-network and cleanup behavior when adding the
local receipt path. Do not treat foundation-only or generated-hostname setup as
an accepted public production release.

## 3. Adapt docs checks without exposing tokens

Add `delivery:docs-check -- internal|external` and
`delivery:docs-verify -- internal|external`. Select the corresponding directory
under `DELIVER_ARTIFACTS/docs`, never a fresh docs build.

Reuse `scripts/docs/publish.py::verify` for audience, exact hashes and committed
policy. Update `check_artifact.py` and the local branch of docs manifest validation
to bind to commit/local receipt instead of `SOURCE_CI_RUN_ID`; retain the typed
CI path if still used. Preserve `internal-reader`, `no-store`, public/internal
content/search separation and automated anonymous-denial/content checks in
`scripts/docs/hosted.py`. Both sites must pass to accept the release.

Do not invoke `publish.py::main` for local publishing: it prints an Azure DevOps
secret-setting command. Delivery Boy obtains the selected site's token in memory
and passes it only via `SWA_CLI_DEPLOYMENT_TOKEN`, from an isolated working
folder with keychain storage disabled. Keep the existing Azure site access rules;
no new manual login/cache test is required by this integration.

## 4. Set up shared evidence and coordination

Through reviewed Bicep/administration, provision private evidence storage,
retention and the sample's environment/docs lease blobs. Give the build/check/
deploy identities the scoped permissions they need. Every local machine and any
remaining pipeline must use these same locks, including docs-only publication.
The existing Azure DevOps environment locks do not coordinate with Blob leases.
Disable an obsolete release trigger only after its replacement is integrated and
verified, or make both paths acquire the same leases first.

Keep the prepared receipt SHA-256 outside the writable artifact store. V1 trusts
allowed builders/storage writers; it does not sign Windows evidence or isolate
check writers cryptographically. Add stronger identity/attestation controls
before treating receipts from untrusted machines as release approval.

## 5. Verify before adoption

Add application tests for the local producer, source/config/target mismatches,
stale images/docs, wrong Windows receipts, missing coverage/security/browser
checks, wrong subscription, retired Dev targets, actual shared-lock contention,
migration failure with writers held, job timeout, unsafe secret output, and failed
docs access/content checks. Keep the existing tests and gates in `task test`.

Run Delivery Boy plan and offline/online preflight first. Then, in the ThankTank
integration task and with its deployment authorization, run a complete local
build, real Windows check, isolated Dev deployment and all hosted checks. Exercise
failure/recovery with two machines before production adoption. A lease retained
after a crash requires fencing the old process/jobs and checking schema state;
never restart old code merely because it used to work.

Azure Repos PR build-validation policy remains a separate decision. A self-hosted
Azure Pipelines agent can run the shared Task commands without hosted minutes,
or a separately implemented authenticated status publisher can support an agreed
status policy. Delivery Boy v1 does neither and changes no branch policies. Do
not remove the existing gate or mark a local receipt as a hosted build.

Update the relevant roadmap items and `docs/internal/{ci,azure-delivery,
development-release,production-release,database-migrations,documentation-sites,
docs-access-verification,recovery-rehearsal}.md`, plus the root README. Separate
implemented/local, deployed, and accepted states. Leave hosted/Windows/recovery
items open until their checks pass. No ThankTank file, policy or infrastructure
changed during the Delivery Boy implementation.
