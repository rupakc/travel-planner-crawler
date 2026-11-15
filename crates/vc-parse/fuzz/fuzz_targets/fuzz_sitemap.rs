//! Sitemap handler (XML, gzip'd XML and plain text): no panics, entry cap honoured, DTDs refused.
#![no_main]

use libfuzzer_sys::fuzz_target;
use vc_core::media::MediaType;
use vc_core::parse::ContentHandler;
use vc_parse::SitemapHandler;
use vc_parse_fuzz::{check, limits, meta};

fuzz_target!(|data: &[u8]| {
    let lim = limits();
    for media in [MediaType::Sitemap, MediaType::SitemapText] {
        if let Ok(p) = SitemapHandler.handle(&meta(media), data, &lim) {
            check(&p, &lim);
            assert!(p.links.len() <= lim.max_sitemap_entries);
        }
    }
});
