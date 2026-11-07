# Documentation link verification

Run the deterministic checks with Python 3.11 or newer:

```sh
python3 -m unittest discover -s scripts -p 'test_doc_links.py'
python3 scripts/check-doc-links.py
```

The checker inventories tracked and nonignored untracked Markdown files plus
Rust doc-comment prose under `src/`. It checks local file/directory destinations,
Markdown heading anchors (including duplicate headings), explicit HTML `id`/anchor
`name` attributes, source line anchors, and full/collapsed/shortcut reference
links. Inline links, images, angle destinations, escaped or balanced parentheses,
optional titles, percent-encoded fragments and HTTP(S) autolinks are supported.
Fenced/indented code, inline code and HTML comments do not supply links. Empty
destinations and `#` mean the current document/top, as in Markdown.

Undefined explicit reference labels fail; an unmatched shortcut such as
`[ordinary prose]` is literal text. Rustdoc owns API symbol resolution through
the existing strict `cargo doc` command. This checker also resolves
`crate#section`, `crate::module#section` and module section links to source
module headings, excluding item-only documentation. Same-repository GitHub
`blob/main` and `tree/main` links are also checked against the working tree;
their external report still observes the currently published remote URL.
It does not implement a general Markdown renderer: raw HTML links,
block-comment rustdoc, generated rustdoc API anchors, bare URL detection,
blockquote-nested code blocks and renderer extensions are outside its inventory.
Keep repository navigation in the checked forms; extend the regression fixtures
before adopting new syntax. Rustdoc remains the behavior/API source of truth.

## External observations

```sh
python3 scripts/check-doc-links.py --external --report target/doc-links.json
```

CI's **Documentation links** job runs both commands and uploads the JSON report
as `documentation-links`. Offline/local errors fail this job. External status
is report-only: outages, authentication, rate limits and runner network policy
must not masquerade as stale repository links or silently count as success.
The default limits are 100 unique URLs in sorted order, five seconds per URL,
120 seconds overall, five redirects, 256 KiB per response and zero retries.
Every inventoried URL remains in the report, including URLs left unvisited when
the budget expires. Each request runs in a separate process so DNS and slow
streams cannot evade the wall-clock limit. No cookies or credentials are added;
nonpublic literal addresses and credential-bearing URLs are not probed.
Proxy configuration and TLS certificate validation are preserved.

The report records source locations, the checked Git commit, package version,
requested/final URLs, HTTP status, rate-limit `Retry-After`, and observed
docs.rs versions or GitHub refs. Floating `latest`, unversioned docs.rs links,
and `main`/`master` refs are explicit. It does not infer that a reachable page
contains this checkout's behavior, or that an observed version matches a release.
External fragments are checked only against bounded HTML response anchors;
truncated/non-HTML bodies leave missing anchors unverified. JavaScript-rendered
anchors may require manual review. An HTTP 200 is availability evidence only.

Maintainers own link triage alongside the affected documentation. Review
unavailable, missing-fragment, redirected and floating-version entries in the
artifact; retry unavailable/rate-limited sites later without increasing CI
budgets or suppressing local errors. Report network restrictions as such.
Release readiness separately checks the intended published version and content.
The checker and its fixtures use only the Python standard library to avoid a
new parser/network dependency and its update burden; its deliberately bounded
syntax is the maintenance tradeoff. No hosting or publication is performed.
