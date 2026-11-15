//! An RFC 9309 robots.txt parser and matcher (pure, no I/O).
//!
//! Behaviour (checked against Google's own test cases, `tests/robots.rs`):
//!
//! - Lines end in CR, LF or CRLF; a leading UTF-8 byte-order mark (or a prefix of one) is
//!   skipped; `#` starts a comment; lines are cut at [`MAX_LINE_BYTES`].
//! - Directive names are case-insensitive. A missing colon is tolerated (`disallow /`), as are
//!   the common misspellings Google accepts (`useragent`, `dissallow`, …).
//! - A group is one or more `user-agent` lines followed by rules; a `user-agent` line after a
//!   rule starts the next group. Lines other than `user-agent`/`allow`/`disallow`/
//!   `crawl-delay` (sitemaps, unknown keys) never close a group. Rules before the first group
//!   are ignored.
//! - A `user-agent` value is the product token up to the first whitespace, compared
//!   case-insensitively. Every group naming our token is merged; when none does, every `*` group
//!   is merged; when there is none, nothing is disallowed.
//! - Patterns and paths are normalised the same way before matching (non-ASCII bytes and unsafe
//!   ASCII are percent-encoded, escapes of unreserved characters are decoded and the others
//!   upper-cased, as `vc_core::CanonicalUrl` does), so `Disallow: /%7Euser` covers `/~user`.
//! - `*` matches any run of characters, a trailing `$` anchors the end. The longest matching
//!   pattern (in octets) wins; on a tie `allow` wins. `/robots.txt` is always allowed.

use std::time::Duration;

/// Lines are cut after this many bytes (Google: 8 × 2083).
pub const MAX_LINE_BYTES: usize = 8 * 2083;

/// Crawl-delay values are capped here (the per-key delay is capped at 1 h anyway).
pub const MAX_CRAWL_DELAY: Duration = Duration::from_secs(3600);

/// One `allow`/`disallow` rule with its normalised pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The normalised pattern (never empty).
    pub pattern: String,
    /// `allow` (true) or `disallow` (false).
    pub allow: bool,
}

#[derive(Debug, Clone, Default)]
struct Group {
    agents: Vec<String>,
    rules: Vec<Rule>,
    crawl_delay: Option<f64>,
}

/// A parsed robots.txt.
#[derive(Debug, Clone, Default)]
pub struct ParsedRobots {
    groups: Vec<Group>,
    sitemaps: Vec<String>,
}

/// The rules that apply to one user agent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentRules {
    /// The merged rules.
    pub rules: Vec<Rule>,
    /// The first `Crawl-delay` of the merged groups (seconds), if any.
    pub crawl_delay: Option<f64>,
    /// Whether a group names the agent itself (rather than falling back to `*`).
    pub explicit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    UserAgent,
    Allow,
    Disallow,
    CrawlDelay,
    Sitemap,
    Other,
}

fn key_of(name: &str) -> Key {
    match name.to_ascii_lowercase().as_str() {
        "user-agent" | "useragent" | "user_agent" => Key::UserAgent,
        "allow" => Key::Allow,
        "disallow" | "dissallow" | "dissalow" | "disalow" | "diasllow" | "disallaw" => {
            Key::Disallow
        }
        "crawl-delay" | "crawldelay" | "crawl_delay" => Key::CrawlDelay,
        "sitemap" | "site-map" => Key::Sitemap,
        _ => Key::Other,
    }
}

/// Splits one line (comment already removed, trimmed) into a key and a value.
fn split_line(line: &str) -> Option<(Key, &str)> {
    if let Some((k, v)) = line.split_once(':') {
        let k = k.trim();
        if k.is_empty() {
            return None;
        }
        return Some((key_of(k), v.trim()));
    }
    // A missing colon: `disallow /` (Google accepts this, RFC 9309 tolerates it).
    let (k, v) = line.split_once(|c: char| c.is_ascii_whitespace())?;
    let key = key_of(k.trim());
    if key == Key::Other {
        return None;
    }
    Some((key, v.trim()))
}

/// Splits `body` into lines on CR, LF or CRLF, skipping a leading (partial) UTF-8 BOM and
/// cutting each line at [`MAX_LINE_BYTES`].
fn lines(body: &[u8]) -> impl Iterator<Item = &[u8]> {
    let bom = [0xEF_u8, 0xBB, 0xBF];
    let skip = body
        .iter()
        .zip(bom.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let body = &body[skip..];
    let mut rest = body;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest
            .iter()
            .position(|&b| b == b'\n' || b == b'\r')
            .unwrap_or(rest.len());
        let line = &rest[..end];
        let mut next = end;
        if next < rest.len() {
            next += if rest[next] == b'\r' && rest.get(next + 1) == Some(&b'\n') {
                2
            } else {
                1
            };
        }
        rest = &rest[next..];
        Some(&line[..line.len().min(MAX_LINE_BYTES)])
    })
}

impl ParsedRobots {
    /// Parses a robots.txt body. Never fails: anything unparseable is ignored.
    pub fn parse(body: &[u8]) -> Self {
        let mut out = ParsedRobots::default();
        let mut current: Option<Group> = None;
        let mut in_agents = false;
        for raw in lines(body) {
            let raw = match raw.iter().position(|&b| b == b'#') {
                Some(i) => &raw[..i],
                None => raw,
            };
            let text = String::from_utf8_lossy(raw);
            let Some((key, value)) = split_line(text.trim()) else {
                continue;
            };
            match key {
                Key::UserAgent => {
                    if !in_agents {
                        if let Some(g) = current.take() {
                            out.groups.push(g);
                        }
                        current = Some(Group::default());
                        in_agents = true;
                    }
                    let token = value
                        .split(|c: char| c.is_ascii_whitespace())
                        .next()
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if !token.is_empty()
                        && let Some(g) = current.as_mut()
                    {
                        g.agents.push(token);
                    }
                }
                Key::Allow | Key::Disallow => {
                    in_agents = false;
                    let Some(g) = current.as_mut() else { continue };
                    let pattern = normalize(value);
                    if !pattern.is_empty() {
                        g.rules.push(Rule {
                            pattern,
                            allow: key == Key::Allow,
                        });
                    }
                }
                Key::CrawlDelay => {
                    in_agents = false;
                    let Some(g) = current.as_mut() else { continue };
                    if g.crawl_delay.is_none()
                        && let Ok(d) = value.parse::<f64>()
                        && d.is_finite()
                        && d >= 0.0
                    {
                        g.crawl_delay = Some(d);
                    }
                }
                Key::Sitemap => {
                    if !value.is_empty() {
                        out.sitemaps.push(value.to_string());
                    }
                }
                Key::Other => {}
            }
        }
        if let Some(g) = current {
            out.groups.push(g);
        }
        out
    }

    /// `Sitemap:` values in file order (unvalidated).
    pub fn sitemaps(&self) -> &[String] {
        &self.sitemaps
    }

    /// Whether some group names `agent` explicitly (not via `*`).
    pub fn names_agent(&self, agent: &str) -> bool {
        let a = agent.to_ascii_lowercase();
        !a.is_empty() && self.groups.iter().any(|g| g.agents.contains(&a))
    }

    /// The rules for `agent`: every group naming it, else every `*` group, else none.
    pub fn rules_for(&self, agent: &str) -> AgentRules {
        let explicit = self.names_agent(agent);
        let want = if explicit {
            agent.to_ascii_lowercase()
        } else {
            "*".to_string()
        };
        let mut out = AgentRules {
            explicit,
            ..AgentRules::default()
        };
        for g in self.groups.iter().filter(|g| g.agents.contains(&want)) {
            out.rules.extend(g.rules.iter().cloned());
            if out.crawl_delay.is_none() {
                out.crawl_delay = g.crawl_delay;
            }
        }
        out
    }
}

impl AgentRules {
    /// Whether `path_and_query` (starting with `/`) may be fetched.
    pub fn allowed(&self, path_and_query: &str) -> bool {
        let path = normalize(path_and_query);
        let path = if path.is_empty() {
            "/".to_string()
        } else {
            path
        };
        if path == "/robots.txt" {
            return true;
        }
        let mut best: Option<(usize, bool)> = None;
        for r in &self.rules {
            if !matches(r.pattern.as_bytes(), path.as_bytes()) {
                continue;
            }
            let len = r.pattern.len();
            best = match best {
                Some((l, a)) if l > len || (l == len && a) => Some((l, a)),
                _ => Some((len, r.allow)),
            };
        }
        best.is_none_or(|(_, allow)| allow)
    }

    /// The crawl delay as a [`Duration`], capped at [`MAX_CRAWL_DELAY`].
    pub fn crawl_delay(&self) -> Option<Duration> {
        self.crawl_delay.map(|s| {
            Duration::try_from_secs_f64(s).map_or(MAX_CRAWL_DELAY, |d| d.min(MAX_CRAWL_DELAY))
        })
    }

    /// Whether the whole site (`/`) is disallowed for this agent.
    pub fn blocks_root(&self) -> bool {
        !self.allowed("/")
    }
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

/// Normalises a pattern or a path for matching: non-ASCII, control, space and
/// `" < > \` { }` bytes are percent-encoded; `%XX` escapes of unreserved characters are
/// decoded and other escapes upper-cased. `*` and `$` are kept. Idempotent.
pub fn normalize(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let b = s.as_bytes();
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            let v = h * 16 + l;
            if unreserved(v) {
                out.push(v as char);
            } else {
                out.push('%');
                out.push(HEX[usize::from(h)] as char);
                out.push(HEX[usize::from(l)] as char);
            }
            i += 3;
            continue;
        }
        if c <= 0x20 || c >= 0x7F || matches!(c, b'"' | b'<' | b'>' | b'`' | b'{' | b'}') {
            out.push('%');
            out.push(HEX[usize::from(c >> 4)] as char);
            out.push(HEX[usize::from(c & 0xF)] as char);
        } else {
            out.push(c as char);
        }
        i += 1;
    }
    out
}

/// Whether `pattern` (with `*` wildcards and an optional trailing `$`) matches a prefix of
/// `path` (or all of it, with `$`). Linear-space, O(n·m) worst case, no recursion.
pub fn matches(pattern: &[u8], path: &[u8]) -> bool {
    let (pattern, anchored) = match pattern.split_last() {
        Some((b'$', rest)) => (rest, true),
        _ => (pattern, false),
    };
    // Classic greedy wildcard matching with one backtrack point.
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    loop {
        if p == pattern.len() {
            if !anchored || t == path.len() {
                return true;
            }
        } else if pattern[p] == b'*' {
            star = Some((p, t));
            p += 1;
            continue;
        } else if t < path.len() && pattern[p] == path[t] {
            p += 1;
            t += 1;
            continue;
        }
        // Mismatch (or anchored pattern ended early): backtrack to the last `*`.
        match star {
            Some((sp, st)) if st < path.len() => {
                star = Some((sp, st + 1));
                p = sp + 1;
                t = st + 1;
            }
            _ => return false,
        }
    }
}

/// Convenience for tests and the fuzz target: is `path_and_query` allowed for `agent`?
pub fn is_allowed(body: &[u8], agent: &str, path_and_query: &str) -> bool {
    ParsedRobots::parse(body)
        .rules_for(agent)
        .allowed(path_and_query)
}
