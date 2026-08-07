---
title: "Teams"
description: "How the planned paid service differs from the free local CLI."
weight: 7
---

Delivery Boy Teams is a planned paid hosted service for groups that need shared control around releases. It is in private development. Public signup and billing are closed.

Teams is optional. Without it, the CLI still runs on your machine and connects directly to your targets. A release does not pass through Delivery Boy infrastructure.

## What stays free and local

The `deliver` CLI will continue to work without an account or hosted service. Local and CI users keep:

- `.deliver.yml` in their repository;
- plan, preflight, deploy, verify, and rollback commands;
- local secret providers and SSH targets;
- the built-in deployers.

## What Teams will add

The paid service is intended to add:

- shared run history and logs;
- roles and approval rules;
- schedules and notifications;
- Slack controls;
- isolated remote runners;
- managed retention, audit, and support.

The service will not require users to move basic release logic out of their repository.

## Preview status

Before Teams accepts untrusted users or payment, customer jobs need isolated workers with scoped secrets, network limits, resource limits, and log redaction. The service also needs invites, enforced roles, account recovery, billing controls, and an audit trail.

Until those checks pass, treat Teams as a private alpha for trusted users only.
