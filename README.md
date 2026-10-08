# Pagewatch

A small Rust CLI for a polite, one-shot health check of a **public website**.
Give it one URL; it follows links in server-rendered HTML on that exact origin,
reports broken pages and redirect chains, and measures HTTP response timings.
No account, service, browser, JavaScript runtime, or background monitor.

## Quick start

With Git installed, install a stable Rust toolchain from [rustup.rs](https://rustup.rs),
then clone the repository and build from its root:

```sh
git clone https://github.com/Velqarin/pagewatch.git
cd pagewatch
cargo build --release --locked
./target/release/pagewatch --help
./target/release/pagewatch https://example.com --max-pages 10
./target/release/pagewatch https://example.com --json > report.json
```

These commands run the built binary directly; installation is optional. To make
`pagewatch` available from other directories, install it from the repository root
and ensure Cargo's binary directory (normally `~/.cargo/bin`) is on your `PATH`:

```sh
cargo install --path . --locked
pagewatch --help
```

After installing, start small on your own site:

```sh
pagewatch https://your-site.example --max-pages 5 --max-depth 2 --vantage "my laptop / home Wi-Fi"
```

Without installation, use `./target/release/pagewatch` from the repository root
in place of `pagewatch`. Run either command with `--help` for every option.

## What it checks

- Normalized HTTP(S) anchor/area links, relative URLs and HTML `<base>`
- Exact scheme + host + port scope; subdomains and HTTP→HTTPS redirects are
  different origins. Start with the canonical HTTPS URL
- Fragment deduplication, redirect reuse, and loop/chain limits
- HTTP status, response size, header timing for each hop, and total fetch time
- Optional case-sensitive literal check in the starting URL’s final HTML source
- Human-readable output or versioned JSON (`schema_version: 1`)

Queries are preserved because different queries can be different pages. It does
not sort queries, guess routes, or equate trailing-slash URLs. Non-HTTP links are
ignored. Only `text/html` responses are parsed for links.

## Expected HTML text

```sh
pagewatch https://your-site.example --expect-text 'Welcome' --max-pages 5
```

`--expect-text` requires a nonempty, case-sensitive literal substring in the
starting URL's final `text/html` response after permitted redirects. It checks
UTF-8-decoded HTML source (with replacement for invalid bytes), not visible or
JavaScript-rendered text; markup and entities are not normalized. Choose a
stable public marker. Descendant pages are not required to contain it.

Missing text, an empty body, a non-HTML response, or an uncheckable starting page
cannot pass. HTTP errors still fail even when the marker exists. A mismatch does
not stop ordinary link discovery. JSON adds `expected_text` only when requested,
with `text`, `matched`, and `error`; text output prints the same result.
The supplied marker appears in reports, so do not use secrets. Without this
option, crawl behavior and JSON fields are unchanged.

## Politeness and safety

- Sequential requests, at least 1 second between request starts by default
- Reads `/robots.txt` first and obeys Pagewatch-specific or wildcard rules
- A missing robots file (404/410) allows crawling. Other unsuccessful responses,
  fetch failures, oversized robots files or cross-origin robots redirects stop
  the run. An empty successful robots response allows crawling
- Honors the largest numeric `Crawl-delay` in any robots group, conservatively;
  invalid values or delays above 60 seconds stop the run
- Stops the entire run on HTTP 403 or 429, without retries or bypass attempts
- No cookies, login, forms, credentials, private endpoint guessing or proxy use
- Rejects URL credentials and private/special-use IPv4/IPv6 targets. All DNS
  answers must pass validation; addresses are pinned before connecting to prevent
  DNS rebinding. No release-build option disables this protection
- Automatic HTTP redirects are disabled. Every manual hop is rechecked for
  origin and robots permission; destinations outside scope are reported,
  never requested
- TLS certificate validation remains enabled

Environment proxy variables are deliberately ignored so a proxy cannot resolve
an unchecked destination. On a proxy-only network, requests may fail: run from
a network with ordinary direct public access. Do not weaken security checks to
bypass access restrictions. This is a public-site checker, not a comprehensive
security scanner or network sandbox.

## Bounds

| Option | Default | Allowed range |
|---|---:|---:|
| `--max-pages` | 25 | 1–1000 |
| `--max-depth` | 3 | 0–20 |
| `--delay-ms` | 1000 | 250–60000 |
| `--timeout-secs` | 15 | 1–120 |
| `--max-body-bytes` | 2000000 | 1–10000000 |
| `--max-redirects` | 5 | 0–10 |

Depth zero checks the starting page. The discovery budget limits scheduled URLs,
including the starting URL; redirect hops can add HTTP requests but each chain
has its own bound. Robots exclusions do not consume the discovery budget.
The body cap reads at most one extra byte to detect overflow, then refuses to
parse that page. Compression is not requested. DNS has its own timeout, and
`--timeout-secs` also applies to each HTTP request and its body, not the whole
crawl; pacing and multiple redirects can make total elapsed time longer.

## Interpreting results

Exit codes:

- **0:** No observed page errors in the checked subset
- **1:** At least one page had HTTP 4xx/5xx, a network/body error, or an unsafe,
  blocked, looping, or excessive redirect; also a missing/uncheckable expected text
- **2:** Invalid invocation/target, DNS failure, or an interrupted crawl (including
  inaccessible robots or HTTP 403/429)

`truncated: true` means a page/depth/discovery limit omitted potential coverage.
Robots exclusions are listed separately. Neither condition alone changes an
otherwise successful exit code, except that an expected-text check fails if
robots rules prevent checking the starting URL. A success is not a claim that the whole site is
healthy. Fatal setup failures go to stderr even with `--json`; interrupted crawls
produce a report. Report timestamps are Unix seconds in UTC. `--vantage` is a
user-supplied label, not verified geolocation.

`headers_ms` measures sending the request to receiving headers, including
connection/TLS overhead where applicable. `fetch_ms` additionally includes pacing,
redirects and bounded body reads. These are **HTTP fetch timings, not browser
Core Web Vitals**. Cached redirect destinations include `reused_url` and are not
fetched again.

**SPA limitation:** a JavaScript-only application may expose few or no links in
its HTML shell. Pagewatch does not discover client-side routes, inspect rendered
UI, measure browser performance, or test authenticated areas. A 200 status can
also be a soft 404; the optional expected-text check can catch a missing marker,
but does not prove content correctness. Robots rules are not a
substitute for permission to assess a site. Use only sites you are authorized to
check. URLs and query strings appear in reports; review before sharing reports.

## Troubleshooting

### `pagewatch: command not found`

A successful build does not install the command. From the repository root, try
`./target/release/pagewatch --help` after `cargo build --release --locked`.
For use from other directories, run `cargo install --path . --locked` and add the
installation's `bin` directory to your shell's `PATH` (normally `~/.cargo/bin`;
Cargo prints the destination during installation). If `cargo` itself is missing,
finish the Rust toolchain setup and reopen your shell before retrying.

### DNS or connection failures, including proxy-only networks

Check the hostname and read stderr first. Setup errors such as
`DNS resolution failed` or `DNS resolution timed out` exit with code 2 before a
JSON report is produced, so redirecting `--json` output can leave an empty file.
Do not interpret that file as a successful check.

Pagewatch deliberately ignores environment proxy variables and validates every
resolved address. A website working in a browser does not prove this direct
HTTP client can reach it. Use a network where direct public access is permitted;
do not disable target validation, TLS checks, or network restrictions. A
private/special-use address rejection is intentional, including when any DNS
answer is non-public. Connection failures after setup appear in page errors or
the report's `stopped` reason; inspect those before drawing a site-health conclusion.

### A JavaScript application returns 200 but few pages are checked

Inspect the server-delivered HTML and its Content-Type. Discovery uses
`a[href]` and `area[href]` in successful `text/html` responses, not links added
by JavaScript. Increasing crawl limits will not expose client-side routes.
Use a separate browser-based test for rendered UI and client-side navigation.
A successful HTTP response alone does not verify the page's content.

### The crawl is incomplete

Check `truncated`, `stopped`, `skipped_robots`, `skipped_out_of_scope`, and
individual page errors in the JSON report:

- `truncated: true`: a page/discovery or depth limit omitted links. For a site
  you are authorized to check, increase `--max-pages` or `--max-depth` gradually
  within the [documented bounds](#bounds), keeping polite pacing.
- A non-null `stopped`: the crawl ended early. Respect robots failures and HTTP
  403/429; increasing limits does not override these stops.
- Robots exclusions and out-of-origin links are intentionally skipped. Start
  with the canonical scheme, host, and port; subdomains are separate origins.
- A body-size error means that page was not parsed for links. Review
  `--max-body-bytes` and the [bounds](#bounds) before choosing a larger cap.

Exit code 0 means no observed page errors in the checked subset. Even
`truncated: false` does not prove whole-site coverage: unlinked pages,
JavaScript-only routes, robots exclusions, and other origins remain outside it.

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Tests use ephemeral loopback HTTP fixtures with a test-only injected client;
none contact the public internet. Production target validation has no bypass.
The committed lockfile makes application dependency resolution reproducible.

## Manual GitHub-hosted Frostal check

The **Frostal public check (manual)** Actions workflow runs only when explicitly
started with **Run workflow**. It has no schedule and does not crawl on pushes or
pull requests. Its fixed target is `https://frostal.us/`; there are no custom URL
inputs, secrets, or credentials required.

It uses a GitHub-hosted Ubuntu 24.04 runner, a locked Cargo build, at most five
scheduled pages, depth two, three redirects per chain, one-second minimum pacing,
ten-second request timeouts, and a 2 MB body cap. It requires the literal
`Frostal.us` in the starting page’s final HTML source. This is an HTML marker
check, not a rendered-browser test. Existing origin, public-IP,
robots, and HTTP 403/429 protections stay enabled. The crawl has a five-minute
wall-clock cap (plus ten seconds for termination); the whole job is capped at
15 minutes, including an eight-minute build limit. Concurrency is limited to
one Frostal workflow run at a time.

Open the run's summary or download its **frostal-public-check** artifact for
`report.txt` and `report.json`, the underlying CLI JSON, stderr, and CLI exit
status. The human report is rendered from that single JSON crawl: it does not
run a second crawl. Metadata records UTC timestamps, commit, run URL, and the
GitHub-hosted vantage. Artifacts expire after 14 days.

Reports are attempted even if build or crawl fails. A setup/build/DNS failure or
hard timeout is an unavailable/incomplete assessment, not evidence that the site
is broken. Access or robots stops remain incomplete; ordinary findings can be
HTTP failures or fetch errors. The workflow preserves a nonzero CLI exit status
after uploading evidence. A runner outage or forced cancellation may prevent
report creation or artifact upload; use the Actions logs in that case.

GitHub may make logs and artifacts accessible to people with repository/run
access. Only public-site observations are recorded. The official checkout and
artifact actions are pinned to verified v7.0.1 commit SHAs using the Node 24
runtime; repository token permissions are read-only and checkout does not retain
credentials. Running this workflow does not change the website or its hosting.
