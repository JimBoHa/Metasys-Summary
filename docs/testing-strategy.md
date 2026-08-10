# Testing and debugging strategy

This document defines the test regimen for Metasys Summary. The goal is to catch regressions early, make failures diagnosable, and keep production Metasys and SQL systems isolated from routine development.

## Quality goals

1. Every pull request runs the same deterministic verification command developers can run locally.
2. Authentication, authorization, database writes, and read-only external connectors receive the deepest coverage.
3. Tests never require production credentials, production IP addresses, or production exports.
4. Failures identify the broken boundary and preserve useful, scrubbed diagnostics.
5. Coverage increases by adding meaningful boundary and failure-path assertions, not assertions written only to raise a percentage.

Before this foundation was added, the main crate had 37 tests and the isolated legacy-SQL helper had 2 tests. They cover analytics, configuration validation, alarm parsing, SQL query validation and bucketing, SQLite persistence, reporting, authentication primitives, scoped portal access, CSRF, and selected web routes. The largest gaps are network contract fixtures, end-to-end browser workflows, migrations from historical schemas, fault injection, accessibility, packaging, and sustained-load behavior.

## Test lanes

| Lane | Trigger | Target duration | Purpose |
|---|---|---:|---|
| PR gate | Every pull request and branch push | Under 10 minutes | Formatting, strict lint, all unit/integration tests, static contracts, repository hygiene, and disposable HTTP smoke test |
| Coverage | Pull requests after baseline tooling lands | Under 12 minutes | Produce line/branch coverage, enforce critical-module floors, and prevent unexplained decreases |
| Nightly resilience | Scheduled and manually dispatched | Under 45 minutes | Property/fuzz seeds, migration matrix, timeout/error injection, concurrency and longer-running query tests |
| Private integration | Manual or scheduled on a protected runner | Under 30 minutes | Read-only checks against lab Metasys REST/legacy and SQL Server versions; never available to forked PRs |
| Release qualification | Version tag or manual dispatch | Under 60 minutes | macOS bundle, signing, LaunchAgent lifecycle, upgrade/rollback, clean-machine and LAN checks |

The PR gate is blocking. Nightly or private-integration failures block a release until triaged, but do not expose secrets or make external changes.

## Required PR gate

The repository-level `scripts/verify.sh` command is the source of truth and runs:

- `rustfmt` for the main service and standalone legacy helper;
- all locked Rust tests for both crates;
- Clippy with warnings denied for all targets;
- JavaScript syntax checks;
- HTML/JavaScript static contracts, including duplicate and missing element IDs;
- high-confidence credential, private-key, private-network-address, and generated-artifact checks;
- a disposable demo-mode server with browser-like bootstrap, authenticated page/API requests, CSRF rejection/acceptance, security-header checks, logout, and post-logout denial.

GitHub Actions runs this exact command on a hosted macOS runner because macOS Keychain, application packaging, and native behavior are part of the product boundary. The smoke server uses a new temporary SQLite database, a loopback-only port, the demo connector, and a missing temporary config path. It cannot read the installed production database or Metasys/SQL credentials.

## Risk and coverage matrix

| Area | Primary risks | Required automated coverage |
|---|---|---|
| Portal authentication | credential bypass, session fixation, weak cookies, brute-force bypass | password tests, bootstrap single-use test, login-rate-limit boundaries, cookie flags, expiration, logout invalidation |
| Authorization and scope | role escalation, cross-building/floor/region data exposure | table-driven role × page × endpoint matrix; floor-only and region-only fixtures; direct-ID access denial |
| CSRF and local-only settings | remote configuration, state change without token | every unsafe route with missing/invalid/valid token; loopback, LAN peer, and forwarded-header cases |
| Metasys REST connector | version drift, pagination escape, malformed enums, partial pages | sanitized fixtures for v2-v6; pagination and hostile next-link cases; missing/null/wrong-type fields; timeout/status cases |
| Legacy Metasys connector | login/session expiry, SignalR shape drift, limited history | sanitized login/alarm/override/temperature fixtures; session-expired response; malformed hub messages; retry boundaries |
| SQL trends | unintended writes, unbounded reads, type mismatch, TLS helper failure | query-policy table tests; point/range/interval boundaries; row-limit enforcement; float/digital fixtures; helper protocol and timeout tests |
| SQLite storage | migration loss, duplicate events, broken foreign keys, partial transactions | fresh/reopen tests; historical schema fixtures; idempotent migrations; rollback on injected failure; concurrent read/write tests |
| Analytics | empty history, severity ordering, timezone edges, unstable rankings | golden small datasets; tie behavior; DST/month/year boundaries; property tests for counts and percentages |
| Email reports | unintended recipient, secret leakage, schedule duplication | rendering snapshots; escaping; recipient validation; DST scheduling; SMTP timeout/TLS failure; exactly-once attempt boundaries |
| Floor plans and zones | oversized/malformed PDF, invalid geometry, scope leak | file-size/type cases; corrupt/multipage PDFs; polygon validation/property tests; authorized and unauthorized image/PDF reads |
| Browser interface | broken navigation, missing elements, stale API assumptions | static contracts now; Playwright workflows next; responsive, accessibility, keyboard, chart/table/export, and error-state coverage |
| macOS packaging | unsigned/missing helper, bad plist, failed restart | bundle layout, `plutil`, `codesign`, LaunchAgent bootstrap/kickstart/bootout, clean install and upgrade tests |
| Operational resilience | slow upstreams, database lock, process restart, data growth | deterministic timeout injection, WAL contention, restart recovery, bounded-memory load and 24-hour soak tests |

## Planned implementation sequence

### Phase 0 — foundation (this PR)

- Add one local/CI verification entrypoint.
- Add macOS GitHub Actions with read-only repository permissions and cancellation of superseded runs.
- Add static UI contracts and repository hygiene checks.
- Add a disposable authenticated HTTP smoke test.
- Add web security-header regression coverage.

### Phase 1 — connector and security contracts (highest priority)

- Move representative, sanitized Metasys REST v2-v6 and legacy JSON into `tests/fixtures/metasys/`.
- Table-drive parser tests across missing, null, alternate-enum, and malformed values.
- Introduce a local mock HTTP server for login, pagination, status, timeout, TLS-policy, session-expiry, and retry behavior.
- Convert authorization assertions into a complete role/endpoint matrix.
- Test session expiration, rate-limit windows, cookie attributes, forwarded headers, and every state-changing endpoint's CSRF behavior.
- Add SQL helper protocol tests for invalid JSON, oversized responses, timeout/termination, unexpected columns, numeric types, and the 5,000-row cap.

Exit condition: every external response parser and protected route has success, denial, malformed-input, and upstream-failure cases.

### Phase 2 — persistence, reports, and time

- Store historical SQLite schemas as minimal SQL fixtures and test each supported upgrade path.
- Verify migrations are idempotent and leave foreign-key checks clean.
- Add transaction rollback and WAL read/write concurrency cases.
- Add deterministic clock injection for alarm windows, report schedules, session expiry, login throttling, DST transitions, and year boundaries.
- Snapshot report HTML/text after normalization and assert no credential or internal-error leakage.

Exit condition: a database from every released version can upgrade, reopen, and retain expected records; time-based behavior is independent of the test machine's clock and timezone.

### Phase 3 — browser workflows and accessibility

- Add Playwright using the disposable demo server.
- Cover bootstrap, login/logout, role-specific sidebar visibility, scoped floor navigation, request creation, operator notes/status, administration, SQL settings validation, point selection, trend loading, zoom, table view, and CSV export.
- Run desktop and mobile viewport projects with keyboard-only navigation.
- Add automated accessibility checks and fail on serious/critical violations.
- Add a small visual-regression set for the login page, building portal, alarm summary, trend workspace, and mobile drawer. Review baseline changes intentionally.

Exit condition: each user role has at least one complete happy-path workflow and one denied workflow; core pages meet the agreed accessibility baseline.

### Phase 4 — private integration and release qualification

- Use dedicated read-only lab accounts and GitHub environments requiring approval.
- Test supported Metasys API versions, the legacy UI, modern SQL TLS, and the isolated legacy-TLS helper without sending commands or modifying server data.
- Build and inspect the `.app`, verify identifiers/signatures/helper placement, install a temporary LaunchAgent, exercise restart, and verify local/LAN HTTP access.
- Test upgrade from the previous signed release and a documented rollback using a database/binary backup.

Exit condition: a tagged candidate passes both clean-install and upgrade qualification on supported macOS versions.

### Phase 5 — resilience and performance

- Add fixed-seed property tests for polygons, query validators, enum normalization, ranking invariants, and date ranges.
- Keep a small regression corpus from fuzz discoveries.
- Measure point-catalog, 5,000-sample trend, large alarm history, large floor-plan, and concurrent-user behavior.
- Run restart, upstream-timeout, SQLite-busy, truncated-response, and slow-client scenarios.
- Establish memory/latency budgets from measured baselines, then fail only on statistically meaningful regressions.

## Coverage policy

1. First publish coverage without a blocking global threshold and record the baseline by module.
2. Immediately require 90% line coverage for pure security/validation modules where deterministic testing is practical.
3. Ratchet the repository floor upward; a PR may not reduce coverage without an explanation and an approved exception.
4. Require changed-code coverage for new logic, including at least one failure-path assertion for parsers, storage, authentication, and external I/O.
5. Exclude only generated code and OS glue that is exercised by release qualification; document every exclusion.

Recommended tooling for a follow-up PR is `cargo llvm-cov` with Cobertura/LCOV artifacts. Coverage services are optional; GitHub Actions artifacts remain the canonical output.

## Fixtures and secrets

- Fixtures must be synthetic or irreversibly sanitized before review.
- Remove hostnames, IP addresses, usernames, email addresses, GUIDs tied to the site, cookies, tokens, certificates, and free-text operator notes.
- Preserve only the minimum schema shape needed for the test.
- Use `example.test`, documentation address ranges, deterministic UUIDs, and clearly labeled test passwords.
- Never record live browser sessions or commit a production SQLite copy, PDF, SQL export, packet capture, or Keychain item.
- Private-integration secrets live only in a protected GitHub environment and are never available to pull requests from forks.

## Debugging and failure artifacts

- CI sets `RUST_BACKTRACE=1` and keeps full Rust test names in logs.
- Test failures should report endpoint, role, expected status, and a scrubbed response summary.
- Browser failures should upload traces, screenshots, and console/network logs after redaction.
- Mock connector failures should preserve the fixture name and request sequence, never authorization headers.
- Flaky tests are defects. Quarantine requires an owner, linked issue, deterministic reproduction work, and an expiry date; silent retries are not a fix.

## Definition of done for feature PRs

- New behavior has success, boundary, denial, and failure-path tests at the lowest useful layer.
- Any new endpoint is added to the role/CSRF matrix.
- Any schema change includes fresh-install, migration, reopen, and rollback/failure tests.
- Any external payload change includes sanitized fixtures.
- Any visible workflow updates the applicable browser/accessibility coverage.
- `./scripts/verify.sh` passes locally.
- Documentation states any private-integration or manual release check still required.
