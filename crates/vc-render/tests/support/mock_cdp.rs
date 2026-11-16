//! A scripted mock of the CDP slice (`CdpBrowser`/`CdpTab`). It behaves like an intercepted
//! Chromium: navigation pauses the document request and resolves when it is answered; answering
//! a resource "runs" it, which pauses the follow-up requests scripted for that URL; XHR/fetch
//! bodies are appended to the DOM (an SPA filling itself in).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;
use vc_core::render::BlockReason;
use vc_render::cdp::{CdpBrowser, CdpError, CdpTab, Fulfilment, PausedRequest, ResourceKind};

/// What the page does.
#[derive(Debug, Clone, Default)]
pub struct Script {
    /// When a URL is answered, these requests are made (in order).
    pub on_load: HashMap<String, Vec<Req>>,
    /// Requests made on the first scroll to the bottom.
    pub on_scroll: Vec<Req>,
    /// Requests made when a selector is clicked.
    pub on_click: HashMap<String, Vec<Req>>,
    /// URLs that re-request themselves after each answer (polling), after this pause.
    pub poll: HashMap<String, Duration>,
}

#[derive(Debug, Clone)]
pub struct Req {
    pub url: String,
    pub kind: ResourceKind,
    pub method: String,
}

pub fn req(url: &str, kind: ResourceKind) -> Req {
    Req {
        url: url.into(),
        kind,
        method: "GET".into(),
    }
}

impl Script {
    pub fn load(mut self, url: &str, reqs: Vec<Req>) -> Self {
        self.on_load.insert(url.into(), reqs);
        self
    }
}

#[derive(Debug, Default, Clone)]
pub struct MockLog {
    pub user_agents: Vec<String>,
    pub paused: Vec<String>,
    pub fulfilled: Vec<(String, u16)>,
    pub failed: Vec<(String, BlockReason)>,
    pub clicks: Vec<String>,
    pub scrolls: u32,
    pub tabs_opened: u32,
    pub tabs_closed: u32,
}

pub struct MockBrowser {
    pub script: Script,
    pub log: Arc<Mutex<MockLog>>,
}

impl MockBrowser {
    pub fn new(script: Script) -> Arc<Self> {
        Arc::new(MockBrowser {
            script,
            log: Arc::new(Mutex::new(MockLog::default())),
        })
    }

    pub fn log(&self) -> MockLog {
        self.log.lock().unwrap().clone()
    }
}

#[async_trait]
impl CdpBrowser for MockBrowser {
    async fn open_tab(
        &self,
    ) -> Result<(Box<dyn CdpTab>, UnboundedReceiver<PausedRequest>), CdpError> {
        let (tx, rx) = unbounded_channel();
        self.log.lock().unwrap().tabs_opened += 1;
        let tab = MockTab {
            st: Arc::new(TabState {
                script: self.script.clone(),
                log: self.log.clone(),
                tx,
                next: AtomicU64::new(1),
                requests: Mutex::new(HashMap::new()),
                dom: Mutex::new(String::new()),
                xhr: Mutex::new(Vec::new()),
                nav: Mutex::new(None),
                scrolled: Mutex::new(false),
            }),
        };
        Ok((Box::new(tab), rx))
    }
}

struct TabState {
    script: Script,
    log: Arc<Mutex<MockLog>>,
    tx: UnboundedSender<PausedRequest>,
    next: AtomicU64,
    requests: Mutex<HashMap<String, Req>>,
    dom: Mutex<String>,
    xhr: Mutex<Vec<String>>,
    nav: Mutex<Option<oneshot::Sender<Result<(), CdpError>>>>,
    scrolled: Mutex<bool>,
}

impl TabState {
    fn emit(&self, r: &Req) {
        let id = format!("req-{}", self.next.fetch_add(1, Ordering::SeqCst));
        self.requests.lock().unwrap().insert(id.clone(), r.clone());
        self.log.lock().unwrap().paused.push(r.url.clone());
        let _ = self.tx.send(PausedRequest {
            id,
            url: r.url.clone(),
            method: r.method.clone(),
            kind: r.kind,
            accept: Some("*/*".into()),
        });
    }

    fn finish_nav(&self, res: Result<(), CdpError>) {
        if let Some(tx) = self.nav.lock().unwrap().take() {
            let _ = tx.send(res);
        }
    }
}

struct MockTab {
    st: Arc<TabState>,
}

#[async_trait]
impl CdpTab for MockTab {
    async fn set_user_agent(&self, ua: &str) -> Result<(), CdpError> {
        self.st.log.lock().unwrap().user_agents.push(ua.into());
        Ok(())
    }

    async fn navigate(&self, url: &str) -> Result<(), CdpError> {
        let (tx, rx) = oneshot::channel();
        *self.st.nav.lock().unwrap() = Some(tx);
        self.st.emit(&req(url, ResourceKind::Document));
        rx.await
            .unwrap_or_else(|_| Err(CdpError::Protocol("tab closed".into())))
    }

    async fn scroll_to_bottom(&self) -> Result<u64, CdpError> {
        let first = {
            let mut s = self.st.scrolled.lock().unwrap();
            let f = !*s;
            *s = true;
            f
        };
        self.st.log.lock().unwrap().scrolls += 1;
        if first {
            for r in &self.st.script.on_scroll {
                self.st.emit(r);
            }
        }
        Ok(1000 + 100 * self.st.xhr.lock().unwrap().len() as u64)
    }

    async fn click_all(&self, selector: &str) -> Result<u32, CdpError> {
        self.st.log.lock().unwrap().clicks.push(selector.into());
        match self.st.script.on_click.get(selector) {
            Some(reqs) => {
                for r in reqs {
                    self.st.emit(r);
                }
                Ok(1)
            }
            None => Ok(0),
        }
    }

    async fn content(&self) -> Result<String, CdpError> {
        let dom = self.st.dom.lock().unwrap().clone();
        let xhr = self.st.xhr.lock().unwrap().join("\n");
        Ok(format!("{dom}\n{xhr}"))
    }

    async fn fulfill(&self, id: &str, response: Fulfilment) -> Result<(), CdpError> {
        let r = self
            .st
            .requests
            .lock()
            .unwrap()
            .remove(id)
            .ok_or_else(|| CdpError::Protocol(format!("unknown request {id}")))?;
        self.st
            .log
            .lock()
            .unwrap()
            .fulfilled
            .push((r.url.clone(), response.status));
        let body = String::from_utf8_lossy(&response.body).to_string();
        match r.kind {
            ResourceKind::Document if (300..400).contains(&response.status) => {
                let loc = response
                    .headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("location"))
                    .map(|(_, v)| v.clone())
                    .ok_or_else(|| CdpError::Protocol("redirect without location".into()))?;
                self.st.emit(&req(&loc, ResourceKind::Document));
                return Ok(());
            }
            ResourceKind::Document => {
                *self.st.dom.lock().unwrap() = body;
                self.st.finish_nav(Ok(()));
            }
            ResourceKind::Xhr | ResourceKind::Fetch => self.st.xhr.lock().unwrap().push(body),
            _ => {}
        }
        if let Some(reqs) = self.st.script.on_load.get(&r.url) {
            for f in reqs {
                self.st.emit(f);
            }
        }
        if let Some(pause) = self.st.script.poll.get(&r.url).copied() {
            let st = self.st.clone();
            tokio::spawn(async move {
                tokio::time::sleep(pause).await;
                st.emit(&r);
            });
        }
        Ok(())
    }

    async fn fail(&self, id: &str, reason: &BlockReason) -> Result<(), CdpError> {
        let r = self
            .st
            .requests
            .lock()
            .unwrap()
            .remove(id)
            .ok_or_else(|| CdpError::Protocol(format!("unknown request {id}")))?;
        self.st
            .log
            .lock()
            .unwrap()
            .failed
            .push((r.url.clone(), reason.clone()));
        if r.kind == ResourceKind::Document {
            self.st.finish_nav(Err(CdpError::Navigation(
                "net::ERR_BLOCKED_BY_CLIENT".into(),
            )));
        }
        Ok(())
    }

    async fn close(&self) {
        self.st.log.lock().unwrap().tabs_closed += 1;
    }
}
