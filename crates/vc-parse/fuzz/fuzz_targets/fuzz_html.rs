//! HTML handler: arbitrary bytes must never panic, hang or overflow the stack.
#![no_main]

use libfuzzer_sys::fuzz_target;
use vc_core::media::MediaType;
use vc_core::parse::ContentHandler;
use vc_parse::HtmlHandler;
use vc_parse_fuzz::{check, limits, meta};

fuzz_target!(|data: &[u8]| {
    let lim = limits();
    if let Ok(p) = HtmlHandler::default().handle(&meta(MediaType::Html), data, &lim) {
        check(&p, &lim);
    }
});
