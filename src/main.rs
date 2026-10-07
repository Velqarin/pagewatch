use clap::Parser;
use reqwest::{blocking::Client, header, redirect::Policy};
use scraper::{Html, Selector};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Read,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use url::Url;

const AGENT: &str = "Pagewatch/0.1";

/// Check public, single-origin HTML links without JavaScript or authentication.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Absolute public http(s) URL. Redirects cannot change its origin.
    url: String,
    #[arg(long, default_value_t = 25, value_parser = clap::value_parser!(u32).range(1..=1000))]
    max_pages: u32,
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u32).range(0..=20))]
    max_depth: u32,
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u64).range(250..=60000))]
    delay_ms: u64,
    #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=120))]
    timeout_secs: u64,
    #[arg(long, default_value_t = 2_000_000, value_parser = clap::value_parser!(u64).range(1..=10_000_000))]
    max_body_bytes: u64,
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u32).range(0..=10))]
    max_redirects: u32,
    /// Label for the machine/network running this check (not geolocation).
    #[arg(long, default_value = "local execution environment")]
    vantage: String,
    #[arg(long)]
    json: bool,
}

#[derive(Serialize, Debug)]
struct Hop {
    url: Url,
    status: u16,
    headers_ms: u128,
}
#[derive(Serialize, Debug)]
struct Page {
    reused_url: Option<Url>,
    url: Url,
    depth: u32,
    hops: Vec<Hop>,
    status: Option<u16>,
    fetch_ms: u128,
    bytes: usize,
    error: Option<String>,
}
#[derive(Serialize)]
struct Report {
    schema_version: u8,
    started_unix_seconds: u64,
    target: Url,
    vantage: String,
    user_agent: &'static str,
    timing_note: &'static str,
    pages: Vec<Page>,
    skipped_robots: Vec<Url>,
    skipped_out_of_scope: usize,
    truncated: bool,
    stopped: Option<String>,
}
impl Report {
    fn exit_code(&self) -> i32 {
        if self.stopped.is_some() {
            2
        } else if self
            .pages
            .iter()
            .any(|p| p.error.is_some() || p.status.is_some_and(|s| s >= 400))
        {
            1
        } else {
            0
        }
    }
}

fn normalized(mut url: Url) -> Result<Url, String> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err("only absolute http(s) URLs are supported".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs containing credentials are not allowed".into());
    }
    url.set_fragment(None);
    Ok(url)
}
fn same_origin(a: &Url, b: &Url) -> bool {
    a.origin() == b.origin()
}

// Conservative public-unicast allowlist: reject special-use, private, multicast,
// documentation, mapped/transition and reserved ranges. DNS is validated once
// and pinned in reqwest, preventing validation/connect DNS rebinding.
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let [a, b, c, _] = v.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && (b == 168 || b == 0 || (b == 2) || (b == 88 && c == 99)))
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(v) => {
            let s = v.segments();
            // Only global-unicast 2000::/3; exclude IETF special-use,
            // documentation and 6to4 (which embeds potentially private IPv4).
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && s[1] < 0x200)
                && !(s[0] == 0x2001 && s[1] == 0xdb8)
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && (s[1] & 0xf000) == 0)
        }
    }
}
fn client(args: &Args, target: &Url) -> Result<Client, String> {
    let host = target.host_str().ok_or("missing host")?;
    let dns_host = host.trim_matches(['[', ']']).to_owned();
    let port = target.port_or_known_default().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = (dns_host.as_str(), port)
            .to_socket_addrs()
            .map(|a| a.collect::<Vec<SocketAddr>>());
        let _ = send.send(result);
    });
    let addresses = receive
        .recv_timeout(Duration::from_secs(args.timeout_secs))
        .map_err(|_| "DNS resolution timed out")?
        .map_err(|e| format!("DNS resolution failed: {e}"))?;
    if addresses.is_empty() || addresses.iter().any(|s| !public_ip(s.ip())) {
        return Err(
            "target resolves to a private or special-use address; public targets only".into(),
        );
    }
    client_builder(args)
        .resolve_to_addrs(host, &addresses)
        .build()
        .map_err(|e| e.to_string())
}
fn client_builder(args: &Args) -> reqwest::blocking::ClientBuilder {
    Client::builder()
        .redirect(Policy::none())
        .no_proxy()
        .user_agent(AGENT)
        .timeout(Duration::from_secs(args.timeout_secs))
        .connect_timeout(Duration::from_secs(args.timeout_secs))
}

struct Crawler<'a> {
    args: &'a Args,
    target: Url,
    client: Client,
    robots: String,
    fetched: HashMap<Url, (Option<u16>, Option<String>)>,
    last_request: Option<Instant>,
    delay: Duration,
    stop: Option<String>,
}
impl Crawler<'_> {
    fn allowed(&self, url: &Url) -> bool {
        robotstxt::DefaultMatcher::default().one_agent_allowed_by_robots(
            &self.robots,
            "Pagewatch",
            url.as_str(),
        )
    }
    fn fetch(&mut self, url: Url, depth: u32, check_robots: bool) -> (Page, String, bool) {
        let start = Instant::now();
        let mut page = Page {
            reused_url: None,
            url: url.clone(),
            depth,
            hops: vec![],
            status: None,
            fetch_ms: 0,
            bytes: 0,
            error: None,
        };
        let mut current = url;
        let mut visited = HashSet::new();
        let mut body = String::new();
        let mut html = false;
        let result = (|| -> Result<(), String> {
            loop {
                if !same_origin(&self.target, &current) {
                    return Err(format!(
                        "redirect outside origin was not followed: {current}"
                    ));
                }
                if check_robots && !self.allowed(&current) {
                    return Err(format!("redirect blocked by robots.txt: {current}"));
                }
                if !visited.insert(current.clone()) {
                    return Err("redirect loop".into());
                }
                if check_robots && let Some((status, error)) = self.fetched.get(&current) {
                    page.status = *status;
                    page.reused_url = Some(current.clone());
                    return error.clone().map_or(Ok(()), Err);
                }
                if let Some(last) = self.last_request {
                    std::thread::sleep(self.delay.saturating_sub(last.elapsed()));
                }
                let began = Instant::now();
                self.last_request = Some(began);
                let response = self
                    .client
                    .get(current.clone())
                    .send()
                    .map_err(|e| format!("HTTP request failed: {e}"))?;
                let status = response.status().as_u16();
                page.status = Some(status);
                page.hops.push(Hop {
                    url: current.clone(),
                    status,
                    headers_ms: began.elapsed().as_millis(),
                });
                if status == 403 || status == 429 {
                    let reason = format!("HTTP {status} at {current}; stopped without retries");
                    self.stop = Some(reason.clone());
                    return Err(reason);
                }
                if matches!(status, 301 | 302 | 303 | 307 | 308) {
                    if page.hops.len() > self.args.max_redirects as usize {
                        return Err("redirect limit reached".into());
                    }
                    let location = response
                        .headers()
                        .get(header::LOCATION)
                        .ok_or("redirect missing Location")?
                        .to_str()
                        .map_err(|_| "invalid Location header")?;
                    current = normalized(current.join(location).map_err(|e| e.to_string())?)?;
                    continue;
                }
                html = response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| {
                        v.split(';')
                            .next()
                            .is_some_and(|m| m.trim().eq_ignore_ascii_case("text/html"))
                    });
                let mut bytes = Vec::new();
                response
                    .take(self.args.max_body_bytes + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| format!("body read failed: {e}"))?;
                page.bytes = bytes.len();
                if bytes.len() as u64 > self.args.max_body_bytes {
                    return Err("body size limit reached; page not parsed".into());
                }
                body = String::from_utf8_lossy(&bytes).into_owned();
                return Ok(());
            }
        })();
        page.fetch_ms = start.elapsed().as_millis();
        page.error = result.err();
        if check_robots {
            for visited_url in visited {
                self.fetched
                    .insert(visited_url, (page.status, page.error.clone()));
            }
        }
        (page, body, html)
    }
    fn run(mut self) -> Report {
        let mut report = Report {
            schema_version: 1,
            started_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            target: self.target.clone(),
            vantage: self.args.vantage.clone(),
            user_agent: AGENT,
            timing_note: "HTTP header timings per hop; fetch_ms includes pacing, redirects and bounded body read. Not browser/Core Web Vitals measurements.",
            pages: vec![],
            skipped_robots: vec![],
            skipped_out_of_scope: 0,
            truncated: false,
            stopped: None,
        };
        let mut robots_url = self.target.clone();
        robots_url.set_path("/robots.txt");
        robots_url.set_query(None);
        let (robots_page, robots, _) = self.fetch(robots_url, 0, false);
        if robots_page.error.is_some() || !matches!(robots_page.status, Some(200..=299 | 404 | 410))
        {
            report.stopped = Some(format!(
                "robots.txt could not be safely retrieved (status {:?}): {}",
                robots_page.status,
                robots_page
                    .error
                    .unwrap_or_else(|| "unexpected response".into())
            ));
            return report;
        }
        if robots_page.status.is_some_and(|s| s < 300) {
            self.robots = robots;
        }
        // Crawl-delay is an extension: conservatively honor the largest value in
        // any group, rather than accidentally overriding a site's slower pace.
        for line in self.robots.lines() {
            if let Some((key, value)) = line.split('#').next().unwrap_or("").split_once(':')
                && key.trim().eq_ignore_ascii_case("crawl-delay")
            {
                let seconds = match value.trim().parse::<f64>() {
                    Ok(seconds) => seconds,
                    Err(_) => {
                        report.stopped = Some("invalid robots.txt crawl-delay; run stopped".into());
                        return report;
                    }
                };
                {
                    if seconds.is_finite() && seconds >= 0.0 {
                        if seconds > 60.0 {
                            report.stopped = Some(
                                "robots.txt requests crawl-delay above 60s; run stopped".into(),
                            );
                            return report;
                        }
                        self.delay = self.delay.max(Duration::from_secs_f64(seconds));
                    } else {
                        report.stopped = Some("invalid robots.txt crawl-delay; run stopped".into());
                        return report;
                    }
                }
            }
        }
        let mut queue = VecDeque::from([(self.target.clone(), 0)]);
        let mut seen = HashSet::from([self.target.clone()]);
        let mut scheduled = 1usize;
        let selector = Selector::parse("a[href], area[href]").unwrap();
        let base_selector = Selector::parse("base[href]").unwrap();
        while let Some((url, depth)) = queue.pop_front() {
            if self.fetched.contains_key(&url) {
                continue;
            }
            if report.pages.len() >= self.args.max_pages as usize {
                report.truncated = true;
                break;
            }
            if !self.allowed(&url) {
                report.skipped_robots.push(url);
                continue;
            }
            let (page, body, html) = self.fetch(url.clone(), depth, true);
            for hop in &page.hops {
                seen.insert(hop.url.clone());
            }
            if html && page.error.is_none() && page.status.is_some_and(|s| s < 400) {
                let document = Html::parse_document(&body);
                let final_url = page.hops.last().map(|h| &h.url).unwrap_or(&url);
                let base = document
                    .select(&base_selector)
                    .next()
                    .and_then(|e| e.value().attr("href"))
                    .and_then(|href| final_url.join(href).ok())
                    .unwrap_or_else(|| final_url.clone());
                for element in document.select(&selector) {
                    let Some(link) = element
                        .value()
                        .attr("href")
                        .and_then(|h| base.join(h).ok())
                        .and_then(|u| normalized(u).ok())
                    else {
                        continue;
                    };
                    if !same_origin(&self.target, &link) {
                        report.skipped_out_of_scope += 1;
                        continue;
                    }
                    if !self.allowed(&link) {
                        if !report.skipped_robots.contains(&link) {
                            report.skipped_robots.push(link);
                        }
                        continue;
                    }
                    if seen.contains(&link) {
                        continue;
                    }
                    if depth >= self.args.max_depth || scheduled >= self.args.max_pages as usize {
                        report.truncated = true;
                        continue;
                    }
                    seen.insert(link.clone());
                    scheduled += 1;
                    queue.push_back((link, depth + 1));
                }
            }
            report.pages.push(page);
            if self.stop.is_some() {
                report.stopped = self.stop;
                break;
            }
        }
        report
    }
}
fn main() {
    let args = Args::parse();
    let result = Url::parse(&args.url)
        .map_err(|e| e.to_string())
        .and_then(normalized)
        .and_then(|target| {
            let client = client(&args, &target)?;
            Ok(Crawler {
                args: &args,
                target,
                client,
                robots: String::new(),
                fetched: HashMap::new(),
                last_request: None,
                delay: Duration::from_millis(args.delay_ms),
                stop: None,
            }
            .run())
        });
    match result {
        Err(error) => {
            eprintln!("pagewatch: {error}");
            std::process::exit(2);
        }
        Ok(report) => {
            if args.json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                println!(
                    "Pagewatch • {}\nVantage: {} • Unix time: {}\n{}",
                    report.target, report.vantage, report.started_unix_seconds, report.timing_note
                );
                for page in &report.pages {
                    println!(
                        "{} {} ({} ms, {} bytes, {} redirects){}",
                        page.status.map_or("ERR".into(), |s| s.to_string()),
                        page.url,
                        page.fetch_ms,
                        page.bytes,
                        page.hops
                            .iter()
                            .filter(|h| matches!(h.status, 301 | 302 | 303 | 307 | 308))
                            .count(),
                        page.error
                            .as_ref()
                            .map_or(String::new(), |e| format!(" — {e}"))
                    );
                    for hop in &page.hops {
                        println!(
                            "  {} {} (headers {} ms)",
                            hop.status, hop.url, hop.headers_ms
                        );
                    }
                }
                println!(
                    "{} pages checked; {} robots exclusions; {} out-of-origin links; limits reached: {}",
                    report.pages.len(),
                    report.skipped_robots.len(),
                    report.skipped_out_of_scope,
                    report.truncated
                );
                if let Some(reason) = &report.stopped {
                    println!("STOPPED: {reason}");
                }
                println!(
                    "Coverage: server HTML links only; no JavaScript, login, forms, subdomains, or guessed routes."
                );
            }
            std::process::exit(report.exit_code());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };

    struct Fixture {
        url: String,
        hits: Arc<Mutex<Vec<String>>>,
        done: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Fixture {
        fn new(routes: Vec<(&'static str, u16, &'static str, &'static str)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let hits = Arc::new(Mutex::new(Vec::new()));
            let done = Arc::new(AtomicBool::new(false));
            let (h, d) = (hits.clone(), done.clone());
            let thread = thread::spawn(move || {
                while !d.load(Ordering::Relaxed) {
                    let Ok((mut stream, _)) = listener.accept() else {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                            break;
                        }
                    }
                    h.lock().unwrap().push(path.clone());
                    let (_, status, headers, body) = routes
                        .iter()
                        .find(|r| r.0 == path)
                        .copied()
                        .unwrap_or(("", 404, "", "missing"));
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} Fixture\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
                        body.len()
                    );
                }
            });
            Self {
                url,
                hits,
                done,
                thread: Some(thread),
            }
        }
        fn run(&self, overrides: &[&str]) -> Report {
            let mut cli = vec!["pagewatch", &self.url];
            cli.extend_from_slice(overrides);
            let args = Args::parse_from(cli);
            // Test-only transport injection. No CLI or release private-target override.
            Crawler {
                target: Url::parse(&self.url).unwrap(),
                client: client_builder(&args).build().unwrap(),
                args: &args,
                robots: String::new(),
                fetched: HashMap::new(),
                last_request: None,
                delay: Duration::ZERO,
                stop: None,
            }
            .run()
        }
        fn hits(&self) -> Vec<String> {
            self.hits.lock().unwrap().clone()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.done.store(true, Ordering::Relaxed);
            self.thread.take().unwrap().join().unwrap();
        }
    }

    #[test]
    fn normalization_and_scope() {
        let a = normalized(Url::parse("https://EXAMPLE.com:443/a/../b#part").unwrap()).unwrap();
        assert_eq!(a.as_str(), "https://example.com/b");
        assert!(!same_origin(
            &a,
            &Url::parse("http://example.com/b").unwrap()
        ));
        assert!(!same_origin(
            &a,
            &Url::parse("https://sub.example.com/b").unwrap()
        ));
        assert!(normalized(Url::parse("https://user:password@example.com").unwrap()).is_err());
        assert!(normalized(Url::parse("file:///etc/passwd").unwrap()).is_err());
    }
    #[test]
    fn denies_non_public_ips() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "192.168.1.1",
            "172.31.1.1",
            "192.0.2.1",
            "198.18.0.1",
            "224.1.1.1",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "2002:7f00:1::",
            "3fff::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(public_ip(ip.parse().unwrap()));
        }
        let args = Args::parse_from(["pagewatch", "http://127.0.0.1"]);
        assert!(client(&args, &Url::parse(&args.url).unwrap()).is_err());
    }
    #[test]
    fn crawls_dedupes_fragments_and_reports_broken_links() {
        let f = Fixture::new(vec![
            (
                "/",
                200,
                "",
                "<a href='/ok#one'>a</a><a href='/ok#two'>b</a><a href='/bad'>bad</a><a href='https://example.org'>external</a>",
            ),
            ("/ok", 200, "", "fine"),
            ("/bad", 404, "", "missing"),
        ]);
        let r = f.run(&[]);
        assert_eq!(r.pages.len(), 3);
        assert_eq!(r.exit_code(), 1);
        assert_eq!(r.skipped_out_of_scope, 1);
        assert_eq!(f.hits().iter().filter(|x| *x == "/ok").count(), 1);
    }
    #[test]
    fn obeys_robots_including_redirects() {
        let f = Fixture::new(vec![
            (
                "/robots.txt",
                200,
                "",
                "User-agent: *\nDisallow: /private\n",
            ),
            (
                "/",
                200,
                "",
                "<a href='/private'>no</a><a href='/redirect'>r</a>",
            ),
            ("/redirect", 302, "Location: /private\r\n", ""),
        ]);
        let r = f.run(&[]);
        assert_eq!(r.skipped_robots.len(), 1);
        assert_eq!(r.exit_code(), 1);
        assert!(!f.hits().contains(&"/private".into()));
    }
    #[test]
    fn stops_on_rate_limit_before_next_page() {
        let f = Fixture::new(vec![
            (
                "/",
                200,
                "",
                "<a href='/limit'>limit</a><a href='/later'>later</a>",
            ),
            ("/limit", 429, "", ""),
        ]);
        let r = f.run(&[]);
        assert_eq!(r.exit_code(), 2);
        assert!(!f.hits().contains(&"/later".into()));
    }
    #[test]
    fn robots_failure_stops_before_root() {
        for status in [403, 429, 500] {
            let f = Fixture::new(vec![("/robots.txt", status, "", "")]);
            let r = f.run(&[]);
            assert_eq!(r.exit_code(), 2);
            assert_eq!(f.hits(), vec!["/robots.txt"]);
        }
    }
    #[test]
    fn redirects_record_chains_and_reject_external_origin() {
        let f = Fixture::new(vec![
            ("/", 302, "Location: /one\r\n", ""),
            ("/one", 302, "Location: https://example.com/\r\n", ""),
        ]);
        let r = f.run(&[]);
        assert_eq!(r.pages[0].hops.len(), 2);
        assert!(
            r.pages[0]
                .error
                .as_ref()
                .unwrap()
                .contains("outside origin")
        );
        assert_eq!(r.exit_code(), 1);
    }
    #[test]
    fn loops_and_redirect_limits() {
        let f = Fixture::new(vec![
            ("/", 302, "Location: /one\r\n", ""),
            ("/one", 302, "Location: /\r\n", ""),
        ]);
        assert!(f.run(&[]).pages[0].error.as_ref().unwrap().contains("loop"));
        assert!(
            f.run(&["--max-redirects", "0"]).pages[0]
                .error
                .as_ref()
                .unwrap()
                .contains("limit")
        );
    }
    #[test]
    fn bounds_pages_depth_and_body() {
        let f = Fixture::new(vec![
            ("/", 200, "", "<a href='/next'>next</a>"),
            ("/next", 200, "", "okay"),
        ]);
        for option in [["--max-pages", "1"], ["--max-depth", "0"]] {
            let r = f.run(&option);
            assert_eq!(r.pages.len(), 1);
            assert!(r.truncated);
        }
        let r = f.run(&["--max-body-bytes", "10"]);
        assert!(r.pages[0].error.as_ref().unwrap().contains("body size"));
    }
    #[test]
    fn honors_html_base_and_preserves_queries() {
        let f = Fixture::new(vec![(
            "/",
            200,
            "",
            "<base href='/dir/'><a href='page?q=1#fragment'>q1</a><a href='page?q=2'>q2</a>",
        )]);
        let r = f.run(&[]);
        assert_eq!(r.pages.len(), 3);
        assert!(f.hits().contains(&"/dir/page?q=1".into()));
        assert!(f.hits().contains(&"/dir/page?q=2".into()));
    }
    #[test]
    fn json_round_trip_and_clean_success() {
        let f = Fixture::new(vec![("/", 200, "", "hi")]);
        let r = f.run(&[]);
        assert_eq!(r.exit_code(), 0);
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["pages"][0]["status"], 200);
    }
    #[test]
    fn redirects_reuse_already_fetched_destinations() {
        let f = Fixture::new(vec![
            (
                "/",
                200,
                "",
                "<a href='/a'>a</a><a href='/b'>b</a><a href='/c'>c</a>",
            ),
            ("/a", 302, "Location: /b\r\n", ""),
            ("/b", 200, "", "fine"),
            ("/c", 302, "Location: /b\r\n", ""),
        ]);
        let r = f.run(&[]);
        assert_eq!(r.exit_code(), 0);
        assert_eq!(f.hits().iter().filter(|p| *p == "/b").count(), 1);
        assert!(r.pages.last().unwrap().reused_url.is_some());
    }
    #[test]
    fn robots_exclusions_do_not_use_discovery_budget() {
        let f = Fixture::new(vec![
            (
                "/robots.txt",
                200,
                "",
                "User-agent: *\nDisallow: /private\n",
            ),
            (
                "/",
                200,
                "",
                "<a href='/private'>no</a><a href='/ok'>ok</a>",
            ),
            ("/ok", 200, "", "yes"),
        ]);
        let r = f.run(&["--max-pages", "2"]);
        assert_eq!(r.pages.len(), 2);
        assert_eq!(r.exit_code(), 0);
    }
    #[test]
    fn invalid_crawl_delay_fails_closed() {
        let f = Fixture::new(vec![(
            "/robots.txt",
            200,
            "",
            "User-agent: *\nCrawl-delay: invalid\n",
        )]);
        assert_eq!(f.run(&[]).exit_code(), 2);
        assert_eq!(f.hits(), vec!["/robots.txt"]);
    }
    #[test]
    fn reused_alias_preserves_terminal_failure() {
        let f = Fixture::new(vec![
            ("/", 200, "", "<a href='/a'>a</a><a href='/c'>c</a>"),
            ("/a", 302, "Location: /b\r\n", ""),
            ("/b", 404, "", "missing"),
            ("/c", 302, "Location: /a\r\n", ""),
        ]);
        let report = f.run(&[]);
        let alias = report.pages.last().unwrap();
        assert_eq!(alias.status, Some(404));
        assert!(alias.reused_url.is_some());
        assert_eq!(f.hits().iter().filter(|p| *p == "/a").count(), 1);
        assert_eq!(f.hits().iter().filter(|p| *p == "/b").count(), 1);
        assert_eq!(report.exit_code(), 1);
    }
    #[test]
    fn reused_alias_preserves_terminal_redirect_error() {
        let f = Fixture::new(vec![
            ("/", 200, "", "<a href='/a'>a</a><a href='/c'>c</a>"),
            ("/a", 302, "Location: https://example.com/outside\r\n", ""),
            ("/c", 302, "Location: /a\r\n", ""),
        ]);
        let report = f.run(&[]);
        let original = &report.pages[1];
        let alias = &report.pages[2];
        assert_eq!(alias.error, original.error);
        assert!(alias.error.as_ref().unwrap().contains("outside origin"));
        assert!(alias.reused_url.is_some());
        assert_eq!(f.hits().iter().filter(|p| *p == "/a").count(), 1);
    }
}
