//! The narrow slice of the Chrome DevTools Protocol the renderer needs, as traits.
//!
//! [`crate::ChromiumRenderer`] drives a [`CdpBrowser`]; production uses
//! [`crate::chromium::ChromiumBrowser`] (chromiumoxide over a WebSocket to the
//! `chromedp/headless-shell` container), tests use a scripted mock. Keeping the decision logic
//! behind these traits lets every policy rule be tested without a browser.
//!
//! A tab returned by [`CdpBrowser::open_tab`] already has `Fetch.enable` on for every URL
//! pattern, so **every** request Chromium makes arrives as a [`PausedRequest`] and waits until
//! the renderer answers it with [`CdpTab::fulfill`] or [`CdpTab::fail`]. Chromium itself has no
//! network (spec §17.1).

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::mpsc::UnboundedReceiver;
use vc_core::render::BlockReason;

/// The CDP resource type of a paused request (`Network.ResourceType`), reduced to what the
/// renderer distinguishes. Everything but documents, scripts, styles and XHR/fetch is blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    /// A document (main frame or iframe).
    Document,
    /// CSS.
    Stylesheet,
    /// JavaScript.
    Script,
    /// `XMLHttpRequest`.
    Xhr,
    /// `fetch()`.
    Fetch,
    /// An image (blocked).
    Image,
    /// Audio/video (blocked).
    Media,
    /// A web font (blocked).
    Font,
    /// Anything else: prefetch, ping, beacon, manifest, websocket, CORS preflight… (blocked).
    Other,
}

impl ResourceKind {
    /// Whether requests of this type may be fetched at all (spec §5.4: images, media and fonts
    /// are blocked; so is everything that is not needed to build the DOM).
    pub fn allowed(self) -> bool {
        matches!(
            self,
            ResourceKind::Document
                | ResourceKind::Stylesheet
                | ResourceKind::Script
                | ResourceKind::Xhr
                | ResourceKind::Fetch
        )
    }

    /// Whether a successful response may be served from the shared sub-resource cache
    /// (JS and CSS only; XHR data is always fetched fresh).
    pub fn cacheable(self) -> bool {
        matches!(self, ResourceKind::Stylesheet | ResourceKind::Script)
    }
}

/// One `Fetch.requestPaused` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PausedRequest {
    /// The `requestId` to answer.
    pub id: String,
    /// The URL Chromium wants.
    pub url: String,
    /// The HTTP method.
    pub method: String,
    /// The resource type.
    pub kind: ResourceKind,
    /// The `Accept` header Chromium would have sent, if any.
    pub accept: Option<String>,
}

/// A response to hand to Chromium (`Fetch.fulfillRequest`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fulfilment {
    /// HTTP status.
    pub status: u16,
    /// Response headers (no `Content-Encoding`: bodies are already decoded).
    pub headers: Vec<(String, String)>,
    /// The decoded body.
    pub body: Bytes,
}

/// A CDP failure (connection, protocol or a failed navigation).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CdpError {
    /// The CDP endpoint could not be reached.
    #[error("cdp connect: {0}")]
    Connect(String),
    /// A command failed or the session went away.
    #[error("cdp: {0}")]
    Protocol(String),
    /// `Page.navigate` reported an error (e.g. the document request was failed).
    #[error("navigation failed: {0}")]
    Navigation(String),
}

/// One tab (in its own browser context) with request interception on.
#[async_trait]
pub trait CdpTab: Send + Sync {
    /// `Emulation.setUserAgentOverride` (always [`vc_core::USER_AGENT`]).
    async fn set_user_agent(&self, ua: &str) -> Result<(), CdpError>;

    /// `Page.navigate`; resolves once the navigation committed (or failed).
    async fn navigate(&self, url: &str) -> Result<(), CdpError>;

    /// Scrolls to the bottom of the page; returns the document's scroll height afterwards.
    async fn scroll_to_bottom(&self) -> Result<u64, CdpError>;

    /// Clicks every element matching `selector` once; returns how many were clicked. An
    /// invalid selector clicks nothing.
    async fn click_all(&self, selector: &str) -> Result<u32, CdpError>;

    /// The serialised DOM (doctype + `documentElement.outerHTML`).
    async fn content(&self) -> Result<String, CdpError>;

    /// `Fetch.fulfillRequest`.
    async fn fulfill(&self, id: &str, response: Fulfilment) -> Result<(), CdpError>;

    /// `Fetch.failRequest`.
    async fn fail(&self, id: &str, reason: &BlockReason) -> Result<(), CdpError>;

    /// Closes the tab and disposes of its browser context.
    async fn close(&self);
}

/// A browser that opens intercepted tabs.
#[async_trait]
pub trait CdpBrowser: Send + Sync {
    /// Opens a fresh tab in a fresh browser context with `Fetch.enable` on for all URL
    /// patterns. Paused requests arrive on the returned receiver; it closes when the tab dies.
    async fn open_tab(
        &self,
    ) -> Result<(Box<dyn CdpTab>, UnboundedReceiver<PausedRequest>), CdpError>;
}
