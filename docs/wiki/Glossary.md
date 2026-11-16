# Glossary

Short definitions of the words used across this wiki, in alphabetical order.

| Term | Meaning |
|---|---|
| **ADC** | Application Default Credentials: how code on a GCP VM gets a token for its service account without a key file. |
| **AI reservation** | A site's signal that its content must not be used by AI: a TDM reservation, `Content-Signal: ai-input=no`, or blocking the major AI crawlers in robots.txt. We keep only titles and links. |
| **api_terms** | Licence class for data used under a provider's API terms (Ticketmaster, Nager.Date). |
| **At-least-once** | A work item may be delivered more than once; handlers are idempotent so repeats are harmless. |
| **Backpressure** | Fetchers pause when the spool is too big or uploads keep failing. |
| **Batch prediction** | Vertex AI's offline mode: many requests in one job, at half price, results within hours. |
| **Canonical URL** | A URL in one standard form (lowercase host, no fragment, no tracking parameters, sorted query), so the same page is not crawled twice. |
| **Capture** | One stored fetch of one URL at one time, with its metadata. Lives in a segment. |
| **CDP** | Chrome DevTools Protocol, used to drive headless Chromium. |
| **CDXJ** | A text index of a WARC file: one line per capture with its URL, time and location. |
| **Challenge** | A page a bot-protection service shows to check if you are human (CAPTCHA, interstitial). We stop and park. |
| **Change feed** | `/v1/changes`: an ordered list of every change, so clients can keep a copy in sync. |
| **Claim** | The right to crawl one politeness key for a short time, with an epoch and a deadline. |
| **ClaimPermit** | The in-memory token a claim gives. No permit, no request. Only `vc-frontier` can create one. |
| **Cadence** | How often a source is expected to change (hourly … yearly, or on demand). |
| **Contract migration** | A migration that removes or renames something; runs only later, separately, after the new image proved itself. |
| **cosign** | A tool that signs container images. "Keyless" means it uses GitHub's identity instead of a stored key. |
| **Crawl-delay** | A robots.txt line asking bots to wait this many seconds between requests. |
| **Dead letter** | A work item that failed too often (8 attempts) and is set aside. |
| **Deadline** | The time a claim expires, checked against the database clock. |
| **Denylist** | `policy/denylist.yaml`: hosts we never request. |
| **Dump** | A large downloadable file of a whole site or dataset (for example Wikivoyage). We stream and filter it. |
| **Entity** | The cross-source view of one real thing, built from several records. |
| **Epoch** | An ever-increasing number given to each claim. Writes from an older epoch are rejected ("fencing"). |
| **EWMA** | Exponentially weighted moving average: a running average of a site's response time. |
| **Expand-only migration** | A migration that only adds things, so the previous image still works. |
| **facts_only** | Licence class for editorial content (blogs, news): facts, headline, short excerpt and link only. |
| **Failpoint** | A named place in the code where a test can inject a crash or error. |
| **Fencing** | Rejecting writes from a holder whose lease is no longer current. |
| **Frontier** | The crawler's to-do list of URLs and the per-site politeness state, in Postgres. |
| **fsync** | Forcing written data from memory onto the disk, so a crash cannot lose it. |
| **GeoNames** | An open database of place names with ids; used for `place_ids`. CC BY 4.0. |
| **Grounding check** | Verifying that extracted values really appear in the page text; ungrounded values are dropped. |
| **halfvec** | pgvector's half-precision vector type; half the size of normal vectors. |
| **High-stakes kind** | `visa_rule`, `advisory`, `emergency_numbers`: wrong data could harm a traveller, so only official sources count. |
| **HNSW** | A fast approximate nearest-neighbour index used for vector search. |
| **Idempotent** | Doing it twice has the same effect as doing it once. |
| **Kind** | A type of record (event, attraction, visa_rule, …), defined by a schema, a prompt and a policy entry. |
| **Lease** | Temporary ownership of a URL or work item, which expires if not renewed. |
| **Ledger** | The table of LLM reservations and spend that keeps calls within the caps. |
| **Licence class** | `open`, `share_alike`, `facts_only`, `api_terms` or `restricted`; decides how data may be used. |
| **Observation** | A point in a time series (FX rate, weather normal), not a fact record. |
| **Oracle** | A test checker that judges the outcome independently of the code under test. |
| **Origin** | scheme + host + port. Owns robots.txt. |
| **Park** | Stop crawling a key for a while (24 h doubling to 30 days) after a block. |
| **Payload hash** | sha256 of the decoded body; equal hashes mean "unchanged". |
| **pgvector** | A Postgres extension for storing and searching vectors. |
| **Politeness key** | The unit of politeness, usually the registrable domain. Owns delay, budget and parking. |
| **Provenance** | Where a value came from: source, URL, capture, extractor, model, prompt. |
| **PSL** | Public Suffix List: tells which part of a host is the registrable domain (`example.co.uk`). |
| **Raw first** | Nothing derived from a capture is visible before the capture is safely stored. |
| **Record** | One item found at one URL (an event, a visa rule), with versions. |
| **Record key** | A stable id for a record within a page, so re-crawls update it instead of duplicating it. |
| **restricted** | Licence class for non-commercial or unknown terms; never ingested. |
| **Revisit** | A short WARC record saying "same as before" instead of storing the body again. |
| **robots.txt** | A file at a site's root telling bots what they may crawl (RFC 9309). |
| **RRF** | Reciprocal rank fusion: a simple way to combine two rankings (text and vector). |
| **Segment** | One compressed WARC file in GCS holding many captures (up to 128 MiB). |
| **share_alike** | Licence class for CC BY-SA and ODbL data; never merged with other licences. |
| **Shard** | A number on each politeness key; fetchers claim from their shards. For scaling out. |
| **Simhash** | A 64-bit text fingerprint where similar texts get similar values. |
| **SKIP LOCKED** | Postgres option that lets many workers take different rows without waiting on each other. |
| **Source class** | Who publishes a source: `official`, `reference`, `editorial`, `community`, `commercial_api`. |
| **Spool** | A local, fsynced file where captures wait before they are packed into a segment. |
| **SSRF** | Server-side request forgery: tricking a server into calling internal addresses. |
| **SURT** | A URL form with the host reversed (`com,example)/path`), used to sort CDXJ lines. |
| **TDM reservation** | A site's legal opt-out from text and data mining (header or TDMRep file). |
| **Testcontainers** | Starting real services (here Postgres) in Docker for tests. |
| **Tombstone** | A marker that data was removed (opt-out, purge, robots); it is never served again. |
| **ULID** | A sortable unique id; segments and captures use them. |
| **UsePolicy** | What we may do with a capture (store, index, LLM, embed, serve), derived from opt-out signals. |
| **Validators** | ETag and Last-Modified, used for conditional requests. |
| **Verbatim field** | A field (phone, URL, price, date) that must appear exactly in the source text. |
| **WAL / wal-g** | Postgres's write-ahead log, and the tool that archives it to GCS for backups. |
| **WARC** | Web ARChive format (ISO 28500), the standard file format for stored web traffic. |
| **xid8** | A Postgres transaction id; used to order the change feed without gaps. |
