# Glossary

Short definitions of the words used across this wiki, in alphabetical order.

| Term | Meaning |
|---|---|
| **Absence condition** | An alert rule that fires when a metric stops arriving, so a silent job (no backups, no drill) is noticed, not only a bad value. |
| **ADC** | Application Default Credentials: how code on a GCP VM gets a token for its service account without a key file. |
| **AI reservation** | A site's signal that its content must not be used by AI: a TDM reservation, `Content-Signal: ai-input=no`, or blocking the major AI crawlers in robots.txt. We keep only titles and links. |
| **api_terms** | Licence class for data used under a provider's API terms (Ticketmaster, Nager.Date). |
| **At-least-once** | A work item may be delivered more than once; handlers are idempotent so repeats are harmless. |
| **Backpressure** | Fetchers pause when the spool is too big or uploads keep failing. |
| **Batch prediction** | Vertex AI's offline mode: many requests in one job, at half price, results within hours. |
| **Borrow** | A render sub-request to an allowlisted site of another politeness key (a CDN) takes that key for one request, only if its delay has passed, it is not parked and no claim holds it. |
| **Breaker** | The LLM spend breaker: one row in Postgres. A billing or budget error trips it, and every role stops calling Gemini until it closes, even after a restart. |
| **Cadence** | How often a source is expected to change (hourly … yearly, or on demand). |
| **Canonical URL** | A URL in one standard form (lowercase host, no fragment, no tracking parameters, sorted query), so the same page is not crawled twice. |
| **Capture** | One stored fetch of one URL at one time, with its metadata. Lives in a segment. |
| **Carbon copy** | Deploy promotion copies the signed image manifest to the `:prod` tag byte for byte, so the signature the VM checks still matches. |
| **CDP** | Chrome DevTools Protocol, used to drive headless Chromium. |
| **CDXJ** | A text index of a WARC file: one line per capture with its URL, time and location. |
| **Challenge** | A page a bot-protection service shows to check if you are human (CAPTCHA, interstitial). We stop and park. |
| **Change feed** | `/v1/changes`: an ordered list of every change, so clients can keep a copy in sync. |
| **Child target** | A part of a rendered page that Chromium runs separately: a worker or an out-of-process iframe (**OOPIF**). Our CDP library cannot intercept its requests; see [Security](Security) for how they are contained. |
| **Claim** | The right to crawl one politeness key for a short time, with an epoch and a deadline. |
| **ClaimPermit** | The in-memory token a claim gives. No permit, no request. Only `vc-frontier` can create one. |
| **Contract migration** | A migration that removes or renames something; runs only later, separately, after the new image proved itself. |
| **cosign** | A tool that signs container images. "Keyless" means it uses GitHub's identity instead of a stored key. |
| **Crawl-delay** | A robots.txt line asking bots to wait this many seconds between requests. |
| **Dead letter** | A work item that failed too often (8 attempts) and is set aside. |
| **Deadline** | The time a claim expires, checked against the database clock. |
| **Denylist** | `policy/denylist.yaml`: hosts we never request. |
| **Deploy drift** | The VM runs a different image than `:prod` points to. `deploy.sh` records when that started; an alert fires after 30 minutes. |
| **Dump** | A large downloadable file of a whole site or dataset (for example Wikivoyage). We stream and filter it. |
| **Entity** | The cross-source view of one real thing, built from several records. |
| **Epoch** | An ever-increasing number given to each claim. Writes from an older epoch are rejected ("fencing"). |
| **EWMA** | Exponentially weighted moving average: a running average of a site's response time. |
| **Expand-only migration** | A migration that only adds things, so the previous image still works. |
| **facts_only** | Licence class for editorial content (blogs, news): facts, headline, short excerpt and link only. |
| **Failpoint** | A named place in the code where a test can inject a crash or error. |
| **Fencing** | Rejecting writes from a holder whose lease is no longer current. |
| **fetched_pending** | URL state between "record fsynced to the spool" and "segment committed". The URL is not leased again until the commit, spool adoption or the 6-hour sweeper returns it to `due`. |
| **forced_off** | `llm.forced_off`: the LLM was switched off at startup because the `[llm]` settings are invalid (for example the cap is too high for the project cap). The crawler runs on and alerts instead of failing. |
| **Frontier** | The crawler's to-do list of URLs and the per-site politeness state, in Postgres. |
| **fsync** | Forcing written data from memory onto the disk, so a crash cannot lose it. |
| **Fuzzing** | Feeding random inputs to parsers to find crashes. `cargo-fuzz` runs nightly on vc-policy, vc-parse and vc-store. |
| **GeoNames** | An open database of place names with ids; used for `place_ids`. CC BY 4.0. |
| **Grounding check** | Verifying that extracted values really appear in the page text; ungrounded values are dropped. |
| **halfvec** | pgvector's half-precision vector type; half the size of normal vectors. |
| **Heartbeat** | A row in the `heartbeats` table that a role, the deployer or a systemd unit writes to say "still alive" or "last run ok". Health checks and alerts read them. |
| **Held digest** | An image digest the VM deployer refused (bad signature, schema too new, failed health check). It is never tried again; the next good `:prod` digest is. |
| **Held record** | A high-stakes record from a reference source kept in `held_records` until a second independent source agrees. Shared by every processor, bounded, expires after 7 days. |
| **High-stakes kind** | `visa_rule`, `advisory`, `emergency_numbers`: wrong data could harm a traveller, so only official sources count. |
| **HNSW** | A fast approximate nearest-neighbour index used for vector search. |
| **Idempotent** | Doing it twice has the same effect as doing it once. |
| **Kind** | A type of record (event, attraction, visa_rule, …), defined by a schema, a prompt and a policy entry. |
| **Lease** | Temporary ownership of a URL or work item, which expires if not renewed. |
| **Ledger** | The table of LLM reservations and spend that keeps calls within the caps. |
| **Licence class** | `open`, `share_alike`, `facts_only`, `api_terms` or `restricted`; decides how data may be used. |
| **Link triage** | Gemini deciding whether an uncertain link is worth crawling; verdicts are cached per URL pattern. See [Agentic Loops](Agentic-Loops). |
| **Marker nonce** | A random-looking tag around page text in an LLM prompt, marking where untrusted text starts and ends. It is a hash of that text, so an unchanged page builds the same request and hits the cache. |
| **Min-schema** | The lowest database schema an image works with (image label `io.voyager-crawler.min-schema`). Superseded as the deploy gate by the `io.voyager-crawler.migrations` list: `deploy.sh` refuses an image when any migration it ships is missing from the database. |
| **N-1 rule** | A new migration must keep the previous image (N-1) working. A test replays that image's recorded queries against the new schema. |
| **Observation** | A point in a time series (FX rate, weather normal), not a fact record. |
| **Ops Agent** | Google's VM agent; here it ships host metrics and the containers' logs to Cloud Monitoring and Logging. |
| **Oracle** | A test checker that judges the outcome independently of the code under test. |
| **Origin** | scheme + host + port. Owns robots.txt. |
| **otelcol** | The OpenTelemetry Collector container (OpenTelemetry is an open standard for metrics and traces). It scrapes every role's `/metrics` and writes `custom.googleapis.com/crawler/*` metrics to Cloud Monitoring. |
| **Park** | Stop crawling a key for a while (24 h doubling to 30 days) after a block. |
| **Payload hash** | sha256 of the decoded body; equal hashes mean "unchanged". |
| **pgvector** | A Postgres extension for storing and searching vectors. |
| **Politeness key** | The unit of politeness, usually the registrable domain. Owns delay, budget and parking. |
| **Probe pause** | If the LLM startup probe fails for any reason but a missing model, the LLM pauses (10 minutes, or until the breaker closes) instead of crash-looping the role. |
| **Provenance** | Where a value came from: source, URL, capture, extractor, model, prompt. |
| **PSL** | Public Suffix List: tells which part of a host is the registrable domain (`example.co.uk`). |
| **Publisher** | The registrable domain behind a source. Two sources of one publisher count as one when we look for independent agreement. |
| **Quarantine** | A sealed spool file that fails 3 times on its own is renamed `*.quarantined`, so newer files still publish; an alert counts them. |
| **Raw first** | Nothing derived from a capture is visible before the capture is safely stored. |
| **Raw-bytes cap** | A daily limit on raw bytes fetched across all fetchers (3 GiB by default), kept in Postgres. |
| **Record** | One item found at one URL (an event, a visa rule), with versions. |
| **Record key** | A stable id for a record within a page, so re-crawls update it instead of duplicating it. |
| **Restore drill** | A check that backups can be restored. Weekly, maintenance rebuilds the newest segments' rows from their CDXJ and compares them with the database. |
| **restricted** | Licence class for non-commercial or unknown terms; never ingested. |
| **Resume cursor** | The saved position of an API or dump adapter in a paged stream (table `adapter_cursors`), so the next run resumes instead of starting over. |
| **Retry-After** | An HTTP header in a 429 or 503 response saying how long to wait. We wait at least that long, and repeats double the wait up to 24 hours. |
| **Revisit** | A short WARC record saying "same as before" instead of storing the body again. |
| **Revisit chain** | A `revisit` that points at another `revisit` (several 304s in a row). The processor follows the chain, at most 64 hops, back to the stored body. |
| **robots.txt** | A file at a site's root telling bots what they may crawl (RFC 9309). |
| **RRF** | Reciprocal rank fusion: a simple way to combine two rankings (text and vector). |
| **Segment** | One compressed WARC file in GCS holding many captures (up to 128 MiB). |
| **Settle** | The last step of an LLM reservation: record the real cost and release the rest. An hourly job settles reservations left open by a crash at their estimate. |
| **Shard** | A number on each politeness key; fetchers claim from their shards. For scaling out. |
| **share_alike** | Licence class for CC BY-SA and ODbL data; never merged with other licences. |
| **Simhash** | A 64-bit text fingerprint where similar texts get similar values. |
| **SKIP LOCKED** | Postgres option that lets many workers take different rows without waiting on each other. |
| **Source class** | Who publishes a source: `official`, `reference`, `editorial`, `community`, `commercial_api`. |
| **Spool** | A local, fsynced file where captures wait before they are packed into a segment. |
| **Spool adoption** | A fetcher takes over the spool directory of a dead fetcher (its lock is free), publishes the files and removes the directory. |
| **SSRF** | Server-side request forgery: tricking a server into calling internal addresses. |
| **Stale record** | A record that a newer capture of its page no longer contains. It is marked stale, never deleted, and becomes active again if it comes back. |
| **Store garbage** | The `store_garbage` table: GCS objects left over by a segment rewrite (purge or expiry). They are deleted once no live segment names them, so a crash cannot resurrect purged bytes. |
| **Sub-resource gap** | The minimum wait between two sub-requests to the same host during one render (`render.subresource_gap_ms`, 250 ms). |
| **SURT** | A URL form with the host reversed (`com,example)/path`), used to sort CDXJ lines. |
| **Sweeper** | A fetcher job that, every 10 minutes, returns URLs stuck in `fetched_pending` for more than 6 hours to `due`. |
| **TDM reservation** | A site's legal opt-out from text and data mining (header or TDMRep file). |
| **Testcontainers** | Starting real services (here Postgres) in Docker for tests. |
| **Text tier** | One compressed JSON-lines object per committed segment in GCS (`text/…`) with each page's title and main text. Used by the auditor and for re-extraction. |
| **Tombstone** | A marker that data was removed (opt-out, purge, robots); it is never served again. |
| **ULID** | A sortable unique id; segments and captures use them. |
| **UsePolicy** | What we may do with a capture (store, index, LLM, embed, serve), derived from opt-out signals. |
| **Validators** | ETag and Last-Modified, used for conditional requests. |
| **Verbatim field** | A field (phone, URL, price, date) that must appear exactly in the source text. |
| **WAL / wal-g** | Postgres's write-ahead log, and the tool that archives it to GCS for backups. |
| **WARC** | Web ARChive format (ISO 28500), the standard file format for stored web traffic. |
| **xid8** | A Postgres transaction id; used to order the change feed without gaps. |
