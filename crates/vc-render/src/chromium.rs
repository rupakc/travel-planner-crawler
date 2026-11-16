//! [`CdpBrowser`] over chromiumoxide, connected to the `chromedp/headless-shell` container on
//! the internal `render` network (CDP only; the container has no route out).
//!
//! Each render gets a fresh browser context (no shared cookies, storage or HTTP cache between
//! renders), a tab with `Fetch.enable` for every URL pattern at the request stage, and is
//! disposed of afterwards. The CDP session is re-established when its handler stops or after
//! `restart_after_renders` tabs.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chromiumoxide::Binary;
use chromiumoxide::browser::Browser;
use chromiumoxide::cdp::browser_protocol::browser::BrowserContextId;
use chromiumoxide::cdp::browser_protocol::fetch::{
    self, EventRequestPaused, FailRequestParams, FulfillRequestParams, HeaderEntry, RequestPattern,
    RequestStage,
};
use chromiumoxide::cdp::browser_protocol::network::{
    ErrorReason, ResourceType, SetUserAgentOverrideParams,
};
use chromiumoxide::cdp::browser_protocol::page::NavigateParams;
use chromiumoxide::cdp::browser_protocol::target::{
    CreateBrowserContextParams, CreateTargetParams, DisposeBrowserContextParams,
};
use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::page::Page;
use futures::StreamExt;
use tokio::sync::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use vc_core::render::BlockReason;

use crate::b64;
use crate::cdp::{CdpBrowser, CdpError, CdpTab, Fulfilment, PausedRequest, ResourceKind};

fn proto(e: impl std::fmt::Display) -> CdpError {
    CdpError::Protocol(e.to_string())
}

struct Conn {
    browser: Arc<Browser>,
    handler: JoinHandle<()>,
    tabs_opened: u32,
}

/// A chromiumoxide connection to a remote headless Chromium.
pub struct ChromiumBrowser {
    cdp_url: String,
    restart_after: u32,
    request_timeout: Duration,
    conn: Mutex<Option<Conn>>,
}

impl std::fmt::Debug for ChromiumBrowser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChromiumBrowser")
            .field("cdp_url", &self.cdp_url)
            .finish_non_exhaustive()
    }
}

impl ChromiumBrowser {
    /// A lazily connected browser at `cdp_url` (`http://host:port`).
    pub fn new(cdp_url: impl Into<String>, restart_after: u32, request_timeout: Duration) -> Self {
        ChromiumBrowser {
            cdp_url: cdp_url.into(),
            restart_after: restart_after.max(1),
            request_timeout,
            conn: Mutex::new(None),
        }
    }

    async fn browser(&self) -> Result<Arc<Browser>, CdpError> {
        let mut g = self.conn.lock().await;
        let stale = g
            .as_ref()
            .is_none_or(|c| c.handler.is_finished() || c.tabs_opened >= self.restart_after);
        if stale {
            // A count-based restart leaves the old session to tabs still using it; its handler
            // ends once the last of them is dropped.
            if let Some(old) = g.take()
                && old.handler.is_finished()
            {
                old.handler.abort();
            }
            let url = resolve_cdp_url(&self.cdp_url).await?;
            let config = HandlerConfig {
                request_timeout: self.request_timeout,
                ..HandlerConfig::default()
            };
            let (browser, mut handler) = Browser::connect_with_config(url, config)
                .await
                .map_err(|e| CdpError::Connect(e.to_string()))?;
            let handler = tokio::spawn(async move {
                while let Some(r) = handler.next().await {
                    if let Err(e) = r {
                        debug!(error = %e, "render: cdp handler");
                    }
                }
            });
            *g = Some(Conn {
                browser: Arc::new(browser),
                handler,
                tabs_opened: 0,
            });
        }
        let c = g
            .as_mut()
            .ok_or_else(|| CdpError::Connect("no session".into()))?;
        c.tabs_opened += 1;
        Ok(c.browser.clone())
    }
}

/// Chromium's DevTools HTTP endpoint refuses `Host` headers that are neither an IP nor
/// `localhost`, so a service name (`http://chromium:9222`) is resolved to its IP first.
pub(crate) async fn resolve_cdp_url(cdp_url: &str) -> Result<String, CdpError> {
    let mut u = url::Url::parse(cdp_url).map_err(|e| CdpError::Connect(e.to_string()))?;
    let host = u
        .host_str()
        .ok_or_else(|| CdpError::Connect("cdp_url has no host".into()))?
        .to_string();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<std::net::IpAddr>().is_ok() || bare == "localhost" {
        return Ok(cdp_url.to_string());
    }
    let port = u.port_or_known_default().unwrap_or(9222);
    let addr = tokio::net::lookup_host((bare, port))
        .await
        .map_err(|e| CdpError::Connect(format!("resolve {bare}: {e}")))?
        .next()
        .ok_or_else(|| CdpError::Connect(format!("resolve {bare}: no address")))?;
    u.set_ip_host(addr.ip())
        .map_err(|()| CdpError::Connect("cannot set ip host".into()))?;
    Ok(u.to_string())
}

fn kind(t: &ResourceType) -> ResourceKind {
    match t {
        ResourceType::Document => ResourceKind::Document,
        ResourceType::Stylesheet => ResourceKind::Stylesheet,
        ResourceType::Script => ResourceKind::Script,
        ResourceType::Xhr => ResourceKind::Xhr,
        ResourceType::Fetch => ResourceKind::Fetch,
        ResourceType::Image => ResourceKind::Image,
        ResourceType::Media => ResourceKind::Media,
        ResourceType::Font => ResourceKind::Font,
        _ => ResourceKind::Other,
    }
}

fn paused(ev: &EventRequestPaused) -> PausedRequest {
    let headers = ev.request.headers.inner();
    let accept = headers
        .get("Accept")
        .or_else(|| headers.get("accept"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    PausedRequest {
        id: ev.request_id.inner().clone(),
        url: ev.request.url.clone(),
        method: ev.request.method.clone(),
        kind: kind(&ev.resource_type),
        accept,
    }
}

#[async_trait]
impl CdpBrowser for ChromiumBrowser {
    async fn open_tab(
        &self,
    ) -> Result<(Box<dyn CdpTab>, UnboundedReceiver<PausedRequest>), CdpError> {
        let browser = self.browser().await?;
        let ctx = browser
            .execute(CreateBrowserContextParams::default())
            .await
            .map_err(proto)?
            .result
            .browser_context_id;
        match open_in(&browser, &ctx).await {
            Ok((page, rx, forward)) => Ok((
                Box::new(ChromiumTab {
                    page,
                    browser,
                    ctx,
                    forward,
                }),
                rx,
            )),
            Err(e) => {
                let _ = browser.execute(DisposeBrowserContextParams::new(ctx)).await;
                Err(e)
            }
        }
    }
}

async fn open_in(
    browser: &Browser,
    ctx: &BrowserContextId,
) -> Result<(Page, UnboundedReceiver<PausedRequest>, JoinHandle<()>), CdpError> {
    let params = CreateTargetParams::builder()
        .url("about:blank")
        .browser_context_id(ctx.clone())
        .build()
        .map_err(CdpError::Protocol)?;
    let page = browser.new_page(params).await.map_err(proto)?;
    let mut events = page
        .event_listener::<EventRequestPaused>()
        .await
        .map_err(proto)?;
    let pattern = RequestPattern::builder()
        .url_pattern("*")
        .request_stage(RequestStage::Request)
        .build();
    page.execute(fetch::EnableParams::builder().pattern(pattern).build())
        .await
        .map_err(proto)?;
    let (tx, rx) = unbounded_channel();
    let forward = tokio::spawn(async move {
        while let Some(ev) = events.next().await {
            if tx.send(paused(&ev)).is_err() {
                break;
            }
        }
    });
    Ok((page, rx, forward))
}

struct ChromiumTab {
    page: Page,
    browser: Arc<Browser>,
    ctx: BrowserContextId,
    forward: JoinHandle<()>,
}

/// A JavaScript string literal for `s` (selectors come from source specs, never from pages,
/// but are quoted defensively anyway).
pub(crate) fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '<' => out.push_str("\\u003c"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl ChromiumTab {
    async fn eval<T: serde::de::DeserializeOwned>(&self, js: String) -> Result<T, CdpError> {
        self.page
            .evaluate(js)
            .await
            .map_err(proto)?
            .into_value::<T>()
            .map_err(proto)
    }
}

#[async_trait]
impl CdpTab for ChromiumTab {
    async fn set_user_agent(&self, ua: &str) -> Result<(), CdpError> {
        self.page
            .execute(SetUserAgentOverrideParams::new(ua))
            .await
            .map_err(proto)?;
        Ok(())
    }

    async fn navigate(&self, url: &str) -> Result<(), CdpError> {
        let r = self
            .page
            .execute(NavigateParams::new(url))
            .await
            .map_err(proto)?;
        match &r.result.error_text {
            Some(t) if !t.is_empty() => Err(CdpError::Navigation(t.clone())),
            _ => Ok(()),
        }
    }

    async fn scroll_to_bottom(&self) -> Result<u64, CdpError> {
        let js = "(() => { const e = document.scrollingElement || document.documentElement; \
                  const h = e ? e.scrollHeight : 0; window.scrollTo(0, h); \
                  return Math.round(h); })()";
        self.eval(js.to_string()).await
    }

    async fn click_all(&self, selector: &str) -> Result<u32, CdpError> {
        let js = format!(
            "(() => {{ let n = 0; let els = []; \
               try {{ els = document.querySelectorAll({}); }} catch (_) {{ return 0; }} \
               for (const el of els) {{ try {{ el.click(); n++; }} catch (_) {{}} }} \
               return n; }})()",
            js_string(selector)
        );
        self.eval(js).await
    }

    async fn content(&self) -> Result<String, CdpError> {
        self.page.content().await.map_err(proto)
    }

    async fn fulfill(&self, id: &str, response: Fulfilment) -> Result<(), CdpError> {
        let headers = response
            .headers
            .into_iter()
            .map(|(name, value)| HeaderEntry { name, value })
            .collect();
        let mut params = FulfillRequestParams::new(id.to_string(), i64::from(response.status));
        params.response_headers = Some(headers);
        params.body = Some(Binary::from(b64::encode(&response.body)));
        self.page.execute(params).await.map_err(proto)?;
        Ok(())
    }

    async fn fail(&self, id: &str, reason: &BlockReason) -> Result<(), CdpError> {
        let r = match reason {
            BlockReason::Budget => ErrorReason::TimedOut,
            BlockReason::FetchFailed => ErrorReason::Failed,
            _ => ErrorReason::BlockedByClient,
        };
        self.page
            .execute(FailRequestParams::new(id.to_string(), r))
            .await
            .map_err(proto)?;
        Ok(())
    }

    async fn close(&self) {
        self.forward.abort();
        if let Err(e) = self.page.clone().close().await {
            debug!(error = %e, "render: close tab");
        }
        if let Err(e) = self
            .browser
            .execute(DisposeBrowserContextParams::new(self.ctx.clone()))
            .await
        {
            warn!(error = %e, "render: dispose browser context");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_string_escapes() {
        assert_eq!(js_string("button.more"), "\"button.more\"");
        assert_eq!(js_string("a[x=\"y\"]"), "\"a[x=\\\"y\\\"]\"");
        assert_eq!(js_string("\\"), "\"\\\\\"");
        assert_eq!(js_string("</script>"), "\"\\u003c/script>\"");
        assert_eq!(js_string("a\nb\u{1}"), "\"a\\nb\\u0001\"");
    }

    #[tokio::test]
    async fn cdp_url_ip_and_localhost_kept() {
        assert_eq!(
            resolve_cdp_url("http://127.0.0.1:9222").await.unwrap(),
            "http://127.0.0.1:9222"
        );
        assert_eq!(
            resolve_cdp_url("http://localhost:9222").await.unwrap(),
            "http://localhost:9222"
        );
        assert!(resolve_cdp_url("not a url").await.is_err());
    }

    #[test]
    fn resource_types_mapped() {
        assert_eq!(kind(&ResourceType::Xhr), ResourceKind::Xhr);
        assert_eq!(kind(&ResourceType::Ping), ResourceKind::Other);
        assert_eq!(kind(&ResourceType::Preflight), ResourceKind::Other);
        assert_eq!(kind(&ResourceType::Font), ResourceKind::Font);
    }
}
