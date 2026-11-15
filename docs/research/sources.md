# Legitimate travel-data sources for a polite crawler (research, 2026-11-15)

Scope: sources for every agent category of the travel planner (flights, weather, hotels, activities, places-to-see, free walking tours, events, visa/entry, SIM/eSIM, tips, emergency card, getting-around, forex, packing) plus blogs, forums and news. The crawler obeys robots.txt, sends an honest bot User-Agent, never evades bot detection.

## How this was checked

- **robots.txt** for 40+ sites was fetched on 2026-11-15 with the honest UA `VoyagerResearchBot/0.1 (+https://github.com/rupakc/travel-planner)`. Raw copies are in `research/robots/`.
- **RSS URLs**: every feed in section 18 was fetched with the same UA and confirmed to return an RSS/Atom document (HTTP 200, `<rss`/`<feed`/`<?xml`). The two feeds marked "slow" answered with XML but timed out on some attempts.
- **APIs**: open endpoints (FCDO Content API, State Dept CADATA, Canada travel JSON, Open-Meteo, ECB, Frankfurter, MET Norway, NWS, Nager.Date, Common Crawl index, Stack Exchange API) were called and answered 200.
- **ToS quotes**: most come from secondary monitoring sites (ConductAtlas) or archived pages, not the live ToS text. They are marked "(secondary)". Re-read the live ToS before launch. Legal sign-off is outside the scope of this research.

**Crawler policy findings that apply everywhere:**
1. A 403/Cloudflare challenge on `robots.txt` or on a feed means the site blocks honest bots. Treat it as **do not crawl**. Do not retry with a browser UA. (RFC 9309 says a 4xx robots.txt means "allow all", but a bot wall is a clear signal of intent.) Sites that did this: GetYourGuide, Viator, FlyerTalk, Fodor's, Atlas Obscura (site; its RSS works), Travel + Leisure, TravelPulse, Travel Weekly, Frommer's, PhocusWire, Travelfish (site 403; robots.txt itself 200), Wikitravel, VisitBritain, Smartraveller (timed out).
2. Honour `Crawl-delay` (Wikivoyage 5 s, CDC 20 s) and `Content-Signal` lines. `iatatravelcentre.com` sends `ai-train=no, use=reference`, and Stack Exchange sends `search=no, ai-train=no`. If we feed text to an LLM at query time, keep it as "reference", link back, and never use it for training.
3. Prefer dumps and APIs over HTML everywhere. HTML crawling is a last resort, only for robots-allowed pages, at ≤1 req/5 s per host. Store only facts and links, never republish full text.

---

## 0. Must NOT crawl (ToS and/or robots.txt)

| Site | robots.txt (honest UA, 2026-11-15) | ToS stance | Legit alternative |
|---|---|---|---|
| **Booking.com** | 200. `User-agent: *` disallows search/fragment/booking paths (`/searchresults` is not explicitly listed, but `/hotel/*` pages are partially disallowed) | ToS bars automated access "including scraping, crawling, copying and downloading", with or without commercial intent, without written permission. Booking may block it and claims injunctive relief (secondary: ConductAtlas) | Booking.com Demand API / Affiliate Partner Programme (approval needed) |
| **Expedia / Hotels.com / Travelocity** | 200. Disallows `/Hotel-Search`, `/Flights-Search`, `/Flight-SearchResults`, `/things-to-do/search?`, checkout, etc. | Expedia.ca ToS (rev. 2026-01-01) bars using "any robot, spider, scraper or other automated means" to access, monitor or copy content without prior express written permission. Travelocity also bars "violating robot exclusion headers" | Expedia Group Rapid / partner APIs (contract) |
| **Tripadvisor** | 200. `ClaudeBot`, `GPTBot`, `CCBot`, `Google-CloudVertexBot` and others are `Disallow: /`. `*` disallows `/api`, `/AllReviews` and many app paths | Booking ToU and Content API Master Terms prohibit bots, spiders, crawlers and scrapers unless Tripadvisor expressly approves them (official booking ToU + secondary) | Tripadvisor Content API (key; display-only, attribution and caching limits; check current quota) |
| **Skyscanner** | 200. `*` disallows `/prices-calendar`, `/dataservices/*`, `/g/*`, deeplinks and `*.rss*` | Archived ToS (Feb 2020): users "may not use any unauthorized automated computer program, software agent, bot, spider … to scan, copy, index, sort or otherwise exploit our Services"; a breach is "a material breach" | Skyscanner Partners API (B2B approval only) |
| **Google Flights / Google Travel / Hotels** | 200. Disallows `/travel/flights/search`, `/travel/search`, `/travel/entity`, `/hotels/rpc` and others | Google ToS bars "using automated means to access content from any of our services in violation of the machine-readable instructions on our web pages (for example, robots.txt files…)" | None official (QPX shut 2018). The app's current SerpAPI use is a third-party paid service: keep it as a separate, documented legal decision |
| **Airbnb** | 200. `*` disallows `/rooms/*/reviews`, `/rooms/*/location`, `/book/`, `/calendar/` and others | "Do not use bots, crawlers, scrapers, or other automated means to access or collect data or other content from or otherwise interact with the Airbnb Platform" (secondary: ConductAtlas, ToS rev. 2026) | None public. Inside Airbnb (insideairbnb.com, CC BY 4.0) publishes snapshot datasets for ~100 cities and can be used for neighbourhood-level context only |
| **GetYourGuide** | **403** (GYG error page; robots.txt blocked for an honest bot) | Site blocks bots; ToS text not retrieved | GetYourGuide Partner API (affiliate approval) |
| **Viator** (Tripadvisor company) | **403**, empty body | Tripadvisor-family terms | Viator Partner API (free for approved affiliates; basic access tier) |
| **Reddit** | 200: `User-agent: * / Disallow: /`. Header points to the "Public Content Policy" | Data API: free tier is **non-commercial only**, 100 queries/min per OAuth client (averaged over 10 min). Use "by a business … or as part of a monetized product or service" needs a commercial agreement (reported ~$12k/month, unverified). Secondary blogs claim new public API access closes 2026-10-31 (unverified) | No crawling. Data API only if we get explicit non-commercial approval, and never feed Reddit content to LLM training |
| **Stack Exchange (site HTML)** | 200: `User-agent: *` + `Content-signal: search=no, ai-train=no` + `Disallow: /` | Content is CC BY-SA, but the site may not be crawled | Data dump (CC BY-SA, community mirror) or the Stack Exchange API (see §17) |
| **Lonely Planet Thorn Tree** | `Disallow: /thorntree/*` | Forum **closed and removed** (read-only 2020, shut Sept 2021) | Nothing to crawl. LP articles: sitemaps exist, but there is no RSS, and ToS reuse is not granted (link-only) |
| **FlyerTalk**, **Fodor's forums**, **Travelfish board** | 403 to honest UA (FlyerTalk and Fodor's robots.txt; Travelfish feeds/site) | Bot-walled | Do not crawl |
| **Wikitravel** | 403 | Bot-walled | Use Wikivoyage (its CC BY-SA fork) |
| **Eventbrite (search pages, RSS)** | `*` disallows `/rss/`, `/events/rss/`, `/atom/` | The public Event Search API was shut down on 2019-12-12 | Eventbrite API only for known organisers/venues/event IDs |
| **Numbeo** | 200, only `/heavy_crawling.any` disallowed | ToS restricts automated extraction (paid API for data) | Numbeo API (paid) or skip |
| **IATA Timatic / timaticweb2** | Unreachable. iatatravelcentre.com robots allows `/` with `Content-Signal: search=yes, ai-train=no, use=reference` | Timatic is a commercial licence product | Official government sources (§8) |

---

## 1. Flights

| # | Source | URL | Method | Licence / ToS | Upstream freq | Recrawl |
|---|---|---|---|---|---|---|
| 1 | OurAirports | https://davidmegginson.github.io/ourairports-data/airports.csv (+ runways, frequencies, countries) | dump (CSV) | Public domain ("released to the public domain") | nightly | weekly (already the app's seed) |
| 2 | Wikidata (airlines, airports, IATA/ICAO codes, hubs) | https://dumps.wikimedia.org/wikidatawiki/entities/ and https://query.wikidata.org/ | dump / SPARQL API | CC0 | weekly dumps | monthly |
| 3 | Wikipedia airport pages, "Airlines and destinations" tables | https://dumps.wikimedia.org/enwiki/ | dump | CC BY-SA 4.0 (attribution + share-alike) | twice monthly | monthly. A good free route-network source (which airline flies where) |
| 4 | US DOT BTS T-100 / DB1B (routes, fares sample) | https://www.transtats.bts.gov/ | dump (CSV) | US public domain | monthly/quarterly | quarterly. US-only fare and route context |
| 5 | OpenSky Network | https://opensky-network.org/data/api | API (free account) | Non-commercial/research terms; attribution | live | on demand only (flight status), not prices |
| 6 | Duffel API | https://duffel.com/docs | API (key) | Commercial API; offer search free, revenue per booking (verify current pricing) | live | on demand, cache ≤30 min |
| 7 | Travelpayouts / Aviasales Data API | https://support.travelpayouts.com/ | API (affiliate token) | Affiliate ToS; cached historical cheapest fares | daily | daily per watched route |
| 8 | OpenFlights routes | https://openflights.org/data | dump | ODbL | **stale (2014)**; use only as fallback | never re-crawl |

Note: **Amadeus Self-Service was shut down on 2026-07-17**, so there is no free tier any more and Enterprise access is contract-only. Kiwi Tequila is closed to new public sign-ups (verify). Live fares legitimately come only from a contracted API (Duffel, Amadeus Enterprise, Travelpayouts cache) or the existing SerpAPI arrangement.

## 2. Weather (forecast + climate for packing)

| # | Source | URL | Method | Licence / ToS | Freq | Recrawl |
|---|---|---|---|---|---|---|
| 1 | Open-Meteo forecast + climate + historical | https://api.open-meteo.com/v1/forecast , https://climate-api.open-meteo.com , https://archive-api.open-meteo.com | API (no key) | Data CC BY 4.0. **Free API is non-commercial only**: <10 000 calls/day, 5 000/h, 600/min. Commercial use needs a subscription or self-hosting (AGPLv3 server) | hourly | forecast: cache 1–3 h. Climate normals: once per city |
| 2 | MET Norway Locationforecast | https://api.met.no/weatherapi/locationforecast/2.0/ | API (no key; **must send an identifying UA**) | CC BY 4.0 / NLOD; honour `Expires`/`If-Modified-Since` | hourly | on demand, honour headers |
| 3 | US NWS API | https://api.weather.gov | API (no key; UA with contact required) | US public domain | hourly | on demand (US destinations) |
| 4 | ECMWF Open Data (IFS/AIFS) | https://www.ecmwf.int/en/forecasts/datasets/open-data | dump (GRIB2) | CC BY 4.0 | 4×/day | only if self-hosting |
| 5 | NOAA GHCN-Daily / climate normals | https://www.ncei.noaa.gov/products/land-based-station/global-historical-climatology-network-daily | dump | US public domain | daily | yearly normals build |
| 6 | Meteostat | https://dev.meteostat.net/ | dump/API | **CC BY-NC 4.0** (non-commercial) | daily | yearly. Use only if the app stays non-commercial |
| 7 | Wikivoyage "Climate" boxes | dump (see §4) | dump | CC BY-SA | twice monthly | monthly |

## 3. Hotels

| # | Source | URL | Method | Licence | Freq | Recrawl |
|---|---|---|---|---|---|---|
| 1 | OpenStreetMap `tourism=hotel|hostel|guest_house` | https://download.geofabrik.de/ (extracts), https://overpass-api.de/api/interpreter | dump / API | **ODbL 1.0**: "© OpenStreetMap contributors", share-alike on derived databases | minutely (diffs) / daily extracts | monthly per country extract. Overpass is shared infrastructure (it returned "at capacity" 502 today): keep queries small, ≤1 concurrent, or self-host |
| 2 | Overture Maps Places | https://docs.overturemaps.org/ (S3/Azure GeoParquet) | dump | CDLA-Permissive-2.0 (Places theme) | monthly | monthly |
| 3 | Foursquare OS Places | https://docs.foursquare.com/data-products/docs/fsq-places-open-source (HF gated / Source Cooperative / Iceberg) | dump | Apache 2.0, attribution "© Foursquare Labs, Inc." | monthly | monthly |
| 4 | Wikivoyage "Sleep" listings | dumps (§4) | dump | CC BY-SA 4.0 | twice monthly | monthly |
| 5 | Wikidata hotels (Q27686) | SPARQL | API | CC0 | live | monthly |
| 6 | Booking.com Demand API / Expedia Rapid / Hotelbeds | partner portals | API (contract) | partner ToS | live | on demand |

Prices: no free, legitimate open source exists. Keep prices as labelled estimates unless they come from a contracted API.

## 4. Activities and places to see

| # | Source | URL | Method | Licence | Freq | Recrawl |
|---|---|---|---|---|---|---|
| 1 | **Wikivoyage dumps** (See/Do/Eat/Drink/Sleep listings, Get in/Get around/Stay safe/Connect/Respect) | https://dumps.wikimedia.org/enwikivoyage/latest/ (`enwikivoyage-latest-pages-articles.xml.bz2`) | dump | **CC BY-SA 4.0**: credit "Wikivoyage contributors" + link + licence, share-alike on adapted text | 1st & 20th of each month | monthly. The site itself has robots `Crawl-delay: 5`; use dumps, not HTML |
| 2 | Wikipedia (summaries, geosearch) | dumps + https://en.wikipedia.org/w/api.php?action=query&list=geosearch | dump / API | CC BY-SA 4.0; API etiquette needs a UA with contact | twice monthly | monthly |
| 3 | Wikidata (heritage sites, museums, coordinates, opening hours P3025, official site P856) | SPARQL / dumps | API/dump | CC0 | weekly | monthly |
| 4 | OpenStreetMap `tourism=attraction|museum|viewpoint`, `historic=*` | Geofabrik / Overpass | dump/API | ODbL | daily | monthly |
| 5 | OpenTripMap | https://opentripmap.io/product (key) | API | Data from OSM (ODbL) + Wikidata/Wikipedia; free key tier (~5 000 req/day per their docs; verify) | — | on demand, cache 30 d |
| 6 | Overture / FSQ OS Places (POIs) | as §3 | dump | CDLA-P-2.0 / Apache 2.0 | monthly | monthly |
| 7 | UNESCO World Heritage List | https://whc.unesco.org/en/list/xml/ (also syndication) | dump (XML) | UNESCO terms: reuse with attribution, non-commercial clause on some content. Verify | yearly (July) | yearly |
| 8 | Viator / GetYourGuide Partner APIs (bookable tours) | partner portals | API (affiliate) | affiliate ToS | live | on demand |

## 5. Free walking tours

| # | Source | URL | Method | Licence / ToS | Recrawl |
|---|---|---|---|---|---|
| 1 | FreeTour.com | https://www.freetour.com (sitemap) | sitemap + HTML | robots `*`: `Allow: /`, disallows `/company/`, `/booking/*`, `/review/*`, `/account/*`. ToS not verified: store name, city, meeting point and link only | monthly, ≤1 req/5 s |
| 2 | GuruWalk | https://www.guruwalk.com (sitemap) | sitemap + HTML | robots `*` disallows `*/api/*`, `*/booking/*`, `*/walks/*/reviews*`, `*/gurus/tours/*` and others; tour pages allowed. ToS not verified | monthly |
| 3 | Wikivoyage "Do" listings mentioning free/tip-based tours | dump | CC BY-SA | monthly |
| 4 | OSM `tourism=information` + `guide`/route relations (`route=foot`, `tourism=attraction` with `tour` tags) | Geofabrik | ODbL | monthly (self-guided walks) |
| 5 | City tourist-board open data (e.g. Paris opendata "Que faire à Paris", ODbL; NYC Open Data) | per-city portals | API/dump | open licences per portal | weekly |

## 6. Events

| # | Source | URL | Method | Licence / ToS | Freq | Recrawl |
|---|---|---|---|---|---|---|
| 1 | Ticketmaster Discovery API v2 | https://developer.ticketmaster.com/products-and-docs/apis/discovery-api/v2/ | API (free key) | 5 000 calls/day, 2–5 req/s (docs conflict); 1 000-item deep-paging cap; branding/attribution rules | live | per destination+dates on demand, cache 6–24 h |
| 2 | PredictHQ | https://www.predicthq.com/pricing | API | **14-day API trial only**. The free plan has **no API access**; paid afterwards | live | only if paid |
| 3 | Eventbrite API v3 | https://www.eventbrite.com/platform/api | API (key) | Public search **shut down 2019-12**; only known organiser/venue/event IDs; robots disallows `/rss/` | live | daily for a curated organiser list |
| 4 | SeatGeek Platform API | https://platform.seatgeek.com/ | API (client_id) | Free with attribution; verify current status | live | on demand |
| 5 | Nager.Date public holidays | https://date.nager.at/api/v3/PublicHolidays/{year}/{CC} | API (no key) | Open source (MIT); free API | yearly | yearly per country |
| 6 | City open-data event feeds (Paris "Que faire à Paris" ODbL, NYC Open Data, Melbourne, London Datastore) | per portal | API/dump | open-gov licences | daily | daily |
| 7 | Wikidata recurring festivals (P837/P2257 with location) | SPARQL | API | CC0 | live | monthly |
| 8 | Time Out news sitemaps | https://www.timeout.com/news_sitemap.xml (+ per city) | sitemap | robots allows articles; `*/rss/` legacy feeds disallowed but `/blog/feed.rss` allowed; ToS reuse not granted, so link-only discovery | daily | daily |

## 7. Visa / entry requirements (official first)

| # | Source | URL | Method | Licence | Freq | Recrawl |
|---|---|---|---|---|---|---|
| 1 | **UK FCDO travel advice Content API** (entry requirements, safety, local laws, health, emergency) | https://www.gov.uk/api/content/foreign-travel-advice/{country} ; change feed https://www.gov.uk/foreign-travel-advice.atom | API (JSON, no key) + Atom | **Open Government Licence v3.0**: "Contains public sector information licensed under the Open Government Licence v3.0" | on change (often daily) | poll Atom hourly; refetch changed countries |
| 2 | **US State Dept travel advisories API** | https://cadataapi.state.gov/api/TravelAdvisories ; RSS https://travel.state.gov/_res/rss/TAsTWs.xml | API + RSS | US public domain | on change | every 6 h |
| 3 | US State Dept country pages ("Entry, Exit and Visa Requirements", for US citizens) + Visa Reciprocity tables | https://travel.state.gov | HTML (robots allows) | public domain | on change | weekly, ≤1 req/5 s |
| 4 | **Canada travel advice JSON** (advisory + entry/exit for Canadians) | https://data.international.gc.ca/travel-voyage/index-alpha-eng.json (+ per-country `cta-cap-{ISO}-eng.json`) | API (JSON) | Open Government Licence – Canada | daily (generated timestamp) | daily |
| 5 | EU visa list (Reg. (EU) 2018/1806 Annex I/II) + ETIAS/EES status | https://eur-lex.europa.eu/eli/reg/2018/1806 ; https://travel-europe.europa.eu/etias_en | HTML | EU reuse policy (Decision 2011/833/EU): free reuse with attribution | on amendment | monthly |
| 6 | UK "Check if you need a UK visa" + ETA eligibility | https://www.gov.uk/api/content/check-uk-visa , https://www.gov.uk/guidance/apply-for-an-electronic-travel-authorisation-eta | API | OGL v3 | on change | weekly |
| 7 | Australia Smartraveller | https://www.smartraveller.gov.au | HTML/RSS | CC BY 4.0 (Commonwealth). **The site timed out for our honest UA today; retry politely, do not evade** | on change | daily if reachable |
| 8 | Wikipedia "Visa requirements for {X} citizens" (~199 articles) | dumps | dump | CC BY-SA 4.0 | twice monthly | monthly. Best global nationality×destination matrix; cross-check against 1–7. `ilyankou/passport-index-dataset` (MIT, scraped from passportindex.org, provenance caveat) is a secondary check |

## 8. SIM / eSIM

| # | Source | URL | Method | Licence | Recrawl |
|---|---|---|---|---|---|
| 1 | Wikivoyage "Connect" sections | dump | dump | CC BY-SA | monthly |
| 2 | Wikipedia "List of mobile network operators of {region}" + operator articles | dump | dump | CC BY-SA | monthly |
| 3 | Wikidata mobile operators (Q1941618 by country) | SPARQL | API | CC0 | quarterly |
| 4 | eSIMDB | https://esimdb.com (sitemaps per language) | sitemap + HTML | robots `*`: `Allow: /` (disallows `/admin/`, `/stats/`, `/partner`). ToS not verified: facts + link only, label prices as observed | weekly, ≤1 req/5 s |
| 5 | Airalo Partner API | https://partners-doc.airalo.com | API (partner) | partner ToS; site robots disallows `/esims`, `/buy-esim`, `/order` | on demand |
| 6 | National regulators (prepaid SIM registration rules, e.g. Ofcom, TRAI, MIC Japan) | per regulator | HTML | open-gov licences vary | quarterly |

## 9. Travel tips / safety / etiquette

FCDO Content API (local laws and customs, safety; OGL v3; Atom-driven), State Dept country info (public domain), Canada JSON (OGL-Canada), Wikivoyage "Respect / Stay safe / Stay healthy / Cope" (CC BY-SA), CDC Travelers' Health https://wwwnc.cdc.gov/travel/destinations/list (public domain; **robots `Crawl-delay: 20`**, weekly), WHO international travel and health (WHO copyright, CC BY-NC-SA 3.0 IGO for most publications: non-commercial). Recrawl: event-driven via the Atom/RSS change feeds, otherwise weekly.

## 10. Emergency numbers card

| # | Source | Method | Licence | Recrawl |
|---|---|---|---|---|
| 1 | Wikidata P2852 "emergency phone number" per country | SPARQL | CC0 | quarterly |
| 2 | Wikipedia "List of emergency telephone numbers" | dump | CC BY-SA | quarterly |
| 3 | FCDO Content API ("Emergency services" part of each country) | API | OGL v3 | on change |
| 4 | State Dept country pages ("Local Laws/Emergency" + embassy contacts) | HTML | public domain | monthly |
| 5 | Canada JSON (embassy/consulate contacts) | API | OGL-Canada | monthly |
| 6 | Embassy lists: Wikidata P531/P137 diplomatic missions; US embassy list https://www.usembassy.gov | SPARQL / HTML | CC0 / public domain | quarterly |

## 11. Getting around (local transport)

| # | Source | URL | Method | Licence | Freq | Recrawl |
|---|---|---|---|---|---|---|
| 1 | Mobility Database catalog (GTFS/GTFS-RT/GBFS) | https://github.com/MobilityData/mobility-database-catalogs and https://files.mobilitydatabase.org/feeds_v2.csv ; API https://api.mobilitydatabase.org (free refresh token) | dump/API | catalog CC0; **each feed has its own licence** (record it per feed) | daily | catalog weekly; GTFS feeds on their own `feed_end_date` or monthly |
| 2 | Transitland v2 REST | https://transit.land/api/v2/rest (free key) | API | Transitland ToS; feed licences tracked per feed | daily | on demand / monthly |
| 3 | GBFS systems list (bike/scooter share) | https://github.com/MobilityData/gbfs/blob/master/systems.csv | dump | CC0 list; feed licences vary | weekly | weekly |
| 4 | OpenStreetMap transit relations (`route=subway|tram|bus|ferry`) | Geofabrik | dump | ODbL | daily | monthly |
| 5 | Wikivoyage "Get in / Get around" | dump | dump | CC BY-SA | twice monthly | monthly |
| 6 | Wikidata metro/airport-rail lines | SPARQL | API | CC0 | — | quarterly |
| 7 | The Man in Seat 61 (rail guides) | https://www.seat61.com | HTML (robots allows) | personal copyright: **link/reference only**, no text reuse | — | quarterly, very slow |
| 8 | Rome2rio | robots allows `/` (disallows `/map`, `/trips`, `/hotels`) | — | ToS not verified; commercial product: **link-only, do not crawl content** without a licence | — | — |

## 12. Forex

| # | Source | URL | Method | Licence | Freq | Recrawl |
|---|---|---|---|---|---|---|
| 1 | **ECB euro reference rates** | https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml (+ `-hist.zip`) | dump (XML/CSV) | Free reuse with source attribution (ECB) | ~16:00 CET on TARGET days | daily 16:30 CET |
| 2 | Frankfurter (ECB-based, open source) | https://api.frankfurter.dev/v1/latest | API (no key) | MIT code; ECB data | daily | daily |
| 3 | US Fed H.10 via FRED | https://fred.stlouisfed.org/docs/api/fred/ (key) | API | public domain (series), FRED ToS | daily (weekly release) | weekly |
| 4 | Other central banks: Bank of England, RBI, Bank of Canada Valet API https://www.bankofcanada.ca/valet/ , RBA | API/CSV | open-gov licences | daily | daily |
| 5 | ExchangeRate-API open endpoint | https://open.er-api.com/v6/latest/USD | API (no key) | Free with **required attribution link**; 1 refresh/day | daily | daily (covers ~160 currencies ECB lacks) |
| 6 | IMF exchange rates | https://www.imf.org/external/np/fin/data/param_rms_mth.aspx | dump | IMF terms (attribution) | daily/monthly | monthly |

## 13. Packing list

Inputs come from weather and climate (§2). Plug type and voltage: Wikidata P2853 "electrical plug type" and P2884 "mains voltage" (CC0), plus Wikipedia "Mains electricity by country" (CC BY-SA). Dress codes and religious-site rules: Wikivoyage "Respect" and FCDO "local laws and customs" (OGL). Health and vaccine items: CDC destinations (public domain, crawl-delay 20). Recrawl quarterly. The IEC World Plugs site is copyright: reference/link only.

## 14. Generic crawl corpus

Common Crawl: https://index.commoncrawl.org/collinfo.json (latest **CC-MAIN-2026-43**, October 2026), monthly-ish. The Common Crawl Terms of Use apply, and the page content keeps its original copyright. Use the CDX index for **URL discovery** (e.g. find travel-blog posts per city) and to avoid hitting origin servers. Still honour the origin's licence before reusing text.

---

## 15. Travel blogs (RSS verified 2026-11-15)

All of these are WordPress-style blogs whose `/feed/` returned RSS (200) to our honest UA. Licence: all-rights-reserved copyright. Use title, link, date and a short excerpt/summary only, always link back, never republish the full text. Cadence: every 12–24 h (conditional GET with ETag/If-Modified-Since).

1. Nomadic Matt – https://www.nomadicmatt.com/feed/
2. Adventurous Kate – https://www.adventurouskate.com/feed/
3. Legal Nomads – https://www.legalnomads.com/feed/
4. The Blonde Abroad – https://www.theblondeabroad.com/feed/
5. Y Travel Blog – https://www.ytravelblog.com/feed/
6. Never Ending Footsteps – https://www.neverendingfootsteps.com/feed/
7. Wanderlust Crew – https://www.wanderlustcrew.com/feed/
8. Dangerous Business – https://www.dangerous-business.com/feed/
9. Alex in Wanderland – https://www.alexinwanderland.com/feed/
10. Goats on the Road – https://www.goatsontheroad.com/feed/
11. Two Wandering Soles – https://www.twowanderingsoles.com/feed/
12. Atlas & Boots – https://www.atlasandboots.com/feed/
13. Be My Travel Muse – https://www.bemytravelmuse.com/feed/
14. Uncornered Market – https://www.uncorneredmarket.com/feed/
15. Migrationology (food) – https://www.migrationology.com/feed/
16. Earth Trekkers – https://www.earthtrekkers.com/feed/
17. Heart My Backpack – https://www.heartmybackpack.com/feed/
18. Hop On World – https://www.hoponworld.com/feed/
19. World of Wanderlust – https://www.worldofwanderlust.com/feed/
20. Lost With Purpose – https://www.lostwithpurpose.com/feed/
21. The Planet D – https://www.theplanetd.com/feed/
22. Wild Junket – https://www.wildjunket.com/feed/
23. Travels of Adam – https://www.travelsofadam.com/feed/
24. Nomadasaurus – https://www.nomadasaurus.com/feed/
25. Salt in our Hair – https://www.saltinourhair.com/feed/
26. Everything Everywhere – https://www.everythingeverywhere.com/feed/ (slow; XML returned)
27. One Mile at a Time (points/airlines) – https://onemileatatime.com/feed/
28. View from the Wing – https://viewfromthewing.com/feed/
29. The Points Guy – https://thepointsguy.com/feed/
30. Head for Points (UK) – https://www.headforpoints.com/feed/
31. Matador Network – https://matadornetwork.com/feed/

Did not work (do not use): expertvagabond.com/feed returns HTML; thepoortraveler.net 403; bucketlistly.blog 403; indefinitelywild 404; thecrazytourist 404.

## 16. Travel news outlets (RSS verified 2026-11-15)

Licence: publisher copyright, headline + link + short excerpt only. Cadence: every 1–3 h for news, 12 h for magazines.

1. The Guardian Travel – https://www.theguardian.com/travel/rss (Guardian Open Platform API also available with key)
2. BBC Travel – https://www.bbc.com/travel/feed.rss
3. New York Times Travel – https://rss.nytimes.com/services/xml/rss/nyt/Travel.xml
4. The Independent Travel – https://www.independent.co.uk/travel/rss (gzip)
5. The Telegraph Travel – https://www.telegraph.co.uk/travel/rss.xml
6. CNN Travel – http://rss.cnn.com/rss/edition_travel.rss
7. CNBC Travel – https://www.cnbc.com/id/10000739/device/rss/rss.html
8. Euronews Travel – https://www.euronews.com/rss?level=vertical&name=travel
9. Condé Nast Traveler (US) – https://www.cntraveler.com/feed/rss
10. Condé Nast Traveller (UK) – https://www.cntraveller.com/feed/rss
11. Smithsonian Travel – https://www.smithsonianmag.com/rss/travel/
12. Skift – https://skift.com/feed/
13. Simple Flying – https://simpleflying.com/feed/
14. TravelDailyNews – https://www.traveldailynews.com/feed/
15. Wanderlust (UK) – https://www.wanderlust.co.uk/rss
16. TheTravel – https://www.thetravel.com/feed/
17. Atlas Obscura (latest) – https://www.atlasobscura.com/feeds/latest (feed works; the site itself is Cloudflare-walled to bots, so feed only)
18. Time Out blog – https://www.timeout.com/blog/feed.rss (robots explicitly allows the new feeds)
19. Travel-Mag – https://www.travel-mag.com/feed/ (slow; XML returned)
20. Government advisories (alerts, not news): https://www.gov.uk/foreign-travel-advice.atom , https://travel.state.gov/_res/rss/TAsTWs.xml

Bot-walled or no feed (do not use): Travel + Leisure (403), TravelPulse (403), Travel Weekly (403), Frommer's (403), PhocusWire (403), Fodor's (403), Lonely Planet (no feed; sitemaps only, incl. `/sitemaps/google-news/sitemap.xml`), Afar, National Geographic, Rough Guides, SmarterTravel, Thrillist (404), Breaking Travel News (404).

Feeds verified: 31 blogs + 19 news outlets = **50**.

## 17. Forums (open access status)

| Forum | Status (2026-11-15) | Use |
|---|---|---|
| **Stack Exchange Travel** (travel.stackexchange.com) | robots `Disallow: /` + `ai-train=no`. Content CC BY-SA (version per post date). The official archive.org dump was killed in 2024 (moved behind login); **community mirror "Stack Exchange Data Dump 2025-12-31"** is on archive.org (`stackexchange_20251231`) and Academic Torrents. The 2025-06-30 dump reportedly contains watermark/bogus entries; 2025-12-31 reportedly does not. Stack Exchange API v2.3 (`api.stackexchange.com`, answered 200; 300 req/day anonymous, 10 000 with key) | Use dump (semi-annual) or API. Attribution: author + link to question + CC BY-SA. **No HTML crawl. Do not use for model training (content signal)** |
| **Rick Steves Travel Forum** (community.ricksteves.com) | robots: `User-Agent: *  Disallow: /users/` only. ricksteves.com robots disallows `/search` etc. No RSS found (`/travel-forum/feed` 404) | HTML allowed by robots, but ToS not verified and content is user copyright: low-rate (≤1 req/10 s), metadata/summaries + link only, weekly |
| **Lonely Planet Thorn Tree** | Closed Sept 2021 and removed; `/thorntree/*` disallowed | none |
| **FlyerTalk** | robots.txt and RSS answer 403 to honest UA (bot wall) | do not crawl |
| **Fodor's Travel Talk** | 403 (bot wall) | do not crawl |
| **Tripadvisor forums** | ToS bans automation; AI bots disallowed site-wide | do not crawl |
| **Reddit** (r/travel, r/solotravel…) | `Disallow: /`; Data API non-commercial only | do not crawl; API only with explicit approval |
| **Travelfish board** | robots.txt permissive, but board/feeds answer 403 | do not crawl |
| Wikivoyage Travellers' pub / talk pages | in dumps, CC BY-SA | optional |

## 18. Attribution requirements (summary)

| Source | Licence | What to show |
|---|---|---|
| Wikivoyage / Wikipedia | CC BY-SA 4.0 | "Text from Wikivoyage/Wikipedia contributors, CC BY-SA 4.0" + article link; adapted text must stay CC BY-SA (keep AI paraphrase separate from verbatim quotes) |
| Wikidata | CC0 | none required (courtesy credit) |
| OpenStreetMap (incl. OpenTripMap, Overpass) | ODbL 1.0 | "© OpenStreetMap contributors" + link to openstreetmap.org/copyright; derived DBs share-alike |
| GeoNames | CC BY 4.0 | "Contains data from GeoNames (geonames.org), CC BY 4.0" (already in the app) |
| OurAirports | Public domain | none |
| Overture Places | CDLA-Permissive-2.0 | keep licence notice |
| FSQ OS Places | Apache 2.0 | "© Foursquare Labs, Inc." + licence notice |
| Open-Meteo / MET Norway / ECMWF | CC BY 4.0 | "Weather data by Open-Meteo.com" / "MET Norway" / "ECMWF", link |
| Meteostat | CC BY-NC 4.0 | credit; non-commercial only |
| UK FCDO / gov.uk | OGL v3 | "Contains public sector information licensed under the Open Government Licence v3.0" |
| Canada travel.gc.ca | OGL – Canada 2.0 | "Contains information licensed under the Open Government Licence – Canada" |
| Smartraveller | CC BY 4.0 | "© Commonwealth of Australia, Smartraveller, CC BY 4.0" |
| US State Dept, CDC, NWS, BTS | public domain | courtesy credit, no implied endorsement |
| EU (EUR-Lex, ECB) | EU reuse / ECB terms | "Source: ECB" / "© European Union" |
| ExchangeRate-API open endpoint | free ToS | visible link "Rates by Exchange Rate API" (required) |
| Ticketmaster, Viator, GYG, Booking partner APIs | partner ToS | provider branding + deep links as specified |
| Stack Exchange | CC BY-SA (3.0/4.0 by date) | author name + profile link + question link + licence |
| GTFS feeds | per feed (CC BY, ODbL, custom) | store the licence per feed and show agency attribution |
| Blogs/news RSS | copyright | headline + source name + link; short excerpts only |
