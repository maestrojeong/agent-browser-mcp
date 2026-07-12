//! agent-browser MCP server.
//!
//! Exposes the ab-browser core as `browser_*` MCP tools over stdio. No agent,
//! no LLM — just the browser, driven by whatever MCP client connects.
//!
//! Core loop the tools encode: **snapshot -> act -> verify**.

use std::collections::HashMap;
use std::sync::Arc;

use ab_browser::{Browser, LaunchOptions, NetworkLog, Page};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::info;

const INSTRUCTIONS: &str = r#"agent-browser — a real Chrome driven over CDP, no bundled agent.

Loop: browser_navigate -> browser_snapshot -> act (click/type) -> re-snapshot to verify.
- snapshot renders the page as an accessibility tree; interactive nodes carry [ref=eN] handles.
- act on them by ref with browser_click / browser_type / browser_press.
- refs go stale when the page changes — re-snapshot before reusing them.
- browser_evaluate runs one-shot JS; browser_screenshot saves a PNG.
Stealth: this browser never enables the detectable CDP domains (no Runtime.enable)."#;

struct PageEntry {
    page: Page,
    refs: HashMap<String, i64>,
    last_text: String,
    netlog: Option<NetworkLog>,
}

/// Order-insensitive line diff: what appeared / disappeared between snapshots.
/// Cheap post-action signal — trims noise so the agent sees only the delta.
fn snapshot_diff(old: &str, new: &str) -> String {
    use std::collections::HashSet;
    let old_lines: HashSet<&str> = old.lines().map(str::trim).collect();
    let new_lines: HashSet<&str> = new.lines().map(str::trim).collect();
    let mut out = String::new();
    for line in new.lines() {
        let t = line.trim();
        if !t.is_empty() && !old_lines.contains(t) {
            out.push_str("+ ");
            out.push_str(line);
            out.push('\n');
        }
    }
    for line in old.lines() {
        let t = line.trim();
        if !t.is_empty() && !new_lines.contains(t) {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
        }
    }
    if out.is_empty() {
        "(no visible change)".to_string()
    } else {
        out
    }
}

#[derive(Default)]
struct State {
    browser: Option<Browser>,
    pages: HashMap<String, PageEntry>,
    next: u64,
}

#[derive(Clone)]
struct BrowserServer {
    state: Arc<Mutex<State>>,
    tool_router: ToolRouter<Self>,
}

// ---- tool parameter schemas ----

#[derive(Debug, Deserialize, JsonSchema)]
struct NavigateArgs {
    /// URL to open (a new tab is created).
    url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PageArg {
    /// Page id returned by browser_navigate (e.g. "p1").
    page: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RefArgs {
    /// Page id (e.g. "p1").
    page: String,
    /// Element ref from the latest snapshot (e.g. "e3").
    #[serde(rename = "ref")]
    ref_: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TypeArgs {
    page: String,
    #[serde(rename = "ref")]
    ref_: String,
    /// Text to type into the focused element.
    text: String,
    /// Replace existing content instead of appending.
    #[serde(default)]
    clear: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PressArgs {
    page: String,
    /// Key name: Enter, Tab, Escape, Backspace, ArrowUp, ArrowDown, or a character.
    key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EvalArgs {
    page: String,
    /// JavaScript expression evaluated in page context.
    expression: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SelectArgs {
    page: String,
    #[serde(rename = "ref")]
    ref_: String,
    /// The option value to select.
    value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitArgs {
    page: String,
    /// Wait until this text appears anywhere on the page.
    #[serde(default)]
    text: Option<String>,
    /// Wait until this CSS selector matches.
    #[serde(default)]
    selector: Option<String>,
    /// Timeout in milliseconds (default 10000).
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NetArgs {
    page: String,
    /// Only include requests whose URL contains this substring.
    #[serde(default)]
    filter: Option<String>,
    /// Max entries to return (default 100).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct BlockArgs {
    page: String,
    /// URL wildcard patterns to block (e.g. "*.png", "*doubleclick*").
    patterns: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct StorageArgs {
    page: String,
    /// File path to save to / load from (JSON: cookies + localStorage).
    path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FormField {
    #[serde(rename = "ref")]
    ref_: String,
    value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct FillFormArgs {
    page: String,
    /// Fields to fill: each { ref, value }. Existing content is replaced.
    fields: Vec<FormField>,
}

/// Build the browser per environment. Default: headful, real profile, no JS
/// patching (fingerprint == a real human Chrome). Overrides:
///   AB_CONNECT=<port>  attach to a Chrome the user already launched (strongest)
///   AB_HEADLESS=1      run headless (a tell; enable AB_STEALTH to compensate)
///   AB_STEALTH=1       inject the JS stealth-patch fallback (headless only)
///   AB_PROFILE=<dir>   persistent profile location
async fn make_browser() -> ab_browser::Result<Browser> {
    if let Ok(port) = std::env::var("AB_CONNECT") {
        return Browser::connect(port.trim().parse().unwrap_or(9222)).await;
    }
    Browser::launch(LaunchOptions {
        headless: std::env::var("AB_HEADLESS").is_ok(),
        inject_stealth: std::env::var("AB_STEALTH").is_ok(),
        ..Default::default()
    })
    .await
}

fn ok(s: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(s.into())])
}

fn fail<E: std::fmt::Display>(e: E) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

impl BrowserServer {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            tool_router: Self::tool_router(),
        }
    }

    /// Clone the Page for a given id (does not hold the lock across ops).
    async fn page_of(&self, id: &str) -> Result<Page, McpError> {
        let st = self.state.lock().await;
        st.pages
            .get(id)
            .map(|e| e.page.clone())
            .ok_or_else(|| fail(format!("unknown page '{id}'")))
    }

    async fn backend_of(&self, id: &str, ref_: &str) -> Result<i64, McpError> {
        let st = self.state.lock().await;
        let entry = st
            .pages
            .get(id)
            .ok_or_else(|| fail(format!("unknown page '{id}'")))?;
        entry
            .refs
            .get(ref_)
            .copied()
            .ok_or_else(|| fail(format!("unknown ref '{ref_}' (re-snapshot?)")))
    }

    /// Persist a fresh snapshot (refs + text) for a page.
    async fn store_snapshot(&self, id: &str, refs: HashMap<String, i64>, text: String) {
        let mut st = self.state.lock().await;
        if let Some(e) = st.pages.get_mut(id) {
            e.refs = refs;
            e.last_text = text;
        }
    }

    async fn last_text(&self, id: &str) -> String {
        let st = self.state.lock().await;
        st.pages.get(id).map(|e| e.last_text.clone()).unwrap_or_default()
    }

    async fn netlog_of(&self, id: &str) -> Option<NetworkLog> {
        let st = self.state.lock().await;
        st.pages.get(id).and_then(|e| e.netlog.clone())
    }

    /// After an action: wait for settle, re-snapshot, diff vs the previous
    /// snapshot, persist the new one, and return the diff for the agent.
    async fn settle_diff(&self, id: &str, page: &Page) -> Result<String, McpError> {
        let before = self.last_text(id).await;
        page.settle().await;
        let snap = page.snapshot().await.map_err(fail)?;
        let diff = snapshot_diff(&before, &snap.text);
        self.store_snapshot(id, snap.refs, snap.text).await;
        Ok(diff)
    }
}

#[tool_router(router = tool_router)]
impl BrowserServer {
    /// Open a URL in a new tab and return its page id plus an accessibility snapshot.
    #[tool(description = "Open a URL in a new browser tab; returns page id + snapshot")]
    async fn browser_navigate(
        &self,
        Parameters(a): Parameters<NavigateArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().await;
        if st.browser.is_none() {
            let b = make_browser().await.map_err(fail)?;
            st.browser = Some(b);
        }
        // Blank page first so the network log captures the navigation itself.
        let page = st
            .browser
            .as_ref()
            .unwrap()
            .new_page("about:blank")
            .await
            .map_err(fail)?;
        let netlog = page.enable_network_log().await.ok();
        page.navigate(&a.url).await.map_err(fail)?;
        let snap = page.snapshot().await.map_err(fail)?;
        st.next += 1;
        let id = format!("p{}", st.next);
        st.pages.insert(
            id.clone(),
            PageEntry {
                page,
                refs: snap.refs.clone(),
                last_text: snap.text.clone(),
                netlog,
            },
        );
        Ok(ok(format!("page {id}\nurl {}\n\n{}", a.url, snap.text)))
    }

    /// Re-render the accessibility snapshot for a page (refreshes [ref] handles).
    #[tool(description = "Accessibility-tree snapshot of a page, with [ref] handles")]
    async fn browser_snapshot(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let snap = page.snapshot().await.map_err(fail)?;
        self.store_snapshot(&a.page, snap.refs.clone(), snap.text.clone())
            .await;
        Ok(ok(format!("page {}\n\n{}", a.page, snap.text)))
    }

    /// Click an element by its snapshot ref, then report what changed.
    #[tool(description = "Click an element by ref (synthesized mouse click); returns settle-diff")]
    async fn browser_click(
        &self,
        Parameters(a): Parameters<RefArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.backend_of(&a.page, &a.ref_).await?;
        let page = self.page_of(&a.page).await?;
        page.click(backend).await.map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("clicked {} on {}\n\n{}", a.ref_, a.page, diff)))
    }

    /// Type text into an element by ref (optionally clearing it first).
    #[tool(description = "Type text into an element by ref; returns settle-diff")]
    async fn browser_type(
        &self,
        Parameters(a): Parameters<TypeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.backend_of(&a.page, &a.ref_).await?;
        let page = self.page_of(&a.page).await?;
        page.type_text(backend, &a.text, a.clear)
            .await
            .map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("typed into {} on {}\n\n{}", a.ref_, a.page, diff)))
    }

    /// Press a named key on a page, then report what changed.
    #[tool(description = "Press a key (Enter, Tab, Escape, ...); returns settle-diff")]
    async fn browser_press(
        &self,
        Parameters(a): Parameters<PressArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.press(&a.key).await.map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("pressed {} on {}\n\n{}", a.key, a.page, diff)))
    }

    /// List recent network requests for a page (URL, method, status).
    #[tool(description = "List recent network requests (url, method, status)")]
    async fn browser_network_requests(
        &self,
        Parameters(a): Parameters<NetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let log = self
            .netlog_of(&a.page)
            .await
            .ok_or_else(|| fail(format!("no network log for '{}'", a.page)))?;
        let entries = log.recent(a.limit.unwrap_or(100), a.filter.as_deref());
        if entries.is_empty() {
            return Ok(ok("(no requests)".to_string()));
        }
        let mut out = String::new();
        for e in &entries {
            let status = if e.failed {
                "FAIL".to_string()
            } else {
                e.status.map(|s| s.to_string()).unwrap_or_else(|| "…".into())
            };
            out.push_str(&format!(
                "{:>4} {:<6} {:<10} {}\n",
                status, e.method, e.resource_type, e.url
            ));
        }
        Ok(ok(out))
    }

    /// Block requests matching URL wildcard patterns (ads, trackers, media).
    #[tool(description = "Block requests by URL wildcard patterns (e.g. *.png, *doubleclick*)")]
    async fn browser_route_block(
        &self,
        Parameters(a): Parameters<BlockArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.set_blocked_urls(&a.patterns).await.map_err(fail)?;
        Ok(ok(format!(
            "blocking {} pattern(s) on {}: {}",
            a.patterns.len(),
            a.page,
            a.patterns.join(", ")
        )))
    }

    /// Save cookies + localStorage of a page to a JSON file (session capture).
    #[tool(description = "Save cookies + localStorage to a JSON file")]
    async fn browser_storage_save(
        &self,
        Parameters(a): Parameters<StorageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let cookies = page.cookies().await.map_err(fail)?;
        let local = page.local_storage().await.unwrap_or(serde_json::json!({}));
        let blob = serde_json::json!({ "cookies": cookies, "localStorage": local });
        tokio::fs::write(&a.path, serde_json::to_vec_pretty(&blob).unwrap_or_default())
            .await
            .map_err(fail)?;
        let n = cookies.as_array().map(|c| c.len()).unwrap_or(0);
        Ok(ok(format!("saved {n} cookies + localStorage to {}", a.path)))
    }

    /// Restore cookies + localStorage from a JSON file (re-auth a session).
    #[tool(description = "Load cookies + localStorage from a JSON file")]
    async fn browser_storage_load(
        &self,
        Parameters(a): Parameters<StorageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let raw = tokio::fs::read(&a.path).await.map_err(fail)?;
        let blob: serde_json::Value = serde_json::from_slice(&raw).map_err(fail)?;
        if let Some(cookies) = blob.get("cookies") {
            page.set_cookies(cookies).await.map_err(fail)?;
        }
        if let Some(local) = blob.get("localStorage") {
            let _ = page.set_local_storage(local).await;
        }
        Ok(ok(format!("loaded session from {} (reload the page to apply)", a.path)))
    }

    /// Hover the pointer over an element by ref.
    #[tool(description = "Hover an element by ref; returns settle-diff")]
    async fn browser_hover(
        &self,
        Parameters(a): Parameters<RefArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.backend_of(&a.page, &a.ref_).await?;
        let page = self.page_of(&a.page).await?;
        page.hover(backend).await.map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("hovered {} on {}\n\n{}", a.ref_, a.page, diff)))
    }

    /// Select an <option> in a dropdown by ref + value.
    #[tool(description = "Select a dropdown option by ref and value; returns settle-diff")]
    async fn browser_select(
        &self,
        Parameters(a): Parameters<SelectArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.backend_of(&a.page, &a.ref_).await?;
        let page = self.page_of(&a.page).await?;
        page.select_option(backend, &a.value).await.map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("selected {:?} in {} on {}\n\n{}", a.value, a.ref_, a.page, diff)))
    }

    /// Navigate back one entry in the page's history.
    #[tool(description = "Go back one history entry; returns settle-diff")]
    async fn browser_back(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        page.go_back().await.map_err(fail)?;
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("went back on {}\n\n{}", a.page, diff)))
    }

    /// Wait until text appears or a selector matches (whichever is given).
    #[tool(description = "Wait for text or a CSS selector to appear")]
    async fn browser_wait(
        &self,
        Parameters(a): Parameters<WaitArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let ms = a.timeout_ms.unwrap_or(10_000);
        let (found, what) = if let Some(t) = &a.text {
            (page.wait_for_text(t, ms).await.map_err(fail)?, format!("text {t:?}"))
        } else if let Some(s) = &a.selector {
            (page.wait_for_selector(s, ms).await.map_err(fail)?, format!("selector {s:?}"))
        } else {
            return Err(fail("provide `text` or `selector`"));
        };
        Ok(ok(format!(
            "{} {} on {}",
            if found { "found" } else { "TIMEOUT waiting for" },
            what,
            a.page
        )))
    }

    /// Run one-shot JavaScript in page context (no Runtime.enable).
    #[tool(description = "Evaluate a JavaScript expression in page context")]
    async fn browser_evaluate(
        &self,
        Parameters(a): Parameters<EvalArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let v = page.evaluate(&a.expression).await.map_err(fail)?;
        Ok(ok(serde_json::to_string(&v).unwrap_or_else(|_| "null".into())))
    }

    /// Extract the page as Markdown (headings, links, lists, code).
    #[tool(description = "Read the page as Markdown (token-efficient content extract)")]
    async fn browser_read(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let md = page.read_markdown().await.map_err(fail)?;
        Ok(ok(md))
    }

    /// Fill several fields in one call (each replaces existing content).
    #[tool(description = "Fill multiple fields at once by ref; returns settle-diff")]
    async fn browser_fill_form(
        &self,
        Parameters(a): Parameters<FillFormArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let mut done = 0;
        for f in &a.fields {
            let backend = self.backend_of(&a.page, &f.ref_).await?;
            page.type_text(backend, &f.value, true).await.map_err(fail)?;
            done += 1;
        }
        let diff = self.settle_diff(&a.page, &page).await?;
        Ok(ok(format!("filled {done} field(s) on {}\n\n{}", a.page, diff)))
    }

    /// Save the page as a PDF file; returns the path. (Headless mode only.)
    #[tool(description = "Save the page as a PDF file (headless mode only); returns the path")]
    async fn browser_pdf(
        &self,
        Parameters(a): Parameters<StorageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let bytes = page.pdf().await.map_err(fail)?;
        tokio::fs::write(&a.path, &bytes).await.map_err(fail)?;
        Ok(ok(format!("{} ({} bytes)", a.path, bytes.len())))
    }

    /// Return the page's full serialized HTML.
    #[tool(description = "Get the page's full HTML (document.documentElement.outerHTML)")]
    async fn browser_get_html(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let mut html = page.html().await.map_err(fail)?;
        const MAX: usize = 200_000;
        if html.len() > MAX {
            html.truncate(MAX);
            html.push_str("\n… (truncated)");
        }
        Ok(ok(html))
    }

    /// Save a full-page PNG screenshot to a temp file; returns its path.
    #[tool(description = "Capture a full-page PNG screenshot; returns the file path")]
    async fn browser_screenshot(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let png = page.screenshot().await.map_err(fail)?;
        let path = std::env::temp_dir().join(format!("ab-{}.png", a.page));
        tokio::fs::write(&path, &png).await.map_err(fail)?;
        Ok(ok(format!("{} ({} bytes)", path.display(), png.len())))
    }

    /// List open pages.
    #[tool(description = "List open page ids")]
    async fn browser_tabs(&self) -> Result<CallToolResult, McpError> {
        let st = self.state.lock().await;
        let ids: Vec<String> = st.pages.keys().cloned().collect();
        Ok(ok(if ids.is_empty() {
            "(no open pages)".to_string()
        } else {
            ids.join(", ")
        }))
    }

    /// Close a page and forget its refs.
    #[tool(description = "Close a page by id")]
    async fn browser_close_page(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().await;
        if st.pages.remove(&a.page).is_some() {
            Ok(ok(format!("closed {}", a.page)))
        } else {
            Err(fail(format!("unknown page '{}'", a.page)))
        }
    }

    /// Probe the page for common automation fingerprints and grade the stealth.
    #[tool(description = "Self-test: report automation fingerprints visible to the page")]
    async fn browser_fingerprint_check(
        &self,
        Parameters(a): Parameters<PageArg>,
    ) -> Result<CallToolResult, McpError> {
        let page = self.page_of(&a.page).await?;
        let js = r#"JSON.stringify({
            webdriver: navigator.webdriver === undefined ? 'undefined' : String(navigator.webdriver),
            plugins: navigator.plugins.length,
            languages: (navigator.languages || []).join(','),
            hasChrome: !!window.chrome,
            hasChromeRuntime: !!(window.chrome && window.chrome.runtime),
            headlessUA: /headless/i.test(navigator.userAgent),
            userAgent: navigator.userAgent
        })"#;
        let raw = page.evaluate(js).await.map_err(fail)?;
        let s = raw.as_str().unwrap_or("{}");
        let v: serde_json::Value = serde_json::from_str(s).unwrap_or(serde_json::Value::Null);

        let mut report = String::from("fingerprint check\n");
        let mut checks: Vec<(bool, String)> = Vec::new();
        let get = |k: &str| v.get(k).cloned().unwrap_or(serde_json::Value::Null);

        let wd = get("webdriver");
        checks.push((
            wd.as_str() == Some("undefined"),
            format!("navigator.webdriver = {wd}"),
        ));
        let plugins = get("plugins").as_u64().unwrap_or(0);
        checks.push((plugins > 0, format!("navigator.plugins = {plugins}")));
        let langs = get("languages");
        checks.push((
            langs.as_str().map(|x| !x.is_empty()).unwrap_or(false),
            format!("navigator.languages = {langs}"),
        ));
        checks.push((get("hasChrome").as_bool().unwrap_or(false), "window.chrome present".into()));
        let headless = get("headlessUA").as_bool().unwrap_or(false);
        checks.push((!headless, format!("headless in UA = {headless}")));

        let mut passed = 0;
        for (good, label) in &checks {
            report.push_str(if *good { "  ✓ " } else { "  ✗ " });
            report.push_str(label);
            report.push('\n');
            if *good {
                passed += 1;
            }
        }
        report.push_str(&format!("\nscore: {passed}/{} passed", checks.len()));
        Ok(ok(report))
    }
}

#[tool_handler(router = self.tool_router)]
impl rmcp::ServerHandler for BrowserServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        info.server_info.name = "agent-browser".to_string();
        info.server_info.version = env!("CARGO_PKG_VERSION").to_string();
        info.instructions = Some(INSTRUCTIONS.to_string());
        info
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ab_mcp=info,ab_browser=info,ab_cdp=warn".into()),
        )
        .init();

    info!("agent-browser MCP server starting on stdio");
    let service = BrowserServer::new().serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
