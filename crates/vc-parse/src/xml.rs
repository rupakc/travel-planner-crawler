//! XML safety and text helpers shared by the sitemap and feed handlers.
//!
//! **No DTDs** (spec §5.2): a document with a `<!DOCTYPE …>` is refused before any parser that
//! might honour its entity declarations sees it (billion laughs, XXE). The whole document is
//! scanned (a DOCTYPE outside the prolog is malformed, but refused all the same).

use quick_xml::Reader;
use quick_xml::events::{BytesRef, Event};
use vc_core::{Error, Result};

/// Refuses documents with a DTD. Malformed prologs are left for the real parser to reject.
pub(crate) fn reject_dtd(body: &[u8]) -> Result<()> {
    let mut r = Reader::from_reader(body);
    r.config_mut().check_end_names = false;
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf) {
            Ok(Event::DocType(_)) => {
                return Err(Error::Policy(
                    "xml dtd/entity declarations not allowed".into(),
                ));
            }
            Ok(Event::Eof) | Err(_) => return Ok(()),
            Ok(_) => {}
        }
        buf.clear();
    }
}

/// Resolves a general reference (`&amp;`, `&#233;`). Unknown named entities (there are no DTDs)
/// are kept literally.
pub(crate) fn push_ref(out: &mut String, r: &BytesRef<'_>) {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        out.push(c);
        return;
    }
    let name = r.xml10_content();
    match quick_xml::escape::resolve_predefined_entity(&name) {
        Some(s) => out.push_str(s),
        None => {
            out.push('&');
            out.push_str(&name);
            out.push(';');
        }
    }
}
