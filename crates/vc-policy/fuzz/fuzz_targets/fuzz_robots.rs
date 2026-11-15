//! Fuzz the robots.txt parser and matcher: arbitrary bodies and paths must never panic, and a
//! verdict must not depend on how often it is asked.
#![no_main]

use libfuzzer_sys::fuzz_target;
use vc_policy::robots::{ParsedRobots, matches, normalize};

fuzz_target!(|data: &[u8]| {
    // Split the input into a body and a path at the first NUL.
    let (body, path) = match data.iter().position(|&b| b == 0) {
        Some(i) => (
            &data[..i],
            String::from_utf8_lossy(&data[i + 1..]).into_owned(),
        ),
        None => (data, "/".to_string()),
    };
    let parsed = ParsedRobots::parse(body);
    for agent in ["VoyagerBot", "*", "GPTBot"] {
        let rules = parsed.rules_for(agent);
        let a = rules.allowed(&path);
        assert_eq!(a, rules.allowed(&path));
        let _ = rules.crawl_delay();
    }
    let n = normalize(&path);
    assert_eq!(normalize(&n), n);
    let _ = matches(body, path.as_bytes());
});
