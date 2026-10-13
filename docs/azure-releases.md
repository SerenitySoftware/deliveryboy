# Local Azure releases

`deliver release` prepares, checks and deploys a retained release through Azure
CLI. It needs no Delivery Boy service, hosted CI runner or Azure DevOps build
record. This opt-in feature is available starting in version 0.3.0:

```sh
cargo install deliveryboy --version 0.3.0 --locked --force
deliver release --help
```

The original `.deliver.yml` commands deployer still handles independent tasks.
Azure releases use a separate `release_version: 1` config and coordinator because
the original executor's automatic undo is unsafe after database changes. Command
hooks retain the same small role: call a project's Taskfile and stop on failure.
They use an argument array and the configured Git root, without shell expansion.

## Workflow

Copy [the example](../examples/thanktank.release.yml) into the application's root,
implement its hooks, and fill in real nonsecret Azure resource identities. Treat
this file and the project tasks as trusted release code. The example is a
contract for integration, not a working ThankTank deployment configuration.

```sh
deliver --config .deliver-release.yml --target dev release plan
deliver --config .deliver-release.yml --target dev release preflight
deliver --config .deliver-release.yml --target dev release preflight --online

deliver --config .deliver-release.yml --target dev release prepare --id dev-20261010-1
# Retain the printed receipt_sha256 independently. Use that exact value below.
deliver --config .deliver-release.yml --target dev release check \
  --id dev-20261010-1 --receipt-sha256 SHA256_FROM_PREPARE --name bundle
# Run this on a native Windows host with the same clean source/configuration.
deliver --config .deliver-release.yml --target dev release check \
  --id dev-20261010-1 --receipt-sha256 SHA256_FROM_PREPARE --name windows
# Run only after all required checks have succeeded.
deliver --config .deliver-release.yml --target dev release deploy \
  --id dev-20261010-1 --receipt-sha256 SHA256_FROM_PREPARE
deliver --config .deliver-release.yml --target dev release history
```

`plan` runs no commands, resolves no secrets and makes no network calls.
`preflight` reads Git and checks tool/policy files. It does not run project hooks,
Docker checks or Bicep changes, or prove that a named Task target exists. `--online` adds Azure reads for subscription,
tenant, private storage, registry, existing sites and lease blobs. Neither form
proves write permission or accepts a release. Unavailable platform checks appear
as pending. Attempting that check on the wrong host fails; deployment cannot omit it.

`prepare` executes each build command once, fails on any nonzero status, and
checks that Git stays clean. Use the project's full test/build entry point once;
do not add a second build to deployment. It snapshots the output into a private
local directory, hashes every file and uploads that exact snapshot. The config
must identify the correct checkout even when called from another directory.

Each `check` downloads that snapshot, verifies its pinned receipt and hashes,
runs its declared command and checks that neither source nor artifacts changed.
It writes an immutable success receipt bound to the manifest, source, target,
config, command and host OS. The config identity hashes its canonical parsed
values, so checkout line endings do not change it. Failed checks retain failure records and cannot
satisfy the gate. A failed check can be retried; a successful check receipt cannot
be replaced. Checks may write safe reports to `DELIVER_EVIDENCE`.

## Artifact contract

The configured artifact directory contains regular files only, with no symlinks.
Its exact file set and SHA-256 hashes become the release manifest. Include test,
coverage, scan, schema, SBOM and rehearsal reports needed for project acceptance.
Keep internal and public docs in separate subdirectories. Also include `images.json`:

```json
{
  "api": {
    "archive": "application/api.tar.gz",
    "image_id": "sha256:REPLACE_WITH_TESTED_ENGINE_ID",
    "local_ref": "project-api:SOURCE_COMMIT",
    "config_digest": "sha256:REPLACE_WITH_SAVED_CONFIG_DIGEST"
  }
}
```

Supply every image configured under the target. Preserve the tested Docker
`image_id`; containerd may report a manifest ID where classic Docker reports a
config ID. `local_ref` names the saved tag. `config_digest` is the SHA-256 of the
config bytes referenced by the archive's `manifest.json`. For classic Docker,
these optional fields can be omitted when `image_id` is the config digest.
Archives come from `docker save`;
Docker can load gzip archives. The project bundle checker must bind those IDs to
the images actually tested and scanned, and verify that the optional config
digest comes from those exact saved bytes. Never rewrite the scan/rehearsal IDs
to fit a different Docker store. V1 accepts single-platform images only.

The Azure adapter loads each retained archive, checks its image ID, pushes a new
release-specific tag and verifies the registry manifest's configuration digest.
It disables writes/deletes for the tag and passes only `registry/repo@sha256:...`
references to the project rollout. Existing tags fail; the CLI never rebinds them.
Container Apps acceptance checks every active revision's image. V1 requires one
container per app/job; projects needing sidecars or multi-platform indexes must
extend the contract rather than weaken these checks.

## Deployment hooks and order

Hooks receive paths/identifiers through environment variables:

| Variable | Meaning |
| --- | --- |
| `DELIVER_ARTIFACTS` | Downloaded, hash-checked immutable payload |
| `DELIVER_CONTEXT` | Private JSON with producer, manifest, pinned images, target and Azure identities |
| `DELIVER_EVIDENCE` | Safe project recovery/verification reports to retain |
| `DELIVER_LOCKS` | Private local JSON pairs of lease blob name and ID, present only during deployment |
| `DELIVER_TARGET` | Explicit target name |
| `DELIVER_SUBSCRIPTION` | Explicit subscription for every project Azure call |
| `DELIVER_RESOURCE_GROUP` | Allowed application resource group |

Build commands receive `DELIVER_TARGET`, `DELIVER_RELEASE_ID` and
`DELIVER_SOURCE_COMMIT`; they write the configured output directory. The runner
removes Azure DevOps producer/job variables. Do not spoof them in application code.
The context's `locks_held` field describes state; it is not an authentication
credential. Project mutation hooks should renew the leases from `DELIVER_LOCKS`
with Azure login before writing. Keep that file local and private.

After all checks pass, deployment acquires every lease in sorted order, validates
docs artifacts, retains current revisions, publishes images, runs `infrastructure`,
stops all declared writer apps and waits for zero replicas, then runs `guard`.
The guard must cover external writers, scheduled jobs and schema rules that
Delivery Boy cannot infer. Infrastructure hooks must not start new application
writers or migrate SQL. They can reuse Task/Bicep to provision job definitions
and bind existing private identities.

Next it records the database boundary, runs `migrate` and `migration_jobs`, then
runs `rollout`. Native jobs must have a Manual trigger and an accepted image
digest. Delivery Boy records the exact execution name and waits for that execution;
an older successful execution cannot satisfy the check. It then checks app image
identities, `verification_jobs`, `verify`, and `browser`.

For each docs site, it compares the built access policy with the committed policy,
requires a reader role for an internal site's catchall route, runs the project's
validation, uploads with SWA CLI, and runs the site's hosted access/content check.
Both sites use the same manifest. Publication can be partial on failure; no
accepted marker exists until every site's checks pass. The CLI does not change
reader invitations, access roles or public/private site routing.

## Private storage and authentication

Use an existing private Azure Storage account/container. Disable blob public
access on the account. Provision the configured lease blobs separately through
reviewed IaC; this CLI never creates them during preflight. All machines and
other deployment paths must use the same account/container and lease names.
Include an environment lock and shared site locks if Dev and production share docs.

Azure calls use the existing `az login` session and explicit subscription IDs.
Unattended hosts can use an existing managed identity or federated login. Grant
only the needed Blob data, ACR, Container Apps and Static Web Apps rights. Login,
role provisioning, networking and identity creation stay outside this command.
No Cloudflare token is needed. Storage operations use `--auth-mode login`, never
account keys or SAS URLs. SWA tokens travel only through the child's environment;
the adapter suppresses raw Azure/Docker/SWA output and never saves the token.

Declare other secret environment variable **names** in `secret_env`. The runner
rejects their values in command arguments, redacts their streamed output (including
short values), and scans retained payloads/reports for those bytes. Keep new
runtime secrets in Key Vault or private application secret-input flows. Hooks
must suppress values they fetch themselves, transformed secrets and Azure errors
that could expose them. No generic CLI can infer every secret in arbitrary code.
Do not put secret values in YAML, Git subjects, ticket links or reports.

Objects use conditional, non-overwriting writes:

- `releases/APP/TARGET/ID/`: reservation, manifest, files, checks, events and recovery evidence.
- `versions/APP/TARGET/VERSION.json`: permanent production version/source binding.
- `accepted/APP/TARGET/ID.json`: the acceptance record, written last.

Set private retention, soft delete/versioning and suitable immutable-storage/RBAC
policies outside the CLI. File hashes detect changes relative to a pinned receipt;
they are not a signature or proof of who ran a check. A storage writer can forge
check receipts. Use trusted builders, constrained storage writers and an
independently retained manifest hash. Signed attestations and role-separated
check writers remain future work. Private local work directories are retained
for recovery, including on failure; remove them when retention policy permits.

## Versions and recovery

Production requires an existing stable `vMAJOR.MINOR.PATCH` tag, at the clean
checkout's commit, with the same tag on the configured remote. Its commit must be
an ancestor of the remote source branch. Fetch that branch beforehand. Delivery
Boy creates or moves no Git tags. Version reservation is permanent, even if the
build fails; use a new version for a fix. A deployment version must exceed every
accepted production version. Dev uses separate IDs and resource groups.
Changelogs use Git subjects since the last accepted release (full history for
the first); `--ticket https://...` adds optional links. Failed/prepared runs never
advance that baseline. V1 does not enrich notes from Azure Boards or PR APIs.

A deployment ID permits one attempt. A failure while publishing images or running
`infrastructure` hooks leaves writers untouched — the running release keeps
serving — and still records the failure and keeps every lease. From the
`stop-writers` phase on, a failure makes the CLI attempt to stop all writers,
retain failure/evidence records, and keep every lease. Ctrl-C counts as a failure
at whichever phase it lands in. It reports when writer shutdown or evidence upload fails. It never reverses
SQL, automatically reactivates old code, or rolls back docs. Image/application
changes may be reversible after a schema compatibility review; database changes
need the application's forward-fix or restore procedure.

Leases never expire and have no force/stale-takeover option. On failure, interrupt,
process crash or lost network, an operator must inspect the last durable intent,
Azure job execution, revisions and schema before recovery. A timed-out job may
still run. Stop/fence the old process and its children on **all** machines before
releasing or breaking leases through Azure administration. Do not infer safety
from lock age. Lease renewal checks ownership between steps; it cannot fence an
arbitrary already-running hook if an administrator breaks a lease.

V1 rejects Git submodules. Ignored build inputs, tool binaries and external
dependencies need the project's own locked-input and evidence checks. Dev can use
any clean commit; production enforces remote source-branch ancestry.

V1 targets already provisioned Azure hosting, not first-boot production bootstrap.
It has no scheduler, remote Windows runner, automatic database restore, signed
attestation service or branch-policy reporter. An Azure Repos build-validation
policy still needs a separately agreed self-hosted agent or external status-check
integration. Keep it enabled until that integration is verified; a local receipt
is not a fabricated Azure DevOps build. Native Windows execution and live Azure
rollout still require integration acceptance.

Azure interfaces: [Blob leases](https://learn.microsoft.com/en-us/cli/azure/storage/blob/lease),
[Container Apps jobs](https://learn.microsoft.com/en-us/cli/azure/containerapp/job),
and [SWA deployment](https://azure.github.io/static-web-apps-cli/docs/cli/swa-deploy/).
