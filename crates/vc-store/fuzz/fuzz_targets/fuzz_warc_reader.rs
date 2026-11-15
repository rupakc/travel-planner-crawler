//! The WARC reader, HTTP block parser, digest check and segment decompressor must never panic
//! on arbitrary bytes (they parse objects read back from storage).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let records = vc_store::warc::WarcReader::new(data).take(1024);
    for r in records.flatten() {
        let _ = vc_store::warc::parse_http(&r.block);
        let _ = vc_store::warc::verify_payload_digest(&r);
        let _ = r.capture_id();
    }
    let _ = vc_store::cdxj::parse_cdxj(&String::from_utf8_lossy(data));
    if let Ok(raw) = vc_store::segment::decompress(data) {
        let _ = vc_store::warc::read_records(&raw);
    }
});
