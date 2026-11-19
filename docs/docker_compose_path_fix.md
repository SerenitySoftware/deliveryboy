# Compose files in subdirectories

Delivery Boy ships Compose files at their repository-relative paths. For
`files: [deploy/compose.yml, overrides/compose.yml]`, the target contains
`<root>/deploy/compose.yml` and `<root>/overrides/compose.yml`. Start, logs,
live-config diff, preflight and rollback use those same paths, in `files` order.
Files with equal basenames in different directories remain distinct.

Compose resolves relative paths against the first Compose file. Ship bind
sources and service env files with `include`, using their repository-relative
paths. For example, `./assets` in `deploy/compose.yml` needs
`include: [deploy/assets]`; `../shared` needs `include: [shared]`.
Delivery Boy creates missing parent directories before copying files.

The rendered `env_file` still lands at the target root under its basename.
A service in `deploy/compose.yml` that needs that file must refer to `../.env`,
or use an explicitly included env file at its own relative path. Delivery Boy
does not rewrite Compose YAML or relocate secrets to match service paths.

An optional leading `./` is normalized. Absolute paths, parent traversal,
reserved deployment-state paths and overlapping destinations are refused before
an executable plan is produced. An include cannot overwrite a Compose file or
the rendered env file. Paths containing spaces or quotes are shell-quoted.

New config snapshots record the ordered Compose paths and the saved paths.
Rollback checks that layout before restoring files or starting containers, and
recreates missing parent directories. Root-only snapshots made by older versions
remain supported. Nested snapshots without layout metadata, or snapshots whose
metadata differs from the current layout, are refused.

If an older version left `<root>/compose.yml` while the configured nested file
`<root>/deploy/compose.yml` is absent, the deployment stops before changing target
files or running target Docker commands. No files are moved or converted.
Migrate the old flat layout explicitly before retrying; do not treat it as an
empty first release. Keep the existing files and snapshots until that migration
has a verified rollback plan.

Offline regression tests use temporary local targets and a Docker stub that
checks each `-f` input exists. They cover nested shipping, duplicate basenames,
failed-release unwind, explicit rollback, missing parents, first-release cleanup,
legacy-layout refusal, corrupt snapshot metadata and root-only compatibility.
They do not prove a real target deployment or container health.
