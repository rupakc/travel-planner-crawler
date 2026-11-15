//! # vc-parse (Task 6, stub)
//!
//! Content handlers (HTML, RSS/Atom, sitemap, JSON, CSV) behind
//! [`vc_core::parse::ContentHandler`], and [`registry`] building the
//! [`vc_core::parse::HandlerRegistry`].

use std::sync::Arc;

use vc_core::Result;
use vc_core::media::{MediaType, MediaTypeMatch};
use vc_core::parse::{ContentHandler, HandlerRegistry, Limits, Parsed};
use vc_core::store::CaptureMeta;

/// The HTML handler.
#[derive(Debug, Default, Clone, Copy)]
pub struct HtmlHandler;

impl ContentHandler for HtmlHandler {
    fn media(&self) -> &'static [MediaTypeMatch] {
        &[MediaTypeMatch::Is(MediaType::Html)]
    }
    fn handle(&self, _meta: &CaptureMeta, _body: &[u8], _lim: &Limits) -> Result<Parsed> {
        todo!("Task 6")
    }
}

/// The registry of all built-in handlers.
pub fn registry() -> HandlerRegistry {
    let mut r = HandlerRegistry::new();
    r.register(Arc::new(HtmlHandler));
    r
}
