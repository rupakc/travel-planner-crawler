# FAQ

## For site owners

**What is VoyagerBot?**
The crawler of Voyager, an open-source AI travel planner. It collects public travel
information so Voyager can cite current, attributed facts. Details:
[BOT.md](https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md).

**How do I block it?**
Add this to robots.txt. It takes effect within 24 hours of our next robots.txt fetch.

```
User-agent: VoyagerBot
Disallow: /
```

**Can I allow crawling but keep my text away from AI?**
Yes. Send `Content-Signal: ai-input=no`, use a TDM reservation, or block the major AI
crawlers (GPTBot, ClaudeBot, CCBot, Google-Extended) while allowing others. We then keep only
titles and links; your text is never sent to a language model or embedded. We never train
models.

**How fast is data removed after an opt-out?**
From the API and index within 24 hours, from raw storage within 7 days, from backups and
snapshots within 21 days.

**How often will it hit my site?**
At most one request at a time, at least 5 seconds apart (longer if you set Crawl-delay or
your site is slow), and at most 3,000 requests a day. A 429 or 503 slows it down; a 401, 403
or challenge stops it for at least 24 hours.

**Does it get past Cloudflare or CAPTCHAs?**
No, never. A challenge means we stop. We never retry with a browser, another identity, a
proxy or a CAPTCHA solver.

**Does it run JavaScript on my site?**
Only for a few JavaScript-only pages, in a sandboxed Chromium with no network of its own.
Every request it makes goes through the same robots, rate and identity rules. Images, fonts,
media, trackers, iframes and other documents are not loaded. A script or style from another
site (a CDN on our allowlist) is fetched only when that site's own delay allows it.

**Do you honour Retry-After?**
Yes. A 429 or 503 stops the visit. We wait at least as long as `Retry-After` says, and each
repeat doubles the wait, up to 24 hours.

**Does it republish my articles?**
No. Blog and news content is served only as facts, a headline, an excerpt of at most 300
characters and a link back to you.

## For developers

**Can I run it today?**
Yes, locally. Every `vcrawl` role is wired. `bash e2e/run.sh` starts the whole crawler
against a scripted fake internet in Docker and checks it (the same run as CI). `cargo test
--workspace` runs the unit and integration tests (needs Docker). The production VM is not
live yet. See [Testing](Testing).

**What does it cost to run?**
About $74 a month with the LLM off, and at most about $104 with the LLM at its full $30
cap, on demand. A 1-year VM commitment lowers both. See [Cost](Cost).

**Why Rust and not an existing crawler framework?**
The obvious Rust option, `spider`, has bot-evasion features on by default, which conflicts
with our rules. Our own core keeps politeness in one database-backed gate.

**Why Postgres for the frontier and the queue?**
Politeness must hold across processes. One database gives claims with fencing, a global rate
bucket and fair claiming with `SKIP LOCKED`, at no extra cost. Traits keep Pub/Sub or another
store possible later.

**Why WARC segments instead of one file per page?**
Cost and replay. One object per page would cost about $125 a month in GCS operations.
Segments are a standard archive format and can be re-processed at any time.

**Is the LLM required?**
No. It is off by default. Structured data (JSON-LD, APIs, dumps) needs no LLM. Gemini adds
extraction from free text when it is on and the budget allows.

**What happens if the crawler is down?**
Voyager treats the crawler as optional. Any error or timeout means "carry on as before".

**How do I add a source?**
Add one YAML file under `sources/`. See [Sources and Licences](Sources-and-Licences).

**Where are decisions recorded?**
`MEMORY.md` (one dated line per decision) and the design spec in
`docs/superpowers/specs/`, where §17 overrides earlier sections.
