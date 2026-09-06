use std::error::Error;
use std::io::Read;
use std::sync::{OnceLock, mpsc::Sender};
use std::thread;
use std::time::Instant;

use crate::browser::http_cache::{MAX_FIELD_BYTES, Metadata, Representation};
use crate::css;
use crate::html;
use crate::image;
use crate::msg::Msg;
use crate::net::{Method, PageId, Request};

/// Read size per chunk: small enough that progress messages arrive steadily
/// on slow links, large enough that syscall overhead is irrelevant.
const CHUNK: usize = 16 * 1024;
const ACCEPT_DOCUMENT: &str =
    "text/html,application/xhtml+xml,application/xml;q=0.9,text/plain;q=0.8,*/*;q=0.5";
const ACCEPT_STYLESHEET: &str = "text/css,*/*;q=0.1";
const ACCEPT_SCRIPT: &str =
    "text/javascript,application/javascript,application/ecmascript,*/*;q=0.1";
const ACCEPT_IMAGE: &str = "image/webp,image/png,image/jpeg,image/gif,*/*;q=0.1";

/// Fetch `url` on a detached worker thread; returns immediately. The worker
/// talks to the rest of the program only by sending `Msg`s into `tx`:
/// - progress: zero or more `Loading`
/// - terminal success: always one `Loaded` (so `--dump` can print any body)
/// - parse: one `Parsed` only when [`is_document`] is true (2xx + document
///   content-type). Non-documents and HTTP errors skip `Parsed` so the TUI
///   error page is not clobbered (M7).
/// - failure: one `NetError` (DNS, TLS, connect, mid-body disconnect)
///
/// It never panics and never prints; if the channel is closed (the app quit),
/// it just stops.
///
/// Parsing lives here, not in the `Loaded` handler: the worker already owns
/// the bytes and already runs off the UI thread, and a Wikipedia-sized parse
/// (~tens of ms) would blow the keypress→screen budget if the UI thread did
/// it. `Loaded` goes out first so a document body shows without waiting on
/// the parse.
pub fn spawn_fetch(id: PageId, request: Request, tx: Sender<Msg>) {
    thread::spawn(move || {
        match fetch(id, &request, &tx) {
            // A hop is terminal for *this* worker: it reports where the
            // response pointed and stops. Whether to go there — and with which
            // cookies — is the event loop's to decide (M11.7a).
            Ok(Some(hop @ Msg::Redirect { .. })) => {
                let _ = tx.send(hop);
            }
            Ok(Some(loaded)) => {
                let Msg::Loaded {
                    body,
                    status,
                    content_type,
                    ..
                } = &loaded
                else {
                    unreachable!("fetch's success message is Loaded or Redirect");
                };
                let should_parse = is_document(*status, content_type.as_deref());
                let text = should_parse.then(|| html::decode_body(body));
                if tx.send(loaded).is_err() {
                    return;
                }
                if let Some(text) = text {
                    let started = Instant::now();
                    let dom = html::parse(&text);
                    let _ = tx.send(Msg::Parsed {
                        id,
                        dom,
                        elapsed: started.elapsed(),
                    });
                }
            }
            // Channel closed mid-stream: nobody is listening anymore.
            Ok(None) => {}
            Err((url, reason)) => {
                let _ = tx.send(Msg::NetError { id, url, reason });
            }
        }
    });
}

/// Re-enter stored bytes through the same asynchronous document seam as a
/// network response. `Loaded` is sent before parsing begins, and `Parsed`
/// follows exactly once when the response is a supported document.
pub fn spawn_cached(
    id: PageId,
    url: String,
    response: Representation,
    elapsed: std::time::Duration,
    tx: Sender<Msg>,
) {
    thread::spawn(move || {
        let should_parse = is_document(response.status, response.content_type.as_deref());
        let text = should_parse.then(|| html::decode_body(&response.body));
        if tx
            .send(Msg::Loaded {
                id,
                url,
                status: response.status,
                body: response.body,
                elapsed,
                content_type: response.content_type,
                set_cookie: Vec::new(),
                metadata: response.metadata,
            })
            .is_err()
        {
            return;
        }
        if let Some(text) = text {
            let started = Instant::now();
            let dom = html::parse(&text);
            let _ = tx.send(Msg::Parsed {
                id,
                dom,
                elapsed: started.elapsed(),
            });
        }
    });
}

/// Whether a response should be parsed and styled as a document (vs an error
/// page). Charset parameters are ignored. Missing/empty content-type is
/// treated as a document — many servers omit it for HTML.
///
/// Single source of truth for the worker (`Parsed` gate) and the TUI
/// (`error_page` / `App` error-page path). Do not reimplement elsewhere.
pub fn is_document(status: u16, content_type: Option<&str>) -> bool {
    if !(200..300).contains(&status) {
        return false;
    }
    let Some(ct) = content_type else {
        return true;
    };
    let mime = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    mime.is_empty()
        || mime == "text/html"
        || mime == "application/xhtml+xml"
        || mime == "text/plain"
        || mime == "text/xml"
        || mime == "application/xml"
}

/// Fetch one linked stylesheet on its own detached worker (M4.3). One
/// `Msg::Stylesheet` goes out and nothing else — a stylesheet has no visible
/// byte counter, so there is no `Loading` progress to report.
///
/// The CSS is parsed here for the same reason the HTML is: the worker owns the
/// bytes and is already off the UI thread. Anything short of a successful
/// response yields `None`; the page is then styled by whatever else it has,
/// which is what "render unstyled, then restyle" means when a sheet is missing
/// rather than slow.
pub fn spawn_stylesheet(id: PageId, slot: usize, request: Request, tx: Sender<Msg>) {
    thread::spawn(move || {
        let sheet = match get(&request, ACCEPT_STYLESHEET) {
            // A 404's body is an error page, not CSS; parsing it would put
            // whatever HTML-shaped garbage recovers into the cascade.
            Ok((status, body)) if (200..300).contains(&status) => {
                // Charset: lossy UTF-8, the same seam `html::decode_body`
                // documents. `@charset` and the HTTP header wait for a page
                // that needs them.
                Some(css::parse(&String::from_utf8_lossy(&body)))
            }
            _ => None,
        };
        let _ = tx.send(Msg::Stylesheet { id, slot, sheet });
    });
}

/// The largest external script this engine will run. A mis-served 200 MB file
/// must not take the process with it, and no honest script is anywhere near
/// this — jQuery is 90 KB, React with its DOM package is under 150 KB.
/// Refusing loudly (a console line, M10.7) beats decoding a gigabyte to find
/// out it was a video.
pub const MAX_SCRIPT_BYTES: usize = 4 * 1024 * 1024;

/// Fetch one `<script src>` on a detached worker (M10.10), modelled on
/// `spawn_stylesheet`: the slot is allocated in document order before the
/// fetch starts, so arrival order cannot change execution order.
///
/// `None` means "this slot will never run": a failed fetch, a non-success
/// status, or a body past [`MAX_SCRIPT_BYTES`]. The page is degraded, never an
/// error page — a missing script is the same class of problem as a missing
/// stylesheet.
pub fn spawn_script(id: PageId, slot: usize, request: Request, tx: Sender<Msg>) {
    thread::spawn(move || {
        let source = match get(&request, ACCEPT_SCRIPT) {
            // A 404's body is an error page, not JavaScript. Running it would
            // put whatever HTML-shaped garbage recovers into the engine.
            Ok((status, body)) if (200..300).contains(&status) => {
                if body.len() > MAX_SCRIPT_BYTES {
                    None
                } else {
                    // The same lossy-UTF-8 seam every other body decode uses.
                    Some(crate::html::decode_body(&body))
                }
            }
            _ => None,
        };
        let _ = tx.send(Msg::Script { id, slot, source });
    });
}

/// The largest response body `fetch()` will hand to a page. A script that
/// asks for a video and calls `text()` on it must find a wall, not the
/// process's memory limit.
pub const MAX_FETCH_BYTES: usize = 8 * 1024 * 1024;

/// What a page's `fetch()` got back. Plain owned data — no engine types, no
/// `reqwest` types — because it travels inside a `Msg`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct JsResponse {
    pub status: u16,
    pub status_text: String,
    /// The URL after redirects, which is what `response.url` reports.
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// Perform one `fetch()` on a detached worker (M10.12), and send the result
/// back as a message. The same discipline as every other producer: the UI
/// thread never waits, and the promise settles in a later turn.
///
/// A non-success status is **not** an error: `fetch` resolves with `ok: false`
/// for a 404, which pages get wrong constantly and so must we not. `Err` here
/// means the request never completed at all.
pub fn spawn_js_fetch(
    page: PageId,
    request: u64,
    ask: Request,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    tx: Sender<Msg>,
) {
    thread::spawn(move || {
        let result = js_request(&ask, &method, &headers, body.as_deref());
        let _ = tx.send(Msg::JsFetch {
            page,
            request,
            result,
        });
    });
}

fn js_request(
    ask: &Request,
    method: &str,
    headers: &[(String, String)],
    body: Option<&str>,
) -> Result<JsResponse, String> {
    let client = client()?;
    let url = ask.url.as_str();
    let mut req = match method {
        "POST" => client.post(url),
        _ => client.get(url),
    };
    req = with_request_headers(req, ask, "*/*");
    let mut authored = reqwest::header::HeaderMap::new();
    for (name, value) in headers {
        // Credentials, source identity and transport framing belong to the
        // browser. A page cannot forge them around the jar/referrer policy or
        // make reqwest talk to a host other than the URL it approved.
        if request_owned_header(name) {
            continue;
        }
        let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(name.as_bytes()),
            reqwest::header::HeaderValue::from_str(value),
        ) else {
            continue;
        };
        authored.insert(name, value);
    }
    // `headers` replaces defaults with page-authored values for fields fetch
    // is allowed to control (notably `Accept` and `Content-Type`).
    let mut req = req.headers(authored);
    if let Some(body) = body {
        req = req.body(body.to_string());
    }

    let mut resp = req.send().map_err(describe)?;
    let status = resp.status();
    let final_url = resp.url().to_string();
    let headers = resp
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect();

    let mut bytes = Vec::new();
    resp.read_to_end(&mut bytes).map_err(describe)?;
    if bytes.len() > MAX_FETCH_BYTES {
        return Err(format!(
            "response is {} bytes, over the {MAX_FETCH_BYTES}-byte limit",
            bytes.len()
        ));
    }

    Ok(JsResponse {
        status: status.as_u16(),
        status_text: status.canonical_reason().unwrap_or_default().to_string(),
        url: final_url,
        headers,
        // The same lossy-UTF-8 seam every other body decode uses.
        body: crate::html::decode_body(&bytes),
    })
}

/// Fetch and decode one `<img>` on a detached worker (M8). One `Msg::Image`
/// goes out — success or soft failure. Never an error page: a broken image is
/// a degraded page, not a navigation failure.
pub fn spawn_image(id: PageId, request: Request, tx: Sender<Msg>) {
    thread::spawn(move || {
        let result = match get(&request, ACCEPT_IMAGE) {
            Ok((status, body)) if (200..300).contains(&status) => image::decode(&body),
            Ok((status, _)) => Err(format!("HTTP {status}")),
            Err(e) => Err(e),
        };
        let _ = tx.send(Msg::Image {
            id,
            url: request.url,
            result,
        });
    });
}

/// A whole response in one go, no progress reporting. Used for subresources,
/// where there is no byte counter to feed.
///
/// A subresource *sends* cookies, because a same-origin server may require
/// them to serve the bytes at all — and it can set none: no `Msg` on this path
/// carries a `Set-Cookie`, which is what makes "a subresource cannot start a
/// session" structural rather than a rule somebody has to remember (M11.7,
/// deliverable 2).
fn get(request: &Request, accept: &'static str) -> Result<(u16, Vec<u8>), String> {
    let mut resp = with_request_headers(client()?.get(&request.url), request, accept)
        .send()
        .map_err(describe)?;
    let status = resp.status().as_u16();
    let mut body = Vec::new();
    resp.read_to_end(&mut body).map_err(describe)?;
    Ok((status, body))
}

/// Add the browser-owned request headers. This is the one place a cookie or
/// referrer becomes a header — every worker goes through it, so a new request
/// path cannot silently omit their policy decisions.
fn with_request_headers(
    builder: reqwest::blocking::RequestBuilder,
    request: &Request,
    accept: &'static str,
) -> reqwest::blocking::RequestBuilder {
    let mut builder = builder.header(reqwest::header::ACCEPT, accept);
    if let Some(referrer) = &request.referrer {
        builder = builder.header(reqwest::header::REFERER, referrer);
    }
    match &request.cookie {
        Some(cookie) => builder.header(reqwest::header::COOKIE, cookie),
        None => builder,
    }
}

/// A process-wide blocking client. Its first caller builds it on a worker;
/// later workers clone the cheap handle and share its connection pool. The
/// defaults follow redirects and transparently decompress gzip responses.
fn client() -> Result<reqwest::blocking::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::blocking::Client, String>> = OnceLock::new();
    CLIENT.get_or_init(|| build_client(true)).clone()
}

/// The **document** client: identical, except that it does not follow
/// redirects — each hop comes back as a `Msg::Redirect` and the event loop
/// decides (M11.7a).
///
/// Only the document, and that asymmetry is a decision rather than an
/// oversight. The document is where a session is established, and it is the
/// only request whose URL the reader can see: the URL bar, `location`, history,
/// the fragment and the base every relative href resolves against all have to
/// agree with where the bytes came from. A subresource has none of that, its
/// cross-host credential leak is already closed by the library (which strips
/// `Cookie` on any hop that changes host, port or scheme), and putting four
/// more request kinds through a hop state machine would buy a `Path`
/// recomputation no ladder page has asked for. When one turns up, it arrives
/// then.
fn document_client() -> Result<reqwest::blocking::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::blocking::Client, String>> = OnceLock::new();
    CLIENT.get_or_init(|| build_client(false)).clone()
}

fn build_client(follow_redirects: bool) -> Result<reqwest::blocking::Client, String> {
    let mut defaults = reqwest::header::HeaderMap::new();
    let language = std::env::var("YATA_ACCEPT_LANGUAGE")
        .ok()
        .filter(|value| value.len() <= 256)
        .and_then(|value| reqwest::header::HeaderValue::from_str(&value).ok())
        .unwrap_or_else(|| reqwest::header::HeaderValue::from_static("en-US,en;q=0.5"));
    defaults.insert(reqwest::header::ACCEPT_LANGUAGE, language);
    if let Ok(raw) = std::env::var("YATA_HTTP_HEADERS") {
        defaults.extend(configured_headers(&raw));
    }
    let user_agent = std::env::var("YATA_USER_AGENT")
        .ok()
        .filter(|value| value.len() <= 256 && reqwest::header::HeaderValue::from_str(value).is_ok())
        .unwrap_or_else(|| format!("yata/{}", env!("CARGO_PKG_VERSION")));
    let mut builder = reqwest::blocking::Client::builder()
        .user_agent(user_agent)
        .default_headers(defaults);
    if !follow_redirects {
        builder = builder.redirect(reqwest::redirect::Policy::none());
    }
    builder.build().map_err(describe)
}

/// Parse bounded `Name: value` lines from `YATA_HTTP_HEADERS`. Request-owned,
/// credential and hop-by-hop headers cannot be overridden here; malformed or
/// excessive entries are ignored rather than making the browser unstartable.
fn configured_headers(raw: &str) -> reqwest::header::HeaderMap {
    const MAX_HEADERS: usize = 16;
    const MAX_BYTES: usize = 8 * 1024;
    let mut headers = reqwest::header::HeaderMap::new();
    if raw.len() > MAX_BYTES {
        return headers;
    }
    for line in raw.lines().take(MAX_HEADERS) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let Ok(name) = reqwest::header::HeaderName::from_bytes(name.trim().as_bytes()) else {
            continue;
        };
        if matches!(name.as_str(), "accept" | "accept-language")
            || request_owned_header(name.as_str())
        {
            continue;
        }
        let Ok(value) = reqwest::header::HeaderValue::from_str(value.trim()) else {
            continue;
        };
        headers.insert(name, value);
    }
    headers
}

fn request_owned_header(name: &str) -> bool {
    [
        "accept-encoding",
        "connection",
        "content-length",
        "cookie",
        "host",
        "proxy-authorization",
        "referer",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "user-agent",
    ]
    .iter()
    .any(|owned| name.eq_ignore_ascii_case(owned))
}

/// Where a 3xx says to go, resolved against the URL that produced it — or
/// `None` when this is not a redirect the loop can follow.
///
/// The statuses are HTTP's five: 301, 302, 303, 307 and 308. The worker
/// reports the status and stops; rewriting POST→GET on 301/302/303 (and
/// keeping POST on 307/308) is the event loop's (M11.11).
///
/// A 3xx with no `Location`, or one whose `Location` will not resolve, is not a
/// redirect: it falls through as an ordinary response, which the error-page
/// path already refuses to render (`is_document` is false for every 3xx).
fn redirect_target(
    status: u16,
    headers: &reqwest::header::HeaderMap,
    from: &str,
) -> Option<String> {
    if !matches!(status, 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    let location = headers.get(reqwest::header::LOCATION)?.to_str().ok()?;
    // One URL resolver in the engine, and this is not a second one.
    crate::net::resolve_url(from, location)
}

/// The whole request, run on the worker. `Ok(Some(Loaded))` on success,
/// `Ok(None)` if the channel closed mid-stream, `Err((url, reason))` on any
/// failure (bad URL, DNS, connect, TLS, mid-body disconnect). The error's url
/// is the most precise one known at the point of failure: the requested URL
/// until headers arrive, the post-redirect final URL after.
fn fetch(id: PageId, request: &Request, tx: &Sender<Msg>) -> Result<Option<Msg>, (String, String)> {
    let url = request.url.as_str();
    // Timed on the worker, where the request happens: the duration reaches
    // the app as message data, so the app never reads the clock. The span is
    // the whole request — client build → last body byte.
    let started = Instant::now();
    // Built on the worker (see `client`), so the UI thread never touches
    // reqwest. The document's client does not follow redirects: this worker
    // performs exactly one request and reports what happened (M11.7a).
    let client = document_client().map_err(|reason| (url.to_string(), reason))?;
    let builder = match &request.method {
        Method::Get | Method::Conditional { .. } => client.get(url),
        Method::Post { body } => client
            .post(url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body.clone()),
    };
    let mut builder = with_request_headers(builder, request, ACCEPT_DOCUMENT);
    if let Method::Conditional {
        no_cache,
        if_none_match,
    } = &request.method
    {
        if *no_cache {
            builder = builder.header(reqwest::header::CACHE_CONTROL, "no-cache");
        }
        if let Some(etag) = if_none_match {
            builder = builder.header(reqwest::header::IF_NONE_MATCH, etag);
        }
    }
    let mut resp = builder.send().map_err(|e| (url.to_string(), describe(e)))?;
    let status = resp.status().as_u16();
    // The URL this response came from. With hops through the loop it is the
    // one we asked for; reqwest still reports it, so it stays the single
    // source rather than an echo of the request.
    let final_url = resp.url().to_string();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());
    // Every `Set-Cookie` line, kept apart: a response may carry several, and
    // folding them on commas is the classic way to lose one (a comma is legal
    // inside a cookie value and inside an `Expires` date). Bytes that are not
    // UTF-8 are skipped rather than lossily decoded — a mangled cookie is a
    // credential that silently does not work, and dropping it says so.
    //
    // Not a header map. M11.20's cache will want `Cache-Control` and `ETag`,
    // and it can widen this when it has a second reader; a map now is a field
    // nothing reads.
    let set_cookie: Vec<String> = resp
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_string)
        .collect();
    let metadata = selected_metadata(resp.headers());

    // A hop, and the worker's whole part in it: report where the response says
    // to go and stop. The body of a 3xx is a courtesy page nobody renders, so
    // it is not read — the connection is dropped with the response.
    if let Some(to) = redirect_target(status, resp.headers(), &final_url) {
        return Ok(Some(Msg::Redirect {
            id,
            url: final_url,
            to,
            status,
            elapsed: started.elapsed(),
            set_cookie,
        }));
    }

    let mut body = Vec::new();
    let mut buf = [0u8; CHUNK];
    loop {
        let n = resp
            .read(&mut buf)
            .map_err(|e| (final_url.clone(), describe(e)))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
        let progress = Msg::Loading {
            id,
            bytes_so_far: body.len() as u64,
        };
        if tx.send(progress).is_err() {
            return Ok(None);
        }
    }
    Ok(Some(Msg::Loaded {
        id,
        url: final_url,
        status,
        body,
        elapsed: started.elapsed(),
        content_type,
        set_cookie,
        metadata,
    }))
}

fn selected_metadata(headers: &reqwest::header::HeaderMap) -> Metadata {
    const MAX_LINES: usize = 32;
    fn lines(
        headers: &reqwest::header::HeaderMap,
        name: reqwest::header::HeaderName,
    ) -> (Vec<String>, bool, bool) {
        let mut out = Vec::new();
        let mut over_limit = false;
        let mut non_utf8 = false;
        for value in headers.get_all(name).iter() {
            if out.len() == MAX_LINES {
                over_limit = true;
                break;
            }
            match value.to_str() {
                Ok(value) if value.len() <= MAX_FIELD_BYTES => out.push(value.to_string()),
                Ok(_) => over_limit = true,
                Err(_) => non_utf8 = true,
            }
        }
        (out, over_limit, non_utf8)
    }
    fn one(
        headers: &reqwest::header::HeaderMap,
        name: reqwest::header::HeaderName,
    ) -> (Option<String>, bool) {
        let Some(value) = headers.get(name) else {
            return (None, false);
        };
        match value.to_str() {
            Ok(value) if value.len() <= MAX_FIELD_BYTES => (Some(value.to_string()), false),
            Ok(_) => (None, true),
            Err(_) => (None, false),
        }
    }
    let (cache_control, control_over_limit, _) = lines(headers, reqwest::header::CACHE_CONTROL);
    let (vary, vary_over_limit, vary_non_utf8) = lines(headers, reqwest::header::VARY);
    let (etag, etag_bad) = one(headers, reqwest::header::ETAG);
    let (age, age_bad) = one(headers, reqwest::header::AGE);
    let mut metadata = Metadata::bounded(
        cache_control,
        etag,
        age,
        vary,
        vary_over_limit || vary_non_utf8,
    );
    metadata.over_limit |= control_over_limit || vary_over_limit || etag_bad || age_bad;
    metadata
}

/// reqwest's top-level Display is vague ("error sending request…"); the
/// human-readable cause ("Connection refused") lives down the source chain,
/// so flatten the chain into the reason the user will see.
fn describe(err: impl Error) -> String {
    let mut reason = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        reason.push_str(": ");
        reason.push_str(&cause.to_string());
        source = cause.source();
    }
    reason
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{SocketAddr, TcpListener};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Serve one canned HTTP response on an ephemeral local port, from a test
    /// thread. Tests never hit the network (CLAUDE.md conventions).
    fn serve_once(response: Vec<u8>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Read the request through the blank line before answering, so
            // the client is never racing a response to an unsent request.
            let mut req = Vec::new();
            let mut buf = [0u8; 512];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream.write_all(&response);
        });
        addr
    }

    /// Serve `count` requests on one ephemeral port, answering each with
    /// `respond(request_text)`, and hand back the requests as they were
    /// received. This is how the M11.7 tests ask "what did the server
    /// actually see?" — the presence of a `Cookie:` header on the wire is the
    /// whole claim, and it cannot be checked from this side of the socket.
    fn serve_capturing(
        count: usize,
        respond: impl Fn(&str) -> Vec<u8> + Send + 'static,
    ) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&seen);
        thread::spawn(move || {
            for _ in 0..count {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            req.extend_from_slice(&buf[..n]);
                            if let Some(at) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                                let length = content_length_of(&req[..at]).unwrap_or(0);
                                let need = at + 4 + length;
                                while req.len() < need {
                                    match stream.read(&mut buf) {
                                        Ok(0) | Err(_) => break,
                                        Ok(n) => req.extend_from_slice(&buf[..n]),
                                    }
                                }
                                break;
                            }
                        }
                    }
                }
                let text = String::from_utf8_lossy(&req).into_owned();
                let response = respond(&text);
                captured.lock().unwrap().push(text);
                let _ = stream.write_all(&response);
            }
        });
        (addr, seen)
    }

    fn content_length_of(headers: &[u8]) -> Option<usize> {
        let text = std::str::from_utf8(headers).ok()?;
        text.lines().find_map(|line| {
            line.split_once(':').and_then(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())
                    .flatten()
            })
        })
    }

    /// The `Cookie:` header a captured request carried, if any.
    fn cookie_header(request: &str) -> Option<&str> {
        header(request, "cookie")
    }

    fn header<'a>(request: &'a str, wanted: &str) -> Option<&'a str> {
        request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case(wanted).then(|| value.trim())
        })
    }

    /// A 200 with `body` and `Connection: close`, so each request gets its own
    /// accept.
    fn ok_body(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// Collect every message the worker sends, ending when it drops its
    /// sender. The timeout turns a hung worker into a test failure instead of
    /// a hung test run.
    fn drain(rx: Receiver<Msg>) -> Vec<Msg> {
        let mut msgs = Vec::new();
        loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(msg) => msgs.push(msg),
                Err(RecvTimeoutError::Disconnected) => return msgs,
                Err(RecvTimeoutError::Timeout) => panic!("fetch worker never finished"),
            }
        }
    }

    /// Split a successful worker's message stream into (progress, loaded,
    /// parsed), asserting the M2.3 shape: `Loading`* then `Loaded` then
    /// exactly one `Parsed`.
    fn split_success(msgs: &[Msg]) -> (&[Msg], &Msg, &Msg) {
        let (parsed, rest) = msgs.split_last().expect("worker sent nothing");
        assert!(
            matches!(parsed, Msg::Parsed { .. }),
            "expected Parsed last, got {parsed:?}"
        );
        let (loaded, before_loaded) = rest.split_last().expect("no Loaded before Parsed");
        assert!(
            matches!(loaded, Msg::Loaded { .. }),
            "expected Loaded before Parsed, got {loaded:?}"
        );
        (before_loaded, loaded, parsed)
    }

    /// Serve `body` as CSS on an ephemeral port, once.
    fn serve_css(status_line: &str, body: &'static str) -> SocketAddr {
        serve_once(
            format!(
                "{status_line}\r\nContent-Type: text/css\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
        )
    }

    /// Accept **two** connections before answering either. A sequential
    /// fetcher never opens the second one, so it waits forever on a response
    /// that will not come and the test times out. This is the structural test
    /// for "stylesheets are fetched in parallel" (CLAUDE.md: the UI thread
    /// never blocks) — a wall-clock assertion would only be a guess.
    fn serve_two_but_only_after_both_connect(body: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let mut waiting = Vec::new();
            for _ in 0..2 {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                waiting.push(stream);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            for mut stream in waiting {
                let mut req = Vec::new();
                let mut buf = [0u8; 512];
                while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = stream.write_all(response.as_bytes());
            }
        });
        addr
    }

    #[test]
    fn a_stylesheet_worker_sends_one_parsed_sheet() {
        let addr = serve_css("HTTP/1.1 200 OK", "a:link { color: #000 } p { color: red }");
        let (tx, rx) = mpsc::channel();
        spawn_stylesheet(
            PageId::headless(7),
            3,
            Request::bare(format!("http://{addr}/news.css")),
            tx,
        );

        let msgs = drain(rx);
        assert_eq!(msgs.len(), 1, "one message and nothing else: {msgs:?}");
        match &msgs[0] {
            Msg::Stylesheet { id, slot, sheet } => {
                assert_eq!(*id, PageId::headless(7));
                // The slot travels with the sheet: it is the document position
                // the cascade needs, not the arrival order.
                assert_eq!(*slot, 3);
                let sheet = sheet.as_ref().expect("a 200 text/css body must parse");
                assert_eq!(sheet.rules.len(), 2);
            }
            other => panic!("expected Stylesheet, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_or_unsuccessful_sheet_resolves_to_none() {
        // A 404's body is an error page, not CSS: parsing it would feed
        // whatever the recovery path salvages into the cascade.
        let addr = serve_css("HTTP/1.1 404 Not Found", "<html>nope</html>");
        let (tx, rx) = mpsc::channel();
        spawn_stylesheet(
            PageId::headless(8),
            0,
            Request::bare(format!("http://{addr}/missing.css")),
            tx,
        );
        assert!(matches!(
            drain(rx).as_slice(),
            [Msg::Stylesheet {
                id,
                slot: 0,
                sheet: None
            }] if *id == PageId::headless(8)
        ));

        // A closed port is the same story: a degraded page, not an error page.
        let dead = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let (tx, rx) = mpsc::channel();
        spawn_stylesheet(
            PageId::headless(9),
            1,
            Request::bare(format!("http://{dead}/x.css")),
            tx,
        );
        assert!(matches!(
            drain(rx).as_slice(),
            [Msg::Stylesheet {
                id,
                slot: 1,
                sheet: None
            }] if *id == PageId::headless(9)
        ));
    }

    #[test]
    fn stylesheets_are_fetched_in_parallel() {
        // The server answers nobody until both workers have connected, so this
        // test cannot pass if the fetches happen one after the other — it
        // deadlocks and `drain`'s timeout fails it.
        let addr = serve_two_but_only_after_both_connect("p { color: red }");
        let (tx, rx) = mpsc::channel();
        spawn_stylesheet(
            PageId::headless(10),
            0,
            Request::bare(format!("http://{addr}/a.css")),
            tx.clone(),
        );
        spawn_stylesheet(
            PageId::headless(10),
            1,
            Request::bare(format!("http://{addr}/b.css")),
            tx,
        );

        let msgs = drain(rx);
        assert_eq!(msgs.len(), 2, "one message per sheet: {msgs:?}");
        let mut slots: Vec<usize> = msgs
            .iter()
            .map(|m| match m {
                Msg::Stylesheet { slot, sheet, .. } => {
                    assert!(sheet.is_some(), "both sheets must parse");
                    *slot
                }
                other => panic!("expected Stylesheet, got {other:?}"),
            })
            .collect();
        slots.sort_unstable();
        assert_eq!(slots, vec![0, 1]);
    }

    #[test]
    fn local_server_success_sends_loading_then_loaded_then_parsed() {
        let addr = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world"
                .to_vec(),
        );
        let url = format!("http://{addr}/");
        let (tx, rx) = mpsc::channel();
        spawn_fetch(PageId::headless(1), Request::bare(url.clone()), tx);

        let msgs = drain(rx);
        let (progress, loaded, parsed) = split_success(&msgs);
        assert!(
            !progress.is_empty(),
            "expected at least one Loading before Loaded"
        );
        let mut prev = 0;
        for msg in progress {
            match msg {
                Msg::Loading { id, bytes_so_far } => {
                    assert_eq!(*id, PageId::headless(1));
                    assert!(*bytes_so_far > prev, "byte counts must grow");
                    prev = *bytes_so_far;
                }
                other => panic!("expected only Loading before Loaded, got {other:?}"),
            }
        }
        // The worker measures the whole request; even against localhost the
        // elapsed time can never be zero.
        let Msg::Loaded { elapsed, .. } = loaded else {
            unreachable!()
        };
        assert!(
            *elapsed > Duration::ZERO,
            "the worker must measure the request"
        );
        assert_eq!(
            *loaded,
            Msg::Loaded {
                id: PageId::headless(1),
                url,
                status: 200,
                body: b"hello world".to_vec(),
                elapsed: *elapsed,
                content_type: None,
                set_cookie: Vec::new(),
                metadata: Default::default(),
            }
        );
        // The Parsed message carries the body's tree, built on the worker.
        let Msg::Parsed { id, dom, .. } = parsed else {
            unreachable!()
        };
        assert_eq!(*id, PageId::headless(1));
        assert!(
            html::debug_tree(dom).contains("#text \"hello world\""),
            "the parsed tree must contain the body text:\n{}",
            html::debug_tree(dom)
        );
    }

    #[test]
    fn conditional_document_headers_and_selected_metadata_cross_the_seam_exactly() {
        let (addr, seen) = serve_capturing(1, |_| {
            b"HTTP/1.1 304 Not Modified\r\nCache-Control: no-cache\r\n\
              Cache-Control: max-age=\"60\"\r\nETag: W/\"one\"\r\nAge: 7\r\n\
              Vary: Cookie, Accept-Encoding\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec()
        });
        let (tx, rx) = mpsc::channel();
        spawn_fetch(
            PageId::headless(9),
            Request {
                url: format!("http://{addr}/a"),
                cookie: None,
                referrer: None,
                method: Method::Conditional {
                    no_cache: true,
                    if_none_match: Some("W/\"one\"".into()),
                },
            },
            tx,
        );
        let msgs = drain(rx);
        let metadata = msgs
            .iter()
            .find_map(|msg| match msg {
                Msg::Loaded { metadata, .. } => Some(metadata),
                _ => None,
            })
            .expect("metadata did not cross the channel");
        assert_eq!(metadata.cache_control, ["no-cache", "max-age=\"60\""]);
        assert_eq!(metadata.etag.as_deref(), Some("W/\"one\""));
        assert_eq!(metadata.age.as_deref(), Some("7"));
        assert_eq!(metadata.vary, ["Cookie, Accept-Encoding"]);
        assert!(
            matches!(msgs.last(), Some(Msg::Loaded { status: 304, body, .. }) if body.is_empty())
        );

        let request = &seen.lock().unwrap()[0];
        assert!(
            request
                .to_ascii_lowercase()
                .contains("cache-control: no-cache\r\n"),
            "{request}"
        );
        assert!(
            request.contains("if-none-match: W/\"one\"\r\n"),
            "{request}"
        );
    }

    #[test]
    fn non_utf8_cache_metadata_is_ignored_except_for_vary() {
        use reqwest::header::{CACHE_CONTROL, ETAG, HeaderMap, HeaderValue, VARY};

        let invalid =
            HeaderValue::from_bytes(b"\xff").expect("opaque header byte is valid on wire");
        let mut headers = HeaderMap::new();
        headers.append(CACHE_CONTROL, invalid.clone());
        headers.insert(ETAG, HeaderValue::from_static("\"one\""));
        let metadata = selected_metadata(&headers);
        assert!(metadata.cache_control.is_empty());
        assert_eq!(metadata.etag.as_deref(), Some("\"one\""));
        assert!(!metadata.over_limit, "ignored bytes are not a size attack");
        assert!(!metadata.vary_unusable);

        headers.append(VARY, invalid);
        let metadata = selected_metadata(&headers);
        assert!(metadata.vary.is_empty());
        assert!(metadata.vary_unusable, "an unreadable Vary forbids reuse");
        assert!(
            !metadata.over_limit,
            "unreadable Vary is not an over-limit field"
        );
    }

    #[test]
    fn closed_port_sends_exactly_one_net_error_with_reason() {
        // Bind then drop: the freed ephemeral port refuses connections.
        let addr = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let url = format!("http://{addr}/");
        let (tx, rx) = mpsc::channel();
        spawn_fetch(PageId::headless(2), Request::bare(url.clone()), tx);

        let msgs = drain(rx);
        assert_eq!(msgs.len(), 1, "exactly one message expected, got {msgs:?}");
        match &msgs[0] {
            Msg::NetError {
                id,
                url: reported,
                reason,
            } => {
                assert_eq!(*id, PageId::headless(2));
                assert_eq!(*reported, url);
                assert!(!reason.is_empty(), "reason must be human-readable");
            }
            other => panic!("expected NetError, got {other:?}"),
        }
    }

    #[test]
    fn mid_body_failure_reports_the_url_it_was_reading() {
        // Headers promise 100 bytes; the connection dies after 5.
        let addr = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nhello".to_vec(),
        );
        let url = format!("http://{addr}/");
        let (tx, rx) = mpsc::channel();
        spawn_fetch(PageId::headless(4), Request::bare(url.clone()), tx);

        let msgs = drain(rx);
        let (last, progress) = msgs.split_last().expect("worker sent nothing");
        // The bytes that did arrive before the cut may or may not have
        // produced Loading messages; only the terminal message is pinned.
        for msg in progress {
            assert!(
                matches!(
                    msg,
                    Msg::Loading {
                        id,
                        ..
                    } if *id == PageId::headless(4)
                ),
                "expected only Loading before NetError, got {msg:?}"
            );
        }
        match last {
            Msg::NetError {
                id,
                url: reported,
                reason,
            } => {
                assert_eq!(*id, PageId::headless(4));
                assert_eq!(*reported, url);
                assert!(!reason.is_empty(), "reason must be human-readable");
            }
            other => panic!("expected NetError last, got {other:?}"),
        }
    }

    #[test]
    fn a_redirect_is_a_message_and_the_worker_stops_there() {
        // **M11.7a's whole change on this side.** The document client does not
        // follow redirects: the worker performs one request, reports where the
        // response pointed — `Location` resolved against the URL that sent it —
        // and sends nothing else. No `Loaded`, no `Parsed`, and, crucially, no
        // second request: the server's second slot is never used.
        //
        // A `Set-Cookie` on the hop travels with the message, which is the
        // thing that was lost before: only the final response's headers ever
        // reached `App`, so the 302 that hands out a session set nothing.
        let (addr, seen) = serve_capturing(2, |_| {
            b"HTTP/1.1 302 Found\r\nLocation: /final\r\nSet-Cookie: sid=abc; Path=/\r\n\
              Content-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec()
        });
        let (tx, rx) = mpsc::channel();
        spawn_fetch(
            PageId::headless(5),
            Request::bare(format!("http://{addr}/start")),
            tx,
        );

        let msgs = drain(rx);
        assert_eq!(
            msgs.len(),
            1,
            "a hop is one message and nothing else: {msgs:?}"
        );
        let Msg::Redirect {
            id,
            url,
            to,
            status,
            elapsed,
            set_cookie,
        } = &msgs[0]
        else {
            panic!("expected Redirect, got {:?}", msgs[0]);
        };
        assert_eq!(*id, PageId::headless(5));
        assert_eq!(*url, format!("http://{addr}/start"));
        assert_eq!(*to, format!("http://{addr}/final"));
        assert_eq!(*status, 302);
        assert_eq!(set_cookie, &["sid=abc; Path=/".to_string()]);
        assert!(*elapsed > Duration::ZERO, "the worker must measure its hop");
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the worker followed the redirect itself"
        );
    }

    #[test]
    fn a_post_goes_on_the_wire_with_its_body_and_content_type() {
        // The document worker is the one that sends a form POST (M11.11).
        // `spawn_js_fetch` already knew `.post()`; routing a navigation
        // through it would make a form submission a promise, not a page.
        let (addr, seen) = serve_capturing(1, |_| ok_body("ok"));
        let (tx, rx) = mpsc::channel();
        spawn_fetch(
            PageId::headless(1),
            Request {
                url: format!("http://{addr}/login"),
                cookie: Some("sid=abc".into()),
                referrer: None,
                method: Method::Post {
                    body: "acct=pg&pw=secret".into(),
                },
            },
            tx,
        );
        drain(rx);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        let req = &seen[0];
        assert!(req.starts_with("POST "), "{req}");
        assert!(
            req.to_ascii_lowercase()
                .contains("content-type: application/x-www-form-urlencoded"),
            "{req}"
        );
        assert_eq!(cookie_header(req), Some("sid=abc"));
        assert!(
            req.contains("acct=pg&pw=secret"),
            "the body did not reach the server: {req}"
        );
    }

    #[test]
    fn a_post_302_is_a_redirect_message_with_its_status() {
        // The worker reports the 302 and stops. Rewriting POST→GET is App's.
        let (addr, seen) = serve_capturing(2, |_| {
            b"HTTP/1.1 302 Found\r\nLocation: /app\r\nSet-Cookie: sid=abc; Path=/\r\n\
              Content-Length: 0\r\nConnection: close\r\n\r\n"
                .to_vec()
        });
        let (tx, rx) = mpsc::channel();
        spawn_fetch(
            PageId::headless(1),
            Request {
                url: format!("http://{addr}/login"),
                cookie: None,
                referrer: None,
                method: Method::Post {
                    body: "acct=pg&pw=secret".into(),
                },
            },
            tx,
        );
        let msgs = drain(rx);
        let Msg::Redirect {
            status,
            to,
            set_cookie,
            ..
        } = &msgs[0]
        else {
            panic!("expected Redirect, got {:?}", msgs[0]);
        };
        assert_eq!(*status, 302);
        assert!(to.ends_with("/app"), "{to}");
        assert_eq!(set_cookie, &["sid=abc; Path=/".to_string()]);
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the worker followed the 302 itself"
        );
    }

    #[test]
    fn a_login_chain_on_the_wire_is_post_then_get_with_the_cookie() {
        // The worker reports the 302 and stops (pinned above). The chain
        // still has to exist on the socket: hop 2 is GET /app, the hop's
        // cookie, no body — the request App would spawn after
        // `rewrite_method(302, Post)`.
        let (addr, seen) = serve_capturing(2, |req| {
            if req.starts_with("POST") {
                b"HTTP/1.1 302 Found\r\nLocation: /app\r\nSet-Cookie: sid=abc; Path=/\r\n\
                  Content-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec()
            } else {
                ok_body("app")
            }
        });
        let (tx, rx) = mpsc::channel();
        spawn_fetch(
            PageId::headless(1),
            Request {
                url: format!("http://{addr}/login"),
                cookie: Some("sid=pre".into()),
                referrer: None,
                method: Method::Post {
                    body: "acct=pg&pw=secret".into(),
                },
            },
            tx,
        );
        let msgs = drain(rx);
        let Msg::Redirect { to, status, .. } = &msgs[0] else {
            panic!("expected Redirect, got {:?}", msgs[0]);
        };
        assert_eq!(*status, 302);

        let (tx, rx) = mpsc::channel();
        spawn_fetch(
            PageId::headless(1),
            Request {
                url: to.clone(),
                cookie: Some("sid=abc".into()),
                referrer: None,
                method: Method::Get,
            },
            tx,
        );
        drain(rx);

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[0].starts_with("POST "), "{}", seen[0]);
        assert!(
            seen[0].contains("acct=pg&pw=secret"),
            "POST body missing: {}",
            seen[0]
        );
        assert_eq!(cookie_header(&seen[0]), Some("sid=pre"));
        let get = &seen[1];
        assert!(get.starts_with("GET "), "{get}");
        assert!(get.contains("/app"), "{get}");
        assert_eq!(cookie_header(get), Some("sid=abc"));
        assert!(
            !get.contains("acct=pg") && !get.contains("pw=secret"),
            "the POST body rode the GET: {get}"
        );
    }

    #[test]
    fn a_hop_to_a_scheme_this_browser_cannot_fetch_is_an_error_not_a_panic() {
        // The loop follows a `Location` wherever it points, so the worker is
        // where a `file:` URL — or any other scheme reqwest will not perform —
        // has to fail safely. `NetError` becomes an error page with a reason on
        // it; nothing here may panic or hang (CLAUDE.md).
        let (tx, rx) = mpsc::channel();
        spawn_fetch(PageId::headless(1), Request::bare("file:///etc/hosts"), tx);
        let msgs = drain(rx);
        assert!(
            matches!(msgs.as_slice(), [Msg::NetError { .. }]),
            "expected one NetError, got {msgs:?}"
        );
    }

    #[test]
    fn every_redirect_status_hops_and_everything_else_is_a_response() {
        // HTTP's five, and the two shapes that are *not* a redirect however
        // much they look like one: a 3xx with no `Location`, and one whose
        // `Location` will not resolve. Those fall through to `Loaded` with
        // their 3xx status, which `is_document` refuses — an error page the
        // reader can act on, rather than a hop into nowhere.
        let hop = |status: u16, headers: &str| -> Msg {
            let response = format!(
                "HTTP/1.1 {status} Moved\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let addr = serve_once(response.into_bytes());
            let (tx, rx) = mpsc::channel();
            spawn_fetch(
                PageId::headless(1),
                Request::bare(format!("http://{addr}/x")),
                tx,
            );
            drain(rx)
                .into_iter()
                .find(|msg| matches!(msg, Msg::Redirect { .. } | Msg::Loaded { .. }))
                .expect("worker sent no terminal response")
        };
        for status in [301, 302, 303, 307, 308] {
            match hop(status, "Location: /next\r\n") {
                Msg::Redirect {
                    status: reported, ..
                } => assert_eq!(reported, status, "{status} lost its status"),
                other => panic!("{status} did not hop: {other:?}"),
            }
        }
        assert!(matches!(hop(302, ""), Msg::Loaded { status: 302, .. }));
        assert!(matches!(
            hop(302, "Location: http://[bad\r\n"),
            Msg::Loaded { status: 302, .. }
        ));
        // And a 200 is a 200 even with a `Location` on it.
        assert!(matches!(
            hop(200, "Location: /next\r\n"),
            Msg::Loaded { status: 200, .. }
        ));
    }

    #[test]
    fn gzip_body_is_transparently_decompressed() {
        // `printf 'hello world' | gzip -n -9`, embedded so the test stays
        // offline and dependency-free.
        const GZ: &[u8] = &[
            0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x03, 0xcb, 0x48, 0xcd, 0xc9,
            0xc9, 0x57, 0x28, 0xcf, 0x2f, 0xca, 0x49, 0x01, 0x00, 0x85, 0x11, 0x4a, 0x0d, 0x0b,
            0x00, 0x00, 0x00,
        ];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            GZ.len()
        )
        .into_bytes();
        response.extend_from_slice(GZ);
        let addr = serve_once(response);
        let url = format!("http://{addr}/");
        let (tx, rx) = mpsc::channel();
        spawn_fetch(PageId::headless(6), Request::bare(url.clone()), tx);

        let msgs = drain(rx);
        let (_, loaded, _) = split_success(&msgs);
        let Msg::Loaded { elapsed, .. } = loaded else {
            unreachable!()
        };
        assert_eq!(
            *loaded,
            Msg::Loaded {
                id: PageId::headless(6),
                url,
                status: 200,
                body: b"hello world".to_vec(),
                elapsed: *elapsed,
                content_type: None,
                set_cookie: Vec::new(),
                metadata: Default::default(),
            },
            "the body must arrive decompressed, not as gzip bytes"
        );
    }

    #[test]
    fn bad_url_sends_exactly_one_net_error() {
        let (tx, rx) = mpsc::channel();
        spawn_fetch(
            PageId::headless(3),
            Request::bare("not a url".to_string()),
            tx,
        );
        let msgs = drain(rx);
        assert_eq!(msgs.len(), 1, "exactly one message expected, got {msgs:?}");
        assert!(matches!(
            &msgs[0],
            Msg::NetError { id, url, reason }
                if *id == PageId::headless(3) && url == "not a url" && !reason.is_empty()
        ));
    }

    #[test]
    fn non_document_responses_send_loaded_without_parsed() {
        // HTTP error: body is still delivered (dump/curl semantics) but never
        // parsed into a DOM the TUI would style.
        let addr = serve_once(
            b"HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found"
                .to_vec(),
        );
        let (tx, rx) = mpsc::channel();
        spawn_fetch(
            PageId::headless(20),
            Request::bare(format!("http://{addr}/")),
            tx,
        );
        let msgs = drain(rx);
        assert!(
            msgs.iter()
                .any(|m| matches!(m, Msg::Loaded { status: 404, .. })),
            "expected Loaded for 404: {msgs:?}"
        );
        assert!(
            !msgs.iter().any(|m| matches!(m, Msg::Parsed { .. })),
            "404 must not produce Parsed: {msgs:?}"
        );

        // 200 with a non-document content-type: same shape.
        let addr = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 4\r\nConnection: close\r\n\r\n\x89PNG"
                .to_vec(),
        );
        let (tx, rx) = mpsc::channel();
        spawn_fetch(
            PageId::headless(21),
            Request::bare(format!("http://{addr}/")),
            tx,
        );
        let msgs = drain(rx);
        assert!(
            msgs.iter().any(|m| matches!(
                m,
                Msg::Loaded {
                    status: 200,
                    content_type: Some(ct),
                    ..
                } if ct.starts_with("image/png")
            )),
            "expected Loaded image/png: {msgs:?}"
        );
        assert!(
            !msgs.iter().any(|m| matches!(m, Msg::Parsed { .. })),
            "image/png must not produce Parsed: {msgs:?}"
        );
    }

    // ---- M11.7: cookies on the wire ---------------------------------------

    #[test]
    fn every_worker_sends_the_cookie_header_it_was_given() {
        // Four of the five request paths, because the fifth (`fetch()`) has
        // its own test below. One function decided the header; what is pinned
        // here is that each worker actually puts it on the wire, since a path
        // that quietly dropped it would leave a reader logged out of exactly
        // one kind of subresource.
        let (addr, seen) = serve_capturing(4, |_| ok_body("x"));
        let request = |path: &str| Request {
            url: format!("http://{addr}{path}"),
            cookie: Some("sid=abc".to_string()),
            referrer: None,
            method: crate::net::Method::Get,
        };

        let (tx, rx) = mpsc::channel();
        spawn_fetch(PageId::headless(1), request("/doc"), tx);
        drain(rx);
        let (tx, rx) = mpsc::channel();
        spawn_stylesheet(PageId::headless(1), 0, request("/x.css"), tx);
        drain(rx);
        let (tx, rx) = mpsc::channel();
        spawn_script(PageId::headless(1), 0, request("/x.js"), tx);
        drain(rx);
        let (tx, rx) = mpsc::channel();
        spawn_image(PageId::headless(1), request("/x.png"), tx);
        drain(rx);

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 4, "one request each: {seen:?}");
        for request in seen.iter() {
            assert_eq!(
                cookie_header(request),
                Some("sid=abc"),
                "a worker dropped its Cookie header: {request:?}"
            );
        }
    }

    #[test]
    fn document_requests_identify_yata_negotiate_html_and_send_the_given_referrer() {
        let (addr, seen) = serve_capturing(1, |_| ok_body("<p>ok</p>"));
        let (tx, rx) = mpsc::channel();
        let mut request = Request::bare(format!("http://{addr}/doc"));
        request.referrer = Some("http://source.test/article".into());
        spawn_fetch(PageId::headless(1), request, tx);
        drain(rx);

        let seen = seen.lock().unwrap();
        let request = &seen[0];
        assert_eq!(
            header(request, "user-agent"),
            Some(concat!("yata/", env!("CARGO_PKG_VERSION")))
        );
        assert_eq!(header(request, "accept"), Some(ACCEPT_DOCUMENT));
        assert_eq!(header(request, "accept-language"), Some("en-US,en;q=0.5"));
        assert_eq!(
            header(request, "referer"),
            Some("http://source.test/article")
        );
    }

    #[test]
    fn configured_headers_are_bounded_and_cannot_replace_request_owned_fields() {
        let raw = "DNT: 1\nX-Yata-Test: yes\nCookie: stolen=1\nHost: attacker.test\n\
                   Referer: https://attacker.test/\nUser-Agent: fake\nMalformed";
        let headers = configured_headers(raw);
        assert_eq!(headers.get("dnt").unwrap(), "1");
        assert_eq!(headers.get("x-yata-test").unwrap(), "yes");
        for forbidden in ["cookie", "host", "referer", "user-agent"] {
            assert!(headers.get(forbidden).is_none(), "accepted {forbidden}");
        }
        assert!(configured_headers(&"x".repeat(8 * 1024 + 1)).is_empty());
    }

    #[test]
    fn a_request_with_no_cookie_sends_no_header_at_all() {
        // `None` means the header is absent, not empty: an empty `Cookie:` is
        // a header a server has to parse for nothing.
        let (addr, seen) = serve_capturing(1, |_| ok_body("x"));
        let (tx, rx) = mpsc::channel();
        spawn_stylesheet(
            PageId::headless(1),
            0,
            Request::bare(format!("http://{addr}/x.css")),
            tx,
        );
        drain(rx);
        assert_eq!(cookie_header(&seen.lock().unwrap()[0]), None);
    }

    #[test]
    fn set_cookie_lines_reach_loaded_unfolded() {
        // Several lines, kept apart. Folding them on commas is the classic way
        // to lose one — a comma is legal both inside a value and inside an
        // `Expires` date, which is why the second line here has one.
        let addr = serve_once(
            b"HTTP/1.1 200 OK\r\n\
              Set-Cookie: a=1; Path=/\r\n\
              Set-Cookie: b=2; Expires=Sun, 06 Nov 2094 08:49:37 GMT\r\n\
              Content-Length: 2\r\nConnection: close\r\n\r\nhi"
                .to_vec(),
        );
        let (tx, rx) = mpsc::channel();
        spawn_fetch(
            PageId::headless(1),
            Request::bare(format!("http://{addr}/")),
            tx,
        );
        let msgs = drain(rx);
        let (_, loaded, _) = split_success(&msgs);
        let Msg::Loaded { set_cookie, .. } = loaded else {
            unreachable!()
        };
        assert_eq!(
            *set_cookie,
            vec![
                "a=1; Path=/".to_string(),
                "b=2; Expires=Sun, 06 Nov 2094 08:49:37 GMT".to_string(),
            ]
        );
    }

    #[test]
    fn a_set_cookie_that_is_not_utf8_is_skipped_rather_than_mangled() {
        // A lossy decode would store a credential with a replacement character
        // in it: a cookie that silently does not work. Dropping it says so.
        let mut response =
            b"HTTP/1.1 200 OK\r\nSet-Cookie: bad=\xff\xfe\r\nSet-Cookie: good=1\r\n".to_vec();
        response.extend_from_slice(b"Content-Length: 2\r\nConnection: close\r\n\r\nhi");
        let addr = serve_once(response);
        let (tx, rx) = mpsc::channel();
        spawn_fetch(
            PageId::headless(1),
            Request::bare(format!("http://{addr}/")),
            tx,
        );
        let msgs = drain(rx);
        let (_, loaded, _) = split_success(&msgs);
        let Msg::Loaded { set_cookie, .. } = loaded else {
            unreachable!()
        };
        assert_eq!(*set_cookie, vec!["good=1".to_string()]);
    }

    #[test]
    fn a_subresource_response_has_nowhere_to_put_a_set_cookie() {
        // Deliverable 2, as a structural test rather than a policy: the
        // messages a subresource worker sends have no field a `Set-Cookie`
        // could travel in, so a stylesheet or an image trying to start a
        // session sends exactly what it always sent and nothing more.
        let with_cookie = b"HTTP/1.1 200 OK\r\n\
                            Set-Cookie: sid=abc; Path=/\r\n\
                            Content-Length: 16\r\nConnection: close\r\n\r\np { color: red }";
        let addr = serve_once(with_cookie.to_vec());
        let (tx, rx) = mpsc::channel();
        spawn_stylesheet(
            PageId::headless(1),
            0,
            Request::bare(format!("http://{addr}/x.css")),
            tx,
        );
        assert!(
            matches!(
                drain(rx).as_slice(),
                [Msg::Stylesheet {
                    id,
                    slot: 0,
                    sheet: Some(_)
                }] if *id == PageId::headless(1)
            ),
            "a stylesheet response carried something new"
        );

        let addr = serve_once(with_cookie.to_vec());
        let (tx, rx) = mpsc::channel();
        spawn_script(
            PageId::headless(1),
            0,
            Request::bare(format!("http://{addr}/x.js")),
            tx,
        );
        assert!(matches!(
            drain(rx).as_slice(),
            [Msg::Script {
                id,
                slot: 0,
                source: Some(_)
            }] if *id == PageId::headless(1)
        ));
    }

    /// What driving hops through the event loop costs, measured rather than
    /// asserted. `#[ignore]`d out of the default run because it prints numbers
    /// and claims nothing:
    ///
    /// ```text
    /// cargo test --release --lib measure_the_hop -- --ignored --nocapture
    /// ```
    ///
    /// **A**: the pre-M11.7a shape — one worker, reqwest's default policy,
    /// hops followed inside it on a connection it can reuse. **B**: the shape
    /// this task built — a worker per request, a message back to the loop
    /// between them, and a fresh connection for the second hop. Interleaved and
    /// alternating, because this machine drifts 5–10% between runs.
    ///
    /// Against loopback, so what the number does *not* include is the honest
    /// part: on a real network the extra cost is one TCP (and TLS) handshake
    /// per hop, which no offline benchmark can measure. What it does show is
    /// that the message round trip itself is not the expensive half.
    #[test]
    #[ignore]
    fn measure_the_hop_through_the_loop() {
        const ROUNDS: usize = 20;

        // A 2-hop chain, served for as many requests as both sides will make.
        let serve = |count: usize| {
            serve_capturing(count, |request| {
                if request.starts_with("GET /start") {
                    b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\n\
                      Connection: close\r\n\r\n"
                        .to_vec()
                } else {
                    ok_body("landed")
                }
            })
        };

        // A: one worker, the library follows.
        let inside_the_worker = |addr: SocketAddr| -> Duration {
            let started = Instant::now();
            let client = client().expect("client");
            let mut resp = client
                .get(format!("http://{addr}/start"))
                .send()
                .expect("send");
            let mut body = Vec::new();
            resp.read_to_end(&mut body).expect("body");
            started.elapsed()
        };

        // B: a worker per request, with the loop's decision in between.
        let through_the_loop = |addr: SocketAddr| -> Duration {
            let started = Instant::now();
            let (tx, rx) = mpsc::channel();
            spawn_fetch(
                PageId::headless(1),
                Request::bare(format!("http://{addr}/start")),
                tx.clone(),
            );
            loop {
                match rx.recv().expect("a terminal message") {
                    Msg::Redirect { to, .. } => {
                        spawn_fetch(PageId::headless(1), Request::bare(to), tx.clone());
                    }
                    Msg::Loaded { .. } => break,
                    _ => {}
                }
            }
            started.elapsed()
        };

        let (mut a, mut b) = (Vec::new(), Vec::new());
        for round in 0..=ROUNDS {
            // Which side goes first alternates, so any residue of running
            // second cancels across rounds instead of landing on one column.
            let (one, two) = if round % 2 == 0 {
                let (addr, _) = serve(2);
                let one = inside_the_worker(addr);
                let (addr, _) = serve(2);
                (one, through_the_loop(addr))
            } else {
                let (addr, _) = serve(2);
                let two = through_the_loop(addr);
                let (addr, _) = serve(2);
                (inside_the_worker(addr), two)
            };
            if round > 0 {
                a.push(one);
                b.push(two);
            }
        }
        let summarize = |samples: &[Duration]| {
            let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
            let (lo, hi) = (samples.iter().min().unwrap(), samples.iter().max().unwrap());
            format!("{mean:.2?} ({lo:.2?}-{hi:.2?})")
        };
        // How much of the difference is simply building a second client: every
        // worker in this engine builds its own (see `client`), so a hop that is
        // a second worker pays for a second one.
        let builds: Vec<Duration> = (0..ROUNDS)
            .map(|_| {
                let started = Instant::now();
                let _ = client().expect("client");
                started.elapsed()
            })
            .collect();
        eprintln!(
            "M11.7a one 2-hop chain over loopback, mean of {ROUNDS} interleaved rounds:\n  \
             inside the worker {}  ->  through the loop {}\n  \
             (one client build: {})",
            summarize(&a),
            summarize(&b),
            summarize(&builds),
        );
    }

    #[test]
    fn a_subresource_redirect_to_another_host_still_drops_the_cookie() {
        // **The alarm, moved to where it still rings.** Since M11.7a the
        // document does not follow redirects at all — the loop does, and it
        // asks the jar again for every hop. A *subresource* still follows them
        // inside the worker on reqwest's default policy, so this engine still
        // depends on the library stripping `Cookie` (with `Authorization` and
        // friends) whenever a hop changes host, port or scheme.
        //
        // `localhost` and `127.0.0.1` are the same machine and different
        // hosts, which is what makes this checkable without a network. If it
        // ever stops being true, it is a cross-origin credential leak and this
        // test is the alarm, not a documentation exercise.
        let (elsewhere, seen_elsewhere) = serve_capturing(1, |_| {
            b"HTTP/1.1 200 OK\r\nContent-Type: text/css\r\nContent-Length: 0\r\n\
              Connection: close\r\n\r\n"
                .to_vec()
        });
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let start = listener.local_addr().unwrap();
        let port = elsewhere.port();
        thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut req = Vec::new();
            let mut buf = [0u8; 512];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://localhost:{port}/final.css\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            );
        });

        let (tx, rx) = mpsc::channel();
        spawn_stylesheet(
            PageId::headless(1),
            0,
            Request {
                url: format!("http://{start}/start.css"),
                cookie: Some("sid=abc".to_string()),
                referrer: None,
                method: crate::net::Method::Get,
            },
            tx,
        );
        drain(rx);
        let seen = seen_elsewhere.lock().unwrap();
        assert_eq!(seen.len(), 1, "the redirect was not followed: {seen:?}");
        assert_eq!(
            cookie_header(&seen[0]),
            None,
            "a cookie for one host followed a redirect to another: {:?}",
            seen[0]
        );
    }

    #[test]
    fn a_page_cannot_forge_browser_owned_headers_through_fetch() {
        // `Cookie` and `Referer` are browser-owned in fetch(), and here they
        // have to be: without that, credentials/referrer policy is a
        // suggestion.
        let (addr, seen) = serve_capturing(2, |_| ok_body("{}"));
        let (tx, rx) = mpsc::channel();
        let mut request = Request::bare(format!("http://{addr}/a"));
        request.referrer = Some("http://trusted.test/source".into());
        spawn_js_fetch(
            PageId::headless(1),
            1,
            request,
            "GET".to_string(),
            vec![
                ("Cookie".to_string(), "sid=forged".to_string()),
                ("Referer".to_string(), "http://attacker.test/".to_string()),
                ("User-Agent".to_string(), "forged".to_string()),
                ("X-Ok".to_string(), "kept".to_string()),
            ],
            None,
            tx,
        );
        drain(rx);
        // And with a real one, the jar's answer is the only one that lands —
        // never two `Cookie` headers, which a server is free to read either of.
        let (tx, rx) = mpsc::channel();
        spawn_js_fetch(
            PageId::headless(1),
            2,
            Request {
                url: format!("http://{addr}/b"),
                cookie: Some("sid=real".to_string()),
                referrer: None,
                method: crate::net::Method::Get,
            },
            "GET".to_string(),
            vec![("Cookie".to_string(), "sid=forged".to_string())],
            None,
            tx,
        );
        drain(rx);

        let seen = seen.lock().unwrap();
        assert_eq!(cookie_header(&seen[0]), None, "{:?}", seen[0]);
        assert_eq!(
            header(&seen[0], "referer"),
            Some("http://trusted.test/source")
        );
        assert_ne!(header(&seen[0], "user-agent"), Some("forged"));
        assert!(seen[0].contains("kept"), "an ordinary header was dropped");
        assert_eq!(cookie_header(&seen[1]), Some("sid=real"), "{:?}", seen[1]);
        assert_eq!(
            seen[1].to_ascii_lowercase().matches("cookie:").count(),
            1,
            "two Cookie headers went out: {:?}",
            seen[1]
        );
    }

    #[test]
    fn is_document_matrix() {
        assert!(is_document(200, None));
        assert!(is_document(200, Some("text/html; charset=utf-8")));
        assert!(!is_document(404, Some("text/html")));
        assert!(!is_document(200, Some("image/png")));
    }
}
