# Documentation review and proposed strategy

Reviewed 2026-10-20 against development main `1826210a91254823fff23333bae6dd216ad24982`
and the accompanying #294 corrections. This is a review/proposal, not approval
to publish a site, migrate content or release a crate. Rustdoc remains the
behavior/API source of truth under AGENTS.md and CONTRIBUTING.md.

## Recommendation for review

First make the current sources navigable and verifiable, then shorten the README
by moving tasks to their chosen authoritative home. Keep API/type/lifecycle and
middleware contracts in rustdoc. My preferred eventual direction is a small
operator/user guide rendered as a versioned Pages book, linked to version-matched
rustdoc, **if** its extra preview, publishing and ownership work is accepted.
An expanded rustdoc guide is a credible lower-maintenance alternative. mdBook is
a candidate renderer, not a selected dependency. Do not maintain the same prose
in a book and rustdoc. A site alone cannot fix unsupported product claims.

The current repository already has substantial behavior guidance in crate and
module docs. The missing layer is an audience-oriented path through it. This
PR adds a repository [documentation map](README.md), corrects the known claims,
and exercises recipes without changing hosting or deleting established sections.
A 710-line README at the reviewed revision is the most visible symptom of
unclear topic ownership, not the only problem.

## Inventory and audience/content map

| Surface at reviewed revision | Primary audience / current role | Finding and proposed ownership |
| --- | --- | --- |
| Root README (710 lines) | Evaluators, CLI users, operators, embedders; all-in-one landing page | Setup competes with migration, policy, lifecycle and admin reference. Retain landing/quickstart/caveats; move tasks with preserved anchors after structure review. |
| `src/lib.rs` crate guide (377 lines) | CLI users and Rust embedders | Strong transport, session and shutdown contracts, but starts with Rust construction. Add audience entry points; keep behavior and the one ordering diagram here. |
| `src/config.rs` and `ProxyConfig`/builder rustdoc | Operators needing exact fields; embedders | Typed reference is authoritative, but many TOML fences are fragments/schema sketches. Label fragments; link a complete checked recipe from each major feature. |
| Public middleware, auth, cache, reload, telemetry and admin modules | Embedders and operators diagnosing behavior | Valuable detailed contracts, hidden behind source/module vocabulary. Link by task; keep compiled Rust examples and migration contracts adjacent to APIs. |
| `config.example.toml` | CLI operators editing configuration | Annotated recipe, not every option/default. Commented alternatives cannot all be enabled together; do not call it an exhaustive schema. |
| `examples/*.toml`, Compose config and NGINX example | Operators and contributors | Useful recipes, some depend on unshipped internal peers/npm/IdP. TOML parsing is weaker than CLI preparation; multi-replica runtime now tested through real NGINX. |
| Rust examples under `examples/` | Embedders and contributors | Compiled targets complement doctests, but external runtime requirements vary. Cross-link by task and list required features/peers; do not imply all examples are standalone smoke tests. |
| `docs/production-gateway.md` (151 lines) | Production operators | Useful concern-to-setting mapping; stale flow, secret-check and attribution claims needed correction. Keep operational recipe and links to canonical limits. |
| `docs/architectures.md` (634 lines) | Evaluators and system designers | Research mixed supported, external and future capabilities. Add local status qualifiers, not just a global research disclaimer; preserve its research identity. |
| `docs/comparison.md`, `docs/benchmarks.md` | Evaluators and performance contributors | Product comparisons age; benchmark baseline is 0.4.1, not current. Date evidence and link #296 rather than repeat absolute performance promises. |
| `docs/external-tool-gates.md` | Advanced embedders | Application-owned integration proposal. Reload reference was stale; do not imply an alternate hot-reload policy constructor. |
| CLI `--help`, `--check`, configuration summary | CLI operators / automation | Help is executable truth for flags, but `--check` is offline. Missing secrets fail; it cannot verify backends/JWKS/tool semantics. Prefer concise links over copying full help into docs. |
| `src/skills/*.md` bundled management prompts | Administrators using MCP prompts | Generated advice is product documentation too. Scope/role fragments and transport alternatives were not independently runnable. Keep trust/redaction and no-replay caveats aligned with human guides. |
| `CONTRIBUTING.md` (193 lines), AGENTS.md | Contributors and automation | Good feature/runtime contracts; strict doc warning flag differed. Retain validation/ownership policy here and keep the user guide free of contributor-only mechanics. |
| Helm README, values/schema/templates, Compose README, Dockerfile | Deployers / release maintainers | Listener vs Service ports and secret rollouts are now documented/tested. Actual cluster/ingress behavior and artifact smoke remain distinct; #297 owns release/deployment checks. |
| CI, release workflows, Cargo metadata, release notes | Contributors and release maintainers | docs.rs/latest and installed artifacts lag unreleased fixes. Version/pin and packaging checks belong in release readiness, not an implicit docs publication. |
| Original deep-review assessment | Maintainers | Tracker references `docs/deep-review-2026-10-04.md`, absent in the managed checkout at review time. Do not invent the artifact; live child issues retain evidence. Remove the stale path claim in the tracker. |

Inventory covers each documentation surface, not a claim that every external
link, third-party comparison or deployment environment has been independently
verified. Per-module walkthrough expansion is incremental work below.

## Concrete findings and corrections in #294

1. README auth alternatives repeated `[auth]` in one fence and auth/feature
   snippets omitted required proxy/backend context; JWT omitted the
   required admin token. Separate complete configs and declared fragments, require resource URL audience,
   fail-closed role mapping and explicit management credential.
2. README/module/per-backend diagrams disagreed with configured request order.
   Keep one crate-level diagram, explain request versus response unwinding,
   public-name authorization versus backend-name filtering and management dispatch.
3. Production guide showed `--check` before providing secrets, said missing
   variables only warn, promised caller attribution, equated operation concurrency
   with database connections, and blurred process probes with backend health.
   Replace those claims and link operational limitations directly from the recipe.
4. `${...}` in database CLI args and `${HOME}/projects` are literal, not resolved.
   Use supported backend environment fields with an explicit external server
   contract, or a concrete path. Test configs through the real executable.
5. Mixed-write production/enterprise/compliance recipes enabled global
   coalescing, suppressing independent identical writes. Disable it and add a
   regression assertion; keep the opt-in read-workload recipe.
   Hedging can choose a successful alternative to a failed primary; “first response
   wins” is insufficient. Coalescing independent writes is not usually safe by
   default. Document duplicate execution, suppressed execution and ambiguous
   side effects together with cache generations and owner-loss recovery.
6. Per-role/tenant quota and billing labels, live filter edits/tool kill switches,
   token delegation, injection scanning and immutable caller-attributed audit are
   external/future requirements, not current configuration. Label each affected
   architecture locally. Size limits are not DLP and aliases do not compress output.
7. Library usage consumed one Proxy twice (`into_router`, then `serve`). Show one
   path and link the compiled embedding/lifecycle example. Optional admin auth
   wording in crate rustdoc was misleading: no dedicated token means disabled.
8. Bundled auth/resilience/setup skills mixed partial/invalid examples and could
   recommend caching without adequate write safety. Make Markdown configs complete,
   distinguish ordinary/admin authority and align startup-snapshot/reload advice.
9. Performance percentages/nanoseconds are not current guarantees. Preserve dated
   historical data and send benchmark composition/workload repair to #296.
10. Published artifacts/docs and this checkout differ. Add an explicit version
    boundary and local rustdoc command, keeping exact Git pins and release hold.

## Proposed lean README outline

Target roughly 120–180 lines, measured for usefulness rather than a rigid gate:

1. Name/badges, one-paragraph purpose, maintained scope and supported transports.
2. “Choose your path”: run CLI, deploy, embed, contribute (documentation map links).
3. Installation choices and explicit published-version versus main distinction.
4. One complete loopback quickstart, prerequisites, `--check`, start and MCP URL.
5. Essential production caveats: dedicated management token; separate backend
   credentials; read/write replay/cache safety; session affinity; restart-only policy.
6. A compact capability/feature summary linked to typed reference and task guides.
7. License, contribution and support links.

Move current authentication/resource/RBAC/migration sections to one auth task
path; shutdown/embedding to the crate guide; reload to its canonical contract;
admin endpoint lists to generated OpenAPI + admin guide; outcome/cache/retry
matrices to owning module guides; deployment and operational walkthroughs stay
recipes. Keep current README heading anchors as concise summaries/link stubs
for at least the next release cycle. Move prose only once its destination and
versioned links are checked. Avoid two full copies during transition.

## Proposed navigation / table of contents

- **Start:** what it does, choose CLI/library, installation/version, local first session.
- **Tasks:** connect transports; authenticate public clients; authorize/scoped discovery;
  configure safe resilience/cache; inspect/manage; reload or restart; diagnose failures.
- **Deploy/operate:** production recipe; container/Compose; Helm/ingress; multi-replica
  affinity and owner loss; rotate credentials; collect telemetry; graceful shutdown.
- **Concepts:** MCP namespaces/sessions; management trust; middleware request/response
  flow; failure/outcome/replay semantics; cache isolation/generation; supported limits.
- **Reference:** typed config/builder/API rustdoc; CLI help; generated admin OpenAPI;
  features/protocol versions; migration notes and compatibility boundaries.
- **Contribute:** runtime setup and gates; configured-test coverage; documentation
  ownership; benchmark methodology; deployment/release readiness.
- **Research:** architecture patterns, external gate pilot, dated comparisons.

Readers should navigate by task before knowing a module name. Each task links
one behavioral contract and one checked complete recipe; each reference links
back to a relevant task. Keep research out of the happy-path setup navigation.

## Options compared

| Criterion | Expand rustdoc for user + developer guide | Pages user guide (possibly mdBook) + API rustdoc |
| --- | --- | --- |
| Entry/discovery | Existing docs.rs, no new hosting; crate starts with API conventions | Clear CLI/operator landing and hierarchy; separate stable URL to maintain |
| Search/navigation | Strong API search; prose chapters/sidebar need deliberate structure | Book chapter navigation/prose search; cross-site API search stays separate |
| Executable examples | Rust doctests native; TOML/shell need explicit harness either way | Rust snippets need extraction/compilation or links; mdBook does not validate TOML/workflows automatically |
| Source of truth | Matches current AGENTS/CONTRIBUTING; fewer publication surfaces | Must explicitly change ownership policy for task prose while keeping API/behavior canonical; no duplicated guide text |
| Versions | docs.rs crate versions align with publication; unreleased guidance needs local/commit links | Need released-version switcher/snapshots and a clearly labeled development guide; link matching crate versions |
| Preview/publishing | `cargo doc --open`; existing strict CI/doc publication | Add pinned renderer, PR artifact previews, Pages permissions/environment and deploy workflow; no publication before review |
| Accessibility | Rustdoc handles code/API well; test prose headings, tables and keyboard paths | Choose/test theme, keyboard/search navigation, contrast and mobile; a theme alone is not an accessibility audit |
| Link maintenance | Intra-doc references checked; GitHub/docs.rs/local guide links still need verification | Need book links, cross-site version links, redirects and old GitHub anchors; higher maintenance |
| Operational effort | Lowest new infrastructure; can still shrink README using rustdoc guide chapters | Better task structure at project scale if a maintainer owns navigation, versioning and preview failures |
| Drift risk | Dense API-first prose and duplicated repository recipes are current risks | Dual-source duplication is the main risk; fix via ownership and linked examples, not synchronization by hand |

A small hybrid can keep task Markdown in the repository and render selected
pages once, while linking reference contracts in rustdoc. Do not copy chapters
into both systems. Before selecting it, prototype one auth task with local-only
preview, test navigation/cross-links and show maintenance costs. No site or
mdBook dependency is added by #294.

## Ownership and verification contract

- Behavior/API/types, middleware ordering, ownership and invariants: owning
  rustdoc/source, backed by configured tests. Repository tasks link rather than
  redefine defaults/limits; generated OpenAPI owns endpoint media/status schemas.
- Complete operator recipes: checked example TOML or Markdown config blocks,
  linked by the task guide. CLI flags: actual clap help, not a hand-maintained copy.
- Research/historical measurements: dated repository docs, explicit unsupported
  ingredients and measurement environment; no unsupported promise in a recipe.
- Rust examples: strict doc compilation/tests and all-target feature matrix.
  TOML: strict parse plus `--check`/secret preparation. Runtime: local full session
  with exact list/call output, denial with zero backend work, cleanup, feature
  boundaries; actual external deployments still need deployment verification.
- Links: local inline file paths now checked; anchors, reference-style/external
  URLs, images and release-version destinations need broader verification later.
  Do not report offline link checks as external availability tests.
- A feature/fix PR updates its owner, affected recipes/CLI/skills and regression
  together. A release checks package version, feature matrix, dependency crate
  identities, linked docs version and packaging; publication is separately held.

## Prioritized incremental plan

| Priority / step | Scope / acceptance | Tracking |
| --- | --- | --- |
| P2, now | Correct verified claims; executable Markdown/example configs; local quickstart/JWT sessions and link paths; inventory/options/proposal delivered | #294 |
| P2, alongside backlog | Invariant/protocol tests, truthful benchmark baselines, artifact/deployment smoke and release/doc version alignment | #295, #296, #297 respectively |
| P2, next docs quality | Label/verify module TOML fragments, add checked include/source ownership, Markdown anchors/reference links and optional external-link report; prevent regression without requiring a site | [#338](https://github.com/joshrotenberg/mcp-proxy/issues/338) |
| P3, after review | Slim README and add audience/task paths using agreed ownership; preserve old anchors and review the concrete content map before moves | [#339](https://github.com/joshrotenberg/mcp-proxy/issues/339) |
| Decision, later | Choose rustdoc-only versus versioned Pages guide, renderer, URL/version support and publishing owner; validate one local prototype before any deploy | [#339](https://github.com/joshrotenberg/mcp-proxy/issues/339) |

Do not hold routine correctness work for the future hosting choice. Do not mark
#294 blocked because that choice is open. Follow-ups are outside the original
37-child tracker count; update their own status without inflating that denominator.

## Decisions for the maintainer

1. Prefer expanded rustdoc or approve a local operator-guide prototype with Pages
   as a potential destination? The recommendation above is conditional, not a
   decision already made.
2. Which released documentation versions should remain browsable, and who owns
   redirects, external-link triage and accessibility/version checks?
3. After agreeing topic ownership, approve the short README outline/content moves.
   Until then, retain existing sections/anchors and ship accuracy checks normally.
