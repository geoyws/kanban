//! The operator web view of every registered board.
//!
//! **Why this exists.** Approvals are the bottleneck. Attention items are
//! raised durably and correctly, and settling one means being at a terminal
//! with the right board addressed. Thirteen boards also means no way to see
//! across them without running a command per project. This is the page that
//! answers "what is waiting on me" from a phone.
//!
//! **No second code path.** Every read goes through the same [`Store`] methods
//! the CLI calls. A server that reached past the store would be a second
//! implementation to keep in step, which is the drift ADR-010 and ADR-011 exist
//! to prevent, arriving through a third surface.
//!
//! **It binds loopback and nothing else, deliberately.** There is no `--bind`
//! flag: authentication belongs at the edge (`auth_basic` in nginx), and a flag
//! that could publish an unauthenticated surface to `0.0.0.0` is a footgun
//! whose only correct setting is the default. Fronting it for remote access is
//! the documented arrangement, not a workaround.
//!
//! The write surface is deliberately narrow: an authenticated operator may
//! reply to and resolve an attention item, or open a draft epic. WebSockets
//! carry revision notices, never ledger content or capabilities; the browser
//! fetches the canonical server-rendered projection after a notice.

use crate::model::{Attention, ProjectRecord, SearchOptions, Sitrep, Task};
use crate::registry::{Registry, now_ms};
use crate::search;
use crate::store::Store;
use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use sha1::{Digest, Sha1};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

/// The loopback port `kanban serve` listens on.
///
/// Checked free on this box before it was chosen; nginx reaches it by number,
/// so changing it means changing the vhost too.
pub const DEFAULT_PORT: u16 = 14200;

/// How many rows a detail page will show of any one list.
///
/// The page is for reading, not archaeology: `kb ev --task <id>` has the whole
/// trail and is one command away. A page that renders ten thousand events is
/// slower to load and no more useful.
const DETAIL_ROWS: i64 = 50;

/// A browser reply is a decision note, not a document upload.
const MAX_REPLY_BYTES: usize = 4_096;

enum WebResponse {
    Html(u16, String),
    Redirect(String),
}

/// Serve until killed. Never returns `Ok`.
pub fn serve(port: u16) -> Result<()> {
    let address = format!("127.0.0.1:{port}");
    let server = Server::http(&address)
        .map_err(|error| anyhow::anyhow!("bind {address}: {error}"))
        .with_context(|| format!("serve on {address}"))?;
    eprintln!("kanban serve: http://{address} (loopback only; front it with nginx)");
    for request in server.incoming_requests() {
        if request.url().split('?').next() == Some("/live") {
            // An upgraded socket is long-lived. Keeping it on the accept loop
            // would stop every ordinary page behind the first connected tab.
            thread::spawn(move || websocket(request));
            continue;
        }
        handle(request);
    }
    anyhow::bail!("the listener stopped accepting connections")
}

/// Answer one request, turning an error into a page rather than a dropped
/// connection: a browser given nothing shows its own error, which tells the
/// reader nothing about what went wrong here.
fn handle(mut request: Request) {
    let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| route(&mut request)))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("the page renderer panicked")));
    let (status, html, location) = match response {
        Ok(WebResponse::Html(status, html)) => (status, html, None),
        Ok(WebResponse::Redirect(location)) => (
            303,
            page(
                "Reply recorded",
                "<h1>Reply recorded</h1><p><a href=\"/\">Return to Needs you</a>.</p>",
            ),
            Some(location),
        ),
        Err(error) => (
            500,
            page(
                "Error",
                &format!(
                    "<h1>Error</h1><p class=error>{}</p>",
                    escape(&error.to_string())
                ),
            ),
            None,
        ),
    };
    let header = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..])
        .expect("a static header is always valid");
    let mut response = Response::from_string(html)
        .with_status_code(status)
        .with_header(header);
    if let Some(location) = location {
        response = response.with_header(
            Header::from_bytes(&b"Location"[..], location.as_bytes())
                .expect("a generated relative location is valid"),
        );
    }
    // A client that hung up mid-write is not this server's problem, and
    // crashing on it would take down a page everyone else is still reading.
    let _ = request.respond(response);
}

fn route(request: &mut Request) -> Result<WebResponse> {
    let url = request.url().to_owned();
    if request.method() == &Method::Post {
        return post(request, &url);
    }
    if request.method() != &Method::Get {
        return Ok(WebResponse::Html(
            405,
            page("Method not allowed", "<h1>Method not allowed</h1>"),
        ));
    }
    Ok(WebResponse::Html(200, render(&url)?))
}

/// Route a URL to a rendered page.
fn render(url: &str) -> Result<String> {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(decode)
        .collect::<Vec<_>>();
    let parts = segments.iter().map(String::as_str).collect::<Vec<_>>();
    match parts.as_slice() {
        [] => needs_you(query_value(query, "replied").as_deref()),
        ["boards"] => boards(),
        ["plans"] => plans(query_value(query, "opened").as_deref()),
        ["lanes"] => lanes(),
        ["search"] => search_page(query_value(query, "q").as_deref().unwrap_or("")),
        ["board", project] => board(project),
        ["task", project, id] => task_detail(project, id),
        _ => Ok(page(
            "Not found",
            "<h1>Not found</h1><p>No page at that address. \
             <a href=\"/\">Start over</a>.</p>",
        )),
    }
}

fn post(request: &mut Request, url: &str) -> Result<WebResponse> {
    let path = url.split('?').next().unwrap_or(url);
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(decode)
        .collect::<Vec<_>>();
    let parts = segments.iter().map(String::as_str).collect::<Vec<_>>();
    if !matches!(
        parts.as_slice(),
        ["attention", _, _, "reply"] | ["plan", _, _, "open"]
    ) {
        return Ok(WebResponse::Html(
            404,
            page("Not found", "<h1>Not found</h1>"),
        ));
    }
    if !same_origin(request) {
        return Ok(WebResponse::Html(
            403,
            page(
                "Request refused",
                "<h1>Request refused</h1><p class=error>The action did not come from this site.</p>",
            ),
        ));
    }
    if let ["plan", project, id, "open"] = parts.as_slice() {
        let Ok((_, mut store)) = project_named(project) else {
            return Ok(WebResponse::Html(
                404,
                page("Board not found", "<h1>Board not found</h1>"),
            ));
        };
        let tasks = store.list_tasks(None, None, false)?;
        let is_draft_epic = tasks
            .iter()
            .any(|task| task.id == *id && task.task_type == "epic" && task.status == "draft");
        if !is_draft_epic {
            return Ok(WebResponse::Html(
                409,
                page(
                    "Plan not opened",
                    "<h1>Plan not opened</h1><p class=error>Only an existing draft epic can be opened here.</p>",
                ),
            ));
        }
        if let Err(error) = store.move_task(id, "todo", "geo", serde_json::json!({}), false) {
            return Ok(WebResponse::Html(
                409,
                page(
                    "Plan not opened",
                    &format!(
                        "<h1>Plan not opened</h1><p class=error>{}</p>",
                        escape(&error.to_string())
                    ),
                ),
            ));
        }
        return Ok(WebResponse::Redirect(format!(
            "/plans?opened={}",
            url_encode(id)
        )));
    }
    let length = request.body_length().unwrap_or(0);
    if length == 0 || length > MAX_REPLY_BYTES {
        return Ok(WebResponse::Html(
            400,
            page(
                "Reply required",
                "<h1>Reply required</h1><p class=error>Write a short reply before resolving this item.</p>",
            ),
        ));
    }
    let mut bytes = Vec::with_capacity(length);
    request
        .as_reader()
        .take((MAX_REPLY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_REPLY_BYTES {
        return Ok(WebResponse::Html(
            400,
            page("Reply too long", "<h1>Reply too long</h1>"),
        ));
    }
    let Ok(body) = std::str::from_utf8(&bytes) else {
        return Ok(WebResponse::Html(
            400,
            page("Invalid reply", "<h1>Invalid reply</h1>"),
        ));
    };
    let Ok(value) = strict_form_value(body, "reply") else {
        return Ok(WebResponse::Html(
            400,
            page("Invalid reply", "<h1>Invalid reply</h1>"),
        ));
    };
    let reply = value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let Some(reply) = reply else {
        return Ok(WebResponse::Html(
            400,
            page(
                "Reply required",
                "<h1>Reply required</h1><p class=error>Write a short reply before resolving this item.</p>",
            ),
        ));
    };
    let [_, project, id, _] = parts.as_slice() else {
        unreachable!("the route shape was checked above")
    };
    let Ok((_, mut store)) = project_named(project) else {
        return Ok(WebResponse::Html(
            404,
            page("Board not found", "<h1>Board not found</h1>"),
        ));
    };
    if let Err(error) = store.resolve_attention(id, "geo", Some(&reply)) {
        return Ok(WebResponse::Html(
            409,
            page(
                "Reply not recorded",
                &format!(
                    "<h1>Reply not recorded</h1><p class=error>{}</p>",
                    escape(&error.to_string())
                ),
            ),
        ));
    }
    Ok(WebResponse::Redirect(format!(
        "/?replied={}",
        url_encode(id)
    )))
}

fn query_value(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (decode(&key.replace('+', " ")) == name).then(|| decode(&value.replace('+', " ")))
    })
}

fn header<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|candidate| candidate.field.to_string().eq_ignore_ascii_case(name))
        .map(|candidate| candidate.value.as_str())
}

/// Browser writes are accepted only from the authority they are addressed to.
/// OAuth at the edge authenticates the person; this check prevents another
/// origin from making that authenticated browser submit a hidden form.
fn same_origin(request: &Request) -> bool {
    let Some(host) = header(request, "Host") else {
        return false;
    };
    let Some(origin) = header(request, "Origin") else {
        return false;
    };
    let Some((scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    if scheme != "http" && scheme != "https" {
        return false;
    }
    let authority = rest.split('/').next().unwrap_or("");
    !authority.is_empty() && authority.eq_ignore_ascii_case(host)
}

fn strict_form_value(form: &str, name: &str) -> Result<Option<String>> {
    for pair in form.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if strict_form_decode(key)? == name {
            return Ok(Some(strict_form_decode(value)?));
        }
    }
    Ok(None)
}

fn strict_form_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' => {
                anyhow::ensure!(index + 2 < bytes.len(), "malformed percent escape in reply");
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3])?;
                let byte = u8::from_str_radix(hex, 16)
                    .with_context(|| format!("malformed percent escape %{hex}"))?;
                out.push(byte);
                index += 3;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(out).context("reply form contains invalid UTF-8")
}

fn url_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Percent-decoding, because a project name or task id may contain characters
/// a browser escapes on the way here. Malformed escapes are left as written
/// rather than dropped — a name that does not decode will simply not match a
/// board, which is a clearer outcome than silently looking up a different one.
fn decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every registered project that still has a board file.
///
/// A board that has gone missing is skipped rather than fatal: one broken
/// registration must not take out the page that lists the other twelve.
/// `kanban doctor` is where that gets reported, and it is linked from Boards.
fn projects() -> Result<Vec<(ProjectRecord, Store)>> {
    let registry = Registry::open()?;
    let mut out = Vec::new();
    for project in registry.projects()? {
        let path = Path::new(&project.board_path);
        if !path.exists() {
            continue;
        }
        let store = Store::open(path)?;
        out.push((project, store));
    }
    Ok(out)
}

fn project_named(name: &str) -> Result<(ProjectRecord, Store)> {
    projects()?
        .into_iter()
        .find(|(project, _)| project.name == name)
        .with_context(|| format!("no board named {name}"))
}

// --------------------------------------------------------------- live status

fn websocket(request: Request) {
    if request.method() != &Method::Get || !same_origin(&request) {
        let response =
            Response::from_string("websocket origin refused").with_status_code(StatusCode(403));
        let _ = request.respond(response);
        return;
    }
    let is_upgrade =
        header(&request, "Upgrade").is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let version_ok = header(&request, "Sec-WebSocket-Version") == Some("13");
    let Some(key) = header(&request, "Sec-WebSocket-Key").map(str::to_owned) else {
        let _ =
            request.respond(Response::from_string("websocket key required").with_status_code(400));
        return;
    };
    if !is_upgrade || !version_ok {
        let _ = request
            .respond(Response::from_string("websocket upgrade required").with_status_code(400));
        return;
    }

    let accept = websocket_accept(&key);
    let response = Response::new_empty(StatusCode(101))
        .with_header(
            "Upgrade: websocket"
                .parse::<Header>()
                .expect("static header"),
        )
        .with_header(
            "Connection: Upgrade"
                .parse::<Header>()
                .expect("static header"),
        )
        .with_header(
            format!("Sec-WebSocket-Accept: {accept}")
                .parse::<Header>()
                .expect("SHA-1 base64 is a valid header"),
        );
    let mut stream = request.upgrade("websocket", response);
    let Ok(mut revision) = ledger_revision() else {
        return;
    };
    if write_ws_text(
        &mut stream,
        &format!(r#"{{"type":"ready","revision":"{revision:016x}"}}"#),
    )
    .is_err()
    {
        return;
    }
    let mut ticks = 0_u8;
    loop {
        thread::sleep(Duration::from_secs(1));
        ticks = ticks.wrapping_add(1);
        let Ok(current) = ledger_revision() else {
            continue;
        };
        if current != revision {
            revision = current;
            if write_ws_text(
                &mut stream,
                &format!(r#"{{"type":"refresh","revision":"{revision:016x}"}}"#),
            )
            .is_err()
            {
                return;
            }
            ticks = 0;
        } else if ticks >= 15 {
            if write_ws_text(&mut stream, r#"{"type":"heartbeat"}"#).is_err() {
                return;
            }
            ticks = 0;
        }
    }
}

fn websocket_accept(key: &str) -> String {
    let mut digest = Sha1::new();
    digest.update(key.as_bytes());
    digest.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    BASE64.encode(digest.finalize())
}

fn write_ws_text(stream: &mut (impl Write + ?Sized), text: &str) -> std::io::Result<()> {
    let bytes = text.as_bytes();
    let mut frame = Vec::with_capacity(bytes.len() + 10);
    frame.push(0x81); // FIN + text
    match bytes.len() {
        length @ 0..=125 => frame.push(length as u8),
        length @ 126..=65_535 => {
            frame.push(126);
            frame.extend_from_slice(&(length as u16).to_be_bytes());
        }
        length => {
            frame.push(127);
            frame.extend_from_slice(&(length as u64).to_be_bytes());
        }
    }
    frame.extend_from_slice(bytes);
    stream.write_all(&frame)?;
    stream.flush()
}

/// Fingerprint the files SQLite can currently be writing. WAL mode updates the
/// `-wal` inode rather than the main database, while rollback mode briefly uses
/// `-journal`; both therefore participate in the revision.
fn ledger_revision() -> Result<u64> {
    let registry = Registry::open()?;
    let mut hasher = DefaultHasher::new();
    for project in registry.projects()? {
        project.name.hash(&mut hasher);
        project.board_path.hash(&mut hasher);
        let board = PathBuf::from(&project.board_path);
        hash_file_state(&board, &mut hasher);
        hash_file_state(
            &PathBuf::from(format!("{}-wal", board.display())),
            &mut hasher,
        );
        hash_file_state(
            &PathBuf::from(format!("{}-journal", board.display())),
            &mut hasher,
        );
    }
    Ok(hasher.finish())
}

fn hash_file_state(path: &Path, hasher: &mut impl Hasher) {
    path.hash(hasher);
    match path.metadata() {
        Ok(metadata) => {
            true.hash(hasher);
            metadata.len().hash(hasher);
            metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .hash(hasher);
        }
        Err(_) => false.hash(hasher),
    }
}

// ---------------------------------------------------------------- the screens

/// The landing page, and the reason the server exists: everything open across
/// every board, priority first and then oldest, so interrupts lead while age
/// remains the tie-breaker that prevents starvation within a level.
fn needs_you(replied: Option<&str>) -> Result<String> {
    let mut items: Vec<(String, Attention)> = Vec::new();
    for (project, store) in projects()? {
        for item in store.attention(Some("open"), None, None, 1000, false)? {
            items.push((project.name.clone(), item));
        }
    }
    items.sort_by(|(project_a, item_a), (project_b, item_b)| {
        (item_a.priority, item_a.created_at, &item_a.id, project_a).cmp(&(
            item_b.priority,
            item_b.created_at,
            &item_b.id,
            project_b,
        ))
    });

    let mut html = String::from(
        "<div class=heading><h1>Needs you</h1><span class=live data-live>connecting</span></div>",
    );
    if let Some(id) = replied {
        html.push_str(&format!(
            "<p class=success>Reply recorded for <code>{}</code>.</p>",
            escape(id)
        ));
    }
    if items.is_empty() {
        html.push_str(
            "<p class=empty>Nothing is waiting. \
             An empty list here means every raised item has been settled.</p>",
        );
        return Ok(page("Needs you", &html));
    }
    html.push_str(&format!(
        "<p class=count>{} open across {} boards.</p>",
        items.len(),
        items
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
    ));
    for (project, item) in &items {
        html.push_str("<article class=item>");
        html.push_str(&format!(
            "<p class=meta>{priority} <span class=\"kind kind-{kind}\">{kind}</span> \
             <a href=\"/board/{project_url}\">{project}</a> \
             · raised by {who} · waiting {age}</p>",
            kind = escape(&item.kind),
            priority = priority_badge(item.priority, item.priority_level.as_deref()),
            project_url = escape(project),
            project = escape(project),
            who = escape(&item.raised_by),
            age = age(item.created_at),
        ));
        html.push_str(&format!("<p class=body>{}</p>", escape(&item.body)));
        if let Some(task) = &item.task_id {
            html.push_str(&format!(
                "<p class=meta>about <a href=\"/task/{project}/{task}\">{task}</a></p>",
                project = escape(project),
                task = escape(task),
            ));
        }
        html.push_str(&format!(
            "<form class=reply method=post action=\"/attention/{project}/{id}/reply\">\
             <label for=\"reply-{id}\">Your reply</label>\
             <textarea id=\"reply-{id}\" name=reply maxlength={max} \
             placeholder=\"Answer this item…\"></textarea>\
             <div class=actions><button type=submit>Send reply</button>\
             <button type=button class=quick data-reply=\"Approved. Proceed.\">Approve</button>\
             <button type=button class=quick data-reply=\"Declined. Do not proceed.\">Decline</button>\
             <button type=button class=quick data-reply=\"Proceed with the recommended option.\">Proceed</button>\
             </div></form>",
            project = escape(&url_encode(project)),
            id = escape(&url_encode(&item.id)),
            max = MAX_REPLY_BYTES,
        ));
        html.push_str("</article>");
    }
    Ok(page("Needs you", &html))
}

/// Cross-board retrieval for people who should not need to know which board
/// owns a fact before they can find it. Ranking and bounds are the same shared
/// implementation used by the CLI and MCP tool.
fn search_page(query: &str) -> Result<String> {
    let query = query.trim();
    let mut html = format!(
        "<h1>Search</h1><form class=search-page action=/search method=get>\
         <input name=q value=\"{}\" placeholder=\"Task, decision, handoff, rule…\" autofocus>\
         <button type=submit>Search</button></form>",
        escape(query)
    );
    if query.is_empty() {
        html.push_str(
            "<p class=empty>Search every board, including tasks, notes, checkpoints, \
             handoffs, attention, sitreps, rules, and their audit trail.</p>",
        );
        return Ok(page("Search", &html));
    }
    let options = SearchOptions {
        query: query.to_owned(),
        source: None,
        status: None,
        tags: Vec::new(),
        lane: None,
        after: None,
        before: None,
        include_archived: false,
        limit: 30,
        max_chars: 30_000,
    };
    let registry = Registry::open()?;
    let mut results = Vec::new();
    let mut boards = Vec::new();
    let mut missing = Vec::new();
    for project in registry.projects()? {
        if !Path::new(&project.board_path).is_file() {
            missing.push(project.name);
            continue;
        }
        let store = Store::open(Path::new(&project.board_path))?;
        results.extend(store.search(&project.name, &options)?);
        boards.push(project.name);
    }
    results.extend(search::search_global_rules(
        &registry.global_rules(false)?,
        &options,
    ));
    let receipt = search::bound_receipt(
        query,
        boards,
        missing,
        results,
        options.limit,
        options.max_chars,
    );
    html.push_str(&format!(
        "<p class=count>{} result{} across {} board{} · model <code>{}</code>{}</p>",
        receipt.results.len(),
        if receipt.results.len() == 1 { "" } else { "s" },
        receipt.boards.len(),
        if receipt.boards.len() == 1 { "" } else { "s" },
        escape(&receipt.embedding_model),
        if receipt.truncated { " · bounded" } else { "" },
    ));
    if receipt.results.is_empty() {
        html.push_str("<p class=empty>No matching Kanban knowledge.</p>");
    }
    for result in receipt.results {
        let title = if let Some(task_id) = &result.task_id {
            format!(
                "<a href=\"/task/{}/{}\">{}</a>",
                escape(&result.board),
                escape(task_id),
                escape(&result.title)
            )
        } else {
            escape(&result.title)
        };
        html.push_str(&format!(
            "<article class=search-result><h2>{title}</h2>\
             <p class=meta>{board} · {kind} · score {score:.3}{status}{lane}{tags}</p>\
             <p class=body>{snippet}</p><p class=citation><code>{citation}</code></p></article>",
            title = title,
            board = escape(&result.board),
            kind = escape(&result.source_kind),
            score = result.score,
            status = result
                .status
                .as_ref()
                .map(|status| format!(" · {}", escape(status)))
                .unwrap_or_default(),
            lane = result
                .lane
                .as_ref()
                .map(|lane| format!(" · {}", escape(lane)))
                .unwrap_or_default(),
            tags = tag_list(&result.tags),
            snippet = escape(&result.snippet),
            citation = escape(&result.citation),
        ));
    }
    if !receipt.missing_boards.is_empty() {
        html.push_str(&format!(
            "<p class=error>Missing board files: {}</p>",
            escape(&receipt.missing_boards.join(", "))
        ));
    }
    Ok(page(&format!("Search: {query}"), &html))
}

/// Every board at a glance — the `dashboard` projection, rendered.
fn boards() -> Result<String> {
    let mut html = String::from(
        "<h1>Boards</h1><table><thead><tr>\
        <th>Board</th><th class=n>Open attention</th><th class=n>To do</th>\
        <th class=n>In progress</th><th class=n>Stale</th>\
        <th class=n>Handoffs</th><th class=n>Tasks</th></tr></thead><tbody>",
    );
    let mut rows = Vec::new();
    for (project, store) in projects()? {
        let tasks = store.list_tasks(None, None, false)?;
        let count = |status: &str| tasks.iter().filter(|task| task.status == status).count();
        let attention = store.attention(Some("open"), None, None, 1000, false)?;
        let handoffs = store.handoffs(None, Some("pending"), None, 100, false)?;
        let queued = tasks
            .iter()
            .filter(|task| task.status == "todo")
            .map(|task| (task.priority, task.created_at))
            .chain(
                attention
                    .iter()
                    .map(|item| (item.priority, item.created_at)),
            )
            .chain(handoffs.iter().map(|item| (item.priority, item.created_at)))
            .collect::<Vec<_>>();
        let highest = queued
            .iter()
            .map(|(priority, _)| *priority)
            .min()
            .unwrap_or(i64::MAX);
        let oldest = queued
            .iter()
            .filter(|(priority, _)| *priority == highest)
            .map(|(_, created_at)| *created_at)
            .min()
            .unwrap_or(i64::MAX);
        let row = format!(
            "<tr><td><a href=\"/board/{url}\">{name}</a></td>\
             <td class=\"n{flag}\">{attention}</td><td class=n>{todo}</td>\
             <td class=n>{doing}</td><td class=n>{stale}</td>\
             <td class=n>{handoffs}</td><td class=n>{total}</td></tr>",
            url = escape(&project.name),
            name = escape(&project.name),
            flag = if attention.is_empty() { "" } else { " waiting" },
            attention = attention.len(),
            todo = count("todo"),
            doing = count("in_progress"),
            stale = store.stale_tasks()?.len(),
            handoffs = handoffs.len(),
            total = tasks.len(),
        );
        rows.push((highest, oldest, project.name.clone(), row));
    }
    rows.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    for (_, _, _, row) in rows {
        html.push_str(&row);
    }
    html.push_str("</tbody></table>");
    html.push_str(
        "<p class=meta>Counts come from the same projection as \
         <code>kb dash</code>. Integrity is <code>kb doctor</code>'s job, \
         not this page's.</p>",
    );
    Ok(page("Boards", &html))
}

/// Plans: draft epics, whose body is the plan itself.
///
/// A draft holds back everything beneath it, so this page is also the answer to
/// "what work is currently gated" — the children are listed with each plan
/// because opening the plan is what releases them.
fn plans(opened: Option<&str>) -> Result<String> {
    let mut html = String::from("<h1>Plans</h1>");
    if let Some(id) = opened {
        html.push_str(&format!(
            "<p class=success>Opened plan <code>{}</code>. Its child work is now eligible for claims.</p>",
            escape(id)
        ));
    }
    let mut found = 0;
    for (project, store) in projects()? {
        let tasks = store.list_tasks(None, None, false)?;
        let drafts = tasks
            .iter()
            .filter(|task| task.status == "draft" && task.task_type == "epic")
            .collect::<Vec<_>>();
        for plan in drafts {
            found += 1;
            html.push_str("<article class=plan>");
            html.push_str(&format!(
                "<h2><a href=\"/task/{project}/{id}\">{title}</a></h2>\
                 <p class=meta><a href=\"/board/{project}\">{project}</a> · \
                 {id} · {priority} · drafted {age}{tags}</p>",
                project = escape(&project.name),
                id = escape(&plan.id),
                title = escape(&plan.title),
                priority = priority_badge(plan.priority, plan.priority_level.as_deref()),
                age = ago(plan.created_at),
                tags = tag_list(&plan.tags),
            ));
            let children = tasks
                .iter()
                .filter(|task| task.parent_id.as_deref() == Some(plan.id.as_str()))
                .collect::<Vec<_>>();
            if !children.is_empty() {
                html.push_str(&format!(
                    "<p class=meta>Holds back {} row{}, none claimable until this plan \
                     is opened:</p><ul class=children>",
                    children.len(),
                    if children.len() == 1 { "" } else { "s" }
                ));
                for child in children {
                    html.push_str(&format!(
                        "<li>{priority} <a href=\"/task/{project}/{id}\">{id}</a> \
                         <span class=status>{status}</span> {title}</li>",
                        project = escape(&project.name),
                        id = escape(&child.id),
                        status = escape(&child.status),
                        title = escape(&child.title),
                        priority = priority_badge(child.priority, child.priority_level.as_deref()),
                    ));
                }
                html.push_str("</ul>");
            }
            if let Some(body) = &plan.body {
                html.push_str(&format!("<pre class=plan-body>{}</pre>", escape(body)));
            }
            html.push_str(&format!(
                "<form method=post action=\"/plan/{project_path}/{id_path}/open\">\
                 <button type=submit>Open plan</button></form>\
                 <p class=cmd>Equivalent: <code>kb t mv {id} todo --as geo --project {project}</code></p>",
                project_path = url_encode(&project.name),
                id_path = url_encode(&plan.id),
                id = escape(&plan.id),
                project = escape(&project.name),
            ));
            html.push_str("</article>");
        }
    }
    if found == 0 {
        html.push_str(
            "<p class=empty>No drafted plans. A plan is an epic with \
             <code>--status draft</code>; its body is the plan and its children are \
             the work, gated until it is opened.</p>",
        );
    }
    Ok(page("Plans", &html))
}

/// Where every lane stands, newest first.
///
/// The counterpart to Needs you: that page is what waits on the operator, this
/// is what the agents are doing. A lane that has been posting is legible here
/// without anyone opening a terminal or waiting for a handoff.
fn lanes() -> Result<String> {
    let mut by_lane: std::collections::BTreeMap<(String, String), Vec<Sitrep>> =
        std::collections::BTreeMap::new();
    for (project, store) in projects()? {
        for update in store.sitreps(None, false, None, 200)? {
            by_lane
                .entry((project.name.clone(), update.lane.clone()))
                .or_default()
                .push(update);
        }
    }
    let mut html = String::from("<h1>Lanes</h1>");
    if by_lane.is_empty() {
        html.push_str(
            "<p class=empty>No lane has posted a sitrep. \
             <code>kb sr new \"…\" --as AGENT --lane LANE</code> writes one — no task \
             and no lease required, which is the point of it.</p>",
        );
        return Ok(page("Lanes", &html));
    }
    // Most recently active first. Nothing deletes a sitrep, and nothing
    // should — but that means a lane whose driver is long gone keeps its rows
    // forever, and alphabetical order parks it at the top of the page. Sorting
    // by recency lets a dead lane sink out of the way without destroying what
    // it said, which is the same answer archiving gives within a lane.
    let mut ordered = by_lane.into_iter().collect::<Vec<_>>();
    ordered.sort_by_key(|(_, updates)| {
        std::cmp::Reverse(updates.iter().map(|u| u.created_at).max().unwrap_or(0))
    });
    for ((project, lane), updates) in &ordered {
        html.push_str("<article class=item>");
        html.push_str(&format!(
            "<h2>{lane} <span class=count><a href=\"/board/{project_url}\">{project}</a></span></h2>",
            lane = escape(lane),
            project_url = escape(project),
            project = escape(project),
        ));
        for update in updates {
            html.push_str(&format!(
                "<p class=meta>{author} · {age}{task}{branch}</p><p class=body>{body}</p>",
                author = escape(&update.author),
                age = ago(update.created_at),
                task = update
                    .task_id
                    .as_ref()
                    .map(|id| format!(
                        " · <a href=\"/task/{project}/{id}\">{id}</a>",
                        project = escape(project),
                        id = escape(id)
                    ))
                    .unwrap_or_default(),
                branch = update
                    .branch
                    .as_ref()
                    .map(|branch| format!(" · <span class=lane>{}</span>", escape(branch)))
                    .unwrap_or_default(),
                body = escape(&update.body),
            ));
        }
        html.push_str("</article>");
    }
    Ok(page("Lanes", &html))
}

/// One board's rows, grouped by status in workflow order.
fn board(name: &str) -> Result<String> {
    let (project, store) = project_named(name)?;
    let tasks = store.list_tasks(None, None, false)?;
    let global_rules = Registry::open()?.global_rules_for(Some(&project.name), false)?;
    let project_rules = store.rules(false)?;
    let mut html = format!("<h1>{}</h1>", escape(&project.name));
    html.push_str(&format!(
        "<p class=meta>{} · {} rows</p>",
        escape(&project.canonical_root),
        tasks.len()
    ));
    for (heading, rules) in [
        ("Global rules", global_rules),
        ("Project rules", project_rules),
    ] {
        if rules.is_empty() {
            continue;
        }
        html.push_str(&format!(
            "<h2>{heading} <span class=count>{}</span></h2>",
            rules.len()
        ));
        for rule in rules {
            let headline = rule.body.lines().next().unwrap_or_default();
            let targets = rule
                .board_tags
                .as_ref()
                .map(|tags| format!(" <span class=lane>{}</span>", escape(&tags.join(", "))))
                .unwrap_or_default();
            html.push_str(&format!(
                "<details class=rule><summary><code>{id}</code> {headline}{targets}</summary>\
                 <pre>{body}</pre></details>",
                id = escape(&rule.id),
                headline = escape(headline),
                targets = targets,
                body = escape(&rule.body),
            ));
        }
    }
    for status in crate::model::TASK_STATUSES {
        let rows = tasks
            .iter()
            .filter(|task| task.status == status)
            .collect::<Vec<_>>();
        if rows.is_empty() {
            continue;
        }
        html.push_str(&format!(
            "<h2>{} <span class=count>{}</span></h2><ul class=rows>",
            escape(status),
            rows.len()
        ));
        for task in rows {
            html.push_str(&format!(
                "<li>{priority} <a href=\"/task/{project}/{id}\">{id}</a> \
                 <span class=\"type type-{ty}\">{ty}</span> {title}{lane}{tags}</li>",
                project = escape(&project.name),
                id = escape(&task.id),
                ty = escape(&task.task_type),
                title = escape(&task.title),
                priority = priority_badge(task.priority, task.priority_level.as_deref()),
                lane = task
                    .lane
                    .as_ref()
                    .map(|lane| format!(" <span class=lane>{}</span>", escape(lane)))
                    .unwrap_or_default(),
                tags = tag_list(&task.tags),
            ));
        }
        html.push_str("</ul>");
    }
    Ok(page(&project.name, &html))
}

/// One task in full: what it is, where its work happened, and the trail.
fn task_detail(project_name: &str, id: &str) -> Result<String> {
    let (project, store) = project_named(project_name)?;
    let task = store.require_task(id)?;
    let mut html = format!("<h1>{}</h1>", escape(&task.title));
    html.push_str(&format!(
        "<p class=meta><a href=\"/board/{project}\">{project}</a> · {id} · \
         <span class=\"type type-{ty}\">{ty}</span> \
         <span class=status>{status}</span> · {priority}{tags}</p>",
        project = escape(&project.name),
        id = escape(&task.id),
        ty = escape(&task.task_type),
        status = escape(&task.status),
        priority = priority_badge(task.priority, task.priority_level.as_deref()),
        tags = tag_list(&task.tags),
    ));
    html.push_str(&facts(&project.name, &task));
    if let Some(body) = &task.body {
        html.push_str(&format!("<h2>Body</h2><pre>{}</pre>", escape(body)));
    }

    // Provenance: where the work actually happened. Captured rather than
    // asked for, so it is present when there was a repository and absent
    // when there was not -- never invented.
    if let Some(claim) = store.get_claim(&task.id)? {
        html.push_str("<h2>Held by</h2><dl>");
        html.push_str(&row("agent", &escape(&claim.agent_id)));
        html.push_str(&row("claimed", &stamp(claim.claimed_at)));
        html.push_str(&row("expires", &stamp(claim.expires_at)));
        for (label, value) in [
            ("worktree", claim.worktree.as_deref()),
            ("kind", claim.worktree_kind.as_deref()),
            ("branch", claim.branch.as_deref()),
            ("HEAD", claim.head_sha.as_deref()),
            ("root HEAD", claim.root_head.as_deref()),
        ] {
            if let Some(value) = value {
                html.push_str(&row(label, &escape(value)));
            }
        }
        html.push_str("</dl>");
        // Never the lease token: it is a capability, and a read surface that
        // renders one hands whoever loads the page the ability to write.
    }

    let notes = store.notes(&task.id, DETAIL_ROWS)?;
    if !notes.is_empty() {
        html.push_str("<h2>Notes</h2>");
        for note in notes {
            html.push_str(&format!(
                "<article class=note><p class=meta><span class=kind>{kind}</span> \
                 {author} · {when}</p><pre>{body}</pre></article>",
                kind = escape(&note.kind),
                author = escape(&note.author),
                when = stamp(note.created_at),
                body = escape(&note.body),
            ));
        }
    }

    let checkpoints = store.checkpoints(&task.id, DETAIL_ROWS)?;
    if !checkpoints.is_empty() {
        html.push_str("<h2>Checkpoints</h2>");
        for point in checkpoints {
            html.push_str(&format!(
                "<article class=note><p class=meta><span class=kind>{state}</span> \
                 {author} · {when}</p><dl>",
                state = escape(&point.state),
                author = escape(&point.author),
                when = stamp(point.created_at),
            ));
            html.push_str(&row("summary", &escape(&point.summary)));
            html.push_str(&row("intent", &escape(&point.intent)));
            html.push_str(&row("next", &escape(&point.next_action)));
            for (label, value) in [
                ("branch", point.branch.as_deref()),
                ("HEAD", point.head_sha.as_deref()),
                ("root HEAD", point.root_head.as_deref()),
                ("tree", point.dirty_summary.as_deref()),
            ] {
                if let Some(value) = value {
                    html.push_str(&row(label, &escape(value)));
                }
            }
            html.push_str("</dl></article>");
        }
    }

    let events = store.events(Some(&task.id), None, DETAIL_ROWS, true)?;
    if !events.is_empty() {
        html.push_str(
            "<h2>Trail</h2><table><thead><tr><th>When</th><th>What</th>\
                       <th>Who</th><th>Detail</th></tr></thead><tbody>",
        );
        for event in events {
            html.push_str(&format!(
                "<tr><td class=when>{when}</td><td><code>{kind}</code></td>\
                 <td>{who}</td><td class=payload>{payload}</td></tr>",
                when = stamp(event.created_at),
                kind = escape(&event.kind),
                who = escape(event.actor.as_deref().unwrap_or("—")),
                payload = escape(&compact(&event.payload)),
            ));
        }
        html.push_str("</tbody></table>");
        html.push_str(&format!(
            "<p class=meta>Newest {DETAIL_ROWS} shown. The whole trail is \
             <code>kb ev --task {id} --project {project} --json</code>.</p>",
            id = escape(&task.id),
            project = escape(&project.name),
        ));
    }
    Ok(page(&task.title, &html))
}

// ------------------------------------------------------------------ rendering

fn facts(project: &str, task: &Task) -> String {
    let mut html = String::from("<dl class=facts>");
    html.push_str(&row("created", &stamp(task.created_at)));
    html.push_str(&row("updated", &stamp(task.updated_at)));
    if let Some(done) = task.completed_at {
        html.push_str(&row("completed", &stamp(done)));
    }
    if let Some(parent) = &task.parent_id {
        html.push_str(&row(
            "parent",
            &format!(
                "<a href=\"/task/{project}/{parent}\">{parent}</a>",
                project = escape(project),
                parent = escape(parent),
            ),
        ));
    }
    for (label, value) in [
        ("assignee", task.assignee.as_deref()),
        ("lane", task.lane.as_deref()),
        ("deliverable", task.deliverable.as_deref()),
    ] {
        if let Some(value) = value {
            html.push_str(&row(label, &escape(value)));
        }
    }
    if task.driver_only {
        html.push_str(&row("driver only", "yes"));
    }
    html.push_str("</dl>");
    html
}

fn row(label: &str, value: &str) -> String {
    format!("<dt>{}</dt><dd>{value}</dd>", escape(label))
}

fn tag_list(tags: &[String]) -> String {
    tags.iter()
        .map(|tag| format!(" <span class=tag>{}</span>", escape(tag)))
        .collect()
}

fn priority_badge(priority: i64, level: Option<&str>) -> String {
    match level {
        Some(level) => format!(
            "<span class=\"priority priority-{class}\" title=\"stored priority {priority}\">{level}</span>",
            class = escape(&level.to_ascii_lowercase()),
            level = escape(level),
        ),
        None => format!(
            "<span class=\"priority priority-legacy\" title=\"legacy out-of-band priority\">{priority}</span>"
        ),
    }
}

/// A one-line rendering of an event payload, since the column is free JSON and
/// a pretty-printed object per row would bury the trail it is meant to show.
fn compact(payload: &serde_json::Value) -> String {
    match payload {
        serde_json::Value::Object(map) if map.is_empty() => String::new(),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, value)| match value {
                serde_json::Value::String(text) => format!("{key}={text}"),
                other => format!("{key}={other}"),
            })
            .collect::<Vec<_>>()
            .join("  "),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// **Every** value interpolated into a page goes through this.
///
/// Task titles, note bodies, attention text and plan bodies are written by
/// agents and by whatever they were reading at the time. Rendering one
/// unescaped would let a row on the board execute script in the operator's
/// browser — against a page that, from phase 2, can approve things.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// A millisecond stamp as a readable UTC instant.
///
/// Deliberately not localised: this box runs on UTC, the ledger stores UTC, and
/// a page that quietly shifted stamps would disagree with every `--json` read
/// of the same row.
fn stamp(ms: i64) -> String {
    let seconds = ms.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

/// How long ago, in the coarsest unit that is still true.
fn age(ms: i64) -> String {
    let minutes = (now_ms() - ms).max(0) / 60_000;
    match minutes {
        0 => "just now".to_owned(),
        1 => "1 min".to_owned(),
        m if m < 60 => format!("{m} min"),
        m if m < 1440 => format!("{}h{:02}m", m / 60, m % 60),
        m => format!("{}h", m / 60),
    }
}

/// How long ago, as a phrase that reads correctly in a sentence.
///
/// `age` alone produced "just now ago", because the shortest interval is
/// already a complete phrase and the rest are bare durations.
fn ago(ms: i64) -> String {
    let text = age(ms);
    if text == "just now" {
        text
    } else {
        format!("{text} ago")
    }
}

/// Days since the epoch to a civil date (Howard Hinnant's algorithm).
///
/// Written out rather than pulled in: a date crate would be a sixth dependency
/// for one formatting call, and this is the one calculation in it.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// The page shell.
///
/// The styling here is structural, not a design: readable defaults so phase 1
/// is usable, and a clean skeleton for the `/frontend-design` pass that phase 3
/// is for. It is inline because a second request for a stylesheet is a second
/// route to serve and cache, for a page this size.
fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{title} · kanban</title><style>{CSS}</style></head><body>\
         <nav><a href=\"/\">Needs you</a><a href=\"/lanes\">Lanes</a>\
         <a href=\"/boards\">Boards</a><a href=\"/plans\">Plans</a>\
         <form action=/search method=get><input name=q aria-label=Search placeholder=\"Search Kanban\"></form>\
         </nav><main>{body}</main>\
         <footer>live operator view · <code>kanban serve</code></footer>\
         <script>{JS}</script></body></html>",
        title = escape(title),
    )
}

const JS: &str = r#"
const setLive = text => { const el = document.querySelector('[data-live]'); if (el) el.textContent = text; };
const hasDraftReply = () => [...document.querySelectorAll('textarea[name=reply]')].some(el => el.value.trim());
function bindQuickReplies() {
  document.querySelectorAll('button.quick').forEach(button => {
    button.onclick = () => {
      const form = button.closest('form');
      form.querySelector('textarea[name=reply]').value = button.dataset.reply;
      form.requestSubmit();
    };
  });
}
async function refreshProjection() {
  if (hasDraftReply()) { setLive('update waiting'); return; }
  const response = await fetch(location.pathname + location.search, {credentials: 'same-origin'});
  if (!response.ok) throw new Error(`refresh ${response.status}`);
  const next = new DOMParser().parseFromString(await response.text(), 'text/html').querySelector('main');
  document.querySelector('main').replaceWith(next);
  bindQuickReplies();
  setLive('live');
}
function connectLive() {
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
  const socket = new WebSocket(`${scheme}://${location.host}/live`);
  socket.onopen = () => setLive('live');
  socket.onmessage = event => {
    try { if (JSON.parse(event.data).type === 'refresh') refreshProjection().catch(() => setLive('refresh failed')); } catch (_) {}
  };
  socket.onclose = () => { setLive('reconnecting'); setTimeout(connectLive, 1500); };
  socket.onerror = () => socket.close();
}
bindQuickReplies();
connectLive();
"#;

const CSS: &str = "\
*{box-sizing:border-box}\
body{margin:0;font:16px/1.55 ui-sans-serif,system-ui,-apple-system,sans-serif;\
color:#e6edf3;background:#0d1117}\
nav{display:flex;gap:1rem;padding:.9rem 1.2rem;background:#161b22;\
border-bottom:1px solid #30363d;position:sticky;top:0}\
nav a{color:#e6edf3;text-decoration:none;font-weight:600}\
nav a:hover{color:#58a6ff}\
nav form{margin-left:auto}\
input,button,textarea{font:inherit;color:#e6edf3;background:#0d1117;border:1px solid #30363d;\
border-radius:6px;padding:.35rem .55rem}\
button{cursor:pointer;background:#1f6feb;border-color:#1f6feb;font-weight:600}\
.quick{background:#21262d;border-color:#30363d}\
.search-page{display:flex;gap:.5rem}.search-page input{flex:1}\
main{max-width:60rem;margin:0 auto;padding:1.2rem}\
footer{max-width:60rem;margin:0 auto;padding:1.2rem;color:#8b949e;font-size:.85rem}\
h1{font-size:1.5rem;margin:.2rem 0 1rem}\
h2{font-size:1.05rem;margin:1.6rem 0 .5rem;color:#c9d1d9}\
a{color:#58a6ff}\
code{font:.85em ui-monospace,SFMono-Regular,Menlo,monospace;background:#161b22;\
padding:.1em .35em;border-radius:4px}\
pre{background:#161b22;border:1px solid #30363d;border-radius:6px;padding:.8rem;\
overflow-x:auto;white-space:pre-wrap;word-break:break-word;font:.85rem/1.5 \
ui-monospace,SFMono-Regular,Menlo,monospace}\
table{width:100%;border-collapse:collapse;font-size:.9rem}\
th,td{text-align:left;padding:.45rem .6rem;border-bottom:1px solid #21262d;\
vertical-align:top}\
th{color:#8b949e;font-weight:600}\
td.n,th.n{text-align:right;font-variant-numeric:tabular-nums}\
td.waiting{color:#f0883e;font-weight:700}\
td.when{white-space:nowrap;color:#8b949e}\
td.payload{color:#8b949e;font-size:.85rem;word-break:break-word}\
.item,.note,.plan,.search-result{border:1px solid #30363d;border-radius:8px;padding:.9rem;\
margin:.8rem 0;background:#0f141a}\
.heading{display:flex;align-items:center;justify-content:space-between;gap:1rem}\
.live{color:#3fb950;font-size:.75rem;text-transform:uppercase;letter-spacing:.08em}\
.success{background:#12351f;border:1px solid #2c7a44;border-radius:6px;padding:.6rem .75rem}\
.reply{margin-top:.8rem}.reply label{display:block;color:#8b949e;font-size:.8rem;margin-bottom:.25rem}\
.reply textarea{display:block;width:100%;min-height:4.7rem;resize:vertical}\
.actions{display:flex;flex-wrap:wrap;gap:.45rem;margin-top:.5rem}\
.search-result h2{margin:.1rem 0}.citation{margin:.4rem 0 0;color:#8b949e}\
.meta{color:#8b949e;font-size:.85rem;margin:.2rem 0}\
.body{margin:.5rem 0;white-space:pre-wrap}\
.cmd{margin:.5rem 0 0;font-size:.85rem}\
.empty{color:#8b949e}\
.count{color:#8b949e;font-weight:400;font-size:.85rem}\
.kind,.type,.status,.lane,.tag,.priority{display:inline-block;padding:.05em .5em;\
border-radius:999px;font-size:.75rem;font-weight:600;border:1px solid #30363d}\
.kind-blocking{background:#4a1d1d;border-color:#8b3232}\
.kind-risk{background:#4a2f13;border-color:#9e5a1c}\
.kind-approval{background:#13314a;border-color:#1c5a9e}\
.kind-decision{background:#2a1d4a;border-color:#5a3a9e}\
.kind-review{background:#12351f;border-color:#2c7a44}\
.priority-p0{background:#4a1d1d;border-color:#c34a4a;color:#ffb3b3}\
.priority-p1{background:#4a2f13;border-color:#b46a24;color:#ffd19a}\
.priority-p2{background:#161b22;border-color:#30363d;color:#8b949e}\
.priority-legacy{background:#2a1d4a;border-color:#5a3a9e;color:#d2b8ff}\
.type-epic{background:#2a1d4a}.type-story{background:#13314a}\
.tag{background:#161b22;color:#8b949e}\
.lane{background:#161b22;color:#8b949e}\
ul.rows,ul.children{list-style:none;padding:0;margin:.3rem 0}\
ul.rows li,ul.children li{padding:.3rem 0;border-bottom:1px solid #21262d}\
dl{display:grid;grid-template-columns:max-content 1fr;gap:.15rem .8rem;margin:.4rem 0}\
dt{color:#8b949e;font-size:.85rem}\
dd{margin:0;font-size:.9rem;word-break:break-word}\
.plan-body{max-height:28rem;overflow-y:auto}\
.error{color:#f85149}\
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_page_can_reach_a_method_that_writes() {
        // The e2e compares the board file before and after every page loads,
        // which proves no page *did* write. It cannot prove no page *could*:
        // a mutating call that happens to be a no-op on the day leaves the
        // bytes identical and the capability in place. This reads the module
        // back and asserts the capability is absent.
        //
        // Phase 2 adds exactly two writes -- resolve an attention item, open a
        // draft. When they arrive their names go in `ALLOWED` with the reason,
        // which is a decision someone has to make on purpose rather than a
        // check that quietly stops applying.
        const SOURCE: &str = include_str!("serve.rs");
        // The Needs-you reply form resolves exactly one attention item through
        // the same audited Store operation as `kb att resolve`. No other web
        // route is allowed a mutator.
        const ALLOWED: [&str; 2] = ["move_task", "resolve_attention"];
        let shipped = SOURCE
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .unwrap_or(SOURCE);
        // Every `&mut self` method on Store, which is the complete set of ways
        // this module could change a board.
        const MUTATORS: [&str; 20] = [
            "add_task",
            "move_task",
            "remove_task",
            "patch_metadata",
            "update_task",
            "claim",
            "heartbeat",
            "release",
            "add_note",
            "checkpoint",
            "add_tag",
            "remove_tag",
            "raise_attention",
            "resolve_attention",
            "create_handoff",
            "accept_handoff",
            "signoff_story",
            "advance_story",
            "sweep_expired_claims",
            "initialize",
        ];
        for name in MUTATORS {
            if ALLOWED.contains(&name) {
                continue;
            }
            // Assembled rather than written out: this test reads its own
            // source, and a literal call would match itself.
            let call = format!(".{name}(");
            assert!(
                !shipped.contains(call.as_str()),
                "serve.rs calls {name}, which writes. A read surface that can \
                 write is one refactor away from doing so; add it to ALLOWED \
                 with a reason if that is deliberate."
            );
        }
    }

    #[test]
    fn every_hostile_character_leaves_as_an_entity() {
        // Rows are written by agents reading arbitrary material. One rendered
        // unescaped would run script in the operator's browser, against a page
        // that from phase 2 can approve things.
        let hostile = "<script>alert('x')</script> & \"quoted\"";
        let escaped = escape(hostile);
        for raw in ['<', '>', '"', '\''] {
            assert!(
                !escaped.contains(raw),
                "{raw:?} survived escaping: {escaped}"
            );
        }
        assert!(escaped.contains("&lt;script&gt;"), "{escaped}");
        assert!(escaped.contains("&amp;"), "{escaped}");
        // Ampersands must not be double-encoded on the way through.
        assert_eq!(escape("a & b"), "a &amp; b");
    }

    #[test]
    fn a_page_escapes_its_own_title() {
        let html = page("<b>x</b>", "body");
        assert!(html.contains("&lt;b&gt;x&lt;/b&gt;"), "{html}");
        assert!(!html.contains("<title><b>"), "{html}");
    }

    #[test]
    fn websocket_accept_matches_the_rfc_example() {
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn reply_form_decoding_is_strict() {
        assert_eq!(
            strict_form_value("reply=Proceed+with+%26+verify.", "reply")
                .unwrap()
                .as_deref(),
            Some("Proceed with & verify.")
        );
        assert!(strict_form_value("reply=bad%2", "reply").is_err());
        assert!(strict_form_value("reply=bad%XX", "reply").is_err());
    }

    #[test]
    fn a_stamp_reads_as_a_utc_instant() {
        // 2026-08-24T00:00:00Z, and a value inside that day.
        assert_eq!(stamp(1_787_529_600_000), "2026-08-24 00:00:00Z");
        assert_eq!(stamp(0), "1970-01-01 00:00:00Z");
        // Leap day, because the civil-date arithmetic is written out here.
        assert_eq!(stamp(1_709_164_800_000), "2024-02-29 00:00:00Z");
    }

    #[test]
    fn an_age_uses_the_coarsest_unit_that_is_still_true() {
        let now = now_ms();
        assert_eq!(age(now), "just now");
        assert_eq!(age(now - 60_000), "1 min");
        assert_eq!(age(now - 45 * 60_000), "45 min");
        assert_eq!(age(now - 90 * 60_000), "1h30m");
        assert_eq!(age(now - 3 * 24 * 60 * 60_000), "72h");
        // A stamp from the future is not negative time; it is "just now".
        assert_eq!(age(now + 60_000), "just now");
    }

    #[test]
    fn an_elapsed_phrase_reads_correctly_in_a_sentence() {
        // "just now" is already a complete phrase; the rest are bare
        // durations. Appending "ago" to both produced "just now ago".
        let now = now_ms();
        assert_eq!(ago(now), "just now");
        assert_eq!(ago(now - 45 * 60_000), "45 min ago");
        assert_eq!(ago(now - 90 * 60_000), "1h30m ago");
    }

    #[test]
    fn a_url_decodes_before_it_is_matched() {
        assert_eq!(decode("mx-root"), "mx-root");
        assert_eq!(decode("a%20b"), "a b");
        assert_eq!(decode("%2E%2E"), "..");
        // A malformed escape is left as written rather than dropped: it will
        // simply match no board, which beats silently matching another one.
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(decode("100%"), "100%");
    }

    #[test]
    fn an_event_payload_renders_on_one_line() {
        let payload = serde_json::json!({"tag": "infra", "strippedFrom": 2});
        let line = compact(&payload);
        assert!(line.contains("tag=infra"), "{line}");
        assert!(line.contains("strippedFrom=2"), "{line}");
        assert!(!line.contains('\n'), "{line}");
        // A string value loses its quotes; anything else keeps its JSON shape.
        assert_eq!(compact(&serde_json::json!({})), "");
        assert_eq!(compact(&serde_json::Value::Null), "");
    }
}
