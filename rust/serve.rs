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
//! reply to and resolve an attention item, reopen one that was just decided
//! (the undo), or open a draft epic. The socket
//! carries the coarse revision, after which the browser fetches the canonical
//! server-rendered projection, and it carries NOTICES: a notice is an
//! already-authorized summary this process read through the store's filtered
//! event path — never an envelope, never a payload, and never a cursor the
//! browser would then have to be trusted about
//! (`docs/ui-pubsub-consumption-seams.md`).
//!
//! In opt-in deployments, `--actor-header NAME` threads one trusted edge
//! header into the audit actor for the write surface. The proxy in front of
//! Kanban must strip any client-supplied copy and set that header from a
//! successful auth_request; same-origin still gates the POSTs and the header
//! does not relax it.

use crate::model::{
    Attention, AttentionAnswer, CUSTOM_CHOICE, OPERATOR_ACTOR, ProjectRecord, SearchOptions, Sitrep,
};
use crate::projection;
use crate::registry::{Registry, now_ms, retired_board_message};
use crate::search;
use crate::store::Store;
use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::json;
use sha1::{Digest, Sha1};
use std::collections::{HashMap, hash_map::DefaultHasher};
use std::ffi::CString;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, UNIX_EPOCH};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

/// The mode a `--socket` listener is pinned to: owner and group, nobody else.
///
/// The group is the point. The serving uid keeps read/write, one group gets
/// read/write because nginx's `www-data` is added to it on the host, and
/// everybody else is refused by the kernel on `connect` rather than by
/// anything this process would have to remember to check.
const SOCKET_MODE: u32 = 0o660;

/// How many rows a detail page will show of any one list.
///
/// The page is for reading, not archaeology: `kb ev --task <id>` has the whole
/// trail and is one command away. A page that renders ten thousand events is
/// slower to load and no more useful.
pub(crate) const DETAIL_ROWS: i64 = 50;
/// How many decided rows the Recent decisions page shows.
///
/// The page exists so a decided item leaves Needs you without disappearing:
/// the eye moves to the next open card while the last decisions stay within
/// reach of one Undo. `kb ev` and the search view are the archive, not this.
///
/// Named `pub(crate)` because the JSON projection serves the same cut and
/// reports it as the listing's `limit` (`docs/api/kanban-web.openapi.yaml`,
/// `getDecided`): one bound, read from one place.
pub(crate) const DECIDED_ROWS: i64 = 20;
/// How many of each board's newest decisions one scan reads.
///
/// The page shows the newest `DECIDED_ROWS` across every board, and each
/// board is read decided-first (`recent_resolved_attention`) up to this bound
/// before the merge. A board holding more recent decisions than this bound
/// would need it raised; `kb ev` and search are the archive this page is not.
pub(crate) const DECIDED_SCAN: i64 = 200;
const WEB_UNDO_NOTE: &str = "undone from the web view";
/// How many characters of a body a hover preview renders.
///
/// The preview answers "what is this?", and a body that needs reading whole
/// is one click away in its own tab.
pub(crate) const PREVIEW_BODY_CHARS: usize = 800;

/// How many open attention rows one board hands the cross-board queue.
///
/// The bound the `/` and `/all` arms have always passed, named here because
/// the JSON projection reports it as the listing's `limit`
/// (`docs/api/kanban-web.openapi.yaml`, `getNeedsYou`) and a reported bound
/// that drifted from the one actually passed would be a lie an operator plans
/// around.
pub(crate) const OPEN_ATTENTION_ROWS: i64 = 1_000;
/// How many sitreps one board hands the Lanes grouping.
pub(crate) const LANE_UPDATE_ROWS: i64 = 200;

/// A browser reply is a decision note, not a document upload.
const MAX_REPLY_BYTES: usize = 4_096;
/// The configured actor header is an email-sized audit identity, not a blob.
const MAX_ACTOR_BYTES: usize = 254;

enum WebResponse {
    Html(u16, String),
    /// One JSON body, served as the contract's own content type on success
    /// and on refusal (`docs/api/kanban-web.openapi.yaml`).
    Json(u16, String),
    Redirect(String),
    /// One embedded bundle asset, served as itself.
    Asset(&'static crate::bundle::Asset),
}

struct ServeConfig {
    actor_header: Option<String>,
}

impl ServeConfig {
    fn new(actor_header: Option<String>) -> Result<Self> {
        let actor_header = actor_header
            .map(|value| normalize_actor_header_name(&value).map(str::to_owned))
            .transpose()?;
        Ok(Self { actor_header })
    }

    fn actor_for_write(&self, request: &Request) -> Result<String> {
        match self.actor_header.as_deref() {
            None => Ok(OPERATOR_ACTOR.to_owned()),
            Some(name) => configured_actor(request, name),
        }
    }
}

/// `sysexits.h` `EX_USAGE`: the command line itself could not be used.
pub const EXIT_USAGE: i32 = 64;

/// A `serve` invocation that named no listener, or named both of them.
///
/// This is the one kanban failure that does not exit 1, because it is the one
/// with a caller that acts on the difference. A supervisor restarting
/// `kanban-serve.service` should keep retrying a listener that lost its
/// socket, and should never retry a unit file that asks for a port and a
/// socket at once: the first is a transient, the second is a typo that will
/// still be a typo after ten restarts.
#[derive(Debug)]
pub struct ListenerUsage(&'static str);

impl ListenerUsage {
    pub fn error(message: &'static str) -> anyhow::Error {
        anyhow::Error::new(Self(message))
    }
}

impl std::fmt::Display for ListenerUsage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for ListenerUsage {}

/// Serve on loopback until killed. Never returns `Ok`.
pub fn serve(port: u16, actor_header: Option<String>) -> Result<()> {
    let config = ServeConfig::new(actor_header)?;
    let address = format!("127.0.0.1:{port}");
    let server = Server::http(&address)
        .map_err(|error| anyhow::anyhow!("bind {address}: {error}"))
        .with_context(|| format!("serve on {address}"))?;
    eprintln!("kanban serve: http://{address} (loopback only; front it with nginx)");
    accept(server, &config)
}

/// Serve on a Unix domain socket until killed. Never returns `Ok`.
///
/// **Why a socket as well as a port.** Loopback is reachable by every uid on
/// the box: any local process can open port 14200, and the write surface is
/// gated by same-origin, which a local process is free to assert about
/// itself. A socket at [`SOCKET_MODE`] moves that boundary into the
/// filesystem, where the kernel enforces it on `connect` — the proxy's group
/// gets in and nothing else does.
///
/// **`--actor-header` stays allowed on this path, deliberately.** The header
/// is only trustworthy because nothing but the proxy can reach the listener,
/// and a socket is strictly more local than loopback: it takes reachability
/// away from every uid outside the owning group and gives none back. The
/// loopback-only invariant the header depends on is therefore preserved
/// rather than relaxed, which is why this path needs no extra gate on it.
pub fn serve_unix(path: &Path, actor_header: Option<String>) -> Result<()> {
    let config = ServeConfig::new(actor_header)?;
    clear_socket_path(path)?;
    // The socket exists the instant `bind` returns and takes its mode from
    // the umask, so an inherited 022 would leave it group- and world-writable
    // for the window before the chmod in `own_socket_path`. Narrowing first
    // means it is never created wide; the chmod then makes the mode exact
    // rather than umask-shaped.
    let inherited_umask = unsafe { libc::umask((0o777 & !SOCKET_MODE) as libc::mode_t) };
    let bound = Server::http_unix(path);
    unsafe { libc::umask(inherited_umask) };
    let server = bound
        .map_err(|error| anyhow::anyhow!("bind {}: {error}", path.display()))
        .with_context(|| format!("serve on unix:{}", path.display()))?;
    if let Err(error) = own_socket_path(path) {
        // A listener whose mode could not be pinned is the surface this path
        // exists to avoid, so the socket is removed rather than served.
        drop(server);
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    eprintln!(
        "kanban serve: unix:{} (proxy-only socket; front it with nginx)",
        path.display()
    );
    accept(server, &config)
}

/// The accept loop both listeners share.
///
/// Neither transport reaches past this: a request arriving over a socket is
/// the same [`Request`] the loopback listener yields, routed by the same
/// handler, so there is no second request path to keep in step.
fn accept(server: Server, config: &ServeConfig) -> Result<()> {
    for request in server.incoming_requests() {
        if request.url().split('?').next() == Some("/live") {
            // An upgraded socket is long-lived. Keeping it on the accept loop
            // would stop every ordinary page behind the first connected tab.
            thread::spawn(move || websocket(request));
            continue;
        }
        handle(request, config);
    }
    anyhow::bail!("the listener stopped accepting connections")
}

/// Make PATH bindable, or say exactly why it is not.
///
/// `bind` fails on any existing path, so a restart after a kill has to remove
/// the socket the last process left. What it must never remove is anything
/// else: an operator who typed a board file's path deserves the file back
/// rather than a truncated ledger, and a socket another uid owns is not this
/// process's to take. A socket with a live listener is refused too — taking
/// the path out from under it would leave nginx talking to a server nobody
/// else can see, which is worse than refusing to start.
///
/// The path must be absolute: a relative one binds against whatever directory
/// the process was started in, which for a unit file is a setting nobody
/// reading `--socket` would think to check.
fn clear_socket_path(path: &Path) -> Result<()> {
    anyhow::ensure!(
        path.is_absolute(),
        "--socket {} must be an absolute path: a relative one lands wherever the process \
         happened to be started, and both nginx and the unit file name it absolutely",
        path.display()
    );
    let parent = path.parent().with_context(|| {
        format!(
            "--socket {} names a directory, not a socket",
            path.display()
        )
    })?;
    let parent_kind = std::fs::symlink_metadata(parent)
        .with_context(|| format!("--socket directory {} cannot be read", parent.display()))?
        .file_type();
    anyhow::ensure!(
        !parent_kind.is_symlink(),
        "--socket directory {} is a symlink: whoever can retarget it decides where this \
         listener lands, so it is refused",
        parent.display()
    );
    anyhow::ensure!(
        parent_kind.is_dir(),
        "--socket directory {} is not a directory",
        parent.display()
    );
    let existing = match std::fs::symlink_metadata(path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("--socket {} cannot be read", path.display()));
        }
    };
    anyhow::ensure!(
        existing.file_type().is_socket(),
        "--socket {} already exists and is not a socket: refusing to replace it",
        path.display()
    );
    let uid = unsafe { libc::geteuid() };
    anyhow::ensure!(
        existing.uid() == uid,
        "--socket {} is a socket owned by uid {}, not this process's uid {uid}: refusing to \
         replace it",
        path.display(),
        existing.uid()
    );
    anyhow::ensure!(
        UnixStream::connect(path).is_err(),
        "--socket {} already has a listener answering on it: refusing to take the path out \
         from under it",
        path.display()
    );
    std::fs::remove_file(path)
        .with_context(|| format!("remove the stale socket at {}", path.display()))
}

/// Pin the socket to this uid, its primary group, and [`SOCKET_MODE`].
///
/// Creation gets ownership and mode from the process, which is close but not
/// a guarantee: a setgid parent directory hands the socket that directory's
/// group instead of this process's. Both are set explicitly and then read
/// back, because "front it with nginx" is only a boundary if the bits the
/// kernel checks on `connect` are actually the bits that were asked for.
fn own_socket_path(path: &Path) -> Result<()> {
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };
    let raw = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("--socket {} contains an interior NUL", path.display()))?;
    anyhow::ensure!(
        unsafe { libc::chown(raw.as_ptr(), uid, gid) } == 0,
        "set the owner of {} to uid {uid} gid {gid}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))
        .with_context(|| format!("set the mode of {} to 0{SOCKET_MODE:o}", path.display()))?;
    let pinned = std::fs::symlink_metadata(path)
        .with_context(|| format!("re-read {} after pinning it", path.display()))?;
    anyhow::ensure!(
        pinned.permissions().mode() & 0o7777 == SOCKET_MODE,
        "{} came back mode 0{:o}, not 0{SOCKET_MODE:o}: this filesystem did not keep the mode \
         the listener needs",
        path.display(),
        pinned.permissions().mode() & 0o7777
    );
    anyhow::ensure!(
        pinned.uid() == uid && pinned.gid() == gid,
        "{} came back owned by uid {} gid {}, not uid {uid} gid {gid}",
        path.display(),
        pinned.uid(),
        pinned.gid()
    );
    Ok(())
}

/// Answer one request, turning an error into a page rather than a dropped
/// connection: a browser given nothing shows its own error, which tells the
/// reader nothing about what went wrong here.
fn handle(mut request: Request, config: &ServeConfig) {
    let response =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| route(&mut request, config)))
            .unwrap_or_else(|_| Err(anyhow::anyhow!("the page renderer panicked")));
    let (status, body, content_type, location, immutable) = match response {
        Ok(WebResponse::Json(status, json)) => (
            status,
            json.into_bytes(),
            "application/json; charset=utf-8",
            None,
            false,
        ),
        Ok(WebResponse::Html(status, html)) => (
            status,
            html.into_bytes(),
            "text/html; charset=utf-8",
            None,
            false,
        ),
        Ok(WebResponse::Asset(asset)) => {
            (200, asset.bytes.to_vec(), asset.content_type, None, true)
        }
        Ok(WebResponse::Redirect(location)) => (
            303,
            page(
                "Reply recorded",
                "<h1>Reply recorded</h1><p><a href=\"/\">Return to Needs you</a>.</p>",
            )
            .into_bytes(),
            "text/html; charset=utf-8",
            Some(location),
            false,
        ),
        Err(error) => (
            500,
            page(
                "Error",
                &format!(
                    "<h1>Error</h1><p class=error>{}</p>",
                    escape(&error.to_string())
                ),
            )
            .into_bytes(),
            "text/html; charset=utf-8",
            None,
            false,
        ),
    };
    let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
        .expect("a static header is always valid");
    let mut response = Response::from_data(body)
        .with_status_code(status)
        .with_header(header);
    if immutable {
        // Safe for a year because the name is the content: a changed asset
        // is a changed name, and a stale page asking for the old one gets
        // the old one or a 404, never the new bytes under the old name.
        response = response.with_header(
            Header::from_bytes(
                &b"Cache-Control"[..],
                &b"public, max-age=31536000, immutable"[..],
            )
            .expect("a static header is always valid"),
        );
    }
    if let Some(location) = location {
        response = response.with_header(
            Header::from_bytes(&b"Location"[..], location.as_bytes())
                .expect("a generated relative location is valid"),
        );
    }
    if status == 405 && content_type.starts_with("application/json") {
        // The contract states the header on every refusal of a method, and a
        // client that has to read prose to learn which method is allowed has
        // been told by the wrong mechanism.
        response = response.with_header(
            Header::from_bytes(&b"Allow"[..], &b"GET"[..])
                .expect("a static header is always valid"),
        );
    }
    // A client that hung up mid-write is not this server's problem, and
    // crashing on it would take down a page everyone else is still reading.
    let _ = request.respond(response);
}

fn route(request: &mut Request, config: &ServeConfig) -> Result<WebResponse> {
    let url = request.url().to_owned();
    // The JSON surface is dispatched ahead of everything else, including the
    // POST arm: `/api/v1` answers `GET` and refuses every other method with
    // `405`, and routing a POST to the form handler first would give one
    // JSON path a write surface the contract does not give it (SPA-13).
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    if let Some(rest) = path.strip_prefix(API_PREFIX) {
        return Ok(api(request.method(), rest, query));
    }
    if request.method() == &Method::Post {
        return post(request, &url, config);
    }
    if request.method() != &Method::Get {
        return Ok(WebResponse::Html(
            405,
            page("Method not allowed", "<h1>Method not allowed</h1>"),
        ));
    }
    // The application and its assets are served ahead of the page router and
    // outside `ROUTE_SHAPES`: they are one artefact rather than page arms,
    // and neither is a server-rendered page the width sweep can measure. The
    // cutover (`t-1f495a7f`) is what moves `/app` onto `/` and folds it into
    // the route table.
    if let Some(response) = bundle_response(&url) {
        return Ok(response);
    }
    Ok(WebResponse::Html(200, render(&url)?))
}

/// The one prefix the JSON projection answers on, and nothing else does
/// (`docs/api/kanban-web.openapi.yaml`, the OQ-3 answer).
const API_PREFIX: &str = "/api/v1";
/// The store's own single generic denial, as a JSON body.
///
/// Byte-identical for a board that is unknown, retired, ambiguous or
/// invisible, and for every `/api/v1` path outside the five implemented ones:
/// the route table is not enumerable either.
const DENIED_OR_NOT_FOUND_JSON: &str = "{\"error\":\"denied or not found\"}";
/// A JSON route answers `GET`. Every other method gets this.
const METHOD_NOT_ALLOWED_JSON: &str = "{\"error\":\"Method not allowed\"}";
/// The generic failure. It never carries a board name, a path, a row title,
/// or the store's own sentence.
const SERVER_ERROR_JSON: &str = "{\"error\":\"The request could not be completed.\"}";
/// How much `q` the search route will read, in bytes of the decoded query.
///
/// Generous for anything a person types and small beside what a script can
/// post: the same shape of bound as `MAX_REPLY_BYTES`, and for the same
/// reason — the work is done per readable board, so an unbounded input is
/// an unbounded scan.
const MAX_QUERY_BYTES: usize = 4_096;
/// What a query past the bound is told. One sentence, no echo of what was
/// sent, and the same shape as every other refusal on this surface.
const QUERY_TOO_LONG_JSON: &str =
    "{\"error\":\"The search query is too long. Shorten it and try again.\"}";

/// Dispatch one `/api/v1` request.
///
/// Every read the pages make; every other path under the prefix gets
/// the same non-enumerating refusal as an unknown board, because a route
/// table that answered "no such route" differently from "no such board"
/// would be an inventory of what exists.
fn api(method: &Method, path: &str, query: &str) -> WebResponse {
    if method != &Method::Get {
        return WebResponse::Json(405, METHOD_NOT_ALLOWED_JSON.to_owned());
    }
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(decode)
        .collect::<Vec<_>>();
    let parts = segments.iter().map(String::as_str).collect::<Vec<_>>();
    let body = match parts.as_slice() {
        ["needs-you"] => encode(projection::needs_you()),
        ["decided"] => encode(projection::decided()),
        ["boards"] => encode(projection::boards()),
        ["lanes"] => encode(projection::lanes()),
        ["deployments"] => encode(projection::deployments()),
        ["subscriptions"] => encode(projection::subscriptions()),
        ["board", project] => encode(projection::board(project)),
        ["task", project, id] => encode(projection::task(project, id)),
        ["deployment", project, id] => encode(projection::deployment(project, id)),
        ["sprints"] => encode(projection::sprints()),
        ["sprints", project] => encode(projection::board_sprints(project)),
        ["sprint", project, id] => encode(projection::sprint(project, id)),
        ["plans"] => encode(projection::plans()),
        // The page exposes one parameter and so does this route: a filter
        // the page cannot set is a filter with no caller (SPA-06).
        //
        // The query is BOUNDED before the store sees it. `SEARCH_MAX_CHARS`
        // bounds what comes back, not what goes in, so without this a
        // megabyte of `q` was a megabyte of LIKE patterns run over every
        // readable board — the same reason a reply is capped at
        // `MAX_REPLY_BYTES` before it is recorded.
        ["search"] => {
            let asked = query_value(query, "q").unwrap_or_default();
            if asked.len() > MAX_QUERY_BYTES {
                return WebResponse::Json(400, QUERY_TOO_LONG_JSON.to_owned());
            }
            encode(projection::search(&asked))
        }
        // One arm for both preview shapes: `/preview/board/{project}`
        // repeats the project as the id, exactly as `render` passes it.
        ["preview", kind, project, id] => encode(projection::preview(kind, project, id)),
        _ => Err(projection::Refusal::DeniedOrNotFound),
    };
    match body {
        Ok(json) => WebResponse::Json(200, json),
        Err(projection::Refusal::DeniedOrNotFound) => {
            WebResponse::Json(404, DENIED_OR_NOT_FOUND_JSON.to_owned())
        }
        Err(projection::Refusal::Failed(_)) => WebResponse::Json(500, SERVER_ERROR_JSON.to_owned()),
    }
}

/// Serialise one projection, keeping a serialisation failure on the same
/// footing as any other: a generic `500`, never the serde message.
fn encode<T: serde::Serialize>(
    projected: projection::Projected<T>,
) -> projection::Projected<String> {
    let value = projected?;
    serde_json::to_string(&value)
        .map_err(|error| projection::Refusal::Failed(anyhow::Error::new(error)))
}

/// The two addresses the embedded bundle answers, or `None` for a URL that
/// belongs to the page router.
///
/// `/assets/<name>` is served from the embedded table and from nowhere else:
/// there is no filesystem path here to traverse, so a name that is not in
/// the table is 404 and that is the whole of the story (SPA-01).
fn bundle_response(url: &str) -> Option<WebResponse> {
    let path = url.split('?').next().unwrap_or(url);
    match path {
        "/app" => Some(WebResponse::Html(200, app_shell())),
        _ => {
            let name = path.strip_prefix("/assets/")?;
            Some(match crate::bundle::asset(name) {
                Some(asset) => WebResponse::Asset(asset),
                None => WebResponse::Html(
                    404,
                    page(
                        "Not found",
                        "<h1>Not found</h1><p>No asset at that address. \
                         <a href=\"/app\">Start over</a>.</p>",
                    ),
                ),
            })
        }
    }
}

/// The application shell: the smallest document that can load the bundle.
///
/// It holds none of the page — the page is what the bundle mounts into
/// `#root` (SPA-05) — and it is written out here rather than committed as a
/// built artefact so that the asset names, which carry a content hash, come
/// from the same table the assets are served from.
///
/// The literal `</head>` is load-bearing: the hax edge injects WebMCP with
/// nginx `sub_filter '</head>' ...` and `sub_filter_once on`, so a shell that
/// spelled the head's close any other way, or emitted it twice, would lose
/// the injection or take it twice (ADR-048's 2026-09-19 addendum).
/// `the_app_shell_closes_its_head_exactly_once_unit` holds it.
fn app_shell() -> String {
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>kanban</title>\
         <link rel=stylesheet href=/assets/{stylesheet}></head><body>\
         <div id=root data-testid=app-root-shell></div>\
         <script type=module src=/assets/{script}></script></body></html>",
        stylesheet = crate::bundle::STYLESHEET,
        script = crate::bundle::SCRIPT,
    )
}

/// Every shape `render` answers, in the order the match below names them.
///
/// This is the registry the WEB-44 width sweep loads: a new arm here without
/// a new shape fails `render_answers_exactly_the_declared_shapes_unit`, and a
/// new shape without a URL in the sweep fails the sweep itself. So a route
/// cannot be added and left unmeasured on a phone.
///
/// Every one of them answers the same document since `t-bf255880` wave 2:
/// the shapes are what the ADDRESS BAR may hold, and the page each one
/// names is the bundle's (`web/src/router.ts` reads the same list on the
/// other side). The previews left with the pages — a hover reads
/// `/api/v1/preview/...` and builds its own card.
#[cfg(test)]
const ROUTE_SHAPES: &[&str] = &[
    "/",
    "/all",
    "/decided",
    "/boards",
    "/sprints",
    "/sprints/{project}",
    "/sprint/{project}/{id}",
    "/plans",
    "/deployments",
    "/subscriptions",
    "/lanes",
    "/search",
    "/board/{project}",
    "/task/{project}/{id}",
    "/deployment/{project}/{id}",
];

/// Route a URL to a rendered page.
///
/// Every shape answers the application shell: the bundle reads the address
/// bar and renders the page, and the data comes from `/api/v1`. What is
/// left here is the address space itself — an address this application has
/// a page for is answered with the application, and everything else is the
/// refusal document.
fn render(url: &str) -> Result<String> {
    let path = url.split_once('?').map_or(url, |(path, _)| path);
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(decode)
        .collect::<Vec<_>>();
    let parts = segments.iter().map(String::as_str).collect::<Vec<_>>();
    match parts.as_slice() {
        []
        | ["all"]
        | ["decided"]
        | ["boards"]
        | ["sprints"]
        | ["sprints", _]
        | ["sprint", _, _]
        | ["plans"]
        | ["deployments"]
        | ["subscriptions"]
        | ["lanes"]
        | ["search"]
        | ["board", _]
        | ["task", _, _]
        | ["deployment", _, _] => Ok(app_shell()),
        _ => Ok(page(
            "Not found",
            "<h1>Not found</h1><p>No page at that address. \
             <a href=\"/\">Start over</a>.</p>",
        )),
    }
}

fn post(request: &mut Request, url: &str, config: &ServeConfig) -> Result<WebResponse> {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let segments = path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(decode)
        .collect::<Vec<_>>();
    let parts = segments.iter().map(String::as_str).collect::<Vec<_>>();
    if !matches!(
        parts.as_slice(),
        ["attention", _, _, "reply"]
            | ["attention", _, _, "reopen"]
            | ["attention", _, _, "check"]
            | ["plan", _, _, "open"]
            | ["subscription", _, _, "pause" | "resume"]
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
    let actor = match config.actor_for_write(request) {
        Ok(actor) => actor,
        Err(error) => {
            return Ok(WebResponse::Html(
                400,
                page(
                    "Invalid actor",
                    &format!(
                        "<h1>Invalid actor</h1><p class=error>{}</p>",
                        escape(&error.to_string())
                    ),
                ),
            ));
        }
    };
    if let ["plan", project, id, "open"] = parts.as_slice() {
        let Ok((_, mut store)) = project_named(project) else {
            return Ok(WebResponse::Html(
                404,
                page("Board not found", "<h1>Board not found</h1>"),
            ));
        };
        let tasks = store.list_tasks(None, None, None, None, false)?;
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
        if let Err(error) = store.move_task(id, "todo", &actor, serde_json::json!({}), false) {
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
    if let ["subscription", project, id, verb @ ("pause" | "resume")] = parts.as_slice() {
        let pause = *verb == "pause";
        let Ok((_, mut store)) = project_named(project) else {
            return Ok(WebResponse::Html(
                404,
                page("Board not found", "<h1>Board not found</h1>"),
            ));
        };
        // Idempotent because the store makes it so: `set_subscription_paused`
        // returns the row untouched when it already holds the requested
        // state, without a second ledger event and without moving
        // `paused_at`. A double submit therefore lands on the page rather
        // than erroring or re-stamping who paused it, and this handler needs
        // no read-then-write of its own — which could not be atomic anyway.
        let changed = if pause {
            store.pause_subscription(id, &actor)
        } else {
            store.resume_subscription(id, &actor)
        };
        if let Err(error) = changed {
            return Ok(WebResponse::Html(
                409,
                page(
                    "Subscription unchanged",
                    &format!(
                        "<h1>Subscription unchanged</h1><p class=error>{}</p>",
                        escape(&error.to_string())
                    ),
                ),
            ));
        }
        // Land where the affected row is visible. A paused row is hidden by
        // the default filter, so pausing carries `show=all` — an action whose
        // result vanishes from the page reads as an action that failed.
        // Resuming keeps whatever filter the form was submitted from.
        let show_all = pause || query_value(query, "show").as_deref() == Some("all");
        return Ok(WebResponse::Redirect(format!(
            "/subscriptions?{}changed={}",
            if show_all { "show=all&" } else { "" },
            url_encode(id)
        )));
    }
    if let ["attention", project, id, "reopen"] = parts.as_slice() {
        let Ok((_, mut store)) = project_named(project) else {
            return Ok(WebResponse::Html(
                404,
                page("Board not found", "<h1>Board not found</h1>"),
            ));
        };
        // The undo is one keypress or one click, so its reopen note is fixed
        // prose rather than a form field: an undo that demanded words would
        // be a dialog wearing a button. The row's previous decision and
        // resolution stay in the hash-chained ledger (ADR-042 §3); this note
        // says where the reopen came from.
        if let Err(error) = store.reopen_attention(id, &actor, WEB_UNDO_NOTE) {
            return Ok(WebResponse::Html(
                409,
                page(
                    "Not reopened",
                    &format!(
                        "<h1>Not reopened</h1><p class=error>{}</p>",
                        escape(&error.to_string())
                    ),
                ),
            ));
        }
        // Land where the operator was: the decided rows carry `back`, the
        // receipts post from Needs you and land back on it. The value is
        // compared, not trusted, so a form cannot redirect elsewhere.
        let back = if query_value(query, "back").as_deref() == Some("/decided") {
            "/decided"
        } else {
            "/"
        };
        return Ok(WebResponse::Redirect(format!(
            "{back}?undone={}",
            url_encode(id)
        )));
    }
    let length = request.body_length().unwrap_or(0);
    if length == 0 || length > MAX_REPLY_BYTES {
        return Ok(WebResponse::Html(400, choice_required()));
    }
    // Read exactly the declared length, never to end-of-stream. nginx sends
    // `Connection: upgrade` on every proxied request when a location carries
    // websocket headers, and tiny_http then hands back the whole socket as
    // the body reader; a read-to-end would wait for a close the proxy never
    // sends, and the browser saw a 504 seventy-five seconds after a click
    // (kb.geoy.ws access log, 2026-09-11). The bound above already refuses
    // anything past `MAX_REPLY_BYTES`.
    let mut bytes = vec![0; length];
    request.as_reader().read_exact(&mut bytes)?;
    let Ok(body) = std::str::from_utf8(&bytes) else {
        return Ok(WebResponse::Html(
            400,
            page("Invalid reply", "<h1>Invalid reply</h1>"),
        ));
    };
    let [_, project, id, _] = parts.as_slice() else {
        unreachable!("the route shape was checked above")
    };
    // The check answer is its own write (ACC-11): one key, through the same
    // Store operation the CLI's `--check-answered` and `attention check` use,
    // settling nothing — an open row stays open, a resolved one stays history.
    if parts[3] == "check" {
        let key = strict_form_value(body, "key")
            .ok()
            .flatten()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let Some(key) = key else {
            return Ok(WebResponse::Html(
                400,
                page(
                    "Check answer incomplete",
                    "<h1>Check answer incomplete</h1><p class=error>Pick one of the check's \
                     answers.</p>",
                ),
            ));
        };
        let Ok((_, mut store)) = project_named(project) else {
            return Ok(WebResponse::Html(
                404,
                page("Board not found", "<h1>Board not found</h1>"),
            ));
        };
        if let Err(error) = store.answer_attention_check_from_trusted_edge(id, &actor, &key) {
            return Ok(WebResponse::Html(
                409,
                page(
                    "Check answer not recorded",
                    &format!(
                        "<h1>Check answer not recorded</h1><p class=error>{}</p>",
                        escape(&error.to_string())
                    ),
                ),
            ));
        }
        return Ok(WebResponse::Redirect(format!(
            "/?checked={}",
            url_encode(id)
        )));
    }
    let (Ok(decision), Ok(outcome), Ok(reply)) = (
        strict_form_value(body, "decision"),
        strict_form_value(body, "outcome"),
        strict_form_value(body, "reply"),
    ) else {
        return Ok(WebResponse::Html(
            400,
            page("Invalid reply", "<h1>Invalid reply</h1>"),
        ));
    };
    let trimmed = |value: Option<String>| {
        value
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    };
    let (outcome, reply) = (trimmed(outcome), trimmed(reply));
    let Some(decision) = trimmed(decision) else {
        return Ok(WebResponse::Html(400, choice_required()));
    };
    // The card posts the key it rendered, or the reserved `custom`. A key the
    // row no longer carries is refused by name by the store (ADR-042 §4
    // refusal 13) rather than mapped onto whatever now sits in that
    // position, which is what makes a card left open in a tab safe.
    let answer = if decision == CUSTOM_CHOICE {
        let (Some(outcome), Some(note)) = (outcome.as_deref(), reply.as_deref()) else {
            // The form is wrong rather than the row, so this is a 400 and not
            // the 409 a settled row gets.
            return Ok(WebResponse::Html(
                400,
                page(
                    "Answer incomplete",
                    &format!(
                        "<h1>Answer incomplete</h1><p class=error>{}</p>",
                        escape(&incomplete_answer_refusal(
                            id,
                            &actor,
                            outcome.as_deref(),
                            reply.as_deref()
                        ))
                    ),
                ),
            ));
        };
        AttentionAnswer::custom(outcome, note)
    } else {
        AttentionAnswer {
            choice: Some(&decision),
            // The picker belongs to the free-text answer. An authored choice
            // carries its own verdict, so a picker the operator left set is
            // not forwarded onto a choice that would refuse it.
            outcome: None,
            note: reply.as_deref(),
        }
    };
    let Ok((_, mut store)) = project_named(project) else {
        return Ok(WebResponse::Html(
            404,
            page("Board not found", "<h1>Board not found</h1>"),
        ));
    };
    if let Err(error) = store.resolve_attention_from_trusted_edge(id, &actor, &answer, None) {
        return Ok(WebResponse::Html(
            409,
            page(
                "Decision not recorded",
                &format!(
                    "<h1>Decision not recorded</h1><p class=error>{}</p>",
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

/// A POST that named no choice at all: an empty body, or a form with no
/// `decision`. Both are the same missing thing, so they read the same.
fn choice_required() -> String {
    page(
        "Choice required",
        "<h1>Choice required</h1><p class=error>Pick one of the item's choices, \
         or write your own answer.</p>",
    )
}

/// The ROUTE's words for a free-text answer missing its verdict or its words.
///
/// One wording for the CLI, the MCP tool and this route: the route asks for
/// it rather than restating it, so a change to the refusal cannot leave a
/// stale copy behind here. The composer never consults the row's choices on
/// this path, which is why it can be asked before the board is opened.
///
/// The CARD does not speak these words. It has its own sentence for the same
/// case, in the page's language (`INCOMPLETE_ANSWER` in [`JS`]); flags belong
/// to the surface that has them. What the card quotes verbatim is what this
/// route, or the network, actually said about an attempt that was made.
fn incomplete_answer_refusal(
    id: &str,
    actor: &str,
    outcome: Option<&str>,
    note: Option<&str>,
) -> String {
    AttentionAnswer {
        choice: Some(CUSTOM_CHOICE),
        outcome,
        note,
    }
    .decide(id, &[], actor, now_ms())
    .expect_err("a custom answer missing its verdict or its words is refused")
    .to_string()
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

fn configured_actor(request: &Request, name: &str) -> Result<String> {
    let mut values = request
        .headers()
        .iter()
        .filter(|candidate| candidate.field.to_string().eq_ignore_ascii_case(name))
        .map(|candidate| candidate.value.as_str());
    let Some(first) = values.next() else {
        anyhow::bail!("actor header {name} is required");
    };
    if values.next().is_some() {
        anyhow::bail!("actor header {name} must appear exactly once");
    }
    normalize_actor_bytes(first.as_bytes())
}

fn normalize_actor_header_name(value: &str) -> Result<&str> {
    if value.is_empty() {
        anyhow::bail!("actor header name is required");
    }
    if !value.is_ascii() || !value.bytes().all(is_http_token) {
        anyhow::bail!("actor header name must be a valid HTTP token");
    }
    Ok(value)
}

fn normalize_actor_bytes(bytes: &[u8]) -> Result<String> {
    let value = std::str::from_utf8(bytes).context("actor header contains invalid UTF-8")?;
    if value.is_empty() {
        anyhow::bail!("actor header is required");
    }
    if value.len() > MAX_ACTOR_BYTES {
        anyhow::bail!("actor header must be at most {MAX_ACTOR_BYTES} bytes");
    }
    if !value.is_ascii()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        anyhow::bail!("actor header must be ASCII without whitespace or control characters");
    }
    Ok(value.to_owned())
}

fn is_http_token(byte: u8) -> bool {
    matches!(
        byte,
        b'!'
            | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
            | b'0'..=b'9'
            | b'A'..=b'Z'
            | b'a'..=b'z'
    )
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
pub(crate) fn projects() -> Result<Vec<(ProjectRecord, Store)>> {
    let registry = Registry::open()?;
    let mut out = Vec::new();
    for project in registry.projects_active()? {
        let path = Path::new(&project.board_path);
        if !path.exists() {
            continue;
        }
        let store = Store::open_as_caller(path)?;
        out.push((project, store));
    }
    Ok(out)
}

/// Every active project whose board this caller may read.
///
/// Whole-estate listings use the read-only open and skip only its typed
/// authorization refusal. Named and write routes keep `projects`, whose
/// writable stores preserve their existing command behavior.
pub(crate) fn listing_projects() -> Result<Vec<(ProjectRecord, Store)>> {
    let registry = Registry::open()?;
    let mut out = Vec::new();
    for project in registry.projects_active()? {
        let path = Path::new(&project.board_path);
        if !path.exists() {
            continue;
        }
        let Some(store) = Store::open_for_estate_listing_as_caller(path)? else {
            continue;
        };
        out.push((project, store));
    }
    Ok(out)
}

pub(crate) fn project_named(name: &str) -> Result<(ProjectRecord, Store)> {
    let matches = projects()?
        .into_iter()
        .filter(|(project, _)| project.name == name)
        .collect::<Vec<_>>();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap()),
        0 => {
            let retired = Registry::open()?.by_name_all(name)?;
            match retired.as_slice() {
                [] => Err(anyhow::anyhow!("no board named {name}")),
                [project] => Err(anyhow::anyhow!(retired_board_message(
                    &project.name,
                    project.archived_note.as_deref(),
                    "opening it"
                ))),
                many => Err(anyhow::anyhow!(
                    "{} retired Kanban projects are named {name}; use `kanban workspace list --all --json` to inspect their board paths: {}",
                    many.len(),
                    crate::project_candidates(many)
                )),
            }
        }
        _ => Err(anyhow::anyhow!(
            "{} Kanban projects are named {name}; choose a unique board name before using /board: {}",
            matches.len(),
            matches
                .iter()
                .map(|(project, _)| {
                    if project.workspace_roots.is_empty() {
                        format!("{} (rootless)", project.name)
                    } else {
                        format!("{} [{}]", project.name, project.workspace_roots.join(", "))
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

// --------------------------------------------------------------- live status

/// How many new events one board may hand a connection one by one before it
/// is told a count instead.
///
/// Five, because that is where an operator stops reading lines and starts
/// reading a number, and because a single operator action writes fewer than
/// that: resolving an attention item is one event, claiming a task is one.
/// A normal action therefore always arrives as itself, and only a burst — a
/// lane's script, a sweep retiring six dead leases at once — collapses into
/// one sentence. The flood this prevents is the whole point: six individual
/// frames past the fold say less than "6 changes while you were away".
const NOTICE_LAG_THRESHOLD: usize = 5;

/// The most rows one scan reads from one board.
///
/// A ceiling, not a cursor: it bounds the work one tick can do while catching
/// up, and the remainder is read by the next scan.
const NOTICE_SCAN_LIMIT: i64 = 500;

/// One already-authorized ledger row, borrowed from the batch it came in.
struct NoticeRow<'a> {
    seq: i64,
    kind: &'a str,
    task: Option<&'a str>,
}

/// What one board's scan produced, and where that board's position now is.
struct BoardNotices {
    frames: Vec<String>,
    cursor: i64,
    truncated: bool,
}

/// What one tick's scan produced across every board.
struct NoticeScan {
    frames: Vec<String>,
    /// Scan again on the next tick even if nothing changed: a page was full,
    /// or a board was busy and its rows have not been read yet.
    again: bool,
}

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
    // The position this connection reads from, and the ONLY copy of it: a
    // local in the frame of the thread `serve` spawned for this socket. No
    // static, no shared map, no lock — so one connection has no name for
    // another's position and cannot read, advance or stall it. It dies with
    // the socket, which is precisely why a reconnect starts at the head again
    // and the browser never has to hold a cursor of its own.
    let mut positions = notice_positions_at_head();
    if write_ws_text(
        &mut stream,
        &format!(r#"{{"type":"ready","revision":"{revision:016x}","noticesFrom":"now"}}"#),
    )
    .is_err()
    {
        return;
    }
    let mut ticks = 0_u8;
    let mut rescan = false;
    loop {
        thread::sleep(Duration::from_secs(1));
        ticks = ticks.wrapping_add(1);
        let Ok(current) = ledger_revision() else {
            continue;
        };
        let changed = current != revision;
        let mut sent = false;
        if changed || rescan {
            // Gated on the coarse revision because a board file that did not
            // change cannot have gained an event. WHAT is new is decided by
            // this connection's position and never by the revision, so a
            // write that lands mid-scan is read by the next scan rather than
            // lost to this one: the baseline moves, the position does not.
            let scan = notice_scan(&mut positions);
            rescan = scan.again;
            for frame in scan.frames {
                if write_ws_text(&mut stream, &frame).is_err() {
                    return;
                }
                sent = true;
            }
        }
        if changed {
            revision = current;
            if write_ws_text(
                &mut stream,
                &format!(r#"{{"type":"refresh","revision":"{revision:016x}"}}"#),
            )
            .is_err()
            {
                return;
            }
            sent = true;
        }
        if sent {
            ticks = 0;
        } else if ticks >= 15 {
            if write_ws_text(&mut stream, r#"{"type":"heartbeat"}"#).is_err() {
                return;
            }
            ticks = 0;
        }
    }
}

/// Every active board's current head, for a connection that has just opened.
///
/// A board this cannot open or read is simply absent from the map, and the
/// first scan meets it as a first sight and adopts ITS head then. One busy
/// open must not become a replay of a whole trail.
fn notice_positions_at_head() -> HashMap<PathBuf, i64> {
    let mut positions = HashMap::new();
    let Ok(boards) = projects() else {
        return positions;
    };
    for (project, store) in boards {
        if let Ok(head) = store.event_head_seq() {
            positions.insert(PathBuf::from(&project.board_path), head);
        }
    }
    positions
}

/// Read every active board forward from where this connection stands.
///
/// Never fails: a long-lived socket must not die because one board was busy
/// for fifty milliseconds. A board that could not be read keeps its position
/// and is retried, because skipping it is how a notice becomes an absence.
/// A board that has left the registry drops out of the map, so the map is
/// bounded by the boards that exist rather than by every board ever seen.
fn notice_scan(positions: &mut HashMap<PathBuf, i64>) -> NoticeScan {
    let Ok(boards) = projects() else {
        return NoticeScan {
            frames: Vec::new(),
            again: true,
        };
    };
    let mut frames = Vec::new();
    let mut again = false;
    let mut next = HashMap::with_capacity(boards.len());
    for (project, store) in boards {
        let path = PathBuf::from(&project.board_path);
        let cursor = positions.get(&path).copied();
        match board_notices(&project.name, &store, cursor) {
            Ok(mut board) => {
                frames.append(&mut board.frames);
                again |= board.truncated;
                next.insert(path, board.cursor);
            }
            Err(_) => {
                again = true;
                if let Some(cursor) = cursor {
                    next.insert(path, cursor);
                }
            }
        }
    }
    *positions = next;
    NoticeScan { frames, again }
}

/// One board's newly-visible rows, as frames, and where its position lands.
///
/// Every row here came through [`Store::events_since_filtered`], which runs
/// each one past `visible_events` — so a row the operator may not see is
/// never named in a frame. The unfiltered `events_since` has no place on this
/// path: its purpose is to move a cursor past rows a filter rejected, and a
/// notice built from it would name exactly those rows.
///
/// The position advances only over rows this connection was actually handed,
/// so nothing is skipped. A trailing run of rows the operator may not see is
/// therefore re-examined by the next scan, which costs one indexed query and
/// sends no frame.
fn board_notices(board: &str, store: &Store, cursor: Option<i64>) -> Result<BoardNotices> {
    let head = store.event_head_seq()?;
    let Some(cursor) = cursor else {
        // First sight of a board, including one adopted while this connection
        // was open. Starting at zero would announce a whole board's history as
        // if it had just happened.
        return Ok(BoardNotices {
            frames: Vec::new(),
            cursor: head,
            truncated: false,
        });
    };
    if head < cursor {
        // The board rewound — restored from a backup. Re-anchor instead of
        // sitting ahead of the head reading nothing for the rest of the day.
        return Ok(BoardNotices {
            frames: Vec::new(),
            cursor: head,
            truncated: false,
        });
    }
    let visible = store.events_since_filtered(
        None,
        &[],
        &[],
        &[],
        &[],
        &[],
        cursor,
        NOTICE_SCAN_LIMIT,
        false,
    )?;
    let Some(last) = visible.last().map(|event| event.seq) else {
        return Ok(BoardNotices {
            frames: Vec::new(),
            cursor,
            truncated: false,
        });
    };
    let rows = visible
        .iter()
        .map(|event| NoticeRow {
            seq: event.seq,
            kind: &event.kind,
            task: event.task_id.as_deref(),
        })
        .collect::<Vec<_>>();
    Ok(BoardNotices {
        frames: notice_batch(board, &rows, |id| {
            store.require_task(id).ok().map(|task| task.title)
        }),
        cursor: last,
        // A full page means more may be waiting, so scan again next tick
        // rather than sitting on it until the next write. Exact while the
        // filter passes everything, which is `serve`'s own case; under
        // managed enforcement a page trimmed by the filter reads as short,
        // and the remainder then waits for the next revision change. The
        // position has still advanced only over what was delivered, so that
        // is a delay and never a skip.
        truncated: visible.len() as i64 == NOTICE_SCAN_LIMIT,
    })
}

/// Either a frame per row, or one frame saying how many there were.
///
/// `title` is a lookup rather than a field because the summarised path must
/// not pay for it: a burst of four hundred rows collapses into one sentence,
/// and reading four hundred task titles to build a sentence that names none
/// of them would be work done to be thrown away.
fn notice_batch(
    board: &str,
    rows: &[NoticeRow<'_>],
    title: impl Fn(&str) -> Option<String>,
) -> Vec<String> {
    match rows.last() {
        None => Vec::new(),
        Some(last) if rows.len() > NOTICE_LAG_THRESHOLD => {
            vec![behind_frame(board, last.seq, rows.len())]
        }
        Some(_) => rows
            .iter()
            .map(|row| notice_frame(board, row, &title))
            .collect(),
    }
}

/// One thing that happened, in the words a person reads.
///
/// The whole frame is the key, the words, the row it is about and the board
/// it is on. No payload — `attention_raised` carries the raised item's tag
/// list in its own — no kind, no actor, no seq presented as a cursor, no
/// capability. Built through `serde_json` rather than formatted, because a
/// task title is arbitrary operator text and a hand-written frame is one
/// quotation mark away from being a different frame.
fn notice_frame(
    board: &str,
    row: &NoticeRow<'_>,
    title: impl Fn(&str) -> Option<String>,
) -> String {
    let mut frame = json!({
        "type": "notice",
        "key": notice_key(board, row.seq),
        "what": plain_words(row.kind),
        "board": board,
    });
    if let Some(task) = row.task {
        frame["task"] = json!(task);
        // Absent rather than empty when the row is gone: a blank title reads
        // as a task with no name, which is a different claim.
        if let Some(title) = title(task) {
            frame["title"] = json!(title);
        }
    }
    frame.to_string()
}

/// How far behind this connection was, as one sentence.
fn behind_frame(board: &str, through: i64, count: usize) -> String {
    json!({
        "type": "behind",
        "key": notice_key(board, through),
        "what": format!("{count} changes while you were away"),
        "board": board,
    })
    .to_string()
}

/// The stable key for one event.
///
/// `(board, seq)` is the ledger's own identity for an event: `seq` is
/// per-board, and the registry refuses a second ACTIVE board with the same
/// name, so the pair cannot collide between two boards one operator sees.
/// The browser dedupes on it, which is what makes a redelivered notice
/// harmless rather than a second action.
fn notice_key(board: &str, seq: i64) -> String {
    format!("{board}#{seq}")
}

/// A ledger kind as words rather than as a token.
///
/// Derived from the kind instead of a table of sentences. A table goes stale
/// the moment a new kind is recorded, and its fallback would have to say
/// something like "something changed" — turning a named event into an unnamed
/// one, which is the absence-reads-as-a-finding failure in miniature.
fn plain_words(kind: &str) -> String {
    let mut words = kind.replace('_', " ");
    if let Some(first) = words.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    words
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
    for project in registry.projects_active()? {
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

fn task_open_attention(store: &Store, task_id: &str) -> Result<Vec<Attention>> {
    store.attention(
        Some("open"),
        None,
        Some(task_id),
        None,
        None,
        OPEN_ATTENTION_ROWS,
        false,
    )
}

/// How many open items one task is carrying, at the page's own bound.
///
/// Shared with [`crate::projection`] rather than counted twice: the plans
/// listing badges this number and a second count would be a second answer.
pub(crate) fn task_attention_count(store: &Store, task_id: &str) -> Result<usize> {
    Ok(task_open_attention(store, task_id)?.len())
}

// ------------------------------------------------- the cross-board orderings

/// The decisions room's order: newest decided first, then id, then board so
/// the list never flickers between two rows settled in the same
/// millisecond.
///
/// Lives here rather than in the projection because it is the answer to one
/// question — which decision is newest — and two surfaces that answered it
/// differently would put the Undo on different rows.
pub(crate) fn sort_decided_queue(items: &mut [(String, Attention)]) {
    items.sort_by(|(project_a, item_a), (project_b, item_b)| {
        let decided_a = item_a.resolved_at.unwrap_or(i64::MIN);
        let decided_b = item_b.resolved_at.unwrap_or(i64::MIN);
        decided_b
            .cmp(&decided_a)
            .then_with(|| item_a.id.cmp(&item_b.id))
            .then_with(|| project_a.cmp(project_b))
    });
}

/// Deck order, in one place: priority first, then oldest, then id, then
/// board.
///
/// Interrupts lead and age is the tie-breaker that prevents starvation
/// within a level. The queue is read through `/api/v1/needs-you`, and both
/// surfaces that render it — the deck and the list — take the order from
/// there rather than sorting again.
pub(crate) fn sort_open_queue(items: &mut [(String, Attention)]) {
    items.sort_by(|(project_a, item_a), (project_b, item_b)| {
        (item_a.priority, item_a.created_at, &item_a.id, project_a).cmp(&(
            item_b.priority,
            item_b.created_at,
            &item_b.id,
            project_b,
        ))
    });
}

// The deck itself is no longer rendered here: `/` answers the application
// shell and the bundle mounts the deck (`t-1f495a7f`, spec SPA-51). What
// stayed behind is everything the deck was laid OVER — `open_attention`,
// `open_cards`, `decision_card`, `EMPTY_QUEUE`, `reply_notices` — because
// `/all` still serves the plain list from exactly those pieces, and the
// mounted deck reads the same queue through `/api/v1/needs-you`.

/// The first `bound` characters of a body, cut on a character boundary so the
/// markdown renderer never sees half a grapheme, with an ellipsis when it cut.
pub(crate) fn excerpt(text: &str, bound: usize) -> String {
    if text.chars().count() <= bound {
        return text.to_owned();
    }
    format!("{}…", text.chars().take(bound).collect::<String>())
}

/// What the search surface asks for, fixed here rather than exposed: the
/// page has never had a filter control, so neither has the route that
/// serves it (SPA-06).
pub(crate) const SEARCH_LIMIT: usize = 30;
pub(crate) const SEARCH_MAX_CHARS: usize = 30_000;

/// One cross-board retrieval, bounded, as both served surfaces read it.
///
/// Shared rather than copied: `projection::search` answers the JSON route
/// from this function, so the two surfaces cannot disagree about which
/// boards were searched, how the results were ranked, or where the bound
/// fell.
///
/// An empty query performs no search at all and returns the empty receipt —
/// an unbounded listing dressed as a search is how a read surface lets
/// absence read as a finding.
pub(crate) fn search_receipt(query: &str) -> Result<crate::model::SearchReceipt> {
    let query = query.trim();
    let options = SearchOptions {
        query: query.to_owned(),
        source: None,
        status: None,
        tags: Vec::new(),
        lane: None,
        after: None,
        before: None,
        include_archived: false,
        limit: SEARCH_LIMIT,
        max_chars: SEARCH_MAX_CHARS,
    };
    if query.is_empty() {
        return Ok(search::bound_receipt(
            query,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            options.limit,
            options.max_chars,
        ));
    }
    let registry = Registry::open()?;
    let mut results = Vec::new();
    let mut boards = Vec::new();
    let mut missing = Vec::new();
    for project in registry.projects_active()? {
        if !Path::new(&project.board_path).is_file() {
            missing.push(project.name);
            continue;
        }
        let store = Store::open_as_caller(Path::new(&project.board_path))?;
        results.extend(store.search(&project.name, &options)?);
        boards.push(project.name);
    }
    // Rule text is scoped to the boards this read actually covered: a rule
    // whose selectors name only some other board never applies here, and
    // serving its body would be a cross-board read dressed as a rule hit
    // (ADR-027).
    results.extend(search::search_rules(
        &registry.rules_targeting_any(&boards, false)?,
        &options,
    ));
    Ok(search::bound_receipt(
        query,
        boards,
        missing,
        // This surface classifies board paths with its own `is_file` check
        // rather than the shared classifier, so it never produces this
        // bucket.
        Vec::new(),
        results,
        options.limit,
        options.max_chars,
    ))
}

/// Every board's sitreps, grouped by `(board, lane)` and ordered most
/// recently active first, plus whether any one board's scan was cut at
/// `limit`.
///
/// Shared by the Lanes page and `/api/v1/lanes`, which is why the bound is a
/// parameter and why the scan asks for one row past it: the JSON envelope has
/// to report whether rows were cut, and a flag inferred from `returned ==
/// limit` would be a guess (ADR-037 §1).
///
/// Most recently active first. Nothing deletes a sitrep, and nothing
/// should — but that means a lane whose driver is long gone keeps its rows
/// forever, and alphabetical order parks it at the top of the page. Sorting
/// by recency lets a dead lane sink out of the way without destroying what
/// it said, which is the same answer archiving gives within a lane.
///
/// Every board this caller may read, not every board: `Store::sitreps` is
/// guarded, and a board it refuses is skipped rather than propagated, so the
/// page and the route answer the readable estate (SPA-08).
#[allow(clippy::type_complexity)]
pub(crate) fn lane_groups(limit: i64) -> Result<(Vec<((String, String), Vec<Sitrep>)>, bool)> {
    let mut by_lane: std::collections::BTreeMap<(String, String), Vec<Sitrep>> =
        std::collections::BTreeMap::new();
    let mut truncated = false;
    for (project, store) in projects()? {
        // A board this caller may not read contributes no lanes, and the page
        // still answers for the boards they may. Skipping rather than
        // refusing keeps the enumeration from becoming an existence oracle:
        // the absent board is indistinguishable from a board with no sitreps.
        // Any other failure is a real fault and propagates.
        let mut updates = match store.sitreps(None, false, None, limit + 1) {
            Ok(updates) => updates,
            Err(error) => match projection::Refusal::from(error) {
                projection::Refusal::DeniedOrNotFound => continue,
                projection::Refusal::Failed(error) => return Err(error),
            },
        };
        if i64::try_from(updates.len()).unwrap_or(i64::MAX) > limit {
            truncated = true;
            updates.truncate(usize::try_from(limit.max(0)).unwrap_or(usize::MAX));
        }
        for update in updates {
            by_lane
                .entry((project.name.clone(), update.lane.clone()))
                .or_default()
                .push(update);
        }
    }
    let mut ordered = by_lane.into_iter().collect::<Vec<_>>();
    ordered.sort_by_key(|(_, updates)| {
        std::cmp::Reverse(updates.iter().map(|u| u.created_at).max().unwrap_or(0))
    });
    Ok((ordered, truncated))
}

// ------------------------------------------------------------------ rendering

/// Long board texts as markdown, so structure reads at a glance (George,
/// 2026-09-11: "use formatting and markdown and etc to make it easier to read
/// as well for all our texts").
///
/// **Raw HTML never survives this.** Board text is agent-authored and this
/// page carries write power, so a body is a document to typeset, never a
/// document to trust: `Html` and `InlineHtml` events are dropped, and a link
/// whose destination is not `http(s)`, `mailto` or same-page keeps its text
/// and points at nothing. Everything else the renderer emits is its own
/// escaped output, so `escape` stays the rule for every scalar the page
/// interpolates and this is the one bounded place with a parser in front.
///
/// A soft break becomes a hard break. Bodies were `pre-wrap` plain text
/// before this, and a receipt's SHA line or a `RESOLVE-WHEN` clause is
/// line-shaped on purpose: markdown's default "newline means space" would
/// reflow those into prose the moment the renderer touched them.
/// The JSON projection typesets a card's body through this same function
/// (`rust/projection.rs`), so the mounted deck renders bytes this parser
/// produced rather than growing a second markdown renderer — and a second
/// sanitiser with it.
pub(crate) fn markdown(text: &str) -> String {
    let mut options = pulldown_cmark::Options::empty();
    options.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
    options.insert(pulldown_cmark::Options::ENABLE_TABLES);
    let rewritten = pulldown_cmark::Parser::new_ext(text, options).map(|event| match event {
        pulldown_cmark::Event::Html(_) | pulldown_cmark::Event::InlineHtml(_) => {
            pulldown_cmark::Event::Text("".into())
        }
        pulldown_cmark::Event::SoftBreak => pulldown_cmark::Event::HardBreak,
        pulldown_cmark::Event::Start(pulldown_cmark::Tag::Link {
            link_type,
            dest_url,
            title,
            id,
        }) if !safe_href(&dest_url) => pulldown_cmark::Event::Start(pulldown_cmark::Tag::Link {
            link_type,
            dest_url: "#".into(),
            title,
            id,
        }),
        other => other,
    });
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, rewritten);
    out
}

/// Whether a markdown link destination may survive into the page. Same-page
/// anchors, web links and mail addresses pass; anything with another scheme —
/// `javascript:` first among them — does not.
fn safe_href(destination: &str) -> bool {
    let trimmed = destination.trim();
    if trimmed.starts_with('#') {
        return true;
    }
    match trimmed.split_once(':') {
        None => true,
        Some((scheme, _)) => [
            scheme.eq_ignore_ascii_case("http"),
            scheme.eq_ignore_ascii_case("https"),
            scheme.eq_ignore_ascii_case("mailto"),
        ]
        .into_iter()
        .any(|allowed| allowed),
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

/// The one document that is not the application: what a refusal is written
/// on.
///
/// Every page of the operator UI is the bundle now (`t-bf255880`), so this
/// is no longer a shell at all — no stylesheet, no script, no navigation.
/// It answers the three things that are not pages: a POST to a path that
/// takes no POST, a POST the route refuses, and a GET of an address that
/// names nothing.
///
/// The `<p class=error>` is a CONTRACT, not decoration: `web/src/api.ts`'s
/// `postForm` parses this document and reads the board's own sentence out
/// of that element to put it under the control that was pressed. A refusal
/// that dropped the class would reach the operator as a bare status code.
fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{title} — kanban</title></head><body>\
         <main id=main>{body}</main></body></html>",
        title = escape(title),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::AuthzContext;
    use crate::model::{
        AddSubscription, AddTask, DecisionCard, DeployIdentity, FinishDeployment, NewSprint,
        StartDeployment, Subscription,
    };
    use crate::policy::{Capability, ScopeTuple, authority};
    use crate::routing::Enforcement;
    use std::env;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tiny_http::TestRequest;

    const RENDER_CHILD_TEST: &str = "serve::tests::serve_render_fixture_child_process";
    const RENDER_CHILD_MARKER: &str = "serve-render-fixture-child";

    /// A restricted row says so, and the holder is served without its lease
    /// token (SPA-09).
    ///
    /// The page that said both is the bundle's now, so what is asserted
    /// here is what the SERVER hands it: the row's own `allowedModels`, and
    /// a holder serialised as `ClaimSummary`, which has no token field to
    /// forget to strip. The rendered sentences are read in Chrome by
    /// `mobile_read_navigation_journey_in_real_chrome_reaches_seeded_records`.
    #[test]
    fn task_detail_lists_allowed_models_and_the_holders_model_unit() {
        let mut task = crate::model::Task {
            id: "t-model".to_owned(),
            task_type: "task".to_owned(),
            parent_id: None,
            title: "Blender scene".to_owned(),
            body: None,
            assignee: None,
            lane: None,
            deliverable: None,
            stale_minutes: None,
            driver_only: false,
            status: "todo".to_owned(),
            priority: 3,
            priority_level: Some("P1".to_owned()),
            created_at: 1,
            updated_at: 2,
            completed_at: None,
            archived: false,
            archived_at: None,
            metadata: serde_json::json!({}),
            tags: Vec::new(),
            allowed_models: Vec::new(),
        };
        let projected =
            |task: &crate::model::Task| serde_json::to_value(task).expect("the row serialises");
        assert_eq!(
            projected(&task)["allowedModels"],
            serde_json::json!([]),
            "an unrestricted row must carry an empty allow-list, not a missing one"
        );
        task.allowed_models = vec!["Astra".to_owned(), "Blender-1".to_owned()];
        assert_eq!(
            projected(&task)["allowedModels"],
            serde_json::json!(["Astra", "Blender-1"])
        );

        let mut claim = crate::model::Claim {
            task_id: "t-model".to_owned(),
            agent_id: "driver-2".to_owned(),
            session_id: None,
            lease_token: "secret-token".to_owned(),
            claimed_at: 1,
            heartbeat_at: 1,
            expires_at: 2,
            worktree: None,
            worktree_kind: None,
            branch: None,
            head_sha: None,
            root_head: None,
            model: None,
        };
        let served = |claim: &crate::model::Claim| {
            serde_json::to_string(&crate::model::ClaimSummary::from(claim))
                .expect("the holder serialises")
        };
        let without = served(&claim);
        assert!(
            !without.contains("\"model\":\""),
            "a claim taken without --model served an empty model: {without}"
        );
        claim.model = Some("Astra".to_owned());
        let with = served(&claim);
        assert!(with.contains("\"model\":\"Astra\""), "{with}");
        for held in [&without, &with] {
            assert!(
                !held.contains("secret-token") && !held.contains("leaseToken"),
                "the lease token must never reach the page: {held}"
            );
        }
    }

    struct RenderFixture {
        epic_id: String,
        story_id: String,
        task_id: String,
        current_deployment_id: String,
        failed_deployment_id: String,
        current_sprint_started_at: i64,
        closed_sprint_started_at: i64,
        closed_sprint_ended_at: i64,
    }

    struct TempDataDir(PathBuf);

    impl TempDataDir {
        fn new(label: &str) -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "kanban-serve-{label}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("create isolated data dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDataDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Spawn this test binary again with an isolated data dir, so a fixture
    /// that has to set `KANBAN_DATA_DIR` never races the rest of the suite.
    fn spawn_fixture_child(data_dir: &Path, test: &str, marker_env: &str, marker: &str) -> Output {
        Command::new(env::current_exe().expect("current test binary"))
            .args(["--exact", "--ignored", test, "--nocapture"])
            .env("KANBAN_DATA_DIR", data_dir)
            .env(marker_env, marker)
            .output()
            .expect("spawn child fixture")
    }

    fn seed_render_fixture(data_dir: &Path) -> RenderFixture {
        fs::create_dir_all(data_dir).expect("create isolated data dir");
        let mut registry = Registry::open().expect("open registry");
        let board_name = "SERVE-RENDER";
        let project = registry
            .register(None, board_name, false, "geoyws")
            .expect("register rootless board");
        let board_path = PathBuf::from(&project.board_path);
        let mut store = Store::open(&board_path).expect("open store");
        store
            .initialize(board_name, "geoyws")
            .expect("initialize board metadata");
        store
            .add_tag("ops", Some("Operational work"), Some("geoyws"))
            .expect("register ops tag");
        store
            .add_tag("release", Some("Release work"), Some("geoyws"))
            .expect("register release tag");
        let epic = store
            .add_task(AddTask {
                id: Some("e-serve-render".to_owned()),
                task_type: "epic".to_owned(),
                parent_id: None,
                title: "Plan <b>render</b>".to_owned(),
                body: Some("Draft body with <script>alert(1)</script>".to_owned()),
                assignee: None,
                lane: None,
                deliverable: None,
                stale_minutes: None,
                driver_only: false,
                status: "draft".to_owned(),
                priority: 0,
                dependencies: vec![],
                metadata: serde_json::json!({"workflowStatus": "planning"}),
                actor: Some("geoyws".to_owned()),
                tags: vec!["ops".to_owned()],
                allowed_models: vec![],
            })
            .expect("add epic");
        let story = store
            .add_task(AddTask {
                id: Some("s-serve-render".to_owned()),
                task_type: "story".to_owned(),
                parent_id: Some(epic.id.clone()),
                title: "Ship <script>render</script>".to_owned(),
                body: Some("Story body with <em>markup</em>".to_owned()),
                assignee: Some("geoyws".to_owned()),
                lane: None,
                deliverable: Some("Release package".to_owned()),
                stale_minutes: None,
                driver_only: false,
                status: "todo".to_owned(),
                priority: 2,
                dependencies: vec![],
                metadata: serde_json::json!({}),
                actor: Some("geoyws".to_owned()),
                tags: vec!["release".to_owned()],
                allowed_models: vec![],
            })
            .expect("add story");
        let task = store
            .add_task(AddTask {
                id: Some("t-serve-render".to_owned()),
                task_type: "task".to_owned(),
                parent_id: Some(epic.id.clone()),
                title: "Implement <i>escape</i>".to_owned(),
                body: Some("Task body with & < >".to_owned()),
                assignee: Some("geoyws".to_owned()),
                lane: Some("driver-2".to_owned()),
                deliverable: None,
                stale_minutes: Some(120),
                driver_only: false,
                status: "in_progress".to_owned(),
                priority: 1,
                dependencies: vec![],
                metadata: serde_json::json!({"focus": "render"}),
                actor: Some("geoyws".to_owned()),
                tags: vec!["ops".to_owned(), "release".to_owned()],
                allowed_models: vec![],
            })
            .expect("add task");
        let done = store
            .add_task(AddTask {
                id: Some("t-serve-done".to_owned()),
                task_type: "task".to_owned(),
                parent_id: Some(epic.id.clone()),
                title: "Completed <span>delivery</span>".to_owned(),
                body: Some("Done body with <u>markup</u>".to_owned()),
                assignee: Some("geoyws".to_owned()),
                lane: Some("driver-3".to_owned()),
                deliverable: None,
                stale_minutes: None,
                driver_only: false,
                status: "done".to_owned(),
                priority: 3,
                dependencies: vec![],
                metadata: serde_json::json!({"done": true}),
                actor: Some("geoyws".to_owned()),
                tags: vec!["release".to_owned()],
                allowed_models: vec![],
            })
            .expect("add done task");
        // The two edges are attached after the rows exist rather than declared
        // at creation. The fixture's whole point is a row rendered mid-work and
        // a row rendered finished, and a completion gate refuses to CREATE
        // either state while a prerequisite is unfinished; editing dependencies
        // is not a work transition, so the board ends up in exactly the shape
        // the page is here to render.
        store
            .update_task(
                &task.id,
                crate::store::UpdateTask {
                    dependencies: Some(vec![story.id.clone()]),
                    ..Default::default()
                },
                "geoyws",
            )
            .expect("attach in-progress task dependency");
        store
            .update_task(
                &done.id,
                crate::store::UpdateTask {
                    dependencies: Some(vec![task.id.clone()]),
                    ..Default::default()
                },
                "geoyws",
            )
            .expect("attach done task dependency");
        store
            .add_note(
                &epic.id,
                "geoyws",
                "decision",
                "Keep the <script> tag escaped & readable.",
            )
            .expect("add note");
        let attention = store
            .raise_attention(
                "Please review <strong>before release</strong>.",
                "decision",
                "geoyws",
                Some(&epic.id),
                0,
                &["ops".to_owned(), "release".to_owned()],
                &DecisionCard::default(),
                None,
            )
            .expect("raise attention");
        store
            .post_sitrep(
                "driver-2",
                "Working <i>quietly</i> on the render page.",
                "agent",
                Some(&task.id),
                None,
            )
            .expect("post driver-2 sitrep");
        store
            .post_sitrep(
                "driver-3",
                "A second lane with <b>markup</b> to sort.",
                "agent",
                Some(&done.id),
                None,
            )
            .expect("post driver-3 sitrep");
        let current_start = store
            .start_deployment(StartDeployment {
                task_id: Some(epic.id.clone()),
                repo: "geoyws/kanban".to_owned(),
                identity: DeployIdentity::Git(
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                ),
                deployer_checkout: None,
                branch: Some("feature/render".to_owned()),
                tier: "@_s".to_owned(),
                environment: "staging".to_owned(),
                host: "serve-host".to_owned(),
                url: "https://serve.invalid/current".to_owned(),
                mechanism: Some("manual".to_owned()),
                operation_id: Some("serve-op-current".to_owned()),
                retry_of: None,
                actor: "geoyws".to_owned(),
                lane: Some("deploy".to_owned()),
                sprint_id: None,
            })
            .expect("start current deployment");
        let current = store
            .finish_deployment(FinishDeployment {
                id: current_start.deployment.id.clone(),
                capability_token: current_start.capability_token.clone(),
                result: "succeeded".to_owned(),
                phase: Some("verification".to_owned()),
                receipt: Some("served <release> successfully".to_owned()),
                artifact_uri: Some("artifact://kanban/<render>".to_owned()),
                served_commit: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned()),
                observed: Vec::new(),
                actor: "geoyws".to_owned(),
                served_version: None,
            })
            .expect("finish current deployment");
        let failed_start = store
            .start_deployment(StartDeployment {
                task_id: Some(story.id.clone()),
                repo: "geoyws/kanban".to_owned(),
                identity: DeployIdentity::Git(
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
                ),
                deployer_checkout: None,
                branch: Some("feature/render-failure".to_owned()),
                tier: "@_bs".to_owned(),
                environment: "staging".to_owned(),
                host: "serve-host".to_owned(),
                url: "https://serve.invalid/failed".to_owned(),
                mechanism: Some("manual".to_owned()),
                operation_id: Some("serve-op-failed".to_owned()),
                retry_of: None,
                actor: "geoyws".to_owned(),
                lane: Some("deploy".to_owned()),
                sprint_id: None,
            })
            .expect("start failed deployment");
        let failed = store
            .finish_deployment(FinishDeployment {
                id: failed_start.deployment.id.clone(),
                capability_token: failed_start.capability_token.clone(),
                result: "failed".to_owned(),
                phase: Some("build".to_owned()),
                receipt: Some("build <failed> because the render check did not pass".to_owned()),
                artifact_uri: None,
                served_commit: None,
                observed: Vec::new(),
                actor: "geoyws".to_owned(),
                served_version: None,
            })
            .expect("finish failed deployment");
        let closed = store
            .create_sprint(NewSprint {
                id: Some("sp-render-closed".to_owned()),
                title: "Closed <release>".to_owned(),
                body: None,
                target_version: "3.1.0".to_owned(),
                scheduled_start: 1_700_000_000_000,
                scheduled_end: 4_102_444_800_000,
                actor: "geoyws".to_owned(),
            })
            .expect("create closed sprint");
        store
            .plan_sprint(
                &closed.id,
                "Ship **3.1.0**\n\n<script>unsafe()</script>",
                std::slice::from_ref(&done.id),
                None,
                false,
                "geoyws",
            )
            .expect("plan closed sprint");
        store
            .start_sprint(&closed.id, "geoyws")
            .expect("start closed sprint");
        let proof_start = store
            .start_deployment(StartDeployment {
                task_id: Some(done.id.clone()),
                repo: "geoyws/kanban".to_owned(),
                identity: DeployIdentity::Git(
                    "cccccccccccccccccccccccccccccccccccccccc".to_owned(),
                ),
                deployer_checkout: None,
                branch: Some("release/3.1.0".to_owned()),
                tier: "@_p".to_owned(),
                environment: "production".to_owned(),
                host: "serve-host".to_owned(),
                url: "https://serve.invalid/3.1.0".to_owned(),
                mechanism: Some("unit fixture".to_owned()),
                operation_id: Some("serve-sprint-proof".to_owned()),
                retry_of: None,
                actor: "geoyws".to_owned(),
                lane: Some("deploy".to_owned()),
                sprint_id: Some(closed.id.clone()),
            })
            .expect("start sprint proof");
        let proof = store
            .finish_deployment(FinishDeployment {
                id: proof_start.deployment.id.clone(),
                capability_token: proof_start.capability_token,
                result: "succeeded".to_owned(),
                phase: Some("verification".to_owned()),
                receipt: Some("served 3.1.0".to_owned()),
                artifact_uri: None,
                served_commit: Some("cccccccccccccccccccccccccccccccccccccccc".to_owned()),
                observed: Vec::new(),
                actor: "geoyws".to_owned(),
                served_version: Some("3.1.0".to_owned()),
            })
            .expect("finish sprint proof");
        let closed_finished = store
            .close_sprint(&closed.id, Some(&proof.id), None, None, "geoyws")
            .expect("close sprint on proof");

        let opaque = store
            .add_task(AddTask {
                id: Some("t-render/opaque?#".to_owned()),
                task_type: "task".to_owned(),
                parent_id: None,
                title: "Opaque <task>".to_owned(),
                body: None,
                assignee: None,
                lane: None,
                deliverable: None,
                stale_minutes: None,
                driver_only: false,
                status: "todo".to_owned(),
                priority: 2,
                dependencies: vec![],
                metadata: serde_json::json!({}),
                actor: Some("geoyws".to_owned()),
                tags: vec!["release".to_owned()],
                allowed_models: vec![],
            })
            .expect("add opaque sprint task");
        let current_sprint = store
            .create_sprint(NewSprint {
                id: Some("sp-render-current".to_owned()),
                title: "Current release".to_owned(),
                body: None,
                target_version: "3.2.0".to_owned(),
                scheduled_start: 1_700_000_000_000,
                scheduled_end: 4_102_444_800_000,
                actor: "geoyws".to_owned(),
            })
            .expect("create current sprint");
        store
            .plan_sprint(
                &current_sprint.id,
                "Current **goal** and criteria.",
                std::slice::from_ref(&opaque.id),
                None,
                false,
                "geoyws",
            )
            .expect("plan current sprint");
        let current_started = store
            .start_sprint(&current_sprint.id, "geoyws")
            .expect("start current sprint");

        let planned = store
            .create_sprint(NewSprint {
                id: Some("sp-render-planned".to_owned()),
                title: "Planned release".to_owned(),
                body: None,
                target_version: "3.3.0".to_owned(),
                scheduled_start: 4_102_444_800_000,
                scheduled_end: 4_102_531_200_000,
                actor: "geoyws".to_owned(),
            })
            .expect("create planned sprint");
        store
            .plan_sprint(&planned.id, "Planned goal.", &[], None, true, "geoyws")
            .expect("plan future sprint");
        let abandoned = store
            .create_sprint(NewSprint {
                id: Some("sp-render-abandoned".to_owned()),
                title: "Abandoned release".to_owned(),
                body: Some("Goal that was not met.".to_owned()),
                target_version: "3.0.0".to_owned(),
                scheduled_start: 1_600_000_000_000,
                scheduled_end: 1_700_000_000_000,
                actor: "geoyws".to_owned(),
            })
            .expect("create abandoned sprint");
        store
            .abandon_sprint(&abandoned.id, "superseded", "geoyws")
            .expect("abandon sprint");
        let archived = store
            .create_sprint(NewSprint {
                id: Some("sp-render-archived".to_owned()),
                title: "Archived release".to_owned(),
                body: None,
                target_version: "2.9.0".to_owned(),
                scheduled_start: 1_500_000_000_000,
                scheduled_end: 1_600_000_000_000,
                actor: "geoyws".to_owned(),
            })
            .expect("create archived sprint");
        store
            .connection
            .execute("UPDATE sprints SET archived=1 WHERE id=?", [&archived.id])
            .expect("archive historical sprint fixture");
        let missing_proof = store
            .create_sprint(NewSprint {
                id: Some("sp-render-missing-proof".to_owned()),
                title: "Closed without proof fixture".to_owned(),
                body: Some("Legacy closed row.".to_owned()),
                target_version: "2.8.0".to_owned(),
                scheduled_start: 1_400_000_000_000,
                scheduled_end: 1_500_000_000_000,
                actor: "geoyws".to_owned(),
            })
            .expect("create legacy sprint fixture");
        store
            .connection
            .execute(
                "UPDATE sprints SET status='closed',ends_at=updated_at WHERE id=?",
                [&missing_proof.id],
            )
            .expect("seed a legacy closed row without proof");

        let empty = registry
            .register(None, "SERVE-SPRINT-EMPTY", false, "geoyws")
            .expect("register empty sprint board");
        let mut empty_store = Store::open(Path::new(&empty.board_path)).expect("open empty board");
        empty_store
            .initialize("SERVE-SPRINT-EMPTY", "geoyws")
            .expect("initialize empty board");
        let current_only = registry
            .register(None, "SERVE-SPRINT-CURRENT-ONLY", false, "geoyws")
            .expect("register current-only sprint board");
        let mut current_only_store =
            Store::open(Path::new(&current_only.board_path)).expect("open current-only board");
        current_only_store
            .initialize("SERVE-SPRINT-CURRENT-ONLY", "geoyws")
            .expect("initialize current-only board");
        let sole_current = current_only_store
            .create_sprint(NewSprint {
                id: Some("sp-only-current".to_owned()),
                title: "Only current sprint".to_owned(),
                body: None,
                target_version: "1.0.0".to_owned(),
                scheduled_start: 0,
                scheduled_end: 4_102_444_800_000,
                actor: "geoyws".to_owned(),
            })
            .expect("create sole current sprint");
        current_only_store
            .plan_sprint(
                &sole_current.id,
                "The only sprint on this board.",
                &[],
                None,
                true,
                "geoyws",
            )
            .expect("plan sole current sprint");
        current_only_store
            .start_sprint(&sole_current.id, "geoyws")
            .expect("start sole current sprint");

        assert_eq!(attention.task_id.as_deref(), Some(epic.id.as_str()));
        assert_eq!(current.status, "succeeded");
        assert_eq!(failed.status, "failed");
        RenderFixture {
            epic_id: epic.id,
            story_id: story.id,
            task_id: task.id,
            current_deployment_id: current.id,
            failed_deployment_id: failed.id,
            current_sprint_started_at: current_started.starts_at,
            closed_sprint_started_at: closed_finished.starts_at,
            closed_sprint_ended_at: closed_finished.ends_at.expect("closed sprint end"),
        }
    }

    fn assert_html_contains(html: &str, needle: &str) {
        assert!(html.contains(needle), "missing {needle:?} in {html}");
    }

    fn assert_page_title(html: &str, title: &str) {
        assert_html_contains(html, &format!("<title>{title} — kanban</title>"));
    }

    /// One parsed rule of the stylesheet: the at-rule it sits inside, what
    /// it selects, and its declarations verbatim.
    ///
    /// `css_selectors` above answers "what is this scoped to"; a design
    /// system has to be asked "what does this DECLARE", so the rules are
    /// parsed rather than substring-matched. The stylesheet is one authored
    /// string with no nested plain rules and no braces inside a value, which
    /// is what makes a parser this small honest about it.
    struct CssRule {
        at: Option<String>,
        selectors: Vec<String>,
        body: String,
    }

    fn css_without_comments(css: &str) -> String {
        let mut stripped = String::with_capacity(css.len());
        let mut rest = css;
        while let Some(open) = rest.find("/*") {
            stripped.push_str(&rest[..open]);
            rest = match rest[open..].find("*/") {
                Some(close) => &rest[open + close + 2..],
                None => "",
            };
        }
        stripped.push_str(rest);
        stripped
    }

    fn css_rules(css: &str) -> Vec<CssRule> {
        let stripped = css_without_comments(css);
        let chars: Vec<char> = stripped.chars().collect();
        let mut rules = Vec::new();
        let mut prelude = String::new();
        let mut at: Vec<String> = Vec::new();
        let mut index = 0;
        while index < chars.len() {
            match chars[index] {
                '{' => {
                    let head = prelude.trim().to_owned();
                    prelude.clear();
                    index += 1;
                    if head.starts_with('@') {
                        at.push(head);
                        continue;
                    }
                    let mut body = String::new();
                    while index < chars.len() && chars[index] != '}' {
                        body.push(chars[index]);
                        index += 1;
                    }
                    index += 1;
                    rules.push(CssRule {
                        at: at.last().cloned(),
                        selectors: head
                            .split(',')
                            .map(|part| part.trim().to_owned())
                            .filter(|part| !part.is_empty())
                            .collect(),
                        body,
                    });
                }
                '}' => {
                    at.pop();
                    index += 1;
                }
                other => {
                    prelude.push(other);
                    index += 1;
                }
            }
        }
        rules
    }

    /// One rule's declarations as `(property, value)`.
    fn css_declarations(body: &str) -> Vec<(String, String)> {
        body.split(';')
            .filter_map(|declaration| {
                let declaration = declaration.trim();
                let (property, value) = declaration.split_once(':')?;
                Some((property.trim().to_owned(), value.trim().to_owned()))
            })
            .collect()
    }

    /// Every declaration the stylesheet makes, paired with the selectors it
    /// makes it on. Keyframe stops are dropped: they are a motion's shape,
    /// not a rule about an element.
    fn css_styled_declarations() -> Vec<(Vec<String>, String, String)> {
        let mut out = Vec::new();
        for rule in css_rules(&bundle_stylesheet()) {
            if rule
                .at
                .as_deref()
                .is_some_and(|at| at.starts_with("@keyframes"))
            {
                continue;
            }
            for (property, value) in css_declarations(&rule.body) {
                out.push((rule.selectors.clone(), property, value));
            }
        }
        out
    }

    /// The selectors of every rule whose declarations mention `needle`.
    fn css_selectors_declaring(needle: &str) -> Vec<String> {
        let mut out = Vec::new();
        for rule in css_rules(&bundle_stylesheet()) {
            if rule
                .at
                .as_deref()
                .is_some_and(|at| at.starts_with("@keyframes"))
            {
                continue;
            }
            if rule.body.contains(needle) {
                out.extend(rule.selectors.clone());
            }
        }
        out
    }

    /// The declarations of the one rule that selects exactly `selector`,
    /// joined across every rule that names it on its own.
    fn css_rule_body(selector: &str) -> String {
        css_rule_body_in(&bundle_stylesheet(), selector)
    }

    /// The same, in a named stylesheet. One stylesheet ships now — the
    /// bundle's — and this is the seam the proofs above read it through.
    fn css_rule_body_in(css: &str, selector: &str) -> String {
        let mut body = String::new();
        for rule in css_rules(css) {
            if rule.selectors.iter().any(|part| part == selector) {
                body.push_str(&rule.body);
                body.push(';');
            }
        }
        assert!(
            !body.is_empty(),
            "no rule in the stylesheet selects {selector}"
        );
        body
    }

    /// A colour token's hex, read out of a stylesheet's `:root` block.
    fn css_token_in(css: &str, name: &str) -> String {
        let root = css_rule_body_in(css, ":root");
        css_declarations(&root)
            .into_iter()
            .find(|(property, _)| property == name)
            .map(|(_, value)| value)
            .unwrap_or_else(|| panic!("no token {name} in :root"))
    }

    /// WCAG 2.2 relative luminance of an `#rrggbb` string.
    fn relative_luminance(hex: &str) -> f64 {
        let hex = hex.trim_start_matches('#');
        assert_eq!(hex.len(), 6, "not a six-digit hex: {hex}");
        let channel = |at: usize| {
            let raw = u8::from_str_radix(&hex[at..at + 2], 16).expect("hex channel");
            let unit = f64::from(raw) / 255.0;
            if unit <= 0.039_28 {
                unit / 12.92
            } else {
                ((unit + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(0) + 0.7152 * channel(2) + 0.0722 * channel(4)
    }

    /// WCAG 2.2 contrast ratio between two token names.
    fn token_contrast(foreground: &str, background: &str) -> f64 {
        token_contrast_in(&bundle_stylesheet(), foreground, background)
    }

    fn token_contrast_in(css: &str, foreground: &str, background: &str) -> f64 {
        let first = relative_luminance(&css_token_in(css, foreground));
        let second = relative_luminance(&css_token_in(css, background));
        let (lighter, darker) = if first >= second {
            (first, second)
        } else {
            (second, first)
        };
        (lighter + 0.05) / (darker + 0.05)
    }

    /// WEB-01, WEB-05 — one serif, and it is the question; mono is for what
    /// is literally code.
    ///
    /// The serif is the page's one memorable element and it is spent on the
    /// question. A third selector reaching for it is the moment the choice
    /// stops meaning anything.
    #[test]
    fn the_stylesheet_names_one_serif_and_reserves_mono_for_code_unit() {
        let stylesheet = bundle_stylesheet();
        let serif_token = css_token_in(&stylesheet, "--serif");
        let named = serif_token.split(',').map(str::trim).collect::<Vec<_>>();
        assert_eq!(
            named,
            [
                "ui-serif",
                "\"New York\"",
                "\"Iowan Old Style\"",
                "Charter",
                "Georgia",
                "serif"
            ],
            "the serif stack moved"
        );
        let mut serif = css_selectors_declaring("var(--serif)");
        serif.sort();
        assert_eq!(serif, vec![".item>h2".to_owned(), "h1".to_owned()]);
        // Mono is for an identifier, a key map and a numeric column, and the
        // allowlist is the spec's (WEB-05). A selector is judged by the
        // element it lands on, so `.choice .key` is `.key`.
        // `.sprint-version` joined the list with the mounted sprints page:
        // a served version is an identifier a reader matches character by
        // character against a deploy receipt, which is what the allowlist
        // is for (WEB-05).
        let allowed = [
            "code",
            ".id",
            ".key",
            ".priority",
            "p.keys",
            "td.n",
            "th.n",
            ".sprint-version",
            // `.check .subject` joined with the ACC card: the check's
            // subject is a path, flag or tier a reader matches character by
            // character against the codebase — the same job `.sprint-version`
            // does against a deploy receipt (WEB-05).
            ".subject",
        ];
        for selector in css_selectors_declaring("var(--mono)") {
            let landed = selector.split_whitespace().last().unwrap_or(&selector);
            assert!(
                allowed.contains(&landed),
                "{selector} is not one of the places mono belongs"
            );
        }
    }

    /// WEB-04 — the type scale is the plan's, and nothing is tracked out.
    #[test]
    fn the_type_scale_is_declared_and_nothing_is_tracked_out_unit() {
        for (selector, declarations) in [
            ("h1", vec!["font-size:1.5rem", "line-height:1.2"]),
            (
                "h2",
                vec![
                    "font-size:1.0625rem",
                    "line-height:1.3",
                    "font-weight:600",
                    "color:var(--subtext)",
                ],
            ),
            ("body", vec!["font-size:1rem", "line-height:1.55"]),
            ("*.meta", vec![]),
            (
                "button",
                vec!["font-size:1rem", "line-height:1.2", "font-weight:600"],
            ),
            (".meta", vec!["font-size:0.8125rem", "line-height:1.4"]),
            (
                ".choice .key",
                vec!["font-size:0.75rem", "font-family:var(--mono)"],
            ),
            (
                ".item>h2",
                vec![
                    "font-size:clamp(1.375rem,1.1rem + 1vw,1.75rem)",
                    "line-height:1.15",
                    "font-weight:600",
                ],
            ),
        ] {
            if declarations.is_empty() {
                continue;
            }
            let body = css_rule_body(selector);
            for declaration in declarations {
                assert!(
                    body.contains(declaration),
                    "{selector} does not declare {declaration}: {body}"
                );
            }
        }
        for (selectors, property, value) in css_styled_declarations() {
            assert_ne!(
                property, "text-transform",
                "{selectors:?} sets text-transform to {value}"
            );
            assert_ne!(
                property, "letter-spacing",
                "{selectors:?} sets letter-spacing to {value}"
            );
        }
    }

    /// WEB-06 — prose stays readable in line length.
    #[test]
    fn prose_blocks_are_bounded_to_seventy_characters_unit() {
        for selector in [".context", ".consequence", ".explain", ".item>h2", ".body"] {
            assert!(
                css_rule_body(selector).contains("max-width:70ch"),
                "{selector} is not bounded to 70ch"
            );
        }
    }

    /// WEB-08 — the token block is the only place a colour is written.
    #[test]
    fn the_token_block_is_the_only_place_a_colour_is_written_unit() {
        let root = css_rule_body(":root");
        let declared: Vec<(String, String)> = css_declarations(&root)
            .into_iter()
            .filter(|(_, value)| value.starts_with('#'))
            .collect();
        assert_eq!(
            declared,
            vec![
                ("--base".to_owned(), "#1e1e2e".to_owned()),
                ("--mantle".to_owned(), "#181825".to_owned()),
                ("--surface0".to_owned(), "#313244".to_owned()),
                ("--surface1".to_owned(), "#45475a".to_owned()),
                ("--text".to_owned(), "#cdd6f4".to_owned()),
                ("--subtext".to_owned(), "#a6adc8".to_owned()),
                ("--overlay".to_owned(), "#9399b2".to_owned()),
                ("--green".to_owned(), "#a6e3a1".to_owned()),
                ("--red".to_owned(), "#f38ba8".to_owned()),
                ("--yellow".to_owned(), "#f9e2af".to_owned()),
                ("--blue".to_owned(), "#89b4fa".to_owned()),
                ("--link".to_owned(), "#89b4fa".to_owned()),
                ("--focus".to_owned(), "#b4befe".to_owned()),
            ],
            "the token block is not the plan's"
        );
        // And no hex is written anywhere else: a colour picked at a rule is
        // a colour that means nothing.
        let stripped = css_without_comments(&bundle_stylesheet());
        let root_at = stripped.find(":root{").expect("the token block");
        let root_end = root_at + stripped[root_at..].find('}').expect("the block closes");
        let outside = format!("{}{}", &stripped[..root_at], &stripped[root_end..]);
        assert!(
            !outside.contains('#'),
            "a hex literal is written outside the token block: {outside}"
        );
    }

    /// WEB-09 — no outlines except the focus ring, and one hairline.
    ///
    /// A zero-valued declaration is a border being TAKEN AWAY, which is what
    /// this requirement is for: the allowlist is about the borders the design
    /// draws, and `border:0` draws none.
    #[test]
    fn no_border_or_outline_exists_outside_the_allowlist_unit() {
        let structural = ["border-radius", "border-collapse", "border-spacing"];
        for (selectors, property, value) in css_styled_declarations() {
            let is_border = (property.starts_with("border") || property.starts_with("outline"))
                && !structural.contains(&property.as_str());
            if !is_border {
                continue;
            }
            if value == "0" || value == "none" {
                continue;
            }
            let selector = selectors.join(",");
            let allowed =
                // the focus ring
                (selector.contains(":focus-visible") && property.starts_with("outline"))
                // the one hairline, between a card's body and its answers:
                // the foot of the answer panel's fade band, so the rule
                // lands where the band is already opaque
                || (property == "border-bottom"
                    && value == "1px solid var(--surface1)"
                    && selector.contains(".decide"))
                // the row separator
                || (property == "border-bottom" && value == "1px solid var(--surface0)")
                // a history receipt's outcome rule
                || (property == "border-left"
                    && value.starts_with("3px solid")
                    && selector.contains(".receipt"))
                // the current destination in the drawer, and the current
                // sprint on the sprints page: the same neutral rule saying
                // the same thing, which is why a hue is not needed for it
                || (property == "border-left"
                    && value == "2px solid var(--text)"
                    && (selector.contains("aria-current")
                        || selector.contains(".is-current")));
            assert!(
                allowed,
                "{selector} declares {property}:{value}, which is not in the allowlist"
            );
        }
    }

    /// WEB-10 — two radii, each with one job.
    #[test]
    fn only_two_radii_exist_and_each_has_one_job_unit() {
        let mut found = Vec::new();
        for (selectors, property, value) in css_styled_declarations() {
            if property != "border-radius" {
                continue;
            }
            assert!(
                value == "8px" || value == "999px",
                "{selectors:?} rounds to {value}, which is neither a control nor a pill"
            );
            found.push(value);
        }
        assert!(
            found.contains(&"8px".to_owned()),
            "no control radius at all"
        );
        assert!(found.contains(&"999px".to_owned()), "no pill radius at all");
    }

    /// WEB-11 — a colour is an outcome.
    ///
    /// The four hues mean approve, reject, defer and other, and they appear
    /// only where one of those is being said. A refusal is the one other
    /// place red is allowed to land, because a refusal IS an outcome the
    /// board reported (spec WEB-29 requires that sentence in red).
    #[test]
    fn an_outcome_hue_appears_only_on_an_outcome_unit() {
        let hues = ["var(--green)", "var(--red)", "var(--yellow)", "var(--blue)"];
        let mut filled = Vec::new();
        for rule in css_rules(&bundle_stylesheet()) {
            if rule
                .at
                .as_deref()
                .is_some_and(|at| at.starts_with("@keyframes"))
            {
                continue;
            }
            if !hues.iter().any(|hue| rule.body.contains(hue)) {
                continue;
            }
            for selector in &rule.selectors {
                let carries_outcome = (selector.contains(".choice")
                    && selector.contains(".outcome-"))
                    || (selector.contains(".receipt") && selector.contains(".outcome-"))
                    || (selector.contains(".pill") && selector.contains(".status-"))
                    || selector.contains(".priority-p0")
                    || selector == "p.error"
                    || selector == ".error"
                    || selector == ":root";
                assert!(
                    carries_outcome,
                    "{selector} is coloured without carrying an outcome or a status"
                );
            }
            for (property, value) in css_declarations(&rule.body) {
                if property == "background" && hues.iter().any(|hue| value.contains(hue)) {
                    filled.extend(rule.selectors.clone());
                }
            }
        }
        // Exactly one selector fills an answer, and it is the recommendation.
        // The hue reaches it through `--hue`, so this is one rule rather than
        // one per outcome.
        let fills = css_selectors_declaring("background:var(--hue");
        assert_eq!(fills, vec![".recommended .choice".to_owned()], "{fills:?}");
        assert!(
            filled.is_empty(),
            "an answer is filled with a literal hue instead of its outcome's: {filled:?}"
        );
    }

    /// WEB-14 — one motion exists in the stylesheet.
    #[test]
    fn one_motion_is_declared_and_nothing_else_animates_unit() {
        let stripped = css_without_comments(&bundle_stylesheet());
        let keyframes: Vec<&str> = stripped
            .match_indices("@keyframes ")
            .map(|(at, _)| {
                let rest = &stripped[at + "@keyframes ".len()..];
                &rest[..rest.find('{').expect("a keyframe block opens")]
            })
            .collect();
        assert_eq!(
            keyframes,
            vec!["deck-in", "deck-out", "deck-out-back"],
            "the stylesheet declares a motion that is not the card advance"
        );
        let mut transitions = Vec::new();
        for (selectors, property, value) in css_styled_declarations() {
            // `transition:none` is a transition being TAKEN AWAY, which is
            // what the reduced-motion block does with the one there is.
            if property.starts_with("transition") && value != "none" {
                transitions.push((selectors.join(","), value.clone()));
            }
            if property.starts_with("animation") && value != "none" {
                let selector = selectors.join(",");
                assert!(
                    selector.contains(".item.entering")
                        || selector.contains(".item.leaving")
                        || selector.contains(".item.leaving-back"),
                    "{selector} animates {value}, and the advance is the only motion"
                );
            }
        }
        assert_eq!(transitions.len(), 1, "{transitions:?}");
        assert!(
            transitions[0].1.contains("transform"),
            "the one transition is not the drawer's transform: {transitions:?}"
        );
        // The baseline's decorations are gone by name.
        for gone in ["pulse", "notice-in", "receipt-landed", "drawer-in"] {
            assert!(
                !stripped.contains(gone),
                "{gone} is still in the stylesheet"
            );
        }
    }

    /// WEB-21 — the long form says there is more.
    #[test]
    fn the_deck_body_fades_at_its_foot_unit() {
        let mut faded = Vec::new();
        for rule in css_rules(&bundle_stylesheet()) {
            if rule.body.contains("mask-image") {
                faded.push(rule);
            }
        }
        assert_eq!(faded.len(), 1, "more than one element fades");
        let fade = &faded[0];
        assert_eq!(fade.selectors, vec![".deck .item>.full .body".to_owned()]);
        assert!(fade.body.contains("linear-gradient"), "{}", fade.body);
        assert!(fade.body.contains("2rem"), "{}", fade.body);
    }

    /// WEB-47 — every answer gets a row of its own, and the panel is not a
    /// scroller.
    ///
    /// Superseded 2026-09-18: the requirement used to permit two-up and the
    /// panel used to be capped at three fifths of the card with its own
    /// `overflow-y`, which squeezed a 37-character label into a 177px
    /// button and put the note field off a 390x844 phone (George: "it's up
    /// but it's squished and not mobile responsive").
    #[test]
    fn every_answer_gets_its_own_row_and_the_panel_scrolls_nothing_unit() {
        let answers = css_rule_body(".alternatives");
        assert!(answers.contains("display:grid"), "{answers}");
        assert!(
            answers.contains("grid-template-columns:1fr;"),
            "the answers are not one column: {answers}"
        );
        let panel = css_rule_body(".deck .item>.decide");
        assert!(panel.contains("flex:none"), "{panel}");
        for banned in ["max-height", "overflow"] {
            assert!(
                !panel.contains(banned),
                "the answer panel still declares {banned}, so it is a nested \
                 scroller again: {panel}"
            );
        }
        // The card's own column is the one scroller the deck has.
        let card = css_rule_body(".deck .item");
        assert!(card.contains("overflow:hidden auto"), "{card}");
    }

    /// Every foreground/background pair ADR-046 puts text on, as one list:
    /// the served stylesheet and the bundle's are held to the same one
    /// (WEB-48, SPA-56), and two lists would be two standards.
    const AA_TOKEN_PAIRS: &[(&str, &str)] = &[
        ("--text", "--base"),
        ("--text", "--mantle"),
        ("--text", "--surface0"),
        ("--text", "--surface1"),
        ("--subtext", "--base"),
        ("--subtext", "--mantle"),
        ("--subtext", "--surface0"),
        ("--overlay", "--base"),
        ("--overlay", "--mantle"),
        ("--base", "--green"),
        ("--base", "--red"),
        ("--base", "--yellow"),
        ("--base", "--blue"),
        ("--green", "--surface0"),
        ("--red", "--surface0"),
        ("--yellow", "--surface0"),
        ("--blue", "--surface0"),
        ("--link", "--base"),
        ("--link", "--mantle"),
    ];

    /// WEB-48 — text contrast is at least 4.5:1 on the surface it sits on.
    ///
    /// Computed from the tokens rather than pinned, so a token change that
    /// drops a pair below AA fails here rather than on George's phone.
    #[test]
    fn every_token_pair_clears_four_and_a_half_to_one_unit() {
        for (foreground, background) in AA_TOKEN_PAIRS {
            let ratio = token_contrast(foreground, background);
            assert!(
                ratio >= 4.5,
                "{foreground} on {background} is {ratio:.2}:1, below AA for body text"
            );
        }
    }

    /// WEB-49 — `--overlay` never sits on `--surface0`.
    #[test]
    fn overlay_never_sits_on_surface0_unit() {
        // The arithmetic first, so the reason this rule exists is checked
        // rather than remembered.
        let ratio = token_contrast("--overlay", "--surface0");
        assert!(
            ratio < 4.5,
            "{ratio:.2}:1 -- if overlay now clears AA on surface0 this rule can go"
        );
        for rule in css_rules(&bundle_stylesheet()) {
            let declarations = css_declarations(&rule.body);
            let quiet = declarations
                .iter()
                .any(|(property, value)| property == "color" && value == "var(--overlay)");
            let filled = declarations
                .iter()
                .any(|(property, value)| property == "background" && value == "var(--surface0)");
            assert!(
                !(quiet && filled),
                "{:?} puts overlay text on a surface0 fill",
                rule.selectors
            );
        }
        // The pill is the fill quiet text is most likely to land on, and it
        // uses `--subtext` for exactly this reason.
        assert!(
            css_rule_body(".pill").contains("color:var(--subtext)"),
            "quiet text inside a pill has to be subtext"
        );
    }

    /// WEB-50 — non-text contrast where it identifies something.
    #[test]
    fn the_focus_ring_and_the_filled_answer_clear_three_to_one_unit() {
        for ground in ["--base", "--mantle"] {
            let ratio = token_contrast("--focus", ground);
            assert!(ratio >= 3.0, "the focus ring is {ratio:.2}:1 on {ground}");
        }
        for hue in ["--green", "--red", "--yellow", "--blue"] {
            let ratio = token_contrast(hue, "--base");
            assert!(
                ratio >= 3.0,
                "a filled answer in {hue} is {ratio:.2}:1 against the page"
            );
        }
    }

    /// The one stylesheet that ships: the bundle's own, as the browser
    /// receives it, with its layout whitespace taken out.
    ///
    /// Every ADR-046 proof in this module reads it. There was a second one
    /// — the served `CSS` const, inlined into every server-rendered page —
    /// and `t-bf255880` wave 2 deleted it with the pages, so the design
    /// system now has one place to be true of.
    ///
    /// The served copy was authored compact and this one is authored to be
    /// read, so the whitespace between a property and its value is a
    /// formatting choice rather than a design one: it is normalised away
    /// here so that a proof states the declaration it is about rather than
    /// the way the file happens to be laid out. Nothing inside a value is
    /// touched beyond collapsing runs of blanks, which no declaration in
    /// this stylesheet distinguishes.
    fn bundle_stylesheet() -> String {
        let asset = crate::bundle::asset(crate::bundle::STYLESHEET)
            .expect("the bundle's stylesheet is embedded under the name the shell links");
        let raw = String::from_utf8(asset.bytes.to_vec()).expect("the stylesheet is UTF-8");
        let mut compact = String::with_capacity(raw.len());
        let mut blank = false;
        for character in raw.chars() {
            if character.is_whitespace() {
                blank = true;
                continue;
            }
            if blank && !compact.is_empty() {
                let previous = compact.chars().next_back().unwrap_or(' ');
                if !matches!(previous, ':' | ';' | ',' | '{' | '}' | '(' | '>')
                    && !matches!(character, ';' | ',' | '{' | '}' | ')' | '>')
                {
                    compact.push(' ');
                }
            }
            blank = false;
            compact.push(character);
        }
        compact
    }

    /// SPA-05, and the edge's injection point.
    ///
    /// The shell is the smallest document that can load the bundle: it links
    /// the two embedded assets by the names they are embedded under, it
    /// carries the mount point and NOT the application root, and it closes
    /// its head exactly once. That last one is the hax edge's: nginx injects
    /// WebMCP with `sub_filter '</head>'` and `sub_filter_once on`, so one
    /// literal `</head>` means the injection lands exactly once.
    #[test]
    fn the_app_shell_closes_its_head_exactly_once_unit() {
        let shell = app_shell();
        assert_eq!(
            shell.matches("</head>").count(),
            1,
            "the edge injects WebMCP at the literal </head>: {shell}"
        );
        assert!(
            shell.starts_with("<!doctype html><html lang=en><head><meta charset=utf-8>"),
            "{shell}"
        );
        assert!(
            shell.contains("<meta name=viewport content=\"width=device-width,initial-scale=1\">"),
            "{shell}"
        );
        assert!(shell.contains("<title>kanban</title>"), "{shell}");
        assert!(
            shell.contains("<div id=root data-testid=app-root-shell></div>"),
            "{shell}"
        );
        // The shell is not the page: the application root is mounted by the
        // bundle, and a test that waits for the shell must not find it here.
        assert!(
            !shell.contains("data-testid=app-root>") && !shell.contains("\"app-root\""),
            "the shell carries the application root: {shell}"
        );
        assert!(
            shell.contains(&format!(
                "<link rel=stylesheet href=/assets/{}>",
                crate::bundle::STYLESHEET
            )),
            "{shell}"
        );
        assert!(
            shell.contains(&format!(
                "<script type=module src=/assets/{}></script>",
                crate::bundle::SCRIPT
            )),
            "{shell}"
        );
        // Everything it reaches for, it reaches for in this binary.
        for attribute in ["href=", "src="] {
            let mut rest = shell.as_str();
            while let Some(at) = rest.find(attribute) {
                let after = &rest[at + attribute.len()..];
                let value = after
                    .split(|character: char| character == '>' || character.is_whitespace())
                    .next()
                    .unwrap_or("");
                assert!(
                    value.starts_with("/assets/"),
                    "the shell points at {value}, which is not an embedded asset"
                );
                rest = after;
            }
        }
    }

    /// SPA-01 — an asset answers under the name its bytes hash to, and
    /// nothing else answers at all.
    #[test]
    fn an_asset_answers_only_under_its_own_content_hashed_name_unit() {
        assert!(
            !crate::bundle::ASSETS.is_empty(),
            "the binary carries no bundle"
        );
        for asset in crate::bundle::ASSETS {
            let url = format!("/assets/{}", asset.name);
            let Some(WebResponse::Asset(served)) = bundle_response(&url) else {
                panic!("{url} is not served from the embedded table");
            };
            assert_eq!(served.bytes, asset.bytes, "{url} served other bytes");
            let extension = asset.name.rsplit_once('.').expect("an extension").1;
            let expected = match extension {
                "js" => "text/javascript; charset=utf-8",
                "css" => "text/css; charset=utf-8",
                other => panic!("nothing decides the Content-Type of a .{other}"),
            };
            assert_eq!(served.content_type, expected, "{url}");
        }

        // A name that is not in the table is not found: not the nearest
        // file, not a path walked out of the tree, and never the filesystem.
        for miss in [
            "/assets/app.0000000000000000.js",
            "/assets/app.0000000000000000.css",
            "/assets/app.js",
            "/assets/",
            "/assets/../Cargo.toml",
            "/assets/web/dist/app.js",
        ] {
            assert!(
                matches!(bundle_response(miss), Some(WebResponse::Html(404, _))),
                "{miss} was not refused"
            );
        }

        // And the bundle answers exactly two addresses: everything else is
        // the page router's, including a URL that merely starts like one.
        assert!(matches!(
            bundle_response("/app"),
            Some(WebResponse::Html(200, _))
        ));
        for page_route in ["/", "/boards", "/apps", "/app/extra", "/assets"] {
            assert!(
                bundle_response(page_route).is_none(),
                "{page_route} was answered by the bundle"
            );
        }
    }

    /// SPA-03 — the version banner's second line is the embedded bundle.
    ///
    /// Recomputed here from the bytes that are actually in this binary, so
    /// the number the release receipt records and the installer compares
    /// against cannot be a constant somebody typed.
    #[test]
    fn the_version_banner_names_the_embedded_bundle_unit() {
        use sha2::{Digest, Sha256};

        let names: Vec<&str> = crate::bundle::ASSETS
            .iter()
            .map(|asset| asset.name)
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "the asset table is not sorted by name");

        let hex =
            |bytes: &[u8]| -> String { bytes.iter().map(|byte| format!("{byte:02x}")).collect() };
        let mut listing = String::new();
        for asset in crate::bundle::ASSETS {
            listing.push_str(&format!(
                "{}  {}\n",
                hex(&Sha256::digest(asset.bytes)),
                asset.name
            ));
        }
        let recomputed = hex(&Sha256::digest(listing.as_bytes()));
        assert_eq!(
            recomputed,
            crate::bundle::SHA256,
            "the stamped fingerprint is not the embedded bytes'"
        );

        let banner = crate::version_string();
        let lines: Vec<&str> = banner.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "the version banner is not two lines: {banner}"
        );
        assert!(lines[0].starts_with("kanban "), "{banner}");
        assert_eq!(lines[1], format!("bundle {recomputed}"), "{banner}");
    }

    /// WEB-03 — no glyph prefix anywhere.
    #[test]
    fn no_heading_or_link_carries_a_glyph_prefix_unit() {
        let stripped = css_without_comments(&bundle_stylesheet());
        // The headings and link texts are the bundle's now, and the width
        // sweep reads them in Chrome on every route
        // (`no_route_overflows_sideways_at_three_widths_in_real_chrome`).
        // What is still decidable here is that the stylesheet injects no
        // glyph of its own, which is the half a rendered page cannot show.
        for injected in [
            "content:'>'",
            "content:'> '",
            "content:\">\"",
            "content: \">\"",
        ] {
            assert!(!stripped.contains(injected), "{stripped}");
        }
    }

    /// WEB-29 — a board refusal is the board's own sentence.
    #[test]
    fn a_refusal_is_the_boards_sentence_in_red_unit() {
        // The refusal DOCUMENT is a contract: `web/src/api.ts`'s `postForm`
        // reads the board's own sentence out of `.error` and puts it under
        // the control that was pressed, so a refused POST has to carry one.
        let refusal = page(
            "Needs you",
            &format!("<p class=error>{}</p>", escape(&choice_required())),
        );
        assert!(
            refusal.contains("<p class=error>"),
            "the refusal document lost the element the client reads: {refusal}"
        );
        let body = css_rule_body(".error");
        assert!(body.contains("color:var(--red)"), "{body}");
        let declarations = css_declarations(&body);
        for (property, value) in &declarations {
            assert!(
                !property.starts_with("background") && !property.starts_with("border"),
                "a refusal is a sentence, not a box: {property}:{value}"
            );
        }
    }

    /// WEB-44 — every route the server answers is a route the sweep loads,
    /// and every one of them answers the application.
    ///
    /// The sweep is a list of URLs in `tests/e2e.rs`, and a list can fall
    /// behind the match it is a list of. So `ROUTE_SHAPES` is the registry
    /// both read, and this drives `render` with one URL per declared shape:
    /// a shape that is declared and not answered fails here, an arm that
    /// answers something no shape declares fails on the head-segment check
    /// below, and a shape the sweep does not load fails in the sweep.
    #[test]
    fn render_answers_exactly_the_declared_shapes_unit() {
        for shape in ROUTE_SHAPES {
            let url = shape
                .replace("{project}", "SHAPES")
                .replace("{id}", "x-1")
                .replace("{kind}", "task");
            let answered = render(&url).unwrap_or_else(|error| panic!("render {url}: {error}"));
            assert_eq!(
                answered,
                app_shell(),
                "{url} is declared but does not answer the application"
            );
        }
        // And nothing else does: an address no shape declares is the
        // refusal document, which is the only other thing `render` writes.
        for unknown in ["/no-such-page", "/preview/task/SHAPES/t-1", "/a/b/c/d"] {
            let answered = render(unknown).expect("render an unknown address");
            assert_ne!(answered, app_shell(), "{unknown} answered the application");
            assert!(answered.contains("No page at that address"), "{answered}");
        }
        // The match's own leading segments, read off the source, so an arm
        // added without a shape is caught rather than silently mounted.
        let source = include_str!("serve.rs");
        let body = source
            .split_once("fn render(url: &str) -> Result<String> {")
            .expect("render is declared")
            .1
            .split_once("\nfn post(")
            .expect("render ends before post")
            .0;
        let matched = body
            .split_once("match parts.as_slice() {")
            .expect("render matches the path")
            .1
            .split_once("=> Ok(app_shell()),")
            .expect("the mounted arms end")
            .0;
        let mut answered = matched
            .split('|')
            .filter_map(|arm| arm.split_once('"').map(|(_, rest)| rest))
            .filter_map(|rest| rest.split_once('"').map(|(head, _)| head.to_owned()))
            .collect::<Vec<_>>();
        answered.push(String::new());
        answered.sort();
        let mut declared = ROUTE_SHAPES
            .iter()
            .map(|shape| {
                shape
                    .split('/')
                    .find(|segment| !segment.is_empty())
                    .filter(|segment| !segment.starts_with('{'))
                    .unwrap_or("")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        declared.sort();
        declared.dedup();
        answered.dedup();
        assert_eq!(
            answered, declared,
            "render answers a leading segment no shape declares, or the other way about"
        );
    }

    /// WEB-41 — one pill style everywhere.
    ///
    /// The stylesheet is asked what it DECLARES: exactly one `.pill` rule,
    /// status modifiers that only recolour it, and no surviving rule that
    /// makes a tag, a kind, a type or a priority into a second badge. The
    /// markup half went with the renderers — nothing in this crate writes a
    /// badge any more — and a class nothing styles cannot be a second
    /// badge.
    #[test]
    fn exactly_one_pill_style_exists_unit() {
        let pill_rules = css_rules(&bundle_stylesheet())
            .into_iter()
            .filter(|rule| rule.selectors == vec![".pill".to_owned()])
            .collect::<Vec<_>>();
        assert_eq!(
            pill_rules.len(),
            1,
            "the stylesheet declares {} rules that select exactly .pill",
            pill_rules.len()
        );
        let declared = css_declarations(&pill_rules[0].body);
        for (property, value) in [
            ("background", "var(--surface0)"),
            ("font-size", "0.75rem"),
            ("border-radius", "999px"),
        ] {
            assert!(
                declared
                    .iter()
                    .any(|(name, held)| name == property && held == value),
                "the pill does not declare {property}:{value}: {declared:?}"
            );
        }
        for rule in css_rules(&bundle_stylesheet()) {
            if !rule
                .selectors
                .iter()
                .any(|selector| selector.contains(".pill") && selector.contains(".status-"))
            {
                continue;
            }
            for (property, value) in css_declarations(&rule.body) {
                assert_eq!(
                    property, "color",
                    "{:?} declares {property}:{value}, so a status modifier is more than a hue",
                    rule.selectors
                );
            }
        }
        // The baseline's second badges are gone from the stylesheet, and the
        // priority is plain text: no fill, no radius, no border.
        for rule in css_rules(&bundle_stylesheet()) {
            for selector in &rule.selectors {
                let landed = selector.split_whitespace().last().unwrap_or(selector);
                for retired in [".tag", ".kind", ".type"] {
                    assert_ne!(
                        landed, retired,
                        "the stylesheet still styles the retired badge {retired}"
                    );
                }
                if !landed.starts_with(".priority") {
                    continue;
                }
                for (property, value) in css_declarations(&rule.body) {
                    assert!(
                        !(property == "background"
                            || property == "border-radius"
                            || property.starts_with("border-")),
                        "{selector} declares {property}:{value}, so the priority is a pill again"
                    );
                }
            }
        }
    }

    /// WEB-42 — tables lose their borders and keep the hairline.
    #[test]
    fn tables_declare_only_the_row_hairline_unit() {
        let parts = ["table", "th", "td", "th.n", "td.n"];
        for rule in css_rules(&bundle_stylesheet()) {
            if !rule
                .selectors
                .iter()
                .any(|selector| parts.contains(&selector.as_str()))
            {
                continue;
            }
            for (property, value) in css_declarations(&rule.body) {
                if property == "border-collapse" || property == "border-spacing" {
                    continue;
                }
                if !property.starts_with("border") {
                    continue;
                }
                assert!(
                    property == "border-bottom" && value == "1px solid var(--surface0)",
                    "{:?} declares {property}:{value}, which is not the row hairline",
                    rule.selectors
                );
            }
        }
        let cells = css_rule_body("td");
        assert!(
            cells.contains("border-bottom:1px solid var(--surface0)"),
            "a table cell declares no row hairline: {cells}"
        );
        let head = css_rule_body("th");
        assert!(
            head.contains("font-size:0.75rem") && head.contains("color:var(--overlay)"),
            "the header row is not quiet .75rem overlay: {head}"
        );
        for numeric in ["td.n", "th.n"] {
            let body = css_rule_body(numeric);
            assert!(
                body.contains("text-align:right") && body.contains("font-family:var(--mono)"),
                "{numeric} is not a right-aligned mono column: {body}"
            );
        }
    }

    /// WEB-55 — the HTTP surface does not move.
    ///
    /// Two halves, both read off the module: the read routes `render`
    /// answers, held to the registry the width sweep loads, and the four
    /// verbs `post` accepts. A route or a verb added or removed here is a
    /// decision somebody has to make on purpose.
    #[test]
    fn the_route_table_and_the_write_allowlist_are_unchanged_unit() {
        assert_eq!(
            ROUTE_SHAPES,
            [
                "/",
                "/all",
                "/decided",
                "/boards",
                "/sprints",
                "/sprints/{project}",
                "/sprint/{project}/{id}",
                "/plans",
                "/deployments",
                "/subscriptions",
                "/lanes",
                "/search",
                "/board/{project}",
                "/task/{project}/{id}",
                "/deployment/{project}/{id}",
            ],
            "the read surface moved"
        );
        const SOURCE: &str = include_str!("serve.rs");
        let allowlist = SOURCE
            .split_once("    if !matches!(\n        parts.as_slice(),\n")
            .expect("post declares an allowlist")
            .1
            .split_once("    ) {")
            .expect("the allowlist closes")
            .0;
        // Whitespace-normalised so the claim is about the arms rather than
        // about how `rustfmt` wrapped them.
        let verbs = allowlist.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            verbs,
            "[\"attention\", _, _, \"reply\"] | [\"attention\", _, _, \"reopen\"] \
             | [\"attention\", _, _, \"check\"] | [\"plan\", _, _, \"open\"] \
             | [\"subscription\", _, _, \"pause\" | \"resume\"]",
            "the write surface moved"
        );
    }

    const COUNTS_CHILD_TEST: &str = "serve::tests::web_counts_child_process";
    const COUNTS_CHILD_MARKER: &str = "web-counts-child";

    /// WEB-28 — counts read as counts.
    ///
    /// The aggregate is every registered board's open queue, so this one
    /// needs a registry: it runs in a child process with its own data dir,
    /// the way every other registry-backed case in this module does.
    #[test]
    fn counts_read_as_sentences_unit() {
        let data_dir = TempDataDir::new("web-counts");
        let output = spawn_fixture_child(
            data_dir.path(),
            COUNTS_CHILD_TEST,
            "KANBAN_WEB_COUNTS_CHILD",
            COUNTS_CHILD_MARKER,
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "child counts fixture failed\nstdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(
            stdout.contains("test serve::tests::web_counts_child_process ... ok"),
            "child did not execute the ignored counts fixture\n{stdout}"
        );
    }

    /// How many open items the deck's queue holds, read where the deck
    /// reads it: the JSON projection, which is the one place the count the
    /// mounted page renders comes from.
    fn queue_length() -> usize {
        let listing = projection::needs_you().unwrap_or_else(|_| panic!("project the queue"));
        serde_json::to_value(&listing).expect("the listing serialises")["returned"]
            .as_u64()
            .expect("a returned count") as usize
    }

    #[test]
    #[ignore]
    fn web_counts_child_process() {
        let Ok(marker) = env::var("KANBAN_WEB_COUNTS_CHILD") else {
            return;
        };
        if marker != COUNTS_CHILD_MARKER {
            return;
        }
        let mut registry = Registry::open().expect("open registry");
        let mut decided = Vec::new();
        for (board, rows) in [("px", 3), ("atmux", 9)] {
            let project = registry
                .register(None, board, false, "geoyws")
                .expect("register a board");
            let mut store =
                Store::open(&PathBuf::from(&project.board_path)).expect("open the board");
            store.initialize(board, "geoyws").expect("initialize");
            for row in 0..rows {
                let item = store
                    .raise_attention(
                        &format!("{board} row {row} needs a decision."),
                        "decision",
                        "codex@driver",
                        None,
                        0,
                        &[],
                        &DecisionCard::default(),
                        None,
                    )
                    .expect("raise an open item");
                if board == "atmux" {
                    decided.push((project.board_path.clone(), item.id));
                }
            }
        }

        // Twelve open across two boards. Both surfaces that say so — the
        // deck's `12 left` and the list's `12 open across 2 boards` — are
        // rendered by the bundle and read in Chrome; what the SERVER owes
        // them is ONE queue, and that is what is counted here.
        assert_eq!(queue_length(), 12, "the queue is not every board's");
        let list = render("/all").expect("render the open list");
        assert_eq!(list, app_shell(), "/all is not the application shell");
        for data_bearing in ["data-open-count", "open across", "<article class=item"] {
            assert!(
                !list.contains(data_bearing),
                "the shell carries {data_bearing}: {list}"
            );
        }

        // And the queue is the open rows', not the page's: settling the
        // nine atmux rows leaves three.
        for (board_path, id) in &decided {
            let mut store = Store::open(&PathBuf::from(board_path)).expect("reopen the board");
            store
                .resolve_attention(
                    id,
                    OPERATOR_ACTOR,
                    &AttentionAnswer {
                        choice: Some("approve"),
                        outcome: None,
                        note: None,
                    },
                    None,
                )
                .expect("settle an atmux row");
        }
        assert_eq!(queue_length(), 3, "settled rows are still in the queue");

        // Neither document the server still writes reaches a third party
        // (WEB-54): the shell, and the refusal a nonexistent address gets.
        for route in ["/all", "/no/such/page"] {
            let html = render(route).unwrap_or_else(|error| panic!("render {route}: {error}"));
            assert_no_third_party(&html, route);
        }
    }

    /// WEB-54, SPA-57 — nothing the operator loads reaches a third party.
    ///
    /// Three documents are all there are now: the application shell, the
    /// refusal page, and the two assets the shell links. The shell's own
    /// links are same-site by construction and the ASSETS are where a
    /// third-party reference would actually hide — a font, a CDN, an
    /// analytics beacon — so the bundle's bytes are read here too.
    #[test]
    fn the_document_references_no_third_party_unit() {
        assert_no_third_party(&app_shell(), "/");
        assert_no_third_party(
            &page("Not found", "<h1>Not found</h1>"),
            "the refusal document",
        );
        let script = crate::bundle::asset(crate::bundle::SCRIPT).expect("the embedded script");
        let bundled = [
            ("the stylesheet", bundle_stylesheet()),
            (
                "the script",
                String::from_utf8_lossy(script.bytes).into_owned(),
            ),
        ];
        // A fetch, not a mention: React carries XML namespace URIs and the
        // address of its own error index as data, and neither is a request.
        // What would actually leave the estate is a stylesheet import, a
        // font, an image or a script naming an absolute origin.
        for (what, bytes) in bundled {
            for fetched in [
                "url(http",
                "@import \"http",
                "src=\"http",
                "href=\"http",
                "fonts.googleapis",
                "fonts.gstatic",
            ] {
                assert!(
                    !bytes.contains(fetched),
                    "{what} fetches {fetched}, which is outside this estate"
                );
            }
        }
    }

    fn assert_no_third_party(html: &str, route: &str) {
        // The shell links the two embedded assets; anything else that
        // fetches is a third party by another name.
        for forbidden in ["<img ", "<iframe ", "<script src=http"] {
            assert!(
                !html.contains(forbidden),
                "{route} reaches outside the document with {forbidden}"
            );
        }
        for attribute in ["href=", "src="] {
            let mut rest = html;
            while let Some(at) = rest.find(attribute) {
                let after = &rest[at + attribute.len()..];
                let value = if let Some(quoted) = after.strip_prefix('"') {
                    quoted.split('"').next().unwrap_or("")
                } else {
                    after
                        .split(|c: char| c == '>' || c.is_whitespace())
                        .next()
                        .unwrap_or("")
                };
                assert!(
                    value.starts_with('/') || value.starts_with('#'),
                    "{route} points at {value}, which is not same-site"
                );
                rest = after;
            }
        }
    }

    /// The ROUTE's refusal for a half-written custom answer is ONE sentence,
    /// whatever the row, the actor or the missing half.
    ///
    /// It is the wording the CLI and the MCP tool refuse with, it is the page
    /// a no-script POST lands on, and the card quotes it verbatim when the
    /// route says it. So it has to be stable: the refusal is decided before
    /// the id, the actor or either half is read. If that ever stops being
    /// true, this fails rather than the card quoting a sentence that varies
    /// by row.
    #[test]
    fn the_routes_refusal_is_one_sentence_for_every_id_actor_and_missing_half() {
        let sentence = incomplete_answer_refusal("", "", None, None);
        assert_eq!(
            sentence,
            "attention: a custom answer needs --outcome (approve, reject, defer, other) and --note"
        );
        for id in ["", "a-1", "e-88cd75c1", "a-\"><script>"] {
            for actor in ["", OPERATOR_ACTOR, "codex@driver-2"] {
                // The three ways an answer can be incomplete. Both halves
                // present is the recorded answer and has no refusal at all.
                for (outcome, note) in [
                    (None, None),
                    (Some("approve"), None),
                    (None, Some("after the pin lands")),
                ] {
                    assert_eq!(
                        incomplete_answer_refusal(id, actor, outcome, note),
                        sentence,
                        "the refusal varied for id {id:?} actor {actor:?} outcome {outcome:?} note {note:?}"
                    );
                }
            }
        }
    }

    /// WEB-45, SPA-54 — what the operator's phone is promised, read off the
    /// two artefacts that still make the promise: the shell document and
    /// the one stylesheet it links.
    #[test]
    fn operator_shell_keeps_phone_touch_and_live_status_contract() {
        let shell = app_shell();
        assert!(
            shell.contains("width=device-width,initial-scale=1"),
            "{shell}"
        );
        let stylesheet = bundle_stylesheet();
        // 44 CSS px is the floor a thumb needs (spec WEB-45, Apple's HIG
        // minimum), and 2.75rem is that at the root size this page sets.
        assert!(stylesheet.contains("min-height:2.75rem"), "{stylesheet}");
        assert!(!stylesheet.contains("min-height:2.6rem"), "{stylesheet}");
        assert!(
            stylesheet.contains("env(safe-area-inset-bottom)"),
            "{stylesheet}"
        );
        assert!(stylesheet.contains(".attention-count"), "{stylesheet}");
        assert!(stylesheet.contains("max-width:700px"), "{stylesheet}");
        assert!(stylesheet.contains(":focus-visible"), "{stylesheet}");
    }

    #[test]
    fn configured_actor_header_name_follows_http_token_grammar() {
        assert_eq!(
            normalize_actor_header_name("X-Kanban-Actor").unwrap(),
            "X-Kanban-Actor"
        );
        assert_eq!(
            normalize_actor_header_name("x_kanban_actor").unwrap(),
            "x_kanban_actor"
        );
        assert!(normalize_actor_header_name("").is_err());
        assert!(normalize_actor_header_name("X Kanban Actor").is_err());
        assert!(normalize_actor_header_name("X:Kanban:Actor").is_err());
        assert!(normalize_actor_header_name("X-Kanban-Actor ").is_err());
    }

    #[test]
    fn actor_bytes_must_be_present_and_untrimmed() {
        assert_eq!(normalize_actor_bytes(b"ifca-sso").unwrap(), "ifca-sso");
        assert!(normalize_actor_bytes(b"").is_err());
        assert!(normalize_actor_bytes(b" ifca-sso").is_err());
        assert!(normalize_actor_bytes(b"ifca sso").is_err());
        assert!(normalize_actor_bytes(b"ifca-sso\n").is_err());
    }

    #[test]
    fn invalid_actor_header_configuration_fails_closed_at_startup() {
        assert!(ServeConfig::new(Some("X Kanban".to_owned())).is_err());
        let config =
            ServeConfig::new(Some("X-Kanban-Actor".to_owned())).expect("valid actor header name");
        assert_eq!(config.actor_header.as_deref(), Some("X-Kanban-Actor"));
    }

    #[test]
    #[ignore]
    fn serve_render_fixture_child_process() {
        let Ok(marker) = env::var("KANBAN_SERVE_RENDER_CHILD") else {
            return;
        };
        if marker != RENDER_CHILD_MARKER {
            return;
        }
        let data_dir = env::var_os("KANBAN_DATA_DIR")
            .map(PathBuf::from)
            .expect("child data dir");
        // An empty registry has no boards to group sprints by, and the
        // projection says so with an empty listing -- the sentence a
        // reader sees is the mounted page's (`t-bf255880`).
        let no_boards =
            serde_json::to_string(&projection::sprints().expect("project sprints with no boards"))
                .expect("the empty index serialises");
        assert_eq!(
            no_boards, "{\"items\":[],\"returned\":0,\"limit\":null,\"truncated\":false}",
            "the empty sprint index is not empty"
        );

        let fixture = seed_render_fixture(&data_dir);

        // `/` is the mounted application now (`t-1f495a7f`): the route
        // answers the shell and nothing data-bearing, which is SPA-51's
        // whole claim. Everything the CARD owes is asserted on `/all`
        // below, which renders it from the same `decision_card` the deck
        // reads through `/api/v1/needs-you`.
        let home = render("/").expect("render the landing route");
        assert_eq!(home, app_shell(), "/ is not the application shell");
        for data_bearing in [
            "<article class=item",
            "<form class=decide",
            "data-deck-cards",
            "data-testid=app-root>",
            "Please review before release",
        ] {
            assert!(
                !home.contains(data_bearing),
                "the shell carries {data_bearing}: {home}"
            );
        }
        // ...and the query the deck used to be rendered with is the
        // client's to read off the address bar: the same shell answers it.
        let replied = render(&format!("/?replied={}", fixture.epic_id)).expect("render replied");
        assert_eq!(replied, home, "?replied= is a different page from /");

        // `/all` is the mounted list since `t-bf255880` wave 2: the same
        // shell, and the same `/api/v1/needs-you` the deck reads. Every
        // claim this fixture used to make about the served card — the
        // eyebrow, the folded own-words answer, the live submit, the
        // card's own refusal sentence — is made in Chrome on this route by
        // the cases `list_tab` drives, and what the SERVER owes it is the
        // row below.
        for route in [
            "/all".to_owned(),
            format!("/all?replied={}", fixture.epic_id),
        ] {
            let answered = render(&route).unwrap_or_else(|error| panic!("render {route}: {error}"));
            assert_eq!(
                answered,
                app_shell(),
                "{route} is not the application shell"
            );
        }
        let queue = serde_json::to_value(projection::needs_you().expect("project the queue"))
            .expect("the queue serialises");
        let card = queue["items"]
            .as_array()
            .expect("a listing")
            .iter()
            .find(|card| card["board"] == "SERVE-RENDER")
            .unwrap_or_else(|| panic!("the seeded ask is not in the queue: {queue}"));
        assert_eq!(
            card["attention"]["body"], "Please review <strong>before release</strong>.",
            "{queue}"
        );
        let typeset = card["bodyHtml"].as_str().expect("the typeset ask");
        assert!(
            typeset.contains("Please review") && !typeset.contains("<strong>"),
            "the raiser's markup was not neutralised: {typeset}"
        );

        // `/boards`, `/lanes`, `/decided` and `/board/{project}` answer the
        // application shell since `t-bf255880`: what they render is the
        // bundle's, proved in the browser, and what the SERVER owes them is
        // `/api/v1/boards`, `/api/v1/lanes`, `/api/v1/decided` and
        // `/api/v1/board/{project}` (`tests/e2e.rs`).
        for mounted in ["/boards", "/lanes", "/decided", "/board/SERVE-RENDER"] {
            assert_eq!(
                render(mounted).unwrap_or_else(|error| panic!("render {mounted}: {error}")),
                app_shell(),
                "{mounted} is not the application shell"
            );
        }

        // `/plans` is mounted since `t-bf255880`, and what it renders is
        // the projection below. The open control, the banner and the real
        // write are proved in Chrome by
        // `opening_a_draft_plan_in_real_chrome_moves_the_real_task_to_todo`.
        assert_eq!(
            render("/plans").expect("render plans"),
            app_shell(),
            "/plans is not the application shell"
        );
        assert_eq!(
            render(&format!("/plans?opened={}", fixture.epic_id)).expect("render opened plans"),
            app_shell(),
            "?opened= is a different page from /plans"
        );
        let plans = serde_json::to_value(projection::plans().expect("project the plans"))
            .expect("the plans listing serialises");
        let plan = plans["items"]
            .as_array()
            .expect("a listing")
            .iter()
            .find(|card| card["task"]["id"] == fixture.epic_id)
            .unwrap_or_else(|| panic!("the drafted plan is not in the listing: {plans}"));
        // Agent-authored text reaches the client typeset by the one
        // renderer, with its raw HTML inert rather than carried through as
        // markup.
        assert_eq!(
            plan["bodyHtml"], "<p>Draft body with alert(1)</p>\n",
            "{plans}"
        );
        assert_eq!(plan["task"]["title"], "Plan <b>render</b>", "{plans}");
        let children = plan["children"].as_array().expect("the gated rows");
        assert_eq!(children.len(), 3, "{plans}");
        assert!(
            children
                .iter()
                .any(|child| child["task"]["title"] == "Implement <i>escape</i>"),
            "{plans}"
        );

        // `/deployments` and the attempt detail are mounted since
        // `t-bf255880` wave 1, so this reads the projection they render
        // from. Escaping is no longer a property of these bytes: the client
        // builds the DOM, and `markdown_renders_in_real_chrome_and_raw_html_stays_inert`
        // is where agent-authored text is held inert.
        let deployments = serde_json::to_string(
            &crate::projection::deployments().unwrap_or_else(|_| panic!("project deployments")),
        )
        .expect("the deployment index serialises");
        assert!(deployments.contains("geoyws/kanban"), "{deployments}");
        assert!(deployments.contains("build <failed>"), "{deployments}");
        let deployment_detail = serde_json::to_string(
            &crate::projection::deployment("SERVE-RENDER", &fixture.current_deployment_id)
                .unwrap_or_else(|_| panic!("project the attempt")),
        )
        .expect("the attempt serialises");
        assert!(
            deployment_detail.contains(&fixture.current_deployment_id),
            "{deployment_detail}"
        );
        assert!(
            deployment_detail.contains("artifact://kanban/<render>"),
            "{deployment_detail}"
        );
        assert!(
            deployment_detail.contains("served <release> successfully"),
            "{deployment_detail}"
        );

        // Search is the mounted application now (`t-bf255880`), so what the
        // server owes it is the projection rather than markup: the same
        // rows, carrying the operator's own text unescaped, because
        // escaping is the client's once the client builds the DOM.
        let Ok(search_empty) = crate::projection::search("") else {
            panic!("the empty search was refused")
        };
        let search_empty = serde_json::to_string(&search_empty).expect("serialise");
        assert_html_contains(&search_empty, "\"items\":[]");
        assert_html_contains(&search_empty, "\"boards\":[]");

        let Ok(search) = crate::projection::search("render") else {
            panic!("searching the boards was refused")
        };
        let search = serde_json::to_string(&search).expect("serialise");
        assert_html_contains(&search, "\"query\":\"render\"");
        assert_html_contains(&search, "kanban://SERVE-RENDER/task/e-serve-render");
        assert_html_contains(&search, "Plan <b>render</b>");
        assert_html_contains(&search, "Ship <script>render</script>");

        // The task detail is mounted too, and its three rows are read off
        // the projection for the same reason: what the server owes the page
        // is the row's own text and the one typeset body, not markup.
        let detail = |id: &str| {
            let Ok(row) = crate::projection::task("SERVE-RENDER", id) else {
                panic!("the task detail for {id} was refused")
            };
            serde_json::to_value(row).expect("the detail as JSON")
        };
        let task_detail = detail(&fixture.epic_id);
        assert_eq!(task_detail["task"]["title"], "Plan <b>render</b>");
        assert_eq!(task_detail["task"]["tags"], serde_json::json!(["ops"]));
        // The ask this epic is carrying, and its body as the page reads it.
        // The row's own text arrives unescaped — escaping is the client's
        // now — and the typeset copy beside it is the server's `markdown`,
        // which is where the raiser's markup is neutralised.
        assert_eq!(
            task_detail["openAttention"][0]["attention"]["kind"],
            "decision"
        );
        assert_eq!(
            task_detail["openAttention"][0]["attention"]["body"],
            "Please review <strong>before release</strong>."
        );
        let typeset_ask = task_detail["openAttention"][0]["bodyHtml"]
            .as_str()
            .expect("the typeset ask");
        assert!(
            typeset_ask.contains("Please review") && !typeset_ask.contains("<strong>"),
            "the raiser's markup was not neutralised: {typeset_ask}"
        );
        // The note the CLI wrote, carried as text and typeset beside it.
        assert_eq!(
            task_detail["notes"]["items"][0]["note"]["body"],
            "Keep the <script> tag escaped & readable."
        );
        let typeset_note = task_detail["notes"]["items"][0]["bodyHtml"]
            .as_str()
            .expect("the typeset note");
        assert!(
            typeset_note.contains("tag escaped &amp; readable")
                && !typeset_note.contains("<script"),
            "the note's markup reached the page as markup: {typeset_note}"
        );
        assert!(
            task_detail["events"]["returned"].as_u64().unwrap_or(0) > 0,
            "the task detail served no trail: {task_detail}"
        );

        let story_detail = detail(&fixture.story_id);
        assert_eq!(
            story_detail["task"]["title"],
            "Ship <script>render</script>"
        );
        assert_eq!(story_detail["task"]["type"], "story");
        assert!(
            story_detail["bodyHtml"]
                .as_str()
                .expect("the typeset story body")
                .contains("Story body with markup"),
            "{story_detail}"
        );

        let task_page = detail(&fixture.task_id);
        assert_eq!(task_page["task"]["title"], "Implement <i>escape</i>");
        assert_eq!(task_page["task"]["lane"], "driver-2");
        assert!(
            task_page["bodyHtml"]
                .as_str()
                .expect("the typeset task body")
                .contains("Task body with &amp; &lt; &gt;"),
            "the body was not typeset by the server's markdown: {task_page}"
        );

        let failed_detail = serde_json::to_string(
            &crate::projection::deployment("SERVE-RENDER", &fixture.failed_deployment_id)
                .unwrap_or_else(|_| panic!("project the failed attempt")),
        )
        .expect("the failed attempt serialises");
        assert!(
            failed_detail.contains(&fixture.failed_deployment_id),
            "{failed_detail}"
        );
        assert!(failed_detail.contains("failed"), "{failed_detail}");
        assert!(failed_detail.contains("build <failed>"), "{failed_detail}");

        // The three sprint routes are mounted since `t-bf255880`: each
        // answers the shell, and everything the pages used to be asserted
        // on is asserted where it now comes from -- the projection. The
        // rendered result is judged in Chrome, by
        // `mobile_read_navigation_journey_in_real_chrome_reaches_seeded_records`.
        for route in [
            "/sprints",
            "/sprints/SERVE-RENDER",
            "/sprint/SERVE-RENDER/sp-render-current",
        ] {
            let shell = render(route).unwrap_or_else(|error| panic!("render {route}: {error}"));
            assert_eq!(shell, app_shell(), "{route} is not the application shell");
        }

        let sprints = serde_json::to_value(projection::sprints().expect("project every board"))
            .expect("the sprint index serialises");
        let board_of = |name: &str| {
            sprints["items"]
                .as_array()
                .expect("the index is a listing")
                .iter()
                .find(|item| item["board"] == name)
                .unwrap_or_else(|| panic!("{name} is not in the sprint index: {sprints}"))
                .clone()
        };
        let seeded = board_of("SERVE-RENDER");
        let current = &seeded["current"];
        assert_eq!(current["sprint"]["id"], "sp-render-current", "{sprints}");
        assert_eq!(current["sprint"]["targetVersion"], "3.2.0", "{sprints}");
        assert_eq!(current["sprint"]["status"], "current", "{sprints}");
        assert_eq!(current["openTasks"], 1, "{sprints}");
        assert_eq!(current["doneTasks"], 0, "{sprints}");
        let history = seeded["history"].as_array().expect("a history");
        for (id, state) in [
            ("sp-render-planned", "planned"),
            ("sp-render-closed", "closed"),
            ("sp-render-abandoned", "abandoned"),
            ("sp-render-archived", "planned"),
        ] {
            let card = history
                .iter()
                .find(|card| card["sprint"]["id"] == id)
                .unwrap_or_else(|| panic!("{id} is not in the history: {sprints}"));
            assert_eq!(card["sprint"]["status"], state, "{sprints}");
            assert_eq!(
                card["sprint"]["archived"],
                serde_json::Value::Bool(id == "sp-render-archived"),
                "{id} was mislabeled archived: {sprints}"
            );
        }
        // A board between sprints is in the index saying so, rather than
        // absent from it.
        let empty = board_of("SERVE-SPRINT-EMPTY");
        assert!(empty["current"].is_null(), "{sprints}");
        assert!(
            empty["history"].as_array().expect("a history").is_empty(),
            "{sprints}"
        );

        let board_sprints =
            serde_json::to_value(projection::board_sprints("SERVE-RENDER").expect("one board"))
                .expect("one board's sprints serialise");
        assert_eq!(board_sprints, seeded, "one board differs from the index");

        let sprint_value = |id: &str| {
            serde_json::to_value(
                projection::sprint("SERVE-RENDER", id)
                    .unwrap_or_else(|_| panic!("project sprint {id}")),
            )
            .expect("a sprint serialises")
        };
        let current_sprint = sprint_value("sp-render-current");
        assert_eq!(
            current_sprint["goalHtml"], "<p>Current <strong>goal</strong> and criteria.</p>\n",
            "{current_sprint}"
        );
        // The actual start is the stamp `sprint start` wrote, which is
        // neither end of the schedule.
        assert_eq!(
            current_sprint["sprint"]["startsAt"], fixture.current_sprint_started_at,
            "{current_sprint}"
        );
        assert_ne!(current_sprint["sprint"]["startsAt"], 1_700_000_000_000_i64);
        assert_ne!(current_sprint["sprint"]["startsAt"], 4_102_444_800_000_i64);
        assert!(
            current_sprint["sprint"]["endsAt"].is_null(),
            "{current_sprint}"
        );
        let scope = current_sprint["tasks"].as_array().expect("a scope");
        assert_eq!(scope.len(), 1, "{current_sprint}");
        assert_eq!(scope[0]["id"], "t-render/opaque?#", "{current_sprint}");
        assert_eq!(scope[0]["status"], "todo", "{current_sprint}");
        assert_eq!(scope[0]["title"], "Opaque <task>", "{current_sprint}");

        let closed_sprint = sprint_value("sp-render-closed");
        let proof = &closed_sprint["closingDeployment"];
        assert_eq!(proof["servedVersion"], "3.1.0", "{closed_sprint}");
        assert_eq!(
            proof["servedCommit"], "cccccccccccccccccccccccccccccccccccccccc",
            "{closed_sprint}"
        );
        assert_eq!(
            closed_sprint["sprint"]["startsAt"], fixture.closed_sprint_started_at,
            "{closed_sprint}"
        );
        assert_eq!(
            closed_sprint["sprint"]["endsAt"], fixture.closed_sprint_ended_at,
            "{closed_sprint}"
        );

        // A sprint that recorded no goal carries none, rather than a
        // sentence the projection invented for the page.
        assert!(
            sprint_value("sp-render-archived")["goalHtml"].is_null(),
            "an absent goal was filled in by the projection"
        );
        // A closed sprint from before the proof gate has no attempt to
        // point at, and says so by carrying none.
        assert!(
            sprint_value("sp-render-missing-proof")["closingDeployment"].is_null(),
            "a sprint without proof grew one"
        );
        let planned = sprint_value("sp-render-planned");
        assert_eq!(planned["sprint"]["startsAt"], 0, "{planned}");
        assert!(planned["sprint"]["endsAt"].is_null(), "{planned}");
        let abandoned = sprint_value("sp-render-abandoned");
        assert_eq!(abandoned["sprint"]["status"], "abandoned", "{abandoned}");
        assert!(!abandoned["sprint"]["endsAt"].is_null(), "{abandoned}");

        let current_only = serde_json::to_value(
            projection::board_sprints("SERVE-SPRINT-CURRENT-ONLY").expect("one board"),
        )
        .expect("one board's sprints serialise");
        assert!(!current_only["current"].is_null(), "{current_only}");
        assert!(
            current_only["history"]
                .as_array()
                .expect("a history")
                .is_empty(),
            "{current_only}"
        );

        // The refusal moved with the read: the routes answer the shell to
        // everyone, and the projection is where a board or a sprint that
        // cannot be read is denied.
        assert!(projection::board_sprints("NO-SUCH-SPRINT-BOARD").is_err());
        assert!(projection::sprint("SERVE-RENDER", "sp-no-such").is_err());

        let not_found = render("/no/such/page").expect("render 404 page");
        assert_page_title(&not_found, "Not found");
        assert_html_contains(&not_found, "No page at that address");
        // `/board/{project}` answers the shell for every name now, so the
        // refusal it used to render is the projection's: one non-enumerating
        // denial, whatever the reason (SPA-08).
        assert!(matches!(
            crate::projection::board("NO-SUCH-BOARD"),
            Err(crate::projection::Refusal::DeniedOrNotFound)
        ));
        // `/task/{project}/{id}` answers the shell whatever the id is, so
        // the refusal moved with the surface: a row that is not there is
        // the projection's non-enumerating denial (SPA-08).
        assert!(
            matches!(
                crate::projection::task("SERVE-RENDER", "no-such-task"),
                Err(crate::projection::Refusal::DeniedOrNotFound)
            ),
            "a missing row was not refused"
        );
    }

    #[test]
    fn serve_render_fixture_parent_spawns_child_process() {
        let data_dir = TempDataDir::new("render");
        let output = spawn_fixture_child(
            data_dir.path(),
            RENDER_CHILD_TEST,
            "KANBAN_SERVE_RENDER_CHILD",
            RENDER_CHILD_MARKER,
        );
        if !output.status.success() {
            panic!(
                "child render fixture failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("running 1 test"),
            "child did not run exactly one test\n{stdout}"
        );
        assert!(
            stdout.contains("test serve::tests::serve_render_fixture_child_process ... ok"),
            "child did not execute the ignored render fixture\n{stdout}"
        );
    }

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
        // the same audited Store operation as `kb att resolve`, the undo
        // reopens exactly one through the same audited operation as
        // `kb att reopen` (George, 2026-09-11: undo belongs on the page), and
        // the Subscriptions page pauses or resumes exactly one subscription
        // through the same audited operation as `kb subscription pause` --
        // all idempotent-shaped, all actor-stamped. No other web route is
        // allowed a mutator.
        const ALLOWED: [&str; 6] = [
            "move_task",
            "resolve_attention",
            "resolve_attention_from_trusted_edge",
            "reopen_attention",
            "pause_subscription",
            "resume_subscription",
        ];
        let shipped = SOURCE
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .unwrap_or(SOURCE);
        for name in STORE_MUTATORS {
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

    /// Every `&mut self` method on `Store`, which is the complete set of ways
    /// the web layer could change a board. Shared by the two capability
    /// tests below it, so a new mutator is added once.
    const STORE_MUTATORS: [&str; 29] = [
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
        "update_attention",
        "resolve_attention",
        "reopen_attention",
        "create_handoff",
        "accept_handoff",
        "retire_handoff",
        "signoff_story",
        "advance_story",
        "sweep_expired_claims",
        "initialize",
        "start_deployment",
        "finish_deployment",
        "abandon_deployment",
        "add_subscription",
        "pause_subscription",
        "resume_subscription",
    ];

    #[test]
    fn the_projection_reaches_nothing_but_the_store_and_the_registry() {
        // SPA-07: the projection's only data dependency is `Store` (plus the
        // one named `Registry` read the board page already makes for its
        // rules). An HTTP test can show that today's five routes answer
        // correctly; it cannot show that the module has no way to reach past
        // the store, because a query that happens to agree with the store on
        // the day leaves every assertion green and the capability in place.
        // So this reads the module back, the same way
        // `no_page_can_reach_a_method_that_writes` does.
        const SOURCE: &str = include_str!("projection.rs");
        let shipped = SOURCE
            .split_once("#[cfg(test)]")
            .map(|(before, _)| before)
            .unwrap_or(SOURCE);
        for name in STORE_MUTATORS {
            // Assembled rather than written out, so this file's own source
            // does not match itself.
            let call = format!(".{name}(");
            assert!(
                !shipped.contains(call.as_str()),
                "projection.rs calls {name}, which writes. The JSON surface is \
                 read-only (SPA-13); there is no allowlist here because no \
                 projection has a reason to hold one."
            );
        }
        // No second query implementation: no driver, no connection, no SQL.
        for forbidden in [
            "rusqlite",
            "Connection",
            "SELECT",
            "prepare(",
            "query_row",
            "query_map",
            "execute(",
        ] {
            assert!(
                !shipped.contains(forbidden),
                "projection.rs names {forbidden}. ADR-048 §5 binds the JSON \
                 API to the same Store methods the CLI calls; data no method \
                 exposes is a request for a Store method, not a query in the \
                 web layer."
            );
        }
        // And it reaches for nothing else in this crate: the model's records,
        // the store, the registry, and the page router's own board
        // enumeration, which it shares rather than duplicates.
        const REACHABLE: [&str; 4] = [
            "use crate::model::{",
            "use crate::registry::Registry;",
            "use crate::serve::{",
            "use crate::store::Store;",
        ];
        for line in shipped
            .lines()
            .filter(|line| line.starts_with("use crate::"))
        {
            assert!(
                REACHABLE.iter().any(|allowed| line.starts_with(allowed)),
                "projection.rs imports {line}, which is not the store, the \
                 registry, the model or the shared board enumeration."
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

    /// Markdown is the one place a body meets a parser instead of a scalar
    /// escape, so the two properties that make it safe to render agent text
    /// are pinned here: structure renders, and raw HTML never survives.
    #[test]
    fn markdown_renders_structure_strips_raw_html_and_keeps_line_shape() {
        let html = markdown(
            "## Ship it\n\n- **one** receipt\n- two\n\n<script>alert(1)</script>\nplain <b>bold</b>",
        );
        assert!(html.contains("<h2>Ship it</h2>"), "{html}");
        assert!(html.contains("<strong>one</strong>"), "{html}");
        assert!(html.contains("<li>"), "{html}");
        // The tags are gone; the words between them stay as inert text.
        assert!(!html.contains("<script>"), "{html}");
        assert!(!html.contains("<b>"), "{html}");
        assert!(html.contains("plain bold"), "{html}");
        // A single newline keeps its line, the way the pre-wrap body had it.
        assert!(markdown("first\nsecond").contains("<br"), "{html}");
    }

    #[test]
    fn markdown_neutralizes_unsafe_link_schemes_and_keeps_web_ones() {
        let html = markdown(
            "[bad](javascript:alert(1)) and [good](https://example.com) and [rel](/task/P/T)",
        );
        assert!(html.contains("href=\"#\""), "{html}");
        assert!(html.contains("https://example.com"), "{html}");
        assert!(html.contains("href=\"/task/P/T\""), "{html}");
        assert!(!html.contains("javascript"), "{html}");
        assert!(safe_href("#anchor"));
        assert!(safe_href("/task/P/T"));
        assert!(safe_href("mailto:a@b.example"));
        assert!(safe_href("HTTPS://upper.example"));
        assert!(!safe_href("javascript:alert(1)"));
        assert!(!safe_href("data:text/html,x"));
    }

    #[test]
    fn websocket_accept_matches_the_rfc_example() {
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    fn notice_rows(count: usize) -> Vec<NoticeRow<'static>> {
        (0..count)
            .map(|index| NoticeRow {
                seq: 40 + index as i64,
                kind: "attention_raised",
                task: Some("t-1"),
            })
            .collect()
    }

    #[test]
    fn a_batch_is_summarised_only_past_the_lag_threshold() {
        let title = |_: &str| Some("Approve the release".to_owned());
        // Just under, and exactly at: every change arrives as itself.
        for count in [NOTICE_LAG_THRESHOLD - 1, NOTICE_LAG_THRESHOLD] {
            let frames = notice_batch("BOARD", &notice_rows(count), title);
            assert_eq!(frames.len(), count, "{count} rows: {frames:?}");
            for frame in &frames {
                let frame: serde_json::Value = serde_json::from_str(frame).unwrap();
                assert_eq!(frame["type"], "notice", "{frame}");
            }
        }
        // Just over: one sentence, not a flood, and it says how far behind.
        let frames = notice_batch(
            "BOARD",
            &notice_rows(NOTICE_LAG_THRESHOLD + 1),
            // A summarised batch names no row, so it must not read one
            // either: four hundred title lookups to build a sentence that
            // mentions none of them is work done to be thrown away.
            |_: &str| panic!("a summarised batch read a task title"),
        );
        assert_eq!(frames.len(), 1, "{frames:?}");
        let frame: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(frame["type"], "behind");
        assert_eq!(frame["what"], "6 changes while you were away");
        assert_eq!(frame["key"], "BOARD#45", "the key names the row reached");
        assert_eq!(frame["board"], "BOARD");
        assert!(frame.get("task").is_none(), "{frame}");
        // An empty batch is silence, not an empty announcement.
        assert!(notice_batch("BOARD", &[], title).is_empty());
    }

    #[test]
    fn a_notice_carries_the_summary_contract_and_nothing_else() {
        let frame: serde_json::Value = serde_json::from_str(&notice_frame(
            "BOARD",
            &NoticeRow {
                seq: 7,
                kind: "attention_raised",
                task: Some("t-abc"),
            },
            |id| {
                assert_eq!(id, "t-abc");
                Some("Approve \"the\" release\nnow".to_owned())
            },
        ))
        .expect("a task title is arbitrary operator text and must still parse");
        assert_eq!(
            frame.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["board", "key", "task", "title", "type", "what"],
            "the frame grew a field the summary contract excludes: {frame}"
        );
        assert_eq!(frame["type"], "notice");
        assert_eq!(frame["key"], "BOARD#7");
        assert_eq!(frame["what"], "Attention raised");
        assert_eq!(frame["task"], "t-abc");
        assert_eq!(frame["title"], "Approve \"the\" release\nnow");
        assert_eq!(frame["board"], "BOARD");

        // A board-level row names no task, so it claims none. The title is
        // absent rather than empty when the row it named is gone: a blank
        // title reads as a task with no name, which is a different claim.
        let removed: serde_json::Value = serde_json::from_str(&notice_frame(
            "BOARD",
            &NoticeRow {
                seq: 8,
                kind: "task_removed",
                task: Some("t-gone"),
            },
            |_| None,
        ))
        .unwrap();
        assert_eq!(
            removed.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["board", "key", "task", "type", "what"],
            "{removed}"
        );
        let system: serde_json::Value = serde_json::from_str(&notice_frame(
            "BOARD",
            &NoticeRow {
                seq: 9,
                kind: "sitrep_posted",
                task: None,
            },
            |_| panic!("a row with no task looked one up"),
        ))
        .unwrap();
        assert_eq!(
            system.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["board", "key", "type", "what"],
            "{system}"
        );
        assert_eq!(system["what"], "Sitrep posted");
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
    fn actor_header_validation_fails_closed() {
        assert_eq!(
            normalize_actor_header_name("X-Auth-Request-Email").unwrap(),
            "X-Auth-Request-Email"
        );
        assert!(normalize_actor_header_name(" ").is_err());
        assert!(normalize_actor_header_name("X Auth").is_err());
        assert!(normalize_actor_header_name("X-Auth-Request-Email:bad").is_err());

        assert_eq!(
            normalize_actor_bytes(b"ifca-sso").unwrap(),
            "ifca-sso".to_owned()
        );
        assert!(normalize_actor_bytes(b"  ").is_err());
        assert!(normalize_actor_bytes(b"bad actor").is_err());
        assert!(normalize_actor_bytes(b"bad\tactor").is_err());
        assert!(normalize_actor_bytes("x".repeat(MAX_ACTOR_BYTES + 1).as_bytes()).is_err());
        assert!(normalize_actor_bytes(&[0xf0, 0x28, 0x8c, 0x28]).is_err());
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

    const SUBSCRIPTIONS_CHILD_TEST: &str =
        "serve::tests::serve_subscriptions_fixture_child_process";
    const SUBSCRIPTIONS_CHILD_MARKER: &str = "serve-subscriptions-fixture-child";

    struct SubscriptionsFixture {
        board: String,
        board_path: PathBuf,
        active: String,
        dead: String,
        paused: String,
        secret_ref: String,
        head_event_seq: i64,
        acked_seq: i64,
    }

    fn append_watched_event(store: &Store, created_at: i64) {
        crate::audit::append_board_event(
            &store.connection,
            None,
            "checkpoint_added",
            OPERATOR_ACTOR,
            "{}",
            created_at,
        )
        .expect("append a watched board event");
    }

    fn add_watching_subscription(
        store: &mut Store,
        id: &str,
        max_retries: i64,
        secret_ref: Option<&str>,
    ) -> String {
        store
            .add_subscription(AddSubscription {
                id: Some(id.to_owned()),
                subject_task_id: None,
                relations: Vec::new(),
                kinds: vec!["checkpoint_added".to_owned()],
                prior_statuses: Vec::new(),
                current_statuses: Vec::new(),
                tags: Vec::new(),
                consumer_id: "codex.queue".to_owned(),
                action_id: "enqueue-turn".to_owned(),
                timeout_ms: 30_000,
                max_retries,
                rate_per_minute: 60,
                max_concurrency: 1,
                secret_ref: secret_ref.map(str::to_owned),
                actor: OPERATOR_ACTOR.to_owned(),
            })
            .expect("add subscription")
            .id
    }

    /// One materialized delivery, addressed by its position in event order,
    /// so a fixture can settle the second one differently from the first.
    fn delivery_at(store: &Store, subscription_id: &str, index: i64) -> (String, i64, i64) {
        store
            .connection
            .query_row(
                "SELECT event_id,event_seq,next_attempt_at FROM subscription_deliveries \
                 WHERE subscription_id=? ORDER BY event_seq LIMIT 1 OFFSET ?",
                rusqlite::params![subscription_id, index],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("a materialized delivery")
    }

    /// Claim one delivery and settle it: acknowledged when no code is given,
    /// terminally failed with that code when one is.
    fn settle_delivery(
        store: &mut Store,
        subscription_id: &str,
        index: i64,
        failure_code: Option<&str>,
    ) -> i64 {
        let (event_id, event_seq, due_at) = delivery_at(store, subscription_id, index);
        let claimed = store
            .claim_subscription_delivery(subscription_id, &event_id, due_at, 5_000)
            .expect("claim the delivery")
            .expect("a due delivery");
        let settled = match failure_code {
            None => store
                .finalize_subscription_delivery_success(
                    subscription_id,
                    &event_id,
                    &claimed.lease_token,
                    due_at + 1,
                )
                .expect("acknowledge the delivery"),
            Some(code) => store
                .finalize_subscription_delivery_failure(
                    subscription_id,
                    &event_id,
                    &claimed.lease_token,
                    due_at + 1,
                    false,
                    code,
                )
                .expect("fail the delivery"),
        };
        assert!(settled, "the delivery should have settled");
        event_seq
    }

    fn head_event_seq(board_path: &Path) -> i64 {
        Store::open(board_path)
            .expect("open board")
            .connection
            .query_row("SELECT COALESCE(max(seq),0) FROM events", [], |row| {
                row.get(0)
            })
            .expect("read the event head")
    }

    fn subscription_state(board_path: &Path, id: &str) -> Subscription {
        Store::open(board_path)
            .expect("open board")
            .require_subscription(id)
            .expect("the subscription still exists")
    }

    fn subscription_event_count(board_path: &Path, kind: &str, id: &str) -> i64 {
        Store::open(board_path)
            .expect("open board")
            .connection
            .query_row(
                "SELECT COUNT(*) FROM events \
                 WHERE kind=? AND json_extract(payload,'$.subscriptionID')=?",
                [kind, id],
                |row| row.get(0),
            )
            .expect("count subscription events")
    }

    fn seed_subscriptions_fixture(data_dir: &Path) -> SubscriptionsFixture {
        fs::create_dir_all(data_dir).expect("create isolated data dir");
        let mut registry = Registry::open().expect("open registry");
        let board_name = "SERVE-SUBSCRIPTIONS";
        let project = registry
            .register(None, board_name, false, OPERATOR_ACTOR)
            .expect("register rootless board");
        let board_path = PathBuf::from(&project.board_path);
        let mut store = Store::open(&board_path).expect("open store");
        store
            .initialize(board_name, OPERATOR_ACTOR)
            .expect("initialize board metadata");
        // A subscription may only name a kind the board has already recorded,
        // and only events after its own anchor are ever delivered.
        append_watched_event(&store, 10);
        let secret_ref = "serve_subscriptions_token".to_owned();
        let active =
            add_watching_subscription(&mut store, "sub-serve-active", 3, Some(&secret_ref));
        let dead = add_watching_subscription(&mut store, "sub-serve-dead", 0, None);
        let paused = add_watching_subscription(&mut store, "sub-serve-paused", 3, None);
        append_watched_event(&store, 20);
        append_watched_event(&store, 30);
        store
            .materialize_subscriptions()
            .expect("materialize the queued deliveries");
        let acked_seq = settle_delivery(&mut store, &active, 0, None);
        // Zero retries, so each failure is terminal. Two of them, refused two
        // different ways: the page has to keep them apart rather than report
        // one adapter's problem as the whole story.
        settle_delivery(&mut store, &dead, 0, Some("adapter_timeout"));
        settle_delivery(&mut store, &dead, 1, Some("adapter_spawn"));
        store
            .pause_subscription(&paused, OPERATOR_ACTOR)
            .expect("pause the third subscription");
        drop(store);
        SubscriptionsFixture {
            board: board_name.to_owned(),
            head_event_seq: head_event_seq(&board_path),
            board_path,
            active,
            dead,
            paused,
            secret_ref,
            acked_seq,
        }
    }

    fn same_origin_post(path: &str) -> Request {
        TestRequest::new()
            .with_method(Method::Post)
            .with_path(path)
            .with_header(
                Header::from_bytes(&b"Host"[..], &b"kb.test"[..]).expect("a static header"),
            )
            .with_header(
                Header::from_bytes(&b"Origin"[..], &b"http://kb.test"[..])
                    .expect("a static header"),
            )
            .into()
    }

    fn post_redirect(url: &str, config: &ServeConfig) -> String {
        let mut request = same_origin_post(url);
        match post(&mut request, url, config) {
            Ok(WebResponse::Redirect(location)) => location,
            Ok(WebResponse::Html(status, html)) => {
                panic!("expected a redirect from {url}, got {status}: {html}")
            }
            // `post` answers a page or a redirect; the bundle is a GET-only
            // surface, so this arm can only mean the two were crossed.
            Ok(WebResponse::Asset(asset)) => {
                panic!(
                    "posting {url} answered with the bundle asset {}",
                    asset.name
                )
            }
            // `post` never answers JSON either: the JSON surface is
            // dispatched ahead of the POST arm and refuses every method but
            // GET, so a body here would mean the two were crossed.
            Ok(WebResponse::Json(status, body)) => {
                panic!("posting {url} answered JSON {status}: {body}")
            }
            Err(error) => panic!("posting {url} failed: {error}"),
        }
    }

    fn post_status(url: &str, request: &mut Request, config: &ServeConfig) -> u16 {
        match post(request, url, config) {
            Ok(WebResponse::Html(status, _)) => status,
            Ok(WebResponse::Redirect(location)) => {
                panic!("expected a refusal from {url}, got a redirect to {location}")
            }
            Ok(WebResponse::Asset(asset)) => {
                panic!(
                    "posting {url} answered with the bundle asset {}",
                    asset.name
                )
            }
            Ok(WebResponse::Json(status, body)) => {
                panic!("posting {url} answered JSON {status}: {body}")
            }
            Err(error) => panic!("posting {url} failed: {error}"),
        }
    }

    #[test]
    #[ignore]
    fn serve_subscriptions_fixture_child_process() {
        let Ok(marker) = env::var("KANBAN_SERVE_SUBSCRIPTIONS_CHILD") else {
            return;
        };
        if marker != SUBSCRIPTIONS_CHILD_MARKER {
            return;
        }
        let data_dir = env::var_os("KANBAN_DATA_DIR")
            .map(PathBuf::from)
            .expect("child data dir");
        let fixture = seed_subscriptions_fixture(&data_dir);
        let behind = fixture.head_event_seq - fixture.acked_seq;
        assert!(
            behind > 1,
            "the fixture should leave the active subscription measurably behind the head"
        );

        // `/subscriptions` is mounted since `t-bf255880` wave 1, so what
        // this fixture reads is the projection the page renders from. The
        // rendering rules that used to be asserted on these bytes — the
        // paused filter, the two secret sentences, the position sentence and
        // the dead-letter attribution — are asserted in the browser, by
        // `subscription_pause_and_resume_in_real_chrome_persist_each_state`
        // and `subscription_dead_letters_name_their_codes_in_real_chrome`.
        let listed = serde_json::to_string(
            &crate::projection::subscriptions().unwrap_or_else(|_| panic!("project subscriptions")),
        )
        .expect("the projection serialises");
        let projected: serde_json::Value =
            serde_json::from_str(&listed).expect("the projection is JSON");
        let rows = projected["items"].as_array().expect("the rows");
        let row = |id: &str| -> &serde_json::Value {
            rows.iter()
                .find(|row| row["subscription"]["id"] == id)
                .unwrap_or_else(|| panic!("{id} is missing from the projection: {listed}"))
        };
        // A paused row is served whatever a client would filter for: the
        // filter is a display choice, and a hidden row still has to be
        // countable and offerable.
        for id in [&fixture.active, &fixture.dead, &fixture.paused] {
            row(id);
        }
        // The lookup name of a configured secret IS served — an operator
        // needs to know which secret a consumer resolves, and it resolves to
        // nothing in a browser — and the page is the surface that must not
        // repeat it.
        assert_eq!(
            row(&fixture.active)["subscription"]["secretRef"],
            serde_json::Value::String(fixture.secret_ref.clone()),
            "{listed}"
        );
        let active = row(&fixture.active);
        assert_eq!(active["headEventSeq"], fixture.head_event_seq, "{listed}");
        assert_eq!(
            active["position"]["ackedThroughSeq"], fixture.acked_seq,
            "{listed}"
        );
        assert_eq!(
            fixture.head_event_seq - fixture.acked_seq,
            behind,
            "the fixture's own arithmetic moved"
        );
        assert_eq!(
            row(&fixture.dead)["position"]["ackedThroughSeq"],
            serde_json::Value::Null,
            "{listed}"
        );
        // Two terminal failures on the zero-retry subscription, refused two
        // different ways. The count is what the operator notices; the codes
        // are what they act on, and they come out of the delivery rows in the
        // same projection as the count rather than from a second read that
        // could disagree with it.
        let dead = row(&fixture.dead);
        assert_eq!(dead["position"]["deadLetter"], 2, "{listed}");
        assert_eq!(
            dead["deadLetterCodes"]
                .as_array()
                .expect("the codes")
                .iter()
                .map(|code| (
                    code["code"].as_str().expect("a code").to_owned(),
                    code["deliveries"].as_i64().expect("a count")
                ))
                .collect::<Vec<_>>(),
            vec![
                ("adapter_spawn".to_owned(), 1),
                ("adapter_timeout".to_owned(), 1)
            ],
            "{listed}"
        );
        assert!(
            listed.find(&fixture.active) < listed.find(&fixture.dead),
            "rows should follow creation order: {listed}"
        );
        assert_eq!(
            row(&fixture.paused)["subscription"]["pausedBy"],
            serde_json::Value::String(OPERATOR_ACTOR.to_owned()),
            "{listed}"
        );

        let config = ServeConfig::new(None).expect("the default write actor");
        let pause_url = format!("/subscription/{}/{}/pause", fixture.board, fixture.active);
        let pause_location = format!("/subscriptions?show=all&changed={}", fixture.active);
        assert_eq!(post_redirect(&pause_url, &config), pause_location);
        let paused_once = subscription_state(&fixture.board_path, &fixture.active);
        assert_eq!(paused_once.status, "paused");
        assert_eq!(paused_once.paused_by.as_deref(), Some(OPERATOR_ACTOR));
        let paused_at = paused_once.paused_at.expect("a pause stamp");
        assert_eq!(
            subscription_event_count(&fixture.board_path, "subscription_paused", &fixture.active),
            1
        );

        // The same POST again: a no-op that still lands on the page, with no
        // second event and no re-stamped pause.
        assert_eq!(post_redirect(&pause_url, &config), pause_location);
        let paused_twice = subscription_state(&fixture.board_path, &fixture.active);
        assert_eq!(paused_twice.status, "paused");
        assert_eq!(paused_twice.paused_at, Some(paused_at));
        assert_eq!(paused_twice.paused_by.as_deref(), Some(OPERATOR_ACTOR));
        assert_eq!(
            subscription_event_count(&fixture.board_path, "subscription_paused", &fixture.active),
            1
        );

        let resume_url = format!("/subscription/{}/{}/resume", fixture.board, fixture.active);
        assert_eq!(
            post_redirect(&resume_url, &config),
            format!("/subscriptions?changed={}", fixture.active)
        );
        let resumed = subscription_state(&fixture.board_path, &fixture.active);
        assert_eq!(resumed.status, "active");
        assert_eq!(resumed.paused_at, None);
        assert_eq!(resumed.paused_by, None);

        // Resuming from the paused-inclusive view keeps that filter, and
        // resuming twice is as idempotent as pausing twice.
        let resume_all = format!("{resume_url}?show=all");
        assert_eq!(
            post_redirect(&resume_all, &config),
            format!("/subscriptions?show=all&changed={}", fixture.active)
        );
        assert_eq!(
            subscription_event_count(&fixture.board_path, "subscription_resumed", &fixture.active),
            1
        );

        let mut foreign = TestRequest::new()
            .with_method(Method::Post)
            .with_path(&pause_url)
            .with_header(Header::from_bytes(&b"Host"[..], &b"kb.test"[..]).expect("static"))
            .with_header(
                Header::from_bytes(&b"Origin"[..], &b"http://elsewhere.test"[..]).expect("static"),
            )
            .into();
        assert_eq!(post_status(&pause_url, &mut foreign, &config), 403);
        let unmoved = subscription_state(&fixture.board_path, &fixture.active);
        assert_eq!(unmoved.status, "active");

        let unknown = format!("/subscription/{}/{}/delete", fixture.board, fixture.active);
        let mut request = same_origin_post(&unknown);
        assert_eq!(post_status(&unknown, &mut request, &config), 404);

        let missing_board = format!("/subscription/NO-SUCH-BOARD/{}/pause", fixture.active);
        let mut request = same_origin_post(&missing_board);
        assert_eq!(post_status(&missing_board, &mut request, &config), 404);

        let missing_row = format!("/subscription/{}/sub-not-here/pause", fixture.board);
        let mut request = same_origin_post(&missing_row);
        assert_eq!(post_status(&missing_row, &mut request, &config), 409);

        // The `?changed=` receipt is the mounted page's now, and
        // `subscription_pause_and_resume_in_real_chrome_persist_each_state`
        // reads it in the browser after a real write.
    }

    #[test]
    fn serve_subscriptions_fixture_parent_spawns_child_process() {
        let data_dir = TempDataDir::new("subscriptions");
        let output = spawn_fixture_child(
            data_dir.path(),
            SUBSCRIPTIONS_CHILD_TEST,
            "KANBAN_SERVE_SUBSCRIPTIONS_CHILD",
            SUBSCRIPTIONS_CHILD_MARKER,
        );
        if !output.status.success() {
            panic!(
                "child subscriptions fixture failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("test serve::tests::serve_subscriptions_fixture_child_process ... ok"),
            "child did not execute the ignored subscriptions fixture\n{stdout}"
        );
    }

    // ---------------------------------------------------------- the authz seam
    //
    // Authorization is the SERVING PROCESS's, never the caller's: it is minted
    // in `routing::board_authz` from the effective UID, the two-way passwd
    // check and the principal's grants in the canonical registry, and it is
    // decided in `authz.rs`. A request reaches exactly two things here —
    // `ServeConfig::actor_for_write`, which picks the AUDIT actor, and
    // `same_origin`, which is CSRF defence — and neither can produce
    // authority. These tests pin that separation at the one write a browser
    // can reach, stating the principal's authority explicitly through
    // `Store::open_with_authz` the way the store's tenancy tests do.

    /// The tag every seeded row carries, so a principal holding board scope
    /// alone is still one scope short of the row.
    const AUTHZ_TAG: &str = "alpha";

    /// The one refusal the guard produces, byte-identical for an invisible and
    /// an absent row.
    const AUTHZ_DENIAL: &str = "denied or not found";

    struct AuthzBoard {
        /// Held for its `Drop`: the board file lives in this directory.
        _dir: TempDataDir,
        path: PathBuf,
        board_id: String,
        attention: Vec<String>,
    }

    /// A board file carrying `rows` open attention items, each tagged
    /// [`AUTHZ_TAG`], seeded through the DIRECT estate — the only estate that
    /// can create rows before any principal exists.
    fn seed_authz_board(label: &str, board_id: &str, rows: usize) -> AuthzBoard {
        let dir = TempDataDir::new(label);
        let path = dir.path().join(format!("{board_id}.db"));
        let mut store = Store::open(&path).expect("open the seed board");
        store
            .initialize(label, "seed")
            .expect("initialize the seed board");
        store
            .add_tag(AUTHZ_TAG, None, Some("seed"))
            .expect("register the row tag");
        let attention = (0..rows)
            .map(|index| {
                store
                    .raise_attention(
                        &format!("needs a decision ({index})"),
                        "decision",
                        "seed",
                        None,
                        0,
                        &[AUTHZ_TAG.to_owned()],
                        &DecisionCard::default(),
                        None,
                    )
                    .expect("raise an open item")
                    .id
            })
            .collect();
        drop(store);
        AuthzBoard {
            _dir: dir,
            path,
            board_id: board_id.to_owned(),
            attention,
        }
    }

    /// Open the seeded board under a managed principal holding exactly
    /// `grants` — the shape `routing::board_authz` mints from the kernel,
    /// supplied here so a test can say what the principal holds.
    fn managed_store(board: &AuthzBoard, grants: Vec<(ScopeTuple, Capability)>) -> Store {
        Store::open_with_authz(
            &board.path,
            AuthzContext::new(
                Enforcement::Managed,
                authority(grants),
                board.board_id.clone(),
            ),
        )
        .expect("open the board under the stated authority")
    }

    /// Board write and nothing else: every seeded row carries a tag this
    /// principal was never granted, so every seeded row is outside it.
    fn board_scope_only(board: &AuthzBoard) -> Vec<(ScopeTuple, Capability)> {
        vec![(
            ScopeTuple::Board {
                board_id: board.board_id.clone(),
            },
            Capability::Write,
        )]
    }

    /// Board and tag write: the seeded rows are inside this principal.
    fn board_and_tag_scope(board: &AuthzBoard) -> Vec<(ScopeTuple, Capability)> {
        vec![
            (
                ScopeTuple::Board {
                    board_id: board.board_id.clone(),
                },
                Capability::Write,
            ),
            (
                ScopeTuple::BoardTag {
                    board_id: board.board_id.clone(),
                    tag: AUTHZ_TAG.to_owned(),
                },
                Capability::Write,
            ),
        ]
    }

    #[test]
    fn sprint_projection_counts_and_proof_obey_tag_visibility() {
        let dir = TempDataDir::new("sprint-authz");
        let path = dir.path().join("sprint-authz.db");
        let board_id = "aaaaaaaa-8888-4888-8888-888888888888";
        let mut direct = Store::open(&path).expect("open sprint auth fixture");
        direct
            .initialize("SPRINT-AUTHZ", "seed")
            .expect("initialize sprint auth fixture");
        for tag in ["visible", "hidden"] {
            direct
                .add_tag(tag, None, Some("seed"))
                .expect("register sprint task tag");
        }
        let mut add = |id: &str, status: &str, tag: &str| {
            direct
                .add_task(AddTask {
                    id: Some(id.to_owned()),
                    task_type: "task".to_owned(),
                    parent_id: None,
                    title: format!("{tag} {status}"),
                    body: None,
                    assignee: None,
                    lane: None,
                    deliverable: None,
                    stale_minutes: None,
                    driver_only: false,
                    status: status.to_owned(),
                    priority: 2,
                    dependencies: vec![],
                    metadata: serde_json::json!({}),
                    actor: Some("seed".to_owned()),
                    tags: vec![tag.to_owned()],
                    allowed_models: vec![],
                })
                .expect("add tagged sprint task")
                .id
        };
        let visible_open = add("t-visible-open", "todo", "visible");
        let visible_done = add("t-visible-done", "done", "visible");
        let hidden_open = add("t-hidden-open", "todo", "hidden");
        let hidden_done = add("t-hidden-done", "done", "hidden");

        let sprint = direct
            .create_sprint(NewSprint {
                id: Some("sp-authz".to_owned()),
                title: "Scoped sprint".to_owned(),
                body: None,
                target_version: "4.0.0".to_owned(),
                scheduled_start: 0,
                scheduled_end: 4_102_444_800_000,
                actor: "seed".to_owned(),
            })
            .expect("create scoped sprint");
        direct
            .plan_sprint(
                &sprint.id,
                "Only visible scope contributes.",
                &[
                    visible_open.clone(),
                    visible_done.clone(),
                    hidden_open.clone(),
                    hidden_done.clone(),
                ],
                None,
                false,
                "seed",
            )
            .expect("plan scoped sprint");
        direct
            .start_sprint(&sprint.id, "seed")
            .expect("start scoped sprint");
        let hidden_proof = direct
            .start_deployment(StartDeployment {
                task_id: Some(hidden_open.clone()),
                repo: "geoyws/kanban".to_owned(),
                identity: DeployIdentity::Git(
                    "dddddddddddddddddddddddddddddddddddddddd".to_owned(),
                ),
                deployer_checkout: None,
                branch: None,
                tier: "@_p".to_owned(),
                environment: "production".to_owned(),
                host: "hidden-host".to_owned(),
                url: "https://hidden.invalid".to_owned(),
                mechanism: None,
                operation_id: None,
                retry_of: None,
                actor: "seed".to_owned(),
                lane: None,
                sprint_id: Some(sprint.id.clone()),
            })
            .expect("seed hidden proof subject")
            .deployment;
        drop(direct);

        let managed = Store::open_with_authz(
            &path,
            AuthzContext::new(
                Enforcement::Managed,
                authority(vec![
                    (
                        ScopeTuple::Board {
                            board_id: board_id.to_owned(),
                        },
                        Capability::Write,
                    ),
                    (
                        ScopeTuple::BoardTag {
                            board_id: board_id.to_owned(),
                            tag: "visible".to_owned(),
                        },
                        Capability::Write,
                    ),
                ]),
                board_id.to_owned(),
            ),
        )
        .expect("open scoped sprint projection");
        let row = managed
            .require_sprint(&sprint.id)
            .expect("read board sprint");
        // The counts the sprint card carries are the store's, taken under
        // this principal's authority: `projection::sprint` reads them
        // through `sprint_task_counts`, so a row this principal may not see
        // must not be counted for it.
        let (open, done) = managed
            .sprint_task_counts(&row.id)
            .expect("count the visible scope");
        assert_eq!((open, done), (1, 1), "the counts leaked a hidden row");
        let visible = managed
            .sprint_tasks(&sprint.id)
            .expect("read visible sprint tasks");
        assert_eq!(
            visible
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            [visible_open.as_str(), visible_done.as_str()],
        );
        let denial = managed
            .require_deployment(&hidden_proof.id)
            .expect_err("hidden proof subject must not become a link")
            .to_string();
        assert_eq!(denial, AUTHZ_DENIAL);
    }

    fn same_origin_post_with_header(path: &str, name: &str, value: &str) -> Request {
        TestRequest::new()
            .with_method(Method::Post)
            .with_path(path)
            .with_header(
                Header::from_bytes(&b"Host"[..], &b"kb.test"[..]).expect("a static header"),
            )
            .with_header(
                Header::from_bytes(&b"Origin"[..], &b"http://kb.test"[..])
                    .expect("a static header"),
            )
            .with_header(
                Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("a test header"),
            )
            .into()
    }

    /// **A header cannot grant.** `--actor-header` threads one edge value into
    /// the AUDIT actor and nowhere else, so the capability set of a write stays
    /// the serving principal's whatever the request claims. Every claim — the
    /// SSO identity, the operator's own name, `root` — is refused while the
    /// principal is one scope short of the row, and the identical claim lands
    /// once the PRINCIPAL holds that scope.
    #[test]
    fn actor_header_value_never_widens_the_serving_principals_authority() {
        let claims = ["ifca-sso", OPERATOR_ACTOR, "root", "admin@edge.test"];
        let board = seed_authz_board(
            "authz-header",
            "eeeeeeee-5555-4555-8555-555555555555",
            claims.len() + 1,
        );
        let config =
            ServeConfig::new(Some("X-Kanban-Actor".to_owned())).expect("a valid header name");

        for (index, claimed) in claims.iter().enumerate() {
            let url = format!("/attention/AUTHZ-HEADER/{}/reply", board.attention[index]);
            let request = same_origin_post_with_header(&url, "X-Kanban-Actor", claimed);
            let actor = config
                .actor_for_write(&request)
                .expect("the edge value becomes the audit actor");
            assert_eq!(
                &actor, claimed,
                "the header value must reach the audit actor verbatim"
            );
            let mut store = managed_store(&board, board_scope_only(&board));
            let error = store
                .resolve_attention_from_trusted_edge(
                    &board.attention[index],
                    &actor,
                    &AttentionAnswer::custom("other", "done"),
                    None,
                )
                .expect_err("a header value must not grant a scope the principal lacks");
            assert_eq!(
                error.to_string(),
                AUTHZ_DENIAL,
                "claiming {claimed} in a request header must not widen the authority map"
            );
        }

        // Nothing partly applied: every seeded row is still open.
        let direct = Store::open(&board.path).expect("reopen the board directly");
        let open = direct
            .attention(Some("open"), None, None, None, None, 100, false)
            .expect("read the rows directly");
        assert_eq!(
            open.len(),
            board.attention.len(),
            "a refused write must leave every row open"
        );

        // The same rejected claim, on the same request shape, permitted now —
        // and permitted because the PRINCIPAL gained the scope, not because
        // the request changed at all.
        let mut granted = managed_store(&board, board_and_tag_scope(&board));
        let resolved = granted
            .resolve_attention_from_trusted_edge(
                board.attention.last().expect("a spare row"),
                "ifca-sso",
                &AttentionAnswer::custom("other", "done"),
                None,
            )
            .expect("the principal's own authority permits this write");
        assert_eq!(
            resolved.status, "resolved",
            "the write the authority map permits must land"
        );
    }

    /// **Same-origin authorizes nothing.** A perfectly same-origin POST,
    /// carrying the default loopback audit actor, still fails closed when the
    /// serving principal is short of the board scope, and again when it is
    /// short of only the row's tag. Passing the CSRF gate says the form came
    /// from this site; it says nothing about capability.
    #[test]
    fn same_origin_post_is_csrf_defence_and_grants_no_capability() {
        let board = seed_authz_board(
            "authz-same-origin",
            "ffffffff-6666-4666-8666-666666666666",
            1,
        );
        let config = ServeConfig::new(None).expect("the default write actor");
        let url = format!("/attention/AUTHZ-ORIGIN/{}/reply", board.attention[0]);
        let request = same_origin_post(&url);
        assert!(
            same_origin(&request),
            "the fixture request must pass the CSRF gate"
        );
        let actor = config
            .actor_for_write(&request)
            .expect("the default audit actor");
        assert_eq!(
            actor, OPERATOR_ACTOR,
            "with no --actor-header the audit actor is the loopback operator"
        );

        // Short of the board scope entirely.
        let mut nothing = managed_store(&board, vec![]);
        assert_eq!(
            nothing
                .resolve_attention_from_trusted_edge(
                    &board.attention[0],
                    &actor,
                    &AttentionAnswer::custom("other", "done"),
                    None,
                )
                .expect_err("same-origin must not authorize a principal holding nothing")
                .to_string(),
            AUTHZ_DENIAL,
        );
        // Short of only the row's tag.
        let mut board_scope = managed_store(&board, board_scope_only(&board));
        assert_eq!(
            board_scope
                .resolve_attention_from_trusted_edge(
                    &board.attention[0],
                    &actor,
                    &AttentionAnswer::custom("other", "done"),
                    None,
                )
                .expect_err("same-origin must not authorize past the tag scope")
                .to_string(),
            AUTHZ_DENIAL,
        );

        let direct = Store::open(&board.path).expect("reopen the board directly");
        let rows = direct
            .attention(None, None, None, None, None, 10, false)
            .expect("read the row directly");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].status, "open",
            "a refused write must not settle the row"
        );
        assert_eq!(
            rows[0].resolved_by, None,
            "a refused write must record no resolver"
        );
    }

    /// **Audit identity and authorization principal are separately
    /// observable.** A permitted write records the edge identity verbatim on
    /// the row and in the ledger event, while what permitted it was the
    /// authority map: the SAME identity is refused once the map is short of
    /// the row's tag, and a DIFFERENT identity is permitted once it is not.
    /// The guard reads the map and never the identity.
    #[test]
    fn audit_identity_is_recorded_while_the_authorization_principal_decides() {
        let board = seed_authz_board("authz-audit", "aaaaaaaa-7777-4777-8777-777777777777", 2);
        let config =
            ServeConfig::new(Some("X-Auth-Request-Email".to_owned())).expect("a valid header name");
        let url = format!("/attention/AUTHZ-AUDIT/{}/reply", board.attention[0]);
        let request = same_origin_post_with_header(&url, "X-Auth-Request-Email", "sso@edge.test");
        let actor = config
            .actor_for_write(&request)
            .expect("the edge identity becomes the audit actor");
        assert_eq!(actor, "sso@edge.test");

        let mut granted = managed_store(&board, board_and_tag_scope(&board));
        let resolved = granted
            .resolve_attention_from_trusted_edge(
                &board.attention[0],
                &actor,
                &AttentionAnswer::custom("other", "done"),
                None,
            )
            .expect("the principal holds this row");
        assert_eq!(
            resolved.resolved_by.as_deref(),
            Some(actor.as_str()),
            "the row must record the audit identity verbatim"
        );
        let direct = Store::open(&board.path).expect("reopen the board directly");
        let events = direct
            .events(None, Some("attention_resolved"), 10, true)
            .expect("read the ledger directly");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].actor.as_deref(),
            Some(actor.as_str()),
            "the ledger event must name the audit identity"
        );

        // The same audit identity, refused on the second row: the guard read
        // the authority map, so the identity decided neither call.
        let mut deprived = managed_store(&board, board_scope_only(&board));
        assert_eq!(
            deprived
                .resolve_attention_from_trusted_edge(
                    &board.attention[1],
                    &actor,
                    &AttentionAnswer::custom("other", "done"),
                    None,
                )
                .expect_err("an audit identity cannot authorize anything")
                .to_string(),
            AUTHZ_DENIAL,
        );
        // A different audit identity on that same row, permitted, because the
        // map covers it.
        let mut granted_again = managed_store(&board, board_and_tag_scope(&board));
        let second = granted_again
            .resolve_attention_from_trusted_edge(
                &board.attention[1],
                "other@edge.test",
                &AttentionAnswer::custom("other", "done"),
                None,
            )
            .expect("the principal holds this row");
        assert_eq!(
            second.resolved_by.as_deref(),
            Some("other@edge.test"),
            "the second audit identity must be recorded verbatim too"
        );
    }
}
