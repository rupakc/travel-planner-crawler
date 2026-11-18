//! The relevance gate: every source names the Voyager app section(s) it serves and why, and a
//! news or blog source must be travel-focused (owner rule, 2026-11-17: "whenever the list
//! grows, only add relevant and pertinent sites"; the owner approves every addition).
//!
//! Checks, all errors (see [`check`]):
//! - `relevance` is present, has at least one section (no duplicates) and a `why` of at least
//!   [`MIN_WHY_CHARS`] characters on one line;
//! - an **enabled** `news` or `blogs` source reads only travel-focused URLs: the host, path or
//!   query contains `travel`, or the URL belongs to a whole-travel publication listed in
//!   [`TRAVEL_PUBLICATIONS`] (a reviewed code change, so the list cannot grow silently).

use vc_core::PolitenessKey;
use vc_core::sources::SourceSpec;

/// Shortest acceptable `relevance.why`, in characters (after trimming).
pub const MIN_WHY_CHARS: usize = 20;

/// Categories whose URLs must be travel-focused.
pub const EDITORIAL_FEED_CATEGORIES: &[&str] = &["news", "blogs"];

/// Publications that are travel-focused as a whole, as `(registrable domain, path prefix)`:
/// the whole site when the prefix is empty, else only URLs under that path. A news or blog
/// source whose URL has no `travel` in it must match one of these. Add a publication only when
/// the whole feed is about travel (never general news or a non-travel site), and only with
/// the owner's approval in the PR.
pub const TRAVEL_PUBLICATIONS: &[(&str, &str)] = &[
    // Travel magazines, guides and travel/aviation news.
    ("atlasobscura.com", ""),
    ("skift.com", ""),
    ("simpleflying.com", ""),
    ("lonelyplanet.com", ""),
    ("timeout.com", ""),
    ("wanderlust.co.uk", ""),
    ("matadornetwork.com", ""),
    // CNBC's Travel section feed (section id 10000739), not the whole of CNBC.
    ("cnbc.com", "/id/10000739/"),
    // Independent travel blogs.
    ("adventurouskate.com", ""),
    ("alexinwanderland.com", ""),
    ("atlasandboots.com", ""),
    ("dangerous-business.com", ""),
    ("earthtrekkers.com", ""),
    ("everythingeverywhere.com", ""),
    ("goatsontheroad.com", ""),
    ("headforpoints.com", ""),
    ("heartmybackpack.com", ""),
    ("hoponworld.com", ""),
    ("legalnomads.com", ""),
    ("lostwithpurpose.com", ""),
    ("migrationology.com", ""),
    ("neverendingfootsteps.com", ""),
    ("nomadasaurus.com", ""),
    ("nomadicmatt.com", ""),
    ("onemileatatime.com", ""),
    ("saltinourhair.com", ""),
    ("theblondeabroad.com", ""),
    ("theplanetd.com", ""),
    ("thequantumexplorer.org", ""),
    ("thepointsguy.com", ""),
    ("twowanderingsoles.com", ""),
    ("uncorneredmarket.com", ""),
    ("viewfromthewing.com", ""),
    ("wanderlustcrew.com", ""),
    ("wildjunket.com", ""),
    ("worldofwanderlust.com", ""),
    // Sustainable and budget travel (research 2026-11-18): a green-travel magazine, aviation
    // route news, a travel magazine, budget-travel blogs and cheap-fare deal sites whose URLs
    // lack "travel".
    ("ecophiles.com", ""),
    ("aeroroutes.com", ""),
    ("mexicodesconocido.com.mx", ""),
    ("bemusedbackpacker.com", ""),
    ("backpackerbanter.com", ""),
    ("thesavvybackpacker.com", ""),
    ("nomadicnotes.com", ""),
    ("thriftynomads.com", ""),
    ("expertvagabond.com", ""),
    ("backpackingislife.com", ""),
    ("hippie-inheels.com", ""),
    ("wheregoesrose.com", ""),
    ("gobackpacking.com", ""),
    ("neverendingvoyage.com", ""),
    ("seasia.co", ""),
    ("eurocheapo.com", ""),
    ("tokyocheapo.com", ""),
    ("japancheapo.com", ""),
    ("vietnamcoracle.com", ""),
    ("viajenaviagem.com", ""),
    ("melhoresdestinos.com.br", ""),
    ("360meridianos.com", ""),
    ("elviajerofeliz.com", ""),
    ("southamericabackpacker.com", ""),
    ("generationvoyage.fr", ""),
    // Instinct Voyageur's FeedBurner feed only, not the rest of feeds.feedburner.com.
    ("feedburner.com", "/InstinctVoyageur"),
    ("urlaubstracker.de", ""),
    ("urlaubspiraten.de", ""),
    ("viajerospiratas.es", ""),
    ("dollarflightclub.com", ""),
    ("jacksflightclub.com", ""),
    ("fly4free.com", ""),
    ("theflightdeal.com", ""),
    ("frequentmiler.com", ""),
    ("pointhacks.com.au", ""),
    ("bootsnall.com", ""),
    ("coasttocoast.co.za", ""),
    // Corpus expansion: Europe (2026-11-18): travel magazines and blogs whose URLs lack "travel".
    ("europeupclose.com", ""),
    ("hiddeneurope.eu", ""),
    ("europeanbestdestinations.com", ""),
    // North America outdoor-travel blogs (research 2026-11-18). Hike Bike Travel has
    // `travel` in its host; Narcity (city news) and Outside (fitness and gear too) are not
    // whole-travel publications and stay off.
    ("bearfoottheory.com", ""),
    ("freshoffthegrid.com", ""),
    // Corpus expansion: thematic (2026-11-18): travel magazines and blogs whose URLs lack "travel".
    ("ciaobambino.com", ""),
    ("journeywoman.com", ""),
    // Corpus expansion: Latin America (2026-11-18): travel magazines and blogs whose URLs lack "travel".
    ("twoweeksincostarica.com", ""),
    ("wander-argentina.org", ""),
    ("caribjournal.com", ""),
    // Corpus expansion: Africa and the Middle East (2026-11-18): travel magazines and blogs whose URLs lack "travel".
    ("againstthecompass.com", ""),
    ("turkeysforlife.com", ""),
    ("tourismupdate.com", ""),
    ("istanbulclues.com", ""),
];

/// Whether `url` is travel-focused: `travel` in its host, path or query, or a
/// [`TRAVEL_PUBLICATIONS`] match.
pub fn travel_focused(url: &url::Url) -> bool {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let rest = format!("{}?{}", url.path(), url.query().unwrap_or_default()).to_ascii_lowercase();
    if host.contains("travel") || rest.contains("travel") {
        return true;
    }
    let reg = PolitenessKey::registrable(&host);
    TRAVEL_PUBLICATIONS
        .iter()
        .any(|(d, prefix)| reg == *d && url.path().starts_with(prefix))
}

/// The relevance errors of `spec` (empty when it passes).
pub fn check(spec: &SourceSpec) -> Vec<String> {
    let mut errors = Vec::new();
    match &spec.relevance {
        None => errors.push(
            "relevance is missing: name the app section(s) this source serves and why".into(),
        ),
        Some(r) => {
            if r.sections.is_empty() {
                errors.push("relevance.sections is empty: name at least one app section".into());
            }
            let mut seen = std::collections::BTreeSet::new();
            for s in &r.sections {
                if !seen.insert(*s) {
                    errors.push(format!("relevance.sections: duplicate {}", s.as_str()));
                }
            }
            let why = r.why.trim();
            if why.chars().count() < MIN_WHY_CHARS {
                errors.push(format!(
                    "relevance.why is too short ({} < {MIN_WHY_CHARS} characters): say what \
                     traveller-facing data it gives which section",
                    why.chars().count()
                ));
            }
            if why.contains(['\n', '\r']) {
                errors.push("relevance.why must be one sentence on one line".into());
            }
        }
    }
    if spec.enabled && EDITORIAL_FEED_CATEGORIES.contains(&spec.category.as_str()) {
        for u in spec.seeds.iter().chain(&spec.sitemaps).chain(&spec.feeds) {
            if !travel_focused(u) {
                errors.push(format!(
                    "{} source {u} is not travel-focused: the URL must be a travel section \
                     (contain \"travel\") or a whole-travel publication (TRAVEL_PUBLICATIONS)",
                    spec.category
                ));
            }
        }
    }
    errors
}
