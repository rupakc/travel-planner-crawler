//! Feed handler (RSS / Atom): no panics, link cap honoured, DTDs refused.
#![no_main]

use libfuzzer_sys::fuzz_target;
use vc_core::media::MediaType;
use vc_core::parse::ContentHandler;
use vc_parse::FeedHandler;
use vc_parse_fuzz::{check, limits, meta};

fuzz_target!(|data: &[u8]| {
    let lim = limits();
    if let Ok(p) = FeedHandler.handle(&meta(MediaType::Rss), data, &lim) {
        check(&p, &lim);
        assert!(p.links.len() <= lim.max_links);
    }
});
