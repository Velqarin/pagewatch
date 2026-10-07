# Pagewatch

A small Rust CLI for a polite, one-shot health check of a **public website**.
Give it one URL; it follows links in server-rendered HTML on that exact origin,
reports broken pages and redirect chains, and measures HTTP response timings.
No account, service, browser, JavaScript runtime, or background monitor.

## Quick start

Install a stable Rust toolchain from [rustup.rs](https://rustup.rs), then:

```sh
cargo build --release --locked
./target/release/pagewatch https://example.com --max-pages 10
./target/release/pagewatch https://example.com --json > report.json
```

For your own site, start small:

```sh
pagewatch https://your-site.example --max-pages 5 --max-depth 2 --vantage "my laptop / home Wi-Fi"
```

Run `cargo install --path . --locked` to put the CLI on your Cargo PATH.
Run `pagewatch --help` for every option.

## What it checks

- Normalized HTTP(S) anchor/area links, relative URLs and HTML `<base>`
- Exact scheme + host + port scope; subdomains and HTTP→HTTPS redirects are
  different origins. Start with the canonical HTTPS URL
- Fragment deduplication, redirect reuse, and loop/chain limits
- HTTP status, response size, header timing for each hop, and total fetch time
- Human-readable output or versioned JSON (`schema_version: 1`)

Queries are preserved because different queries can be different pages. It does
not sort queries, guess routes, or equate trailing-slash URLs. Non-HTTP links are
ignored. Only `text/html` responses are parsed for links.

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
  blocked, looping, or excessive redirect
- **2:** Invalid invocation/target, DNS failure, or an interrupted crawl (including
  inaccessible robots or HTTP 403/429)

`truncated: true` means a page/depth/discovery limit omitted potential coverage.
Robots exclusions are listed separately. Neither condition alone changes an
otherwise successful exit code. A success is not a claim that the whole site is
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
also be a soft 404; content correctness is not assessed. Robots rules are not a
substitute for permission to assess a site. Use only sites you are authorized to
check. URLs and query strings appear in reports; review before sharing reports.

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Tests use ephemeral loopback HTTP fixtures with a test-only injected client;
none contact the public internet. Production target validation has no bypass.
The committed lockfile makes application dependency resolution reproducible.
