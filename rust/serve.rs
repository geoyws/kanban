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
    ATTENTION_OUTCOMES, Attention, AttentionAnswer, AttentionChoice, CUSTOM_CHOICE, DeadLetterCode,
    DeploymentAttempt, IDENTITY_MODE_ARTIFACT, OPERATOR_ACTOR, ProjectRecord, SearchOptions,
    Sitrep, Sprint, Subscription, SubscriptionPosition, Task,
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

/// Dispatch one `/api/v1` request.
///
/// The five reads of the first wave; every other path under the prefix gets
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
        ["search"] => encode(projection::search(
            query_value(query, "q").as_deref().unwrap_or(""),
        )),
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
    "/preview/{kind}/{project}/{id}",
    "/preview/board/{project}",
    "/board/{project}",
    "/task/{project}/{id}",
    "/deployment/{project}/{id}",
];

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
        // The Needs-you deck is the mounted application (`t-1f495a7f`): the
        // route answers the shell, the bundle mounts the deck into it, and
        // the data comes from `/api/v1/needs-you` rather than from a second
        // rendering of the same queue here. `?replied=` and `?undone=` are
        // the same page and are read off the address bar by the client, so
        // they need no arm of their own — which is why this arm ignores the
        // query it used to interpolate.
        //
        // Every other route below is still server-rendered until
        // `t-bf255880`, and `/all` is still the plain list this deck was
        // laid over: `decision_card` and its renderers stay exactly where
        // they are.
        [] => Ok(app_shell()),
        ["all"] => all_open(
            query_value(query, "replied").as_deref(),
            query_value(query, "undone").as_deref(),
        ),
        ["decided"] => Ok(app_shell()),
        ["boards"] => Ok(app_shell()),
        ["sprints"] => Ok(app_shell()),
        ["sprints", _] => Ok(app_shell()),
        ["sprint", _, _] => Ok(app_shell()),
        ["plans"] => Ok(app_shell()),
        ["deployments"] => Ok(app_shell()),
        ["subscriptions"] => Ok(app_shell()),
        ["lanes"] => Ok(app_shell()),
        ["search"] => Ok(app_shell()),
        // `/preview` + the item path: `/preview/task/PREVIEW/t-1`. The kind
        // leads, exactly as it does in the path being previewed, so the page
        // script derives the URL with one prefix and nothing else.
        [
            "preview",
            kind @ ("task" | "attention" | "deployment"),
            project,
            id,
        ] => preview_page(project, kind, id),
        ["preview", "board", project] => preview_page(project, "board", project),
        ["board", _project] => Ok(app_shell()),
        ["task", _project, _id] => Ok(app_shell()),
        ["deployment", _project, _id] => Ok(app_shell()),
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
    if let Err(error) = store.resolve_attention_from_trusted_edge(id, &actor, &answer) {
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

fn task_reference(project: &str, store: &Store, task_id: &str) -> String {
    // Every reference link previews on hover and opens in its own tab
    // (data-ref), so an id met mid-sentence answers "what is this?" without
    // leaving the page. Previews inside previews carry the same attribute,
    // which is what makes the nesting work.
    match store.require_task(task_id) {
        Ok(task) => format!(
            "the <span data-task-type>{ty}</span> \
             <a href=\"/task/{project}/{task_id}\" data-task-link=\"{task_id}\" \
             data-ref target=_blank rel=noopener>{title}</a>",
            project = escape(&url_encode(project)),
            task_id = escape(&url_encode(&task.id)),
            title = escape(&task.title),
            ty = escape(&task.task_type),
        ),
        Err(_) => format!(
            "<a href=\"/task/{project}/{task_id}\" data-task-link=\"{task_id}\" \
             data-ref target=_blank rel=noopener>{task_id}</a>",
            project = escape(&url_encode(project)),
            task_id = escape(&url_encode(task_id)),
        ),
    }
}

fn attention_count_badge(count: usize) -> String {
    if count == 0 {
        String::new()
    } else {
        format!(" <span class=attention-count>{count} open attention</span>")
    }
}

/// The same count as a clause inside a row's one sentence (WEB-40): it needs
/// its own connector, or a row with no tags reads `at P0 3 open attention`.
fn attention_clause(count: usize) -> String {
    if count == 0 {
        String::new()
    } else {
        format!(", with{}", attention_count_badge(count))
    }
}

fn attention_section(project: &str, title: &str, items: &[Attention]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut html = format!(
        "<h2>{title} <span class=count>{}</span></h2><ul class=rows>",
        items.len()
    );
    for item in items {
        html.push_str("<li>");
        // The row is a title and one sentence (WEB-40): what the item asks
        // leads it, and everything else about it -- what kind of ask, who
        // raised it, when, at what priority, under which tags, about which
        // row -- reads as the one meta sentence under it.
        html.push_str(&format!(
            "<span class=title>{question}</span>\
             <p class=meta>{article} {kind} ask, raised by {who} {age} \
             at {priority}{tags}{about}</p>",
            question = escape(&card_question(item)),
            article = a_or_an(&item.kind.replace('_', " ")),
            kind = escape(&item.kind.replace('_', " ")),
            priority = priority_badge(item.priority, item.priority_level.as_deref()),
            who = escape(&item.raised_by),
            age = ago(item.created_at),
            tags = tag_list(&item.tags),
            about = item
                .task_id
                .as_ref()
                .map(|task_id| format!(
                    ", about <a href=\"/task/{project}/{task_url}\" \
                     data-ref target=_blank rel=noopener>{task_id}</a>",
                    project = escape(&url_encode(project)),
                    task_url = escape(&url_encode(task_id)),
                    task_id = escape(task_id),
                ))
                .unwrap_or_default(),
        ));
        html.push_str(&format!(
            "<div class=\"body md\">{}</div>",
            markdown(&item.body)
        ));
        html.push_str("</li>");
    }
    html.push_str("</ul>");
    html
}

// ---------------------------------------------------------------- the screens

/// The open queue: every waiting item paired with the board that raised it,
/// and the stores those boards were read from, because every card resolves
/// its own task reference.
type OpenQueue = (
    Vec<(String, Attention)>,
    std::collections::BTreeMap<String, Store>,
);

/// Every open item across every board, priority first and then oldest, so
/// interrupts lead while age remains the tie-breaker that prevents
/// starvation within a level.
fn open_attention() -> Result<OpenQueue> {
    let mut items: Vec<(String, Attention)> = Vec::new();
    let mut stores = std::collections::BTreeMap::new();
    for (project, store) in projects()? {
        let name = project.name.clone();
        for item in store.attention(
            Some("open"),
            None,
            None,
            None,
            None,
            OPEN_ATTENTION_ROWS,
            false,
        )? {
            items.push((name.clone(), item));
        }
        stores.insert(name, store);
    }
    sort_open_queue(&mut items);
    Ok((items, stores))
}

/// Deck order, in one place: priority first, then oldest, then id, then
/// board.
///
/// The page and the JSON projection order the cross-board queue by calling
/// this, rather than by each holding a comparator that has to be kept in
/// step. Two orderings that agree today is a coincidence with a maintenance
/// schedule.
/// The decisions room's order: newest decided first, then id, then board so
/// the list never flickers between two rows settled in the same
/// millisecond.
///
/// Shared with the JSON projection (`rust/projection.rs`'s `decided`) rather
/// than copied: which decision is newest is one question, and two surfaces
/// that answered it differently would put the Undo on different rows.
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

/// The one card loop both open-item screens render: the deck at `/` and the
/// plain list at `/all` are the same cards in the same order, so there is
/// one renderer and the deck is presentation laid over it.
fn open_cards(
    items: &[(String, Attention)],
    stores: &std::collections::BTreeMap<String, Store>,
) -> String {
    let mut html = String::new();
    for (project, item) in items {
        let store = stores
            .get(project)
            .expect("project store map built from same iterator");
        html.push_str(&decision_card(project, store, item));
    }
    html
}

/// What a no-script POST comes back to: the redirect carries the id it
/// settled or reopened, and both open-item screens say so.
fn reply_notices(replied: Option<&str>, undone: Option<&str>) -> String {
    let mut html = String::new();
    if let Some(id) = replied {
        html.push_str(&format!(
            "<p class=success><code>{}</code> is decided and the board has it.</p>",
            escape(id)
        ));
    }
    if let Some(id) = undone {
        html.push_str(&format!(
            "<p class=success>Brought back <code>{}</code> - it is open again \
             and back at its place in this list.</p>",
            escape(id)
        ));
    }
    html
}

/// Nothing waiting, in the words both open-item screens use for it: an
/// answer rather than an absence, and the way on to what was decided.
const EMPTY_QUEUE: &str = "<div class=empty-queue>\
                           <p class=empty>Nothing is waiting. \
                           Every question an agent raised has an answer.</p>\
                           <p><a href=\"/decided\">See what was decided</a></p></div>";

/// The keyboard map, in ONE quiet line, because the digits live on the
/// buttons: a second badge for every key was a keyboard hint competing with
/// the answers it describes.
const LIST_KEYS: &str = "<p class=keys>1–4 answer · u undo · c own</p>";

// The deck itself is no longer rendered here: `/` answers the application
// shell and the bundle mounts the deck (`t-1f495a7f`, spec SPA-51). What
// stayed behind is everything the deck was laid OVER — `open_attention`,
// `open_cards`, `decision_card`, `EMPTY_QUEUE`, `reply_notices` — because
// `/all` still serves the plain list from exactly those pieces, and the
// mounted deck reads the same queue through `/api/v1/needs-you`.

/// Every open item as one plain list, which is what this page was before the
/// deck and what the deck is laid over: the same cards in the same order, one
/// after another for a reader who wants the whole queue at once.
fn all_open(replied: Option<&str>, undone: Option<&str>) -> Result<String> {
    let (items, stores) = open_attention()?;
    let mut html = String::from("<div class=heading><h1>Needs you</h1></div>");
    html.push_str(
        "<p class=explain>Each card is one question an agent is waiting on. Pick an answer \
         and it is recorded on the board at once; the agent continues from there. Undo any \
         decision from Recent decisions.</p>",
    );
    html.push_str(&reply_notices(replied, undone));
    if items.is_empty() {
        html.push_str(EMPTY_QUEUE);
        return Ok(page("Needs you", &html));
    }
    let boards = items
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    html.push_str(&format!(
        "<p class=count><span data-open-count>{}</span> open across {boards} {plural}</p>",
        items.len(),
        plural = if boards == 1 { "board" } else { "boards" },
    ));
    html.push_str(&open_cards(&items, &stores));
    html.push_str(LIST_KEYS);
    Ok(page("Needs you", &html))
}

/// Recent decisions: what was decided, newest first, each one undoable.
///
/// Deciding is one keypress, so the list it leaves behind has to be readable
/// at the same speed: the question, the verdict in the ledger's own words, the
/// note when one rode along, and one Undo. The rows carry the same
/// `data-item`/`data-project` contract the receipts do, so `u` and the undo
/// button land on the same code path both places.
///
/// The previous decision is NOT re-shown from the row: a reopen clears it
/// (ADR-042 §3), so what is rendered here is whatever the row settled with,
/// read once while it was still resolved.
// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn decided_page(undone: Option<&str>) -> Result<String> {
    let mut items: Vec<(String, Attention)> = Vec::new();
    let mut stores = std::collections::BTreeMap::new();
    for (project, store) in projects()? {
        let name = project.name.clone();
        for item in store.recent_resolved_attention(DECIDED_SCAN)? {
            items.push((name.clone(), item));
        }
        stores.insert(name, store);
    }
    sort_decided_queue(&mut items);
    items.truncate(usize::try_from(DECIDED_ROWS).unwrap_or(usize::MAX));

    let mut html = String::from("<div class=heading><h1>Recent decisions</h1></div>");
    if let Some(id) = undone {
        html.push_str(&format!(
            "<p class=success>Brought back <code>{}</code> - it is open again \
             on <a href=\"/\">Needs you</a>.</p>",
            escape(id)
        ));
    }
    if items.is_empty() {
        html.push_str("<p class=empty>Nothing decided yet.</p>");
        return Ok(page("Recent decisions", &html));
    }
    html.push_str(&format!(
        "<p class=count>Newest {} shown. Undoing one puts it back on Needs you.</p>",
        items.len(),
    ));
    for (project, item) in &items {
        let store = stores
            .get(project)
            .expect("project store map built from same iterator");
        html.push_str(&decided_row(project, store, item));
    }
    html.push_str("<p class=keys>u undo</p>");
    Ok(page("Recent decisions", &html))
}

/// One decided row: the question, the decision in the words the ledger keeps,
/// and the undo. `tabindex=0` is what makes `u` able to aim at a row.
fn decided_row(project: &str, store: &Store, item: &Attention) -> String {
    let id = escape(&item.id);
    let id_url = escape(&url_encode(&item.id));
    let project_url = escape(&url_encode(project));
    let decision = decision_words(item);
    let mut html = format!(
        "<article class=decided tabindex=0 data-item=\"{id}\" data-project=\"{project}\" \
         aria-labelledby=\"d-{id}\"><h2 id=\"d-{id}\">{question}</h2>",
        project = escape(project),
        question = escape(&card_question(item)),
    );
    html.push_str(&format!(
        "<p class=decision><span class=\"pill status-{outcome}\">{outcome}</span> {words}</p>",
        outcome = escape(
            item.decision
                .as_ref()
                .map(|d| d.outcome.as_str())
                .unwrap_or("other")
        ),
        words = escape(&decision),
    ));
    if let Some(decision) = &item.decision
        && let Some(note) = &decision.note
    {
        html.push_str(&format!("<p class=note>{}</p>", escape(note)));
    }
    // One sentence, not a chain: who decided it, when, and where it lives.
    html.push_str(&format!(
        "<p class=meta>{who} decided this {when} on \
         <a href=\"/board/{project_url}\" data-ref target=_blank rel=noopener>{project}</a>\
         {about}, {priority}</p>",
        project = escape(project),
        who = escape(item.resolved_by.as_deref().unwrap_or("someone")),
        when = item
            .resolved_at
            .map(ago)
            .unwrap_or_else(|| "at some point".to_owned()),
        about = item
            .task_id
            .as_ref()
            .map(|task| format!(", about {}", task_reference(project, store, task)))
            .unwrap_or_default(),
        priority = priority_badge(item.priority, item.priority_level.as_deref()),
    ));
    html.push_str(&format!(
        "<form class=undo method=post action=\"/attention/{project_url}/{id_url}/reopen\">\
         <input type=hidden name=back value=\"/decided\">\
         <button type=submit class=undo-button data-undo>Undo - open it again</button></form>",
    ));
    html.push_str("</article>");
    html
}

/// What the decision was, in the words the ledger keeps for it: the chosen
/// label, or the custom answer with its verdict. Falls back to the composed
/// resolution for a row settled before decisions were recorded, so a pre-2026
/// decision still reads as itself rather than as nothing.
fn decision_words(item: &Attention) -> String {
    if let Some(decision) = &item.decision {
        if decision.choice == CUSTOM_CHOICE {
            return format!("Your own answer, recorded as {}.", decision.outcome);
        }
        if let Some(choice) = item
            .choices
            .iter()
            .find(|choice| choice.key == decision.choice)
        {
            return format!("{}. {}", choice.label, choice.consequence);
        }
    }
    item.resolution
        .clone()
        .unwrap_or_else(|| "Resolved.".to_owned())
}

/// One item as the card geoyws decides from (ADR-042 §5).
///
/// The order is the whole point, and it is the reverse of what this page used
/// to render: the question, then what is true and what waiting costs, then
/// the answers with the recommendation first — so `1` is always the
/// recommendation. The body is the long form and is folded, and the meta line
/// that used to lead now trails, because a priority pill is not a decision.
/// A row that authored no card is this same card with the default pair and
/// its first body line as the question, not a second template.
///
/// One line sits ABOVE the question: which board asked, what kind of ask it
/// is, who is waiting and for how long (George, 2026-09-17: "I don't know
/// what everything is doing"). It is orientation rather than a decision, so
/// it is small, dim and carries no priority pill — that stays in the trailing
/// meta, where it cannot be mistaken for an answer.
///
/// The free-text answer is FOLDED. It is the rarest path and it was the
/// loudest thing under the choices: a verdict picker, a submit, a release and
/// a hint standing open on every card in a list of 133, all of it asking to
/// be read before the one-click answer above it could be trusted.
///
/// One reply field serves the whole card, and it sits with the choices rather
/// than inside the free-text answer: whatever is written in it rides with
/// whichever choice is clicked, and the free-text answer is that same field
/// plus a verdict. Two fields would be two drafts to lose. That is why the
/// field stays OUTSIDE the fold while the verdict picker goes into it.
fn decision_card(project: &str, store: &Store, item: &Attention) -> String {
    let id = escape(&item.id);
    let id_url = escape(&url_encode(&item.id));
    let project_url = escape(&url_encode(project));
    let mut html = format!(
        "<article class=item tabindex=0 data-testid=deck-card data-item=\"{id}\" \
         data-project=\"{project}\" aria-labelledby=\"q-{id}\">\
         <p class=eyebrow>{who} asked on \
         <a href=\"/board/{project_url}\" data-ref target=_blank rel=noopener>{project}</a>, \
         {age}</p>\
         <h2 id=\"q-{id}\">{question}</h2>",
        project = escape(project),
        age = ago(item.created_at),
        who = escape(&item.raised_by),
        question = escape(&card_question(item)),
    );
    if let Some(context) = &item.context {
        html.push_str(&format!("<p class=context>{}</p>", escape(context)));
    }
    let mut recommended = String::new();
    let mut alternatives = String::new();
    for (index, choice) in ordered_choices(item).iter().enumerate() {
        let rendered = format!(
            "<button type=submit class=\"choice outcome-{outcome}\" name=decision \
             value=\"{key}\" data-label=\"{label}\" data-testid=\"deck-choice-{key}\">\
             <span class=key>{digit}</span>{label}</button>\
             <p class=consequence>{consequence}</p>",
            outcome = escape(&choice.outcome),
            key = escape(&choice.key),
            label = escape(&choice.label),
            consequence = escape(&choice.consequence),
            digit = index + 1,
        );
        if choice.recommended {
            recommended = format!(
                "<fieldset class=recommended data-testid=deck-recommended>\
                 <legend>Recommended</legend>{rendered}</fieldset>"
            );
        } else {
            alternatives.push_str(&format!("<div class=alternative>{rendered}</div>"));
        }
    }
    html.push_str(&format!(
        "<form class=decide data-testid=deck-panel method=post \
         action=\"/attention/{project_url}/{id_url}/reply\">\
         {recommended}"
    ));
    if !alternatives.is_empty() {
        html.push_str(&format!(
            "<div class=alternatives data-testid=deck-answers>{alternatives}</div>"
        ));
    }
    html.push_str(&format!(
        "<div class=reply><label for=\"answer-{id_url}\">Add a note</label>\
         <textarea id=\"answer-{id_url}\" name=reply maxlength={max} \
         data-testid=deck-note></textarea></div>\
         <details class=custom data-custom data-testid=deck-custom>\
         <summary>Answer in my own words</summary>\
         <fieldset class=outcomes>\
         <legend>recorded as</legend><div class=picks>{picks}</div>\
         </fieldset>\
         <div class=actions>\
         <button type=submit class=record name=decision value=custom \
         data-testid=deck-record>Record my answer</button>\
         <button type=button class=clear data-clear data-testid=deck-clear hidden>\
         Clear verdict</button>\
         <p class=hint data-hint data-testid=deck-hint>Pick a verdict and write your \
         reply above.</p>\
         </div></details></form>",
        picks = ATTENTION_OUTCOMES
            .iter()
            .map(|outcome| format!(
                "<label for=\"outcome-{id_url}-{outcome}\">\
                 <input type=radio id=\"outcome-{id_url}-{outcome}\" name=outcome \
                 data-testid=\"deck-outcome-{outcome}\" value={outcome}>{outcome}</label>"
            ))
            .collect::<String>(),
        max = MAX_REPLY_BYTES,
    ));
    html.push_str(&format!(
        "<details class=full data-testid=deck-full><summary>show the full item</summary>\
         <div class=\"body md\" data-testid=deck-body>{}</div></details>",
        markdown(&item.body)
    ));
    // The trailing meta reads as a sentence, and the priority is the only
    // thing in it that is not prose: `about` is where the work is, and the
    // tags are what it was filed under.
    html.push_str(&format!(
        "<p class=meta>{priority}{about}{tags}</p></article>",
        priority = priority_badge(item.priority, item.priority_level.as_deref()),
        about = item
            .task_id
            .as_ref()
            .map(|task| format!(", about {}", task_reference(project, store, task)))
            .unwrap_or_default(),
        tags = tag_list(&item.tags),
    ));
    html
}

/// What the card asks, which for a row that authored no question is the first
/// line of its body — the sentence a raiser puts the verdict in — bounded to
/// the same 160 characters a question is bounded to.
fn card_question(item: &Attention) -> String {
    if let Some(question) = &item.question {
        return question.clone();
    }
    let first = item.body.lines().next().unwrap_or("").trim();
    if first.chars().count() <= 160 {
        return first.to_owned();
    }
    format!("{}…", first.chars().take(159).collect::<String>())
}

/// The choices in the order the card lists them: the recommendation first,
/// then the alternatives as the raiser declared them.
///
/// The order is what the keyboard numbers, so it is decided once here rather
/// than in the renderer and again in the script.
fn ordered_choices(item: &Attention) -> Vec<&AttentionChoice> {
    let mut ordered = Vec::with_capacity(item.choices.len());
    ordered.extend(item.choices.iter().filter(|choice| choice.recommended));
    ordered.extend(item.choices.iter().filter(|choice| !choice.recommended));
    ordered
}

/// One reference answered on hover: what the item is, in one glance.
///
/// The page script fetches this fragment for every `a[data-ref]` anchor, and
/// the anchors the fragment itself renders carry `data-ref` too — which is
/// what makes previews nest. It is a fragment, not a page: a whole document
/// inside a document would bring a second socket and a second copy of the
/// keyboard map into being behind the operator's back.
fn preview_page(project: &str, kind: &str, id: &str) -> Result<String> {
    let (record, store) = project_named(project)?;
    let name = record.name.as_str();
    let body = match kind {
        "task" => task_preview(name, &store, id)?,
        "attention" => attention_preview(name, &store, id)?,
        "deployment" => deployment_preview(name, &store, id)?,
        "board" if id == name => board_preview(name, &store)?,
        _ => return Ok(String::from("<p class=meta>Nothing to preview here.</p>")),
    };
    Ok(format!(
        "<div class=preview-card data-preview-card>{body}</div>"
    ))
}

fn task_preview(project: &str, store: &Store, id: &str) -> Result<String> {
    let task = store.require_task(id)?;
    let mut html = format!(
        "<h3>{title}</h3><p class=meta>{article} {ty} in \
         <span class=\"pill status-{state}\">{status}</span> \
         at {priority}, updated {when}{lane}{parent}</p>",
        title = escape(&task.title),
        article = a_or_an(&task.task_type),
        state = escape(&task.status),
        status = escape(&status_label(&task.status)),
        ty = escape(&task.task_type),
        priority = priority_badge(task.priority, task.priority_level.as_deref()),
        when = ago(task.updated_at),
        lane = task
            .lane
            .as_ref()
            .map(|lane| format!(" in lane {}", escape(lane)))
            .unwrap_or_default(),
        parent = task
            .parent_id
            .as_ref()
            .map(|parent| format!(", part of {}", task_reference(project, store, parent)))
            .unwrap_or_default(),
    );
    if let Some(body) = &task.body {
        html.push_str(&format!(
            "<div class=\"body md\">{}</div>",
            markdown(&excerpt(body, PREVIEW_BODY_CHARS))
        ));
    }
    let open = task_attention_count(store, &task.id)?;
    if open > 0 {
        html.push_str(&format!(
            "<p class=meta>{open} open attention - it is on Needs you.</p>"
        ));
    }
    Ok(html)
}

fn attention_preview(project: &str, store: &Store, id: &str) -> Result<String> {
    // No single-row getter exists and none is added for a hover: the listing
    // is bounded, indexed by id and already the read path every page uses.
    let item = store
        .attention(None, None, None, None, None, 500, false)?
        .into_iter()
        .find(|item| item.id == id)
        .ok_or_else(|| anyhow::anyhow!("attention {id} not found"))?;
    let mut html = format!("<h3>{}</h3>", escape(&card_question(&item)));
    if let Some(context) = &item.context {
        html.push_str(&format!("<p class=meta>{}</p>", escape(context)));
    }
    let state = if item.status == "resolved" {
        format!("decided: {}", escape(&decision_words(&item)))
    } else {
        let choices = ordered_choices(&item)
            .iter()
            .map(|choice| {
                format!(
                    "{}{}",
                    if choice.recommended { "*" } else { "" },
                    escape(&choice.label)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("open - {choices}")
    };
    html.push_str(&format!(
        "<p class=meta>{article} {kind} ask, {state}</p>",
        article = a_or_an(&item.kind.replace('_', " ")),
        kind = escape(&item.kind.replace('_', " ")),
    ));
    if let Some(task) = &item.task_id {
        html.push_str(&format!(
            "<p class=meta>about {}</p>",
            task_reference(project, store, task)
        ));
    }
    Ok(html)
}

fn deployment_preview(project: &str, store: &Store, id: &str) -> Result<String> {
    let row = store.require_deployment(id)?;
    Ok(format!(
        "<h3><code>{id}</code></h3><p class=meta>{repo} on the {tier} tier, \
         {environment} on {host}, {status} {when}, from board {board}</p>",
        id = escape(&row.id),
        repo = escape(&row.repo),
        tier = escape(&row.tier),
        environment = escape(&row.environment),
        host = escape(&row.host),
        status = escape(&row.status),
        when = escape(&ago(row.updated_at)),
        board = escape(project),
    ))
}

fn board_preview(project: &str, store: &Store) -> Result<String> {
    let tasks = store.list_tasks(None, None, None, None, false)?;
    let count = |status: &str| tasks.iter().filter(|task| task.status == status).count();
    let open_attention = store.count_open_attention()?;
    Ok(format!(
        "<h3>{project}</h3><p class=meta>{attention} open attention, {todo} to do, \
         {doing} in progress, {total} tasks in all</p>",
        project = escape(project),
        attention = open_attention,
        todo = count("todo"),
        doing = count("in_progress"),
        total = tasks.len(),
    ))
}

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
    results.extend(search::search_rules(&registry.rules(false)?, &options));
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

/// Cross-board retrieval for people who should not need to know which board
/// owns a fact before they can find it. Ranking and bounds are the same shared
/// implementation used by the CLI and MCP tool.
// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn search_page(query: &str) -> Result<String> {
    let query = query.trim();
    let mut html = format!(
        "<h1>Search</h1><form class=search-page action=/search method=get>\
         <input name=q value=\"{}\" aria-label=\"Search Kanban\" \
         placeholder=\"Task, decision, handoff, rule…\" autofocus>\
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
    let receipt = search_receipt(query)?;
    html.push_str(&format!(
        "<p class=count>{} result{} across {} board{}, model <code>{}</code>{}</p>",
        receipt.results.len(),
        if receipt.results.len() == 1 { "" } else { "s" },
        receipt.boards.len(),
        if receipt.boards.len() == 1 { "" } else { "s" },
        escape(&receipt.embedding_model),
        if receipt.truncated { ", bounded" } else { "" },
    ));
    if receipt.results.is_empty() {
        html.push_str("<p class=empty>No matching Kanban knowledge.</p>");
    }
    for result in receipt.results {
        let title = if let Some(task_id) = &result.task_id {
            format!(
                "<a href=\"/task/{0}/{1}\" data-task-link=\"{1}\" \
                 data-ref target=_blank rel=noopener>{2}</a>",
                escape(&result.board),
                escape(task_id),
                escape(&result.title)
            )
        } else {
            escape(&result.title)
        };
        html.push_str(&format!(
            "<article class=search-result><h2>{title}</h2>\
             <p class=meta>{article} {kind} on {board}, scoring {score:.3}{status}{lane}{tags}</p>\
             <p class=body>{snippet}</p><p class=citation><code>{citation}</code></p></article>",
            title = title,
            article = a_or_an(&result.source_kind.replace('_', " ")),
            board = escape(&result.board),
            kind = escape(&result.source_kind.replace('_', " ")),
            score = result.score,
            status = result
                .status
                .as_ref()
                .map(|status| format!(", {}", escape(&status.replace('_', " "))))
                .unwrap_or_default(),
            lane = result
                .lane
                .as_ref()
                .map(|lane| format!(", in lane {}", escape(lane)))
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

fn deployment_link(project: &str, deployment: &DeploymentAttempt) -> String {
    format!(
        "<a href=\"/deployment/{0}/{1}\" data-deployment-link=\"{2}\" \
         data-ref target=_blank rel=noopener><code>{2}</code></a>",
        escape(&url_encode(project)),
        escape(&url_encode(&deployment.id)),
        escape(&deployment.id),
    )
}

/// What one attempt's build-commit cell says.
///
/// A Git attempt shows the first twelve characters of its commit, as it
/// always has. An artifact-identity attempt shows the words, never a blank
/// and never a truncated `unknown` that could be mistaken for a short SHA
/// (ADR-043 §4).
fn build_commit_cell(deployment: &DeploymentAttempt) -> String {
    if deployment.identity_mode == IDENTITY_MODE_ARTIFACT {
        format!(
            "<span class=meta>{}</span>",
            escape(&deployment.build_commit_label)
        )
    } else {
        format!("<code>{}</code>", escape(&deployment.build_commit[..12]))
    }
}

/// Current releases and the attempts that still need operational attention.
// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn deployments() -> Result<String> {
    let mut current = Vec::new();
    let mut active = Vec::new();
    let mut failures = Vec::new();
    for (project, store) in projects()? {
        current.extend(
            store
                .current_deployments()?
                .into_iter()
                .map(|row| (project.name.clone(), row)),
        );
        active.extend(
            store
                .deployments(Some("started"), None, false, 100)?
                .into_iter()
                .map(|row| (project.name.clone(), row)),
        );
        for status in ["failed", "abandoned"] {
            failures.extend(
                store
                    .deployments(Some(status), None, false, 30)?
                    .into_iter()
                    .map(|row| (project.name.clone(), row)),
            );
        }
    }
    current.sort_by(|a, b| {
        (&a.1.repo, &a.1.tier, &a.1.environment, &a.0).cmp(&(
            &b.1.repo,
            &b.1.tier,
            &b.1.environment,
            &b.0,
        ))
    });
    active.sort_by_key(|(_, row)| std::cmp::Reverse(row.created_at));
    failures.sort_by_key(|(_, row)| std::cmp::Reverse(row.created_at));

    let mut html = String::from(
        "<div class=heading><h1>Deployments</h1></div>\
         <p class=meta>Verified current releases, derived from immutable attempts. Old non-current terminal attempts self-archive from hot views, and <code>kb deploy list --all</code> still reaches them.</p>",
    );
    html.push_str("<h2>Current releases</h2>");
    if current.is_empty() {
        html.push_str("<p class=empty>No verified release has been recorded yet.</p>");
    } else {
        html.push_str("<table><thead><tr><th>Repository</th><th>Tier</th><th>Environment</th><th>Commit</th><th>Host</th><th>Attempt</th><th>Verified</th></tr></thead><tbody>");
        for (project, row) in &current {
            html.push_str(&format!(
                "<tr><td>{repo}<div class=meta>{project}</div></td><td><code>{tier}</code></td><td>{environment}</td><td>{commit}</td><td>{host}</td><td>{attempt}</td><td class=when>{when}</td></tr>",
                repo = escape(&row.repo), project = escape(project), tier = escape(&row.tier),
                environment = escape(&row.environment), commit = build_commit_cell(row),
                host = escape(&row.host), attempt = deployment_link(project, row),
                when = escape(&ago(row.completed_at.unwrap_or(row.updated_at))),
            ));
        }
        html.push_str("</tbody></table>");
    }
    html.push_str("<h2>In progress</h2>");
    if active.is_empty() {
        html.push_str("<p class=empty>No deployment is currently in progress.</p>");
    }
    for (project, row) in &active {
        html.push_str(&format!(
            "<article class=item><p>{attempt} <strong>{repo}</strong> to the <code>{tier}</code> {environment}</p><p class=meta>{commit} on {host}, started {when} by {actor}</p></article>",
            attempt = deployment_link(project, row), repo = escape(&row.repo), tier = escape(&row.tier),
            environment = escape(&row.environment), commit = build_commit_cell(row),
            host = escape(&row.host), when = escape(&ago(row.created_at)), actor = escape(&row.actor),
        ));
    }
    html.push_str("<h2>Recent failures</h2>");
    if failures.is_empty() {
        html.push_str("<p class=empty>No failed or abandoned attempt is in the hot window.</p>");
    }
    for (project, row) in failures.iter().take(30) {
        html.push_str(&format!(
            "<article class=item><p>{attempt} <strong>{repo}</strong> <span class=\"pill status-{status}\">{status}</span></p><p class=meta>The {tier} tier, {environment}, in phase {phase} {when}</p><p class=body>{receipt}</p></article>",
            attempt = deployment_link(project, row), repo = escape(&row.repo), status = escape(&row.status),
            tier = escape(&row.tier), environment = escape(&row.environment),
            phase = escape(row.phase.as_deref().unwrap_or("unknown")), when = escape(&ago(row.updated_at)),
            receipt = escape(row.receipt.as_deref().unwrap_or("No receipt recorded.")),
        ));
    }
    Ok(page("Deployments", &html))
}

// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn deployment_detail(project: &str, id: &str) -> Result<String> {
    let (_, store) = project_named(project)?;
    let row = store.require_deployment(id)?;
    let task = row
        .task_id
        .as_ref()
        .map(|task| {
            format!(
                "<a href=\"/task/{}/{}\" data-ref target=_blank rel=noopener>{}</a>",
                escape(&url_encode(project)),
                escape(&url_encode(task)),
                escape(task)
            )
        })
        .unwrap_or_else(|| "—".to_owned());
    let fields = [
        ("Board", escape(project)),
        ("Status", escape(&row.status)),
        ("Repository", escape(&row.repo)),
        ("Identity mode", escape(&row.identity_mode)),
        (
            "Build commit",
            if row.identity_mode == IDENTITY_MODE_ARTIFACT {
                format!(
                    "<span class=meta>{}</span>",
                    escape(&row.build_commit_label)
                )
            } else {
                format!("<code>{}</code>", escape(&row.build_commit))
            },
        ),
        (
            "Deployer checkout",
            row.deployer_checkout
                .as_ref()
                .map(|value| format!("<code>{}</code>", escape(value)))
                .unwrap_or_else(|| "—".to_owned()),
        ),
        ("Branch", escape(row.branch.as_deref().unwrap_or("—"))),
        ("Tier", escape(&row.tier)),
        ("Environment", escape(&row.environment)),
        ("Host", escape(&row.host)),
        ("URL", escape(&row.url)),
        ("Task", task),
        ("Actor", escape(&row.actor)),
        ("Lane", escape(row.lane.as_deref().unwrap_or("—"))),
        ("Mechanism", escape(row.mechanism.as_deref().unwrap_or("—"))),
        ("Retry of", escape(row.retry_of.as_deref().unwrap_or("—"))),
        ("Phase", escape(row.phase.as_deref().unwrap_or("—"))),
        (
            "Served commit",
            match (&row.served_commit, row.identity_mode.as_str()) {
                (Some(value), _) => format!("<code>{}</code>", escape(value)),
                (None, IDENTITY_MODE_ARTIFACT) => {
                    "not applicable - proved by artifact identity".to_owned()
                }
                (None, _) => "—".to_owned(),
            },
        ),
        ("Started", escape(&stamp(row.created_at))),
        (
            "Completed",
            row.completed_at
                .map(|value| escape(&stamp(value)))
                .unwrap_or_else(|| "—".to_owned()),
        ),
        (
            "Archived",
            if row.archived {
                "yes".to_owned()
            } else {
                "no".to_owned()
            },
        ),
    ];
    let mut html = format!(
        "<h1 data-deployment-detail=\"{0}\">Deployment <code>{0}</code></h1><dl>",
        escape(&row.id)
    );
    for (label, value) in fields {
        html.push_str(&format!(
            "<dt>{0}</dt><dd data-deployment-field=\"{0}\">{1}</dd>",
            escape(label),
            value
        ));
    }
    html.push_str("</dl>");
    if !row.artifacts.is_empty() {
        html.push_str(
            "<h2>Artifact identities</h2><table><thead><tr><th>Role</th><th>Kind</th>\
             <th>Expected</th><th>Observed</th></tr></thead><tbody>",
        );
        for artifact in &row.artifacts {
            html.push_str(&format!(
                "<tr><td>{role}</td><td>{kind}</td><td><code>{expected}</code></td><td>{observed}</td></tr>",
                role = escape(&artifact.role),
                kind = escape(&artifact.kind),
                expected = escape(&artifact.expected),
                observed = artifact
                    .observed
                    .as_ref()
                    .map(|value| format!("<code>{}</code>", escape(value)))
                    .unwrap_or_else(|| "not yet observed".to_owned()),
            ));
        }
        html.push_str("</tbody></table>");
    }
    html.push_str("<h2>Receipt</h2>");
    html.push_str(&format!(
        "<pre data-deployment-receipt>{}</pre>",
        escape(row.receipt.as_deref().unwrap_or("No terminal receipt yet."))
    ));
    if let Some(uri) = row.artifact_uri {
        html.push_str(&format!(
            "<p class=meta>Artifact: <code>{}</code></p>",
            escape(&uri)
        ));
    }
    Ok(page(&format!("Deployment {id}"), &html))
}

const DAY_MS: i64 = 86_400_000;

// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn sprint_days_remaining(scheduled_end: i64) -> i64 {
    let remaining = scheduled_end.saturating_sub(now_ms());
    if remaining <= 0 {
        0
    } else {
        remaining.saturating_add(DAY_MS - 1) / DAY_MS
    }
}

// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn sprint_summary(store: &Store, project: &str, sprint: &Sprint, current: bool) -> Result<String> {
    let (open, done) = store.sprint_task_counts(&sprint.id)?;
    let route = format!("/sprint/{}/{}", url_encode(project), url_encode(&sprint.id));
    let archived = if sprint.archived {
        " <span class=meta data-sprint-archived>archived</span>"
    } else {
        ""
    };
    let goal =
        sprint.body.as_deref().map(markdown).unwrap_or_else(|| {
            "<p class=empty>No goal or success criteria recorded.</p>".to_owned()
        });
    Ok(format!(
        r#"<article class="card{current_class}" data-sprint-summary="{id}"{current_attr}>
        <h2 class=sprint-version><a href="{route}" data-sprint-link="{id}"><code data-sprint-version>{version}</code></a></h2>
        <p><strong data-sprint-title>{title}</strong></p>
        <div class="body md" data-sprint-goal>{goal}</div>
        <p class=meta><span class="pill status-{status}" data-sprint-state>{status}</span>{archived}</p>
        <dl><dt>Scheduled start</dt><dd data-sprint-scheduled-start>{scheduled_start}</dd>
        <dt>Scheduled end</dt><dd data-sprint-scheduled-end>{scheduled_end}</dd>
        <dt>Days remaining</dt><dd data-sprint-days-remaining>{days}</dd>
        <dt>Open tasks</dt><dd data-sprint-open>{open}</dd>
        <dt>Done tasks</dt><dd data-sprint-done>{done}</dd></dl></article>"#,
        current_class = if current { " current" } else { "" },
        current_attr = if current { " data-sprint-current" } else { "" },
        id = escape(&sprint.id),
        route = escape(&route),
        version = escape(&sprint.target_version),
        title = escape(&sprint.title),
        goal = goal,
        status = escape(&sprint.status),
        archived = archived,
        scheduled_start = escape(&stamp(sprint.scheduled_start)),
        scheduled_end = escape(&stamp(sprint.scheduled_end)),
        days = sprint_days_remaining(sprint.scheduled_end),
    ))
}

// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn board_sprints_content(project: &ProjectRecord, store: &Store) -> Result<String> {
    let current = store.current_sprint()?;
    let history = store.sprints(None, true, i64::MAX)?;
    let mut html = String::new();
    html.push_str("<h2>Current sprint</h2>");
    match &current {
        Some(sprint) => html.push_str(&sprint_summary(store, &project.name, sprint, true)?),
        None => html.push_str(
            "<p class=empty data-no-current-sprint>No current sprint. Planned and historical sprints remain below.</p>",
        ),
    }
    let secondary = history
        .iter()
        .filter(|sprint| sprint.status != "current")
        .collect::<Vec<_>>();
    if secondary.is_empty() {
        if current.is_none() {
            html.push_str("<p class=empty data-no-sprints>This board has no sprints.</p>");
        } else {
            html.push_str(
                "<p class=empty data-no-sprint-history>No planned or historical sprints.</p>",
            );
        }
    } else {
        html.push_str(&format!(
            "<section class=sprint-history data-sprint-history><h2>Planned and history <span class=count>{}</span></h2>",
            secondary.len()
        ));
        for sprint in secondary {
            html.push_str(&sprint_summary(store, &project.name, sprint, false)?);
        }
        html.push_str("</section>");
    }
    Ok(html)
}

/// Every registered board's current release boundary and complete sprint history.
// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn sprints() -> Result<String> {
    let mut html = String::from("<h1 data-sprints-overview>Sprints</h1>");
    let boards = projects()?;
    if boards.is_empty() {
        html.push_str("<p class=empty>No boards are registered.</p>");
    }
    for (project, store) in boards {
        html.push_str(&format!(
            r#"<section data-sprint-board="{name}"><h2><a href="/sprints/{route}" data-board-sprints-link>{name}</a></h2>"#,
            name = escape(&project.name),
            route = escape(&url_encode(&project.name)),
        ));
        html.push_str(&board_sprints_content(&project, &store)?);
        html.push_str("</section>");
    }
    Ok(page("Sprints", &html))
}

/// One board's current release boundary and complete planned/closed/abandoned history.
// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn board_sprints(name: &str) -> Result<String> {
    let (project, store) = project_named(name)?;
    let mut html = format!(
        r#"<h1 data-board-sprints="{name}">{name} sprints</h1><p><a href="/board/{route}">Back to board</a></p>"#,
        name = escape(&project.name),
        route = escape(&url_encode(&project.name)),
    );
    html.push_str(&board_sprints_content(&project, &store)?);
    Ok(page(&format!("{} sprints", project.name), &html))
}

/// One sprint's release goal, dates, visible scope and authoritative deployment proof.
// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn sprint_detail(project_name: &str, id: &str) -> Result<String> {
    let (project, store) = project_named(project_name)?;
    let sprint = store.require_sprint(id)?;
    let tasks = store.sprint_tasks(id)?;
    let (open, done) = store.sprint_task_counts(id)?;
    let archived = if sprint.archived { "yes" } else { "no" };
    let actual_start = if sprint.starts_at == 0 {
        "not started".to_owned()
    } else {
        stamp(sprint.starts_at)
    };
    let actual_end = sprint
        .ends_at
        .map(stamp)
        .unwrap_or_else(|| "not ended".to_owned());
    let mut html = format!(
        r#"<h1 data-sprint-detail="{id}"><span data-sprint-title>{title}</span></h1>
        <p class=meta>Release <code data-sprint-version>{version}</code> on {project}.</p>
        <p><a href="/sprints/{board}" data-board-sprints-back>Back to {project} sprints</a></p>
        <div class="card current"><div class="body md" data-sprint-goal>{goal}</div>
        <dl><dt>State</dt><dd><span class="pill status-{status}" data-sprint-state>{status}</span></dd>
        <dt>Scheduled start</dt><dd data-sprint-scheduled-start>{scheduled_start}</dd>
        <dt>Scheduled end</dt><dd data-sprint-scheduled-end>{scheduled_end}</dd>
        <dt>Actual start</dt><dd data-sprint-actual-start>{actual_start}</dd>
        <dt>Actual end</dt><dd data-sprint-actual-end>{actual_end}</dd>
        <dt>Days remaining</dt><dd data-sprint-days-remaining>{days}</dd>
        <dt>Open tasks</dt><dd data-sprint-open>{open}</dd>
        <dt>Done tasks</dt><dd data-sprint-done>{done}</dd>
        <dt>Archived</dt><dd data-sprint-archived>{archived}</dd></dl></div>"#,
        id = escape(&sprint.id),
        version = escape(&sprint.target_version),
        title = escape(&sprint.title),
        board = escape(&url_encode(&project.name)),
        project = escape(&project.name),
        goal = sprint.body.as_deref().map(markdown).unwrap_or_else(|| {
            "<p class=empty>No goal or success criteria recorded.</p>".to_owned()
        }),
        status = escape(&sprint.status),
        scheduled_start = escape(&stamp(sprint.scheduled_start)),
        scheduled_end = escape(&stamp(sprint.scheduled_end)),
        actual_start = escape(&actual_start),
        actual_end = escape(&actual_end),
        days = sprint_days_remaining(sprint.scheduled_end),
    );
    if let Some(deployment_id) = &sprint.closed_by_deployment {
        let deployment = store.require_deployment(deployment_id)?;
        html.push_str(&format!(
            r#"<section class=card data-sprint-deployment-proof><h2>Served deployment proof</h2>
            <p>{link}</p><dl><dt>Served version</dt><dd data-sprint-served-version>{version}</dd>
            <dt>Served commit</dt><dd data-sprint-served-commit>{commit}</dd></dl></section>"#,
            link = deployment_link(&project.name, &deployment),
            version = deployment
                .served_version
                .as_deref()
                .map(escape)
                .unwrap_or_else(|| "not recorded".to_owned()),
            commit = deployment
                .served_commit
                .as_deref()
                .map(|value| format!("<code>{}</code>", escape(value)))
                .unwrap_or_else(|| "not recorded".to_owned()),
        ));
    } else if sprint.status == "closed" {
        html.push_str(
            "<p class=empty data-no-sprint-proof>No served deployment proof is attached.</p>",
        );
    }
    html.push_str(&format!(
        "<h2>Attached tasks <span class=count>{}</span></h2>",
        tasks.len()
    ));
    if tasks.is_empty() {
        html.push_str("<p class=empty data-no-sprint-tasks>No visible tasks are attached.</p>");
    } else {
        html.push_str("<ul class=rows data-sprint-tasks>");
        for task in tasks {
            html.push_str(&format!(
                r#"<li data-task="{id}"><a href="/task/{project}/{task_route}" data-task-link="{id}" data-ref target=_blank rel=noopener data-task-title>{title}</a><p class=meta>Attached as <code>{id}</code> in <span class="pill status-{state}" data-task-state>{status}</span></p></li>"#,
                project = escape(&url_encode(&project.name)),
                task_route = escape(&url_encode(&task.id)),
                id = escape(&task.id),
                state = escape(&task.status),
                status = escape(&status_label(&task.status)),
                title = escape(&task.title),
            ));
        }
        html.push_str("</ul>");
    }
    Ok(page(&format!("Sprint {}", sprint.id), &html))
}

/// Every board at a glance — the `dashboard` projection, rendered.
///
/// The counts, and the order they arrive in, are
/// [`projection::board_summaries`]: the JSON surface serves the same rows
/// from the same computation, so the page and `/api/v1/boards` cannot drift
/// into disagreeing about how many rows a board holds.
// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn boards() -> Result<String> {
    let mut html = String::from(
        "<h1>Boards</h1><table><thead><tr>\
        <th>Board</th><th class=n>Open attention</th><th class=n>To do</th>\
        <th class=n>In progress</th><th class=n>Stale</th>\
        <th class=n>Handoffs</th><th class=n>Tasks</th></tr></thead><tbody>",
    );
    for summary in projection::board_summaries()? {
        html.push_str(&format!(
            "<tr data-board=\"{name}\"><td><a href=\"/board/{url}\" data-board-link data-ref target=_blank rel=noopener>{name}</a></td>\
             <td class=\"n{flag}\">{attention}</td><td class=n>{todo}</td>\
             <td class=n>{doing}</td><td class=n>{stale}</td>\
             <td class=n>{handoffs}</td><td class=n>{total}</td></tr>",
            url = escape(&summary.board),
            name = escape(&summary.board),
            flag = if summary.open_attention == 0 {
                ""
            } else {
                " waiting"
            },
            attention = summary.open_attention,
            todo = summary.todo,
            doing = summary.in_progress,
            stale = summary.stale,
            handoffs = summary.handoffs,
            total = summary.tasks,
        ));
    }
    html.push_str("</tbody></table>");
    html.push_str(
        "<p class=meta>Counts come from the same projection \
         <code>kb dash</code> reads. Integrity is what <code>kb doctor</code> \
         checks, not what this page claims.</p>",
    );
    Ok(page("Boards", &html))
}

/// Plans: draft epics, whose body is the plan itself.
///
/// A draft holds back everything beneath it, so this page is also the answer to
/// "what work is currently gated" — the children are listed with each plan
/// because opening the plan is what releases them.
// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn plans(opened: Option<&str>) -> Result<String> {
    let mut html = String::from("<h1>Plans</h1>");
    if let Some(id) = opened {
        html.push_str(&format!(
            "<p class=success data-plan-opened>Opened plan <code>{}</code> and its child work is now eligible for claims.</p>",
            escape(id)
        ));
    }
    let mut found = 0;
    for (project, store) in projects()? {
        let tasks = store.list_tasks(None, None, None, None, false)?;
        let drafts = tasks
            .iter()
            .filter(|task| task.status == "draft" && task.task_type == "epic")
            .collect::<Vec<_>>();
        for plan in drafts {
            found += 1;
            let plan_attention = task_attention_count(&store, &plan.id)?;
            html.push_str(&format!(
                "<article class=plan data-plan=\"{}\">",
                escape(&plan.id)
            ));
            html.push_str(&format!(
                "<h2><a href=\"/task/{project}/{id}\" data-task-link=\"{id}\" \
                 data-ref target=_blank rel=noopener>{title}</a>{attention}</h2>\
                 <p class=meta>Drafted {age} on \
                 <a href=\"/board/{project}\" data-ref target=_blank rel=noopener>{project}</a> \
                 as <code>{id}</code> at {priority}{tags}</p>",
                project = escape(&project.name),
                id = escape(&plan.id),
                title = escape(&plan.title),
                priority = priority_badge(plan.priority, plan.priority_level.as_deref()),
                age = ago(plan.created_at),
                tags = tag_list(&plan.tags),
                attention = attention_count_badge(plan_attention),
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
                    let child_attention = task_attention_count(&store, &child.id)?;
                    html.push_str(&format!(
                        "<li><a href=\"/task/{project}/{id}\" \
                         data-ref target=_blank rel=noopener>{title}</a>\
                         <p class=meta>In <span class=\"pill status-{state}\">{status}</span> \
                         at {priority}, filed as <code>{id}</code>{attention}</p></li>",
                        project = escape(&project.name),
                        id = escape(&child.id),
                        state = escape(&child.status),
                        status = escape(&status_label(&child.status)),
                        title = escape(&child.title),
                        priority = priority_badge(child.priority, child.priority_level.as_deref()),
                        attention = attention_clause(child_attention),
                    ));
                }
                html.push_str("</ul>");
            }
            if let Some(body) = &plan.body {
                html.push_str(&format!(
                    "<div class=\"body md plan-body\" data-plan-body>{}</div>",
                    markdown(body)
                ));
            }
            html.push_str(&format!(
                "<form method=post action=\"/plan/{project_path}/{id_path}/open\">\
                 <button type=submit data-plan-open>Open plan</button></form>\
                 <p class=cmd>Equivalent: <code>kb t mv {id} todo --as {OPERATOR_ACTOR} --project {project}</code></p>",
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

/// One rendered subscription: the row, the board it belongs to, the position
/// derived for it against that board's head, and what its dead letters were
/// refused with.
struct SubscriptionView {
    board: String,
    subscription: Subscription,
    position: SubscriptionPosition,
    /// Empty unless this subscription has dead letters, which is most of the
    /// time: the codes exist to be loud when a delivery has stopped, not to
    /// occupy the row when nothing has.
    dead_letter_codes: Vec<DeadLetterCode>,
    head_event_seq: i64,
}

/// Subscriptions: what each consumer watches, where it delivers, and how far
/// it has actually got.
///
/// **Position is a presented cursor and nothing else.** Cursor presentation
/// means showing a cursor's *meaning* — never an opaque token, and never a
/// copy kept in the browser: a cursor held client-side becomes a claim the
/// server must trust, and a stale one silently skips rows, which is missing
/// information that reads as absence of information
/// (`docs/ui-pubsub-consumption-seams.md`). Everything in that column is
/// derived per request from the delivery rows and the event head, so a
/// reload is always the truth and there is nothing to invalidate.
///
/// **The one display preference lives in the URL.** `?show=all` lists paused
/// subscriptions, exactly as `/plans?opened=` and `/search?q=` carry theirs.
/// It is deliberately not stored: a preferences table would need a migration,
/// an actor, an authorization rule and a `doctor` check to express something a
/// shareable URL already says, and two operators would then disagree about
/// what "the page" lists. If another display choice arrives, it is another
/// query parameter.
// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn subscriptions(show: Option<&str>, changed: Option<&str>) -> Result<String> {
    let mut views = Vec::new();
    for (project, store) in projects()? {
        // Two statements per board, both outside the row loop: the grouped
        // delivery projection plus the head it is measured against. Paused
        // rows are read whatever the filter says, so a hidden row can be
        // counted and offered rather than reading as "nothing exists".
        let positions = store.subscription_positions()?;
        for subscription in store.subscriptions(None, None, true)? {
            views.push(SubscriptionView {
                board: project.name.clone(),
                position: positions.position(&subscription.id),
                // Copied out of the projection rather than borrowed from it:
                // an empty slice copies without allocating, so a board with
                // nothing dead-lettered pays nothing for this.
                dead_letter_codes: positions.dead_letter_codes(&subscription.id).to_vec(),
                head_event_seq: positions.head_event_seq,
                subscription,
            });
        }
    }
    views.sort_by(|a, b| {
        (&a.board, a.subscription.created_at, &a.subscription.id).cmp(&(
            &b.board,
            b.subscription.created_at,
            &b.subscription.id,
        ))
    });
    Ok(page(
        "Subscriptions",
        &subscriptions_body(&views, show == Some("all"), changed),
    ))
}

fn subscriptions_body(views: &[SubscriptionView], show_all: bool, changed: Option<&str>) -> String {
    let mut html = String::from("<div class=heading><h1>Subscriptions</h1></div>");
    if let Some(id) = changed {
        html.push_str(&format!(
            "<p class=success>Recorded the change to <code>{}</code> and the dispatcher reads its state on the next pass.</p>",
            escape(id)
        ));
    }
    let (shown, hidden): (Vec<_>, Vec<_>) = views
        .iter()
        .partition(|view| show_all || view.subscription.status == "active");
    if shown.is_empty() {
        // An empty list has two very different causes, and saying the wrong
        // one is how absence reads as a finding.
        html.push_str(&if hidden.is_empty() {
            format!(
                "<p class=empty>Nothing is subscribed yet. \
                 <code>kb subscription add --consumer NAME --action NAME --timeout-ms 30000 \
                 --max-retries 3 --rate-per-minute 60 --max-concurrency 1 --as {OPERATOR_ACTOR}</code> \
                 registers one, and it starts watching from the event that created it — \
                 add <code>--kind</code> or <code>--subject</code> or \
                 <code>--current-status</code> or <code>--tag</code> to narrow what it sees.</p>"
            )
        } else {
            format!(
                "<p class=empty>Every subscription here is paused right now. \
                 <a href=\"/subscriptions?show=all\">Show the {} paused one{}</a> to see \
                 where each of them stopped.</p>",
                hidden.len(),
                if hidden.len() == 1 { "" } else { "s" },
            )
        });
        return html;
    }
    html.push_str(
        "<p class=meta>Position is derived per request: the start anchor, the highest acked \
         seq, and the distance to that board's event head. The distance counts board events, \
         and a subscription only receives the ones its filter selects — the queued counts are \
         what is actually waiting for it.</p>",
    );
    html.push_str(
        "<table><thead><tr><th>Subscription</th><th>Watches</th><th>Delivers to</th>\
         <th>State</th><th>Position</th><th>Limits</th></tr></thead><tbody>",
    );
    for view in &shown {
        html.push_str(&subscription_row(view, show_all));
    }
    html.push_str("</tbody></table>");
    if !hidden.is_empty() {
        html.push_str(&format!(
            "<p class=meta>{} paused subscription{} hidden. \
             <a href=\"/subscriptions?show=all\">Show paused subscriptions</a>.</p>",
            hidden.len(),
            if hidden.len() == 1 { " is" } else { "s are" },
        ));
    } else if show_all {
        html.push_str(
            "<p class=meta>Listing paused subscriptions too. \
             <a href=\"/subscriptions\">Show active only</a>.</p>",
        );
    }
    html
}

/// How many codes a dead-letter line names before it summarises the rest.
///
/// The set is not bounded by anything this page controls. One subscription's
/// deliveries can carry its adapter's five or six classifications plus any of
/// the `adapter_*` classes the delivery process itself fails with, so twenty
/// distinct codes on one row is reachable, and naming all of them lets ledger
/// cardinality decide how tall a table row is.
///
/// Three is where the summary starts paying for itself. Measured at 1180px, a
/// count and a thirty-character code fill about one wrapped line of the
/// position column, so three names plus the tail is four lines of the page's
/// loudest treatment — barely shorter than listing five, and still four for
/// twenty. What is left over stays counted rather than dropped, so this caps
/// names and never arithmetic.
const DEAD_LETTER_CODES_NAMED: usize = 3;

/// What is actually waiting, rendered only when something is.
///
/// These three counts are aspects of position rather than peer facts, and at
/// rest all three are zero — three columns of nothing crowded out the sentence
/// that carries the meaning. Silence here reads correctly: nothing queued.
/// A dead-lettered delivery is the one thing on this page that needs a person,
/// so it is the one thing that gets loud, in the same treatment open attention
/// gets on Boards.
fn queued_state(position: SubscriptionPosition, dead_letter_codes: &[DeadLetterCode]) -> String {
    let mut parts = Vec::new();
    if position.pending > 0 {
        parts.push(format!("{} pending", position.pending));
    }
    if position.retry_wait > 0 {
        parts.push(format!(
            "<span class=retrying>{} retrying</span>",
            position.retry_wait
        ));
    }
    if position.dead_letter > 0 {
        parts.push(format!(
            "<span class=dead>{} dead-lettered{}</span>",
            position.dead_letter,
            dead_letter_attribution(position.dead_letter, dead_letter_codes),
        ));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("<div class=queued>{}</div>", parts.join(", "))
}

/// Which refusal the dead letters are, in the same sentence as how many.
///
/// `3 dead-lettered` says which subscription stopped; it does not say whether
/// to check a port or a payload. The code the adapter classified the failure
/// as is exactly that difference, so it rides along: one code becomes
/// `, all opencode_endpoint_unreachable` and an operator goes and looks at a
/// port.
///
/// Mixed codes stay apart — `: 3 opencode_endpoint_unreachable,
/// 1 kimi_frame_oversized` — and are never collapsed into the count or
/// represented by whichever code came first. Two adapters refusing for two
/// reasons is two pieces of work, and "all" said about a mixed set is a false
/// sentence that reads as a diagnosis.
///
/// Beyond `DEAD_LETTER_CODES_NAMED` the tail is summarised rather than
/// listed, and it is summarised with its own numbers — how many codes and how
/// many deliveries — so the named counts plus the tail still add up to the
/// total beside them and the named codes cannot read as the whole story. It
/// carries those numbers instead of pointing at a fuller view because there
/// is no fuller view to point at: `kanban subscription show` prints the
/// subscription row, and nothing in the CLI or on this page lists deliveries
/// one by one.
///
/// No `<code>` markup around the code. The queued line is plain prose in the
/// row (`2 pending`, `3 retrying`), and a monospace chip inside the one bold
/// orange line on the page is decoration competing with the alarm.
fn dead_letter_attribution(dead_letter: i64, codes: &[DeadLetterCode]) -> String {
    match codes {
        // A count with no code behind it is a state the table's CHECK forbids,
        // not an adapter that failed anonymously. Say nothing rather than
        // invent an attribution; the count is still true.
        [] => String::new(),
        [only] if only.deliveries == dead_letter => format!(", all {}", escape(&only.code)),
        _ => {
            let named = codes.len().min(DEAD_LETTER_CODES_NAMED);
            let mut attribution = String::from(": ");
            for (index, code) in codes[..named].iter().enumerate() {
                if index > 0 {
                    attribution.push_str(", ");
                }
                attribution.push_str(&format!("{} {}", code.deliveries, escape(&code.code)));
            }
            let rest = codes.len() - named;
            if rest > 0 {
                let deliveries = dead_letter
                    - codes[..named]
                        .iter()
                        .map(|code| code.deliveries)
                        .sum::<i64>();
                attribution.push_str(&format!(
                    ", and {rest} more code{} across {deliveries} deliver{}",
                    if rest == 1 { "" } else { "s" },
                    if deliveries == 1 { "y" } else { "ies" },
                ));
            }
            attribution
        }
    }
}

fn subscription_row(view: &SubscriptionView, show_all: bool) -> String {
    let subscription = &view.subscription;
    let position = view.position;
    let paused = subscription.status != "active";
    // Nothing acked yet means the subscription is still sitting on its start
    // anchor, which is where it began — not seq 0, and not "caught up".
    let acked_position = position
        .acked_through_seq
        .unwrap_or(subscription.start_event_seq);
    // Neither control is destructive: pausing is reversible and resuming
    // restores the default, so neither gets the approve/decline weight the
    // attention surface uses for a decision. The page's one loud element is a
    // dead-lettered delivery, which is the only thing here needing a person.
    let (verb, verb_label) = if paused {
        ("resume", "Resume delivery")
    } else {
        ("pause", "Pause delivery")
    };
    let position_meta = format!(
        "started at seq {}{}{}",
        subscription.start_event_seq,
        match position.acked_through_seq {
            Some(seq) => format!(", acked through seq {seq}"),
            None => ", nothing acked yet".to_owned(),
        },
        if position.leased == 0 {
            String::new()
        } else {
            format!(", {} in flight", position.leased)
        },
    );
    format!(
        "<tr data-subscription=\"{id}\"><td><code>{id}</code><div class=meta><a href=\"/board/{board_url}\" data-ref target=_blank rel=noopener>{board}</a></div></td>\
         <td>{watches}</td>\
         <td><code>{consumer}</code><div class=meta>action <code>{action}</code> and {secret}</div></td>\
         <td><span class=\"pill status-{status}\" data-subscription-state>{status}</span>{paused_by}\
         <form method=post action=\"/subscription/{board_path}/{id_path}/{verb}{carry}\">\
         <button class=quick type=submit data-subscription-action=\"{verb}\">{verb_label}</button></form></td>\
         <td>{position_sentence}<div class=meta>{position_meta}</div>{queued}</td>\
         <td><div class=meta>{limits}</div></td></tr>",
        id = escape(&subscription.id),
        board_url = escape(&url_encode(&view.board)),
        board = escape(&view.board),
        watches = escape(&watch_sentence(subscription)),
        consumer = escape(&subscription.consumer_id),
        action = escape(&subscription.action_id),
        // Whether a secret is configured is operational; which secret it is
        // stays a host-local lookup name the page has no business repeating.
        secret = if subscription.secret_ref.is_some() {
            "a secret is configured"
        } else {
            "no secret configured"
        },
        status = escape(&subscription.status),
        paused_by = match (&subscription.paused_by, subscription.paused_at) {
            (Some(actor), Some(at)) => format!(
                "<div class=meta>paused by {} {}</div>",
                escape(actor),
                escape(&ago(at))
            ),
            _ => String::new(),
        },
        board_path = url_encode(&view.board),
        id_path = url_encode(&subscription.id),
        carry = if show_all && paused { "?show=all" } else { "" },
        position_sentence = escape(&position_sentence(view.head_event_seq, acked_position)),
        queued = queued_state(position, &view.dead_letter_codes),
        limits = escape(&format!(
            "{} ms timeout, {} retries, {}/min, {} at a time",
            subscription.timeout_ms,
            subscription.max_retries,
            subscription.rate_per_minute,
            subscription.max_concurrency,
        )),
    )
}

/// What a subscription watches, in a sentence.
///
/// Six selector fields rendered as six columns is six things to decode; what
/// an operator wants is to read what the thing is for. Empty selectors narrow
/// nothing, so a subscription with none of them watches the whole board and
/// says exactly that.
///
/// The rule: `Every <kinds> event`, then the narrowing clauses in a fixed
/// order — subject, relations, prior statuses, current statuses, tags — the
/// first attached with a space and any others with commas.
fn watch_sentence(subscription: &Subscription) -> String {
    let opening = if subscription.kinds.is_empty() {
        "Every event".to_owned()
    } else {
        format!("Every {} event", or_list(&subscription.kinds))
    };
    let mut clauses = Vec::new();
    if let Some(task) = &subscription.subject_task_id {
        clauses.push(format!("about task {task}"));
    }
    if !subscription.relations.is_empty() {
        clauses.push(format!(
            "related through {}",
            or_list(&subscription.relations)
        ));
    }
    if !subscription.prior_statuses.is_empty() {
        clauses.push(format!("leaving {}", or_list(&subscription.prior_statuses)));
    }
    if !subscription.current_statuses.is_empty() {
        clauses.push(format!(
            "arriving at {}",
            or_list(&subscription.current_statuses)
        ));
    }
    if !subscription.tags.is_empty() {
        clauses.push(format!("tagged {}", or_list(&subscription.tags)));
    }
    let Some((first, rest)) = clauses.split_first() else {
        return format!("{opening} on the board.");
    };
    let mut sentence = format!("{opening} {first}");
    for clause in rest {
        sentence.push_str(", ");
        sentence.push_str(clause);
    }
    sentence.push('.');
    sentence
}

/// How far behind the board head a subscription is, in words.
///
/// "Caught up" is only honest when nothing sits between the last ack and the
/// head. The count is board events rather than matching events, and the page
/// says so beside the table: a subscription receives only what its filter
/// selects, so calling every newer event a backlog would report work as lost
/// that was never addressed to it.
fn position_sentence(head_event_seq: i64, acked_position: i64) -> String {
    match head_event_seq.saturating_sub(acked_position) {
        behind if behind <= 0 => format!("Caught up with head seq {head_event_seq}."),
        1 => format!("1 board event behind head seq {head_event_seq}."),
        behind => format!("{behind} board events behind head seq {head_event_seq}."),
    }
}

/// "a", "b", or "c", in the Oxford-comma shape the store's refusals use.
fn or_list(values: &[String]) -> String {
    match values {
        [] => String::new(),
        [only] => only.clone(),
        [first, second] => format!("{first} or {second}"),
        _ => {
            let (last, rest) = values.split_last().expect("more than two values");
            format!("{}, or {last}", rest.join(", "))
        }
    }
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

/// Where every lane stands, newest first.
///
/// The counterpart to Needs you: that page is what waits on the operator, this
/// is what the agents are doing. A lane that has been posting is legible here
/// without anyone opening a terminal or waiting for a handoff.
// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn lanes() -> Result<String> {
    let (ordered, _truncated) = lane_groups(LANE_UPDATE_ROWS)?;
    let mut html = String::from("<h1>Lanes</h1>");
    if ordered.is_empty() {
        html.push_str(
            "<p class=empty>No lane has posted a sitrep. \
             <code>kb sr new \"…\" --as AGENT --lane LANE</code> writes one — no task \
             and no lease required, which is the point of it.</p>",
        );
        return Ok(page("Lanes", &html));
    }
    for ((project, lane), updates) in &ordered {
        html.push_str(&format!(
            "<article class=item data-lane=\"{}\">",
            escape(lane)
        ));
        html.push_str(&format!(
            "<h2>{lane} <span class=count><a href=\"/board/{project_url}\" \
             data-ref target=_blank rel=noopener>{project}</a></span></h2>",
            lane = escape(lane),
            project_url = escape(project),
            project = escape(project),
        ));
        for update in updates {
            html.push_str(&format!(
                "<p class=meta>{author} wrote this {age}{task}{branch}</p>\
                 <div class=\"body md\" data-lane-body>{body}</div>",
                author = escape(&update.author),
                age = ago(update.created_at),
                task = update
                    .task_id
                    .as_ref()
                    .map(|id| format!(
                        ", about <a href=\"/task/{project}/{id}\" data-task-link=\"{id}\" \
                         data-ref target=_blank rel=noopener>{id}</a>",
                        project = escape(project),
                        id = escape(id)
                    ))
                    .unwrap_or_default(),
                branch = update
                    .branch
                    .as_ref()
                    .map(|branch| format!(", on {}", escape(branch)))
                    .unwrap_or_default(),
                body = markdown(&update.body),
            ));
        }
        html.push_str("</article>");
    }
    Ok(page("Lanes", &html))
}

/// One board's rows, grouped by status in workflow order.
// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn board(name: &str) -> Result<String> {
    let (project, store) = project_named(name)?;
    let tasks = store.list_tasks(None, None, None, None, false)?;
    let rules = Registry::open()?.applicable_rules(Some(&project.name), None, None, false)?;
    let mut html = format!(
        "<h1>{0}</h1><p><a href=\"/sprints/{1}\" data-board-sprints-link>Sprints for {0}</a></p>",
        escape(&project.name),
        escape(&url_encode(&project.name)),
    );
    let roots = if project.workspace_roots.is_empty() {
        "Rootless".to_owned()
    } else {
        project
            .workspace_roots
            .iter()
            .map(|root| escape(root))
            .collect::<Vec<_>>()
            .join(", ")
    };
    html.push_str(&format!(
        "<p class=meta>Roots: {}, holding {} rows</p>",
        roots,
        tasks.len()
    ));
    if !rules.is_empty() {
        html.push_str(&format!(
            "<h2>Rules <span class=count>{}</span></h2>",
            rules.len()
        ));
        for rule in rules {
            let headline = rule.body.lines().next().unwrap_or_default();
            let targets = if rule.tags.is_empty() {
                String::new()
            } else {
                format!(", tagged {}", escape(&rule.tags.join(", ")))
            };
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
            escape(&status_label(status)),
            rows.len()
        ));
        for task in rows {
            let attention = task_attention_count(&store, &task.id)?;
            html.push_str(&format!(
                "<li data-task=\"{id}\"><a href=\"/task/{project}/{id}\" data-task-link=\"{id}\" \
                 data-ref target=_blank rel=noopener>{title}</a>\
                 <p class=meta>{article} {ty} in \
                 <span class=\"pill status-{state}\">{status}</span> \
                 at {priority}{lane}{tags}{attention}, filed as <code>{id}</code></p></li>",
                project = escape(&project.name),
                id = escape(&task.id),
                article = a_or_an(&task.task_type),
                ty = escape(&task.task_type),
                state = escape(&task.status),
                status = escape(&status_label(&task.status)),
                title = escape(&task.title),
                priority = priority_badge(task.priority, task.priority_level.as_deref()),
                lane = task
                    .lane
                    .as_ref()
                    .map(|lane| format!(", in lane {}", escape(lane)))
                    .unwrap_or_default(),
                tags = tag_list(&task.tags),
                attention = attention_clause(attention),
            ));
        }
        html.push_str("</ul>");
    }
    Ok(page(&project.name, &html))
}

/// One task in full: what it is, where its work happened, and the trail.
// retired by t-bf255880 wave 1; deleted in wave 2
#[allow(dead_code)]
fn task_detail(project_name: &str, id: &str) -> Result<String> {
    let (project, store) = project_named(project_name)?;
    let task = store.require_task(id)?;
    let mut html = format!(
        "<h1 data-task-detail=\"{}\" data-task-title>{}</h1>",
        escape(&task.id),
        escape(&task.title)
    );
    html.push_str(&format!(
        "<p class=meta>{article} {ty} on \
         <a href=\"/board/{project}\" data-ref target=_blank rel=noopener>{project}</a>, \
         in <span class=\"pill status-{state}\" data-task-status>{status}</span> \
         at {priority}{tags}, filed as <code>{id}</code></p>",
        project = escape(&project.name),
        id = escape(&task.id),
        article = a_or_an(&task.task_type),
        ty = escape(&task.task_type),
        state = escape(&task.status),
        status = escape(&status_label(&task.status)),
        priority = priority_badge(task.priority, task.priority_level.as_deref()),
        tags = tag_list(&task.tags),
    ));
    html.push_str(&facts(&project.name, &task));
    if let Some(body) = &task.body {
        html.push_str(&format!(
            "<h2>Body</h2><div class=\"body md\" data-task-body>{}</div>",
            markdown(body)
        ));
    }

    if let Some(claim) = store.get_claim(&task.id)? {
        html.push_str(&held_by(&claim));
    }

    let open_attention = task_open_attention(&store, &task.id)?;
    html.push_str(&attention_section(
        &project.name,
        "Open attention",
        &open_attention,
    ));

    let notes = store.notes(&task.id, DETAIL_ROWS)?;
    if !notes.is_empty() {
        html.push_str("<h2>Notes</h2>");
        for note in notes {
            html.push_str(&format!(
                "<article class=note data-task-note><p class=meta>{article} {kind} note \
                 by {author} at {when}</p><div class=\"body md\">{body}</div></article>",
                article = a_or_an(&note.kind),
                kind = escape(&note.kind),
                author = escape(&note.author),
                when = stamp(note.created_at),
                body = markdown(&note.body),
            ));
        }
    }

    let checkpoints = store.checkpoints(&task.id, DETAIL_ROWS)?;
    if !checkpoints.is_empty() {
        html.push_str("<h2>Checkpoints</h2>");
        for point in checkpoints {
            html.push_str(&format!(
                "<article class=note><p class=meta>{article} {state} checkpoint \
                 by {author} at {when}</p><dl>",
                article = a_or_an(&point.state),
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
            "<h2>Trail</h2><table data-task-trail><thead><tr><th>When</th><th>What</th>\
                       <th>Who</th><th>Detail</th></tr></thead><tbody>",
        );
        for event in events {
            html.push_str(&format!(
                "<tr data-event-kind=\"{kind}\"><td class=when>{when}</td><td><code>{kind}</code></td>\
                 <td>{who}</td><td class=payload>{payload}</td></tr>",
                when = stamp(event.created_at),
                kind = escape(&event.kind),
                who = escape(event.actor.as_deref().unwrap_or("—")),
                payload = escape(&compact(&event.payload)),
            ));
        }
        html.push_str("</tbody></table>");
        html.push_str(&format!(
            "<p class=meta>Newest {DETAIL_ROWS} shown, and \
             <code>kb ev --task {id} --project {project} --json</code> prints the rest.</p>",
            id = escape(&task.id),
            project = escape(&project.name),
        ));
    }
    Ok(page(&task.title, &html))
}

// ------------------------------------------------------------------ rendering

/// The live lease, as the reader needs to see it.
///
/// Provenance: where the work actually happened, and now which model is
/// doing it. Captured rather than asked for, so each line is present when
/// there was something to capture and absent when there was not — never
/// invented. Never the lease token: it is a capability, and a read surface
/// that renders one hands whoever loads the page the ability to write.
fn held_by(claim: &crate::model::Claim) -> String {
    let mut html = String::from("<h2>Held by</h2><dl>");
    html.push_str(&row("agent", &escape(&claim.agent_id)));
    html.push_str(&row("claimed", &stamp(claim.claimed_at)));
    html.push_str(&row("expires", &stamp(claim.expires_at)));
    for (label, value) in [
        ("model", claim.model.as_deref()),
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
    html
}

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
                "<a href=\"/task/{project}/{parent}\" data-ref target=_blank rel=noopener>{parent}</a>",
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
    if !task.allowed_models.is_empty() {
        // Only when the row is restricted: an empty list is the default every
        // row carries, and a `allowed models —` line on every page would be
        // noise that says nothing.
        html.push_str(&row(
            "allowed models",
            &task
                .allowed_models
                .iter()
                .map(|model| escape(model))
                .collect::<Vec<_>>()
                .join(", "),
        ));
    }
    html.push_str("</dl>");
    html
}

fn row(label: &str, value: &str) -> String {
    format!("<dt>{}</dt><dd>{value}</dd>", escape(label))
}

/// The tags a row was filed under, as a fragment of the row's own sentence.
///
/// A tag is neither a state nor an outcome, so it gets no pill and no hue
/// (WEB-41): it is the words the row was filed under, read inside the
/// sentence that already says what the row is.
fn tag_list(tags: &[String]) -> String {
    if tags.is_empty() {
        return String::new();
    }
    format!(
        ", tagged {}",
        tags.iter()
            .map(|tag| escape(tag))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// `A` or `An`, so a meta sentence that opens with a row's kind reads as
/// English rather than as a filled-in template.
fn a_or_an(word: &str) -> &'static str {
    match word.chars().next().map(|first| first.to_ascii_lowercase()) {
        Some('a' | 'e' | 'i' | 'o' | 'u') => "An",
        _ => "A",
    }
}

/// One board status as a heading and a pill read it.
///
/// The stored value is a slug because it is a key (`in_progress`); a section
/// heading and a pill are prose, and WEB-03 and the plan's fifth principle
/// want prose. One function, so the heading and the pill beneath it can
/// never drift into saying the same state two ways.
fn status_label(status: &str) -> String {
    // `todo` is two words in English, and the Boards table already writes it
    // that way; every other status is one word or underscore-joined.
    if status == "todo" {
        return "To do".to_owned();
    }
    let words = status.replace('_', " ");
    let mut characters = words.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => words,
    }
}

fn priority_badge(priority: i64, level: Option<&str>) -> String {
    match level {
        Some(level) => format!(
            "<span class=\"priority priority-{class}\" data-testid=deck-priority \
             title=\"stored priority {priority}\">{level}</span>",
            class = escape(&level.to_ascii_lowercase()),
            level = escape(level),
        ),
        None => format!(
            "<span class=\"priority priority-legacy\" data-testid=deck-priority \
             title=\"legacy out-of-band priority\">{priority}</span>"
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
        // Past a day the coarsest true unit is the day, and it is a word:
        // the card's eyebrow reads `codex@driver asked on px, 3 days ago`
        // (spec WEB-23), and `72h ago` is a duration a reader has to divide.
        m if m < 2880 => "1 day".to_owned(),
        m => format!("{} days", m / 1440),
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
/// A phone-first operator shell. It stays inline because a second request for a
/// stylesheet is another route and cache contract for a page this small.
///
/// The links live behind a hamburger rather than across the top (George,
/// 2026-09-17). Eight destinations and a search box on a 390-wide screen was
/// a scrolling strip of tabs above every page, and on the deck it was the
/// screen's whole top edge spent on navigation nobody uses while deciding.
/// The drawer is one button, and the links inside it keep their `data-nav`
/// names: the destinations did not change, only where they are kept.
/// `<main>` carries no deck marker any more: the deck is the mounted
/// application's, so every page this shell serves is a server-rendered read
/// page (`t-1f495a7f`).
fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{title} — kanban</title><style>{CSS}</style></head><body>\
         <nav aria-label=Primary data-primary-nav>\
         <button type=button class=menu data-menu aria-label=Menu aria-expanded=false \
         aria-controls=nav-drawer><span class=bars aria-hidden=true></span></button>\
         <a class=brand href=\"/\" aria-label=\"Kanban home\">kb</a>\
         <span class=live data-live role=status aria-live=polite>connecting</span></nav>\
         <div class=backdrop data-backdrop hidden></div>\
         <nav class=drawer id=nav-drawer data-drawer aria-label=Destinations hidden>\
         <form action=/search method=get data-nav-search><input name=q aria-label=\"Search Kanban\" placeholder=\"Search\"></form>\
         <div class=nav-links><a href=\"/\" data-nav=needs-you>Needs you</a><a href=\"/all\" data-nav=all>All open</a><a href=\"/decided\" data-nav=decided>Recent decisions</a><a href=\"/lanes\" data-nav=lanes>Lanes</a>\
         <a href=\"/boards\" data-nav=boards>Boards</a><a href=\"/sprints\" data-nav=sprints>Sprints</a><a href=\"/plans\" data-nav=plans>Plans</a><a href=\"/deployments\" data-nav=deployments>Deployments</a>\
         <a href=\"/subscriptions\" data-nav=subscriptions>Subscriptions</a></div>\
         </nav><main id=main>{body}</main>\
         <footer>The live operator view, served by <code>kanban serve</code> on this host.</footer>\
         <script>{JS}</script></body></html>",
        title = escape(title),
    )
}

const JS: &str = r#"
// The first thing this script does is say that it ran. Every rule that turns
// the open list into a deck is scoped to `html.js`, because the deck is one
// card only while there is a script to hide the others: without one the same
// markup has to stay the plain scrolling list it is served as, with every
// card's form posting on its own. This line is what makes that true, so it
// comes before anything that could throw.
document.documentElement.classList.add('js');
const setLive = text => { const el = document.querySelector('[data-live]'); if (el) el.textContent = text; };
// Whether the live socket is up, which is a different question from whether
// a decision is in flight -- and the live line has to answer both.
let liveSocketUp = false;
// A decision that has landed, or been refused, is no longer `sending`. The
// line went on saying so until the socket happened to say something next,
// which on a quiet board was minutes and on a reconnect was the word
// `reconnecting` standing over a decision that had already been recorded.
// So the answer to the click is retired by the click's own path, and the
// line goes back to saying what the socket is: live, or not.
const settleLive = () => setLive(liveSocketUp ? 'live' : 'reconnecting');
// A redelivered notice must not act twice, so every key the page has already
// rendered is remembered. Bounded at 200: a reconnect never replays (the
// server starts a new connection at the head), so this only ever holds a
// short recent history, and a page left open for days must not grow a set
// that never forgets.
const NOTICE_MEMORY = 200;
// How many notices stay on screen. Three, newest on top: the strip is meant
// to be readable at a glance, not to be a log; `kb ev` is the log.
const NOTICE_SHOWN = 3;
const seenNotices = new Set();
let liveConnects = 0;
// A decision is one click and no page load, because the list is long: a
// reload after every answer would throw the reader back to the top of it.
// The card posts its own form, is replaced in place by its receipt, and the
// open count drops by one.
const CHOICE_KEYS = ['1', '2', '3', '4'];
// How long a decision may be in flight before the button says so a second
// time. Chosen to be longer than any answer that is actually going to land:
// a local board answers in milliseconds and the proxy in tens of them, so
// eight seconds means something is wrong rather than something is slow.
const STILL_SENDING_AFTER = 8000;
// A decision, or an undo, that has been posted and not yet answered. The
// card it is on must survive a projection swap, so it counts as an answer in
// progress: the server renders no receipt for a decision it has not recorded
// yet, so swapping the card away mid-flight would leave the operator looking
// at a list with no trace of the click they just made.
const sendingInFlight = () => Boolean(document.querySelector('[data-state=sending]'));
// An answer in progress anywhere on the page: words typed into a reply, a
// verdict picked for a free-text answer, or a decision already posted and
// still in flight. All three are lost the moment the projection is swapped
// -- the server renders every card empty, with no verdict checked -- so all
// three hold the swap. A picked verdict with nothing typed yet is an answer
// being written, not an idle page.
const answerInProgress = () =>
  [...document.querySelectorAll('textarea[name=reply]')].some(el => el.value.trim().length > 0)
  || Boolean(document.querySelector('input[name=outcome]:checked'))
  || sendingInFlight();
// What a click looks like from the instant it is made until the board
// answers (George, 2026-09-17: "I want to know that what I clicked on
// actually did something instead of having no feedback like right now").
//
// The pressed control says what it is doing ON ITS OWN FILL, and nothing is
// disabled: a disabled button is greyed out, which reads as an answer that
// can no longer be given, and it is the one control that cannot report its
// own refusal. The OTHER answers step back instead -- the card carries
// `data-state=sending` and the stylesheet dims every answer but the pressed
// one -- the card is marked busy for a screen reader and for the swap guard,
// and the live line says `sending`. The pressed control's markup is put back
// verbatim on a refusal -- the label, the key badge and all -- because a
// button that came back reading `Sending…` would be a dead button.
//
// A second click cannot post twice: `decide` and `undoDecision` each hold
// their own in-flight flag, which is a guard on the action rather than on
// the control that reaches it.
//
// `settle` is for a decision that landed and is about to be replaced by its
// receipt: there is nothing left to restore, only the timer to stop.
function beginSending(card, pressed, label, stillLabel) {
  const markup = pressed ? pressed.innerHTML : null;
  card.dataset.state = 'sending';
  card.setAttribute('aria-busy', 'true');
  if (pressed) { pressed.dataset.pressed = ''; pressed.textContent = label; }
  setLive('sending');
  const timer = pressed
    ? setTimeout(() => { pressed.textContent = stillLabel; }, STILL_SENDING_AFTER)
    : null;
  const settle = () => { if (timer !== null) clearTimeout(timer); };
  return {
    settle,
    revert: () => {
      settle();
      card.removeAttribute('aria-busy');
      delete card.dataset.state;
      if (pressed) { delete pressed.dataset.pressed; pressed.innerHTML = markup; }
    },
  };
}
const cardOf = node => (node && node.closest ? node.closest('article.item') : null);
// What the composer says when it refuses to post a half-written answer, in
// the page's language.
//
// Two voices, deliberately. This one is the CARD refusing to send an answer
// it can see is half-written, so it speaks to somebody holding a phone: the
// picker's own verdicts and the reply field's own name, no flags for a
// command line they are not using. What the ROUTE or the network said is
// quoted verbatim instead, flags and all, because that is a different thing
// being reported and the operator may have to act on the exact words. The
// route's wording for this same case is `incomplete_answer_refusal`, which
// the CLI and the MCP tool share and which a no-script POST still lands on.
const INCOMPLETE_ANSWER = 'Your own answer needs both halves: pick a verdict (approve, reject, defer or other) and write your reply.';
// The free-text answer carries a verdict or it is not an answer, and the hint
// keeps asking for both halves until both are there. It does NOT say which
// half is missing -- it is one fixed sentence; what identifies the missing
// half is the cursor, which a refused submit moves onto it. The button stays
// live either way: a disabled button is the one control that cannot report
// its own refusal, and it was the card's only feedback channel.
// `aria-disabled` is not set for the same reason -- the control does respond,
// in the card's own words, so announcing it as unavailable would be a lie.
// The release shows up exactly while there is a verdict to release, and a
// refusal never outlives what it refused.
function syncAnswer(form) {
  const text = form.querySelector('textarea[name=reply]');
  const hint = form.querySelector('[data-hint]');
  if (!text || !hint) return;
  const verdict = Boolean(form.querySelector('input[name=outcome]:checked'));
  const ready = verdict && text.value.trim().length > 0;
  hint.hidden = ready;
  const clear = form.querySelector('[data-clear]');
  if (clear) clear.hidden = !verdict;
  // A picked verdict holds the live projection for the whole page, and the
  // only way to release it by thumb is the button inside this fold. So the
  // fold cannot close over one: the hold would be unreachable, which is the
  // frozen page the release exists to prevent.
  const custom = form.querySelector('details[data-custom]');
  if (custom && verdict) custom.open = true;
  if (ready) clearRefusal(form);
}
// Only the composer's own pre-flight sentence, never the board's. What the
// route or the network said is not the operator's to type away. Both callers
// here are about the composer's own: either it has stopped being true, or the
// answer it was asking for has just been abandoned.
function clearRefusal(form) {
  const refusal = form.querySelector('[data-refusal=incomplete]');
  if (!refusal) return;
  undescribeRefusal(cardOf(form) || form, refusal.id);
  refusal.remove();
}
// The way back out of an answer that was started and is not wanted. HTML
// offers no way to un-check a radio group, and a picked verdict holds the
// live projection for the WHOLE page while this release sits on one card:
// the swap replaces all of `<main>`, so there is nothing narrower than the
// page to hold, and the only honest place for the button is beside the
// verdict it clears. A card scrolled out of view holds the page with its own
// release off screen, and the connection line says nothing about it: it is
// the socket's line, not the page's commentary.
// The release therefore has to be reachable by a thumb as well as a
// keyboard: this control, and `Escape` inside the card.
//
// The typed words are NOT cleared -- losing them is the thing the hold exists
// to prevent. The composer's own refusal IS, even though it is still true:
// it was asking for the two halves of an answer the operator has just said
// they are not giving, and a red refusal standing over an abandoned answer
// reads as a failure rather than a prompt. Nothing is lost by removing it --
// the hint under the submit comes straight back and asks for the same two
// halves in calmer words. What the BOARD refused stays.
function clearVerdict(form) {
  form.querySelectorAll('input[name=outcome]:checked').forEach(input => { input.checked = false; });
  clearRefusal(form);
  syncAnswer(form);
}
// --- the deck ---------------------------------------------------------------
// One card on screen, and the next one a keystroke away (George, 2026-09-17:
// "we are looking at one item at a time ... and then we quickly advance
// through item by item without scrolling, e.g. when i hit 1 it sends it off
// and gives me the next item via a quick animation").
//
// The queue is the DOM order the server rendered, which is the priority
// order every other surface uses: the deck decides which of those cards is
// on screen and nothing else. A card whose decision is in flight is
// `data-sent` -- out of the queue, still in the document -- because a
// refusal has to bring it back to the slot it left.
const DECK_SLIDE = 140;
const deckNode = () => document.querySelector('[data-deck-cards]');
const deckQueue = () => {
  const deck = deckNode();
  return deck ? [...deck.querySelectorAll('article.item:not([data-sent])')] : [];
};
const currentCard = () => document.querySelector('article.item[data-current]');
let deckIndex = 0;
let deckShown = null;
// The card that is animating out. It is out of the queue but stays visible
// until the animation lands, so the swap is a move rather than a blink.
let deckGhost = null;
let deckGhostTimer = null;
function bindDeck() {
  const deck = deckNode();
  if (!deck) return;
  const queue = deckQueue();
  deckIndex = Math.min(Math.max(deckIndex, 0), Math.max(0, queue.length - 1));
  const showing = queue[deckIndex] || null;
  [...deck.querySelectorAll('article.item')].forEach(card => {
    if (card === showing) card.dataset.current = ''; else delete card.dataset.current;
    card.hidden = card !== showing && card !== deckGhost;
  });
  if (showing) {
    // The long form is this card's body and the region the deck scrolls, so
    // on the deck it is open. The fold exists for a list of 133 cards; a
    // deck is a list of one.
    const full = showing.querySelector('details.full');
    if (full) full.open = true;
    // ...and the raiser's context is its first block. The server renders it
    // above the answers, which is the order ADR-042 §5 fixes and the order
    // a scriptless browser reads; the DECK moves it inside the body region
    // so the two share one capped, fading scroller. Left as a free block in
    // the card's column it pushed the recommended answer off the first
    // screen of a phone -- 790 characters of context and the answer a deck
    // exists to offer was two scrolls away (George, 2026-09-18).
    //
    // Idempotent by construction: once moved the paragraph is no longer a
    // child of the card, so a re-bind finds nothing to move. A card the
    // server re-rendered arrives with it back in place and is moved again.
    const body = full && full.querySelector('.body');
    const context = showing.querySelector('p.context');
    if (body && context && context.parentElement === showing) body.prepend(context);
    // The trailing meta goes UP, onto the end of the eyebrow: one line of
    // orientation instead of two. It trailed, which on the deck meant the
    // priority pill sat directly under the dissolving last line of the long
    // form with two rem of empty band below it -- the pill over clipped text
    // George objected to on 2026-09-17, moved a few pixels down (George,
    // 2026-09-18, on the v6 screenshots). Above the question it cannot be
    // read as an answer, which is why it left the head of the card in the
    // first place, and beside the eyebrow it is what it is: who asked, where,
    // when, and how urgent.
    //
    // Same bargain as the context: the SERVER still renders the meta line
    // last (ADR-042 §5), so a scriptless page keeps both paragraphs where
    // they were, and the deck's own script joins them. Idempotent for the
    // same reason -- the node is gone once its contents have moved.
    const eyebrow = showing.querySelector('p.eyebrow');
    const meta = showing.querySelector('p.meta');
    if (eyebrow && meta && meta.parentElement === showing) {
      eyebrow.append(' · ');
      while (meta.firstChild) eyebrow.append(meta.firstChild);
      meta.remove();
    }
    // The advance runs when the card on screen CHANGED, and never because a
    // projection was refreshed under an unchanged one: that was the blink
    // (George, 2026-09-17: "it keeps blinking"). `deckShown` survives a
    // refresh because the node does.
    if (showing !== deckShown) {
      showing.classList.add('entering');
      setTimeout(() => showing.classList.remove('entering'), DECK_SLIDE);
    }
  }
  deckShown = showing;
  // An empty queue says so at once, rather than after the board answers.
  // Deciding the last card used to leave a blank deck for the length of a
  // round trip: the card was gone the instant it was posted and the
  // server's empty state was a projection away. The server ships the
  // sentence as a template so there is one wording, not two.
  const template = deck.querySelector('template[data-empty]');
  const placed = deck.querySelector('[data-deck-empty]');
  if (!queue.length && !placed && !deck.querySelector('.empty') && template) {
    const empty = template.content.firstElementChild.cloneNode(true);
    empty.dataset.deckEmpty = '';
    deck.append(empty);
  }
  // ...and takes it back when a refusal, or an undo, puts a card back.
  if (queue.length && placed) placed.remove();
  const position = document.querySelector('.progress');
  // Nothing left to count, and `0 left` over an empty state says it twice.
  if (position) position.hidden = queue.length === 0;
  syncHistoryCount();
}
// Keep the keyboard on the card the deck is showing, so 1-4 keeps deciding
// without a click first. Focus is only taken when it is not already
// somewhere the operator put it: a swap must never pull the cursor out of a
// note being written, out of the menu, or out of the side history.
function focusCurrent() {
  const card = currentCard();
  if (!card) return;
  const focus = document.activeElement;
  if (focus && focus.isConnected && focus !== document.body && focus.matches
    && (card.contains(focus) || focus.closest('[data-drawer], [data-side]') || focus.matches('input, textarea'))) return;
  card.focus();
}
function ghostCard(card, direction) {
  if (deckGhostTimer !== null) clearTimeout(deckGhostTimer);
  if (deckGhost && deckGhost !== card) settleGhost();
  deckGhost = card;
  card.classList.add(direction === 'back' ? 'leaving-back' : 'leaving');
  deckGhostTimer = setTimeout(settleGhost, DECK_SLIDE);
}
function settleGhost() {
  if (deckGhostTimer !== null) clearTimeout(deckGhostTimer);
  deckGhostTimer = null;
  const card = deckGhost;
  deckGhost = null;
  if (!card) return;
  card.classList.remove('leaving', 'leaving-back');
  if (card.isConnected) bindDeck();
}
// Step through the queue without recording anything: `ArrowRight` to the
// next card, `ArrowLeft` back to the one before it.
function deckMove(step) {
  const queue = deckQueue();
  if (queue.length < 2) return;
  const leaving = currentCard();
  const next = Math.min(Math.max(deckIndex + step, 0), queue.length - 1);
  if (next === deckIndex) return;
  deckIndex = next;
  if (leaving) ghostCard(leaving, step < 0 ? 'back' : 'forward');
  bindDeck();
  focusCurrent();
}
// `s` puts this card at the back of the queue and shows the next one, with
// nothing recorded: the answer needs thinking about and there are 132 other
// cards. The position does not move -- the same slot now holds the next
// card -- and the skipped card comes round again at the end.
function deckSkip() {
  const deck = deckNode();
  const card = currentCard();
  if (!deck || !card || deckQueue().length < 2) return;
  deck.append(card);
  ghostCard(card, 'forward');
  bindDeck();
  focusCurrent();
}
// A decision leaves the queue the instant it is posted, which is the whole
// point: the next card is on screen while the board is still answering. The
// posted card stays in the document, hidden and out of the queue, so a
// refusal can bring it back to exactly the slot it left.
function deckSend(card, label) {
  if (!deckNode() || !card.closest('[data-deck-cards]')) return null;
  const slot = deckQueue().indexOf(card);
  card.dataset.sent = '1';
  const pending = showPending(card, label);
  ghostCard(card, 'forward');
  if (slot >= 0) deckIndex = slot;
  bindDeck();
  focusCurrent();
  return {
    restore: () => {
      if (pending) pending.remove();
      delete card.dataset.sent;
      if (deckGhost === card) settleGhost();
      card.classList.remove('leaving', 'leaving-back');
      const back = deckQueue().indexOf(card);
      if (back >= 0) deckIndex = back;
      bindDeck();
      const returned = currentCard();
      if (returned) returned.focus();
    },
  };
}
// Put the deck on one item by id: what an undo needs, because the card it
// brought back is the card the operator is now looking at.
function deckShow(id) {
  if (!deckNode()) return;
  const at = deckQueue().findIndex(card => card.dataset.item === id);
  if (at >= 0) { deckIndex = at; bindDeck(); }
}
// --- the side: toasts above, this sitting's decisions below ------------------
// Every receipt this tab produced, newest first, each with its undo. A
// receipt used to sit where the card had been, which on a deck is a place
// nobody is looking any more -- the next card is there.
function historyRow(row) {
  const history = document.querySelector('[data-history]');
  if (!history) return false;
  const heading = history.querySelector('h2');
  if (heading) heading.after(row); else history.prepend(row);
  return true;
}
// What the history says while the board is still answering. It is NOT a
// receipt: nothing is recorded yet, so it carries no undo and no receipt
// tag. Offering to reverse a decision that may still be refused would be
// offering to reverse something that never happened.
//
// It carries no `role` of its own either: the connection line is the page's
// one `role=status` and the toast strip is its one `role=log`, so a third
// live region here would be two channels claiming the same job.
function showPending(card, label) {
  const row = document.createElement('p');
  row.className = 'pending';
  row.dataset.pending = card.dataset.item;
  row.textContent = `Sending… ${label}`;
  return historyRow(row) ? row : null;
}
function syncHistoryCount() {
  const badge = document.querySelector('[data-history-count]');
  if (badge) badge.textContent = String(document.querySelectorAll('[data-history] [data-receipt]').length);
}
// --- the menu and the side drawer -------------------------------------------
// Eight destinations and a search box used to stand across the top of every
// page, which on a 390-wide screen is a scrolling strip of tabs above the
// thing being read and, on the deck, the whole top edge of the screen spent
// on navigation nobody uses while deciding. One button now, and `m`.
const drawerOpen = () => {
  const drawer = document.querySelector('[data-drawer]');
  return Boolean(drawer) && !drawer.hidden;
};
const historyOpen = () => document.body.hasAttribute('data-history-open');
function syncBackdrop() {
  const backdrop = document.querySelector('[data-backdrop]');
  if (backdrop) backdrop.hidden = !(drawerOpen() || historyOpen());
}
function setDrawer(open) {
  const drawer = document.querySelector('[data-drawer]');
  const button = document.querySelector('[data-menu]');
  if (!drawer) return;
  drawer.hidden = !open;
  if (button) button.setAttribute('aria-expanded', open ? 'true' : 'false');
  syncBackdrop();
  if (open) { const first = drawer.querySelector('a'); if (first) first.focus(); }
  else if (button && drawer.contains(document.activeElement)) button.focus();
}
function setHistory(open) {
  const button = document.querySelector('[data-history-toggle]');
  if (!document.querySelector('[data-side]')) return;
  document.body.toggleAttribute('data-history-open', open);
  if (button) button.setAttribute('aria-expanded', open ? 'true' : 'false');
  syncBackdrop();
}
document.addEventListener('click', event => {
  if (!event.target.closest) return;
  if (event.target.closest('[data-menu]')) { setDrawer(!drawerOpen()); return; }
  if (event.target.closest('[data-history-toggle]')) { setHistory(!historyOpen()); return; }
  if (event.target.closest('[data-backdrop]')) { setDrawer(false); setHistory(false); return; }
  // A destination chosen is a menu finished with, and the click still
  // navigates: the drawer only decides where the links are kept.
  if (event.target.closest('[data-drawer] a')) setDrawer(false);
});
// The note field starts one line tall and grows to four as it is written:
// on a deck it sits in the panel with the answers, where an empty box three
// lines deep is three lines of the screen spent on nothing.
const NOTE_LINES = 4;
function growNote(field) {
  field.style.height = 'auto';
  const line = parseFloat(getComputedStyle(field).lineHeight) || 20;
  const frame = field.offsetHeight - field.clientHeight;
  field.style.height = `${Math.min(field.scrollHeight, line * NOTE_LINES + frame + 16) + frame}px`;
}
function bindCards() {
  // First, and before anything on a card is touched: what the server served
  // is what a later refresh is diffed against.
  snapshotCards();
  document.querySelectorAll('form.decide').forEach(form => {
    if (form.dataset.cardBound) return;
    form.dataset.cardBound = '1';
    form.addEventListener('input', () => syncAnswer(form));
    form.addEventListener('change', () => syncAnswer(form));
    form.addEventListener('click', event => {
      if (event.target.closest('[data-clear]')) clearVerdict(form);
    });
    form.addEventListener('submit', event => { event.preventDefault(); decide(form, event.submitter); });
    const note = form.querySelector('textarea[name=reply]');
    if (note && document.querySelector('[data-deck]')) {
      form.addEventListener('input', () => growNote(note));
      growNote(note);
    }
    syncAnswer(form);
  });
}
// The form's fields plus the button that was pressed. A submit button is
// only submitted when it is the submitter, and the pressed button is the
// whole decision, so it is named here rather than left to the newer
// FormData(form, submitter) overload that a phone's browser may not have.
function decisionBody(form, submitter) {
  const body = new URLSearchParams(new FormData(form));
  if (submitter) body.set('decision', submitter.value);
  return body;
}
// What the receipt says, in the words the ledger will use for the same
// decision: the choice's own label, or the custom answer with its verdict.
function decidedLabel(form, submitter) {
  if (!submitter) return 'Custom answer';
  if (submitter.value !== 'custom') return submitter.dataset.label;
  const outcome = form.querySelector('input[name=outcome]:checked');
  return outcome ? `Custom answer, recorded as ${outcome.value}` : 'Custom answer';
}
// Which of the four outcomes was just recorded, so the receipt can encode it
// as its own left rule: an authored choice carries its outcome on the button
// that was pressed, and the free-text answer carries the verdict that was
// picked for it.
function decidedOutcome(form, submitter) {
  if (submitter && submitter.value !== 'custom') {
    const named = [...submitter.classList].find(name => name.startsWith('outcome-'));
    if (named) return named.slice('outcome-'.length);
    return 'other';
  }
  const outcome = form.querySelector('input[name=outcome]:checked');
  return outcome ? outcome.value : 'other';
}
// Whether the body about to be posted carries words as well as a choice. The
// custom answer IS those words and its label already says so, so only an
// authored key earns the extra sentence.
function replySent(body) {
  return body.get('decision') !== 'custom' && (body.get('reply') || '').trim().length > 0;
}
function showReceipt(card, label, noted, outcome) {
  const receipt = document.createElement('p');
  // No flash: what says the board answered is that the row is THERE, with
  // the outcome it recorded on its own left rule. A one-shot highlight was
  // a second motion competing with the card advance.
  receipt.className = `receipt outcome-${outcome}`;
  receipt.dataset.testid = 'deck-receipt';
  receipt.dataset.receipt = card.dataset.item;
  receipt.dataset.item = card.dataset.item;
  receipt.dataset.project = card.dataset.project || '';
  const decided = document.createElement('span');
  decided.className = 'decided';
  decided.textContent = noted ? `Decided: ${label}. Your reply is recorded.` : `Decided: ${label}.`;
  const undo = document.createElement('button');
  undo.type = 'button';
  undo.className = 'undo-button';
  undo.dataset.undo = '';
  undo.textContent = 'Undo';
  receipt.append(decided, document.createTextNode(' '), undo);
  lastDecided = receipt;
  const following = card.nextElementSibling;
  const held = card.contains(document.activeElement);
  // On the deck the card has already left the queue and the next one is on
  // screen, so a receipt where the card used to be is a receipt nobody is
  // looking at. It goes to the side history instead, newest first, and the
  // row that said `Sending…` becomes it.
  const pending = document.querySelector(`[data-pending="${cssEscape(card.dataset.item)}"]`);
  const filed = pending ? (pending.replaceWith(receipt), true) : historyRow(receipt);
  if (filed) card.remove(); else card.replaceWith(receipt);
  const counter = document.querySelector('[data-open-count]');
  if (counter) counter.textContent = Math.max(0, Number(counter.textContent) - 1);
  if (filed) {
    bindDeck();
    focusCurrent();
    return;
  }
  // Keep the keyboard where the work is: the next card, so 1-4 keeps deciding.
  if (held && following && following.matches('article.item')) following.focus();
}
// The most recent decision on the page, so `u` has a target after the focus
// has already moved on to the next card. Only ever a live DOM node.
let lastDecided = null;
// Reopen one decided item through the same trusted-edge route a reply uses.
// The reopen note is fixed prose on the server; an undo that demanded words
// would be a dialog wearing a button.
async function undoDecision(row) {
  const id = row.dataset.item;
  const project = row.dataset.project;
  if (!id || !project || row.dataset.undoing) return;
  row.dataset.undoing = '1';
  const sending = beginSending(row, row.querySelector('[data-undo]'), 'Undoing…', 'Still undoing…');
  try {
    const response = await fetch(
      `/attention/${encodeURIComponent(project)}/${encodeURIComponent(id)}/reopen`,
      {method: 'POST', credentials: 'same-origin', redirect: 'manual'}
    );
    if (response.type === 'opaqueredirect' || response.ok) {
      sending.settle();
      noteOwnChange(project);
      row.remove();
      syncHistoryCount();
      // The item is open again: pull the fresh projection so it reappears as
      // a card (on Needs you) or leaves this list (on Recent decisions), then
      // put the deck and the keyboard back on the returned card so 1-4 keeps
      // working. The deck has to be moved onto it first: a card the deck is
      // not showing is hidden, and a hidden card cannot take focus.
      await refreshProjection();
      deckShow(id);
      const back = document.querySelector(`article.item[data-item="${cssEscape(id)}"]`);
      if (back) back.focus();
      applyNotice({type: 'notice', key: `undone-${id}-${Date.now()}`, what: `Brought back ${id}. It is open again.`});
    } else {
      sending.revert();
      const page = new DOMParser().parseFromString(await response.text(), 'text/html');
      const refused = page.querySelector('.error');
      showRowRefusal(row, refused ? refused.textContent : `The board refused to bring back ${id} (${response.status}).`);
    }
  } catch (error) {
    sending.revert();
    showRowRefusal(row, `The undo did not reach the board. Try again.`);
  } finally {
    delete row.dataset.undoing;
    // Every exit, not just the one that worked: a refused reopen, or one
    // that never reached the board, is no longer `sending` either, and the
    // line used to go on saying so until the socket happened to speak.
    settleLive();
  }
}
// A refusal on a decided row has no form to live in, so it lives on the row.
//
// It is not a live region: the page has exactly two announcement channels --
// the socket's status line and the toast log -- and a third one that shouts
// whatever the board said would be the page narrating itself, which WEB-52
// forbids. A refusal is inline text tied to the thing that was refused, so
// the row points at it with `aria-describedby` and a reader reaching the row
// reads the sentence as part of it.
function showRowRefusal(row, text) {
  let refusal = row.querySelector('[data-refusal=board]');
  if (!refusal) {
    refusal = document.createElement('p');
    refusal.className = 'error';
    refusal.setAttribute('data-refusal', 'board');
    refusal.id = `refusal-row-${row.dataset.item || 'row'}`;
    row.append(refusal);
  }
  refusal.textContent = text;
  row.setAttribute('aria-describedby', refusal.id);
}
// `CSS.escape` is the standard name; the fallback keeps the selector honest
// on a browser that predates it, and ids are slugs anyway.
const cssEscape = value => (window.CSS && CSS.escape) ? CSS.escape(value) : value.replace(/[^a-zA-Z0-9_-]/g, '\\$&');
// One refusal line per KIND, and no kind ever takes another's line.
// `incomplete` is the composer's own sentence, true only while a half is
// missing, and cleared the moment it stops being true or the answer is
// abandoned. `board` is what the route or the network said about an attempt
// that was actually made: no amount of typing can make it untrue and only
// another attempt may replace it.
//
// Separate nodes because one shared node was a hole. The composer reused and
// retagged it, so a pre-flight refusal OVERWROTE the board's sentence and the
// next keystroke then removed it as the composer's own -- leaving a card that
// looked as though the board had never refused anything, with nothing
// recorded anywhere.
// It is not a live region either, for the reason the row's refusal is not:
// two channels, and a refusal is not an announcement. It is described text,
// so it carries an id and the caller points whatever it focuses -- the half
// of the answer that is missing, or the card the board refused -- at that id
// with `aria-describedby`. The node is returned for that wiring.
function showRefusal(form, text, kind) {
  const card = cardOf(form);
  let refusal = form.querySelector(`[data-refusal="${kind}"]`);
  if (!refusal) {
    refusal = document.createElement('p');
    refusal.className = 'error';
    refusal.setAttribute('data-refusal', kind);
    refusal.id = `refusal-${kind}-${(card && card.dataset.item) || 'card'}`;
    form.append(refusal);
  }
  refusal.textContent = text;
  return refusal;
}
// A refusal describes the control the operator is sent to, and stops
// describing it the moment the sentence is gone: a reader that still reads
// out a refusal which is no longer on the page is worse than one that never
// read it.
function describeRefusal(target, refusal) {
  if (target && refusal) target.setAttribute('aria-describedby', refusal.id);
}
function undescribeRefusal(scope, id) {
  if (!scope || !id) return;
  scope
    .querySelectorAll(`[aria-describedby="${cssEscape(id)}"]`)
    .forEach(element => element.removeAttribute('aria-describedby'));
}
async function decide(form, submitter) {
  const card = cardOf(form);
  if (!card || form.dataset.deciding) return;
  // A free-text answer missing a half is refused here, in the card's own
  // words, with the focus moved to the half that is missing. Nothing is
  // posted. The alternative was a disabled button, which produced no event,
  // no request and no sentence at all: the click simply did nothing. An
  // authored choice carries its own verdict and is never held here, with or
  // without a note.
  if (submitter && submitter.value === 'custom') {
    const text = form.querySelector('textarea[name=reply]');
    const verdict = form.querySelector('input[name=outcome]:checked');
    if (!verdict || !text || text.value.trim().length === 0) {
      const refusal = showRefusal(form, INCOMPLETE_ANSWER, 'incomplete');
      const missing = verdict ? text : form.querySelector('input[name=outcome]');
      if (missing) {
        describeRefusal(missing, refusal);
        missing.focus();
      }
      return;
    }
  }
  // An authored choice carries its own verdict and the route forwards no
  // picker value onto it, so a verdict the operator left picked is not part
  // of THIS decision. The card must not go on showing it as though it were:
  // the release runs before the body is built, so what is posted, what the
  // ledger records and what is on screen say the same thing whether the
  // attempt lands or comes back refused.
  if (submitter && submitter.value !== 'custom') clearVerdict(form);
  form.dataset.deciding = '1';
  const body = decisionBody(form, submitter);
  const label = decidedLabel(form, submitter);
  const outcome = decidedOutcome(form, submitter);
  const noted = replySent(body);
  const sending = beginSending(card, submitter, 'Sending…', 'Still sending…');
  // The deck hands over to the next card now rather than when the board
  // answers: `1` sends this one off and shows the next one, and what this
  // one is doing is said in the side history instead of on a card that is
  // no longer on screen.
  const advanced = deckSend(card, label);
  try {
    const response = await fetch(form.action, {
      method: 'POST',
      body,
      credentials: 'same-origin',
      redirect: 'manual',
    });
    // The route answers a recorded decision with a redirect and every
    // refusal with a page, so an opaque redirect is the receipt. A refusal
    // is shown in the card's own words -- a card left open while the item
    // was rewritten names a choice the row no longer carries, and that is
    // refused by name rather than mapped onto whatever now sits there.
    if (response.type === 'opaqueredirect' || response.ok) {
      sending.settle();
      noteOwnChange(card.dataset.project);
      showReceipt(card, label, noted, outcome);
      settleLive();
      return;
    }
    // Nothing was recorded, so the card comes all the way back before it is
    // told why: a refusal under a disabled button reads as a card that can
    // no longer be answered at all.
    sending.revert();
    if (advanced) advanced.restore();
    settleLive();
    const page = new DOMParser().parseFromString(await response.text(), 'text/html');
    const refused = page.querySelector('.error');
    describeRefusal(card, showRefusal(form, refused ? refused.textContent : `The board refused this decision (${response.status}).`, 'board'));
  } catch (error) {
    sending.revert();
    if (advanced) advanced.restore();
    settleLive();
    describeRefusal(card, showRefusal(form, 'The decision did not reach the board. Try again.', 'board'));
  } finally {
    delete form.dataset.deciding;
  }
}
// Every card node the server has rendered into this page, as it was served.
// It is the only way to know later whether a freshly fetched card SAYS
// anything new: by the time a refresh arrives the live node carries the
// deck's own attributes -- `data-current`, `hidden`, an opened fold, a bound
// form -- so its current markup can never be compared with the server's.
//
// Taken before anything is bound, which is why `bindCards` calls it first.
// A property rather than an attribute: it travels with the node and is not
// served to anybody.
function snapshotCards() {
  document.querySelectorAll('article.item').forEach(card => {
    if (card.servedMarkup === undefined) card.servedMarkup = card.outerHTML;
  });
}
async function refreshProjection() {
  // A decision already in flight holds the swap too, and while it does the
  // live line goes on saying what the page is doing rather than what the
  // socket is waiting for: `sending` is the answer to the click that was
  // just made, and it must not be overwritten a tick later. A draft being
  // written says nothing at all -- the connection line is the connection's,
  // and the four words it may say are the socket's states.
  if (answerInProgress()) { if (sendingInFlight()) setLive('sending'); return; }
  // A card that is halfway out of the deck is not a page to swap: the
  // animation is 140ms and the node it is moving is one of the nodes being
  // replaced, so the swap waits for it to land rather than deleting it
  // mid-flight.
  if (deckGhost) await new Promise(resolve => setTimeout(resolve, DECK_SLIDE));
  const response = await fetch(location.pathname + location.search, {credentials: 'same-origin'});
  if (!response.ok) throw new Error(`refresh ${response.status}`);
  const next = new DOMParser().parseFromString(await response.text(), 'text/html').querySelector('main');
  // The same gate again, and it has to sit exactly here. An answer can be
  // started while the projection is in flight -- the first gate ran a
  // network round trip ago -- and everything below MOVES live nodes into
  // the detached document: the notices strip, then every receipt. Bailing
  // out after that point would delete them instead of preserving a draft.
  if (answerInProgress()) { if (sendingInFlight()) setLive('sending'); return; }
  // Where the deck is, so the swap puts it back: the same item if it is
  // still open, and otherwise the same slot in the queue -- a refresh must
  // not throw the operator back to the top of a queue of 133.
  const showing = currentCard();
  const showingItem = showing ? showing.dataset.item : null;
  const slot = deckIndex;
  // ...and WHERE IN THE CARD the reader is, for the same reason one step
  // smaller. Since the panel stopped being its own scroller the card's own
  // column carries the whole card (2026-09-18), so a card the server
  // re-rendered comes back as a fresh node scrolled to the top: the
  // paragraph being read, the answers, the note, all of it jumps while
  // George is looking at it. Two positions are enough to put him back --
  // the card's column and the body region inside it -- and they are
  // restored only if the SAME item is still the card on screen.
  const showingScroll = showing ? showing.scrollTop : 0;
  const showingBody = showing ? showing.querySelector('.full .body') : null;
  const showingBodyScroll = showingBody ? showingBody.scrollTop : 0;
  // The card the operator is looking at is NOT replaced when the server has
  // nothing new to say about it (George, 2026-09-17: "it keeps blinking").
  // Every notice used to swap all of `<main>`, which meant a brand new node
  // for the same item, which meant the advance ran again on a card that had
  // not moved -- two or three times a minute, under the question being read.
  //
  // So the fresh cards are diffed by `data-item` against what the server
  // served for the same item, and an identical one hands its place back to
  // the live node: same object, same scroll position, same focus, no
  // animation. A card whose markup DID change is replaced, because then the
  // page is showing something the board no longer says.
  const live = new Map();
  document.querySelectorAll('article.item[data-item]').forEach(card => {
    if (card.servedMarkup !== undefined) live.set(card.dataset.item, card);
  });
  next.querySelectorAll('article.item[data-item]').forEach(incoming => {
    const held = live.get(incoming.dataset.item);
    if (held && held.servedMarkup === incoming.outerHTML) incoming.replaceWith(held);
  });
  // The side carries the whole sitting across: the notices and every
  // receipt with its undo live in it, so one node moves instead of a strip
  // plus a list of receipts threaded back in one at a time.
  const side = document.querySelector('[data-side]');
  const fresh = side ? next.querySelector('[data-side]') : null;
  if (side && fresh) fresh.replaceWith(side);
  if (!side) {
    const strip = document.querySelector('[data-notices]');
    if (strip) next.prepend(strip);
    // A receipt outlives the projection it was decided in. The row is gone
    // from the new one, so the receipts move to the head of the list in the
    // order they were decided; an undo that vanished a second after the
    // click would be no undo at all.
    let cursor = next.querySelector('.count');
    document.querySelectorAll('[data-receipt]').forEach(receipt => {
      if (cursor) cursor.after(receipt); else next.prepend(receipt);
      cursor = receipt;
    });
  }
  document.querySelector('main').replaceWith(next);
  bindCards();
  deckIndex = slot;
  // `deckShown` is NOT cleared: the card that was showing is the same node
  // when the server said nothing new about it, and clearing this was the
  // other half of the blink -- `bindDeck` would re-enter the same card.
  bindDeck();
  if (showingItem) deckShow(showingItem);
  // The reader goes back where they were, in the card as well as in the
  // queue. After `deckShow`, because a card that is still hidden has no
  // scrollport to put a position into, and only for the same item: a
  // different card is a different piece of writing and starts at its top.
  const shown = currentCard();
  if (shown && showingItem && shown.dataset.item === showingItem) {
    shown.scrollTop = showingScroll;
    const body = shown.querySelector('.full .body');
    if (body) body.scrollTop = showingBodyScroll;
  }
  focusCurrent();
  setLive('live');
}
function noticeStrip() {
  let strip = document.querySelector('[data-notices]');
  if (!strip) {
    strip = document.createElement('div');
    strip.className = 'notices';
    strip.setAttribute('data-notices', '');
    // A log, not a status: what arrived is a list of entries, and the
    // connection line is the page's one status.
    strip.setAttribute('role', 'log');
    strip.setAttribute('aria-live', 'polite');
    document.querySelector('main').prepend(strip);
  }
  return strip;
}
// What this tab has just done to a board, so the socket does not report the
// operator to themselves. A decision made here is already on screen as a
// receipt in the side history; the same change arriving a second later as
// `Attention resolved` is noise over the card being read next (George,
// 2026-09-17). Only the words this page can cause are suppressed, only on
// the board it just wrote to, and only for half a minute: a change somebody
// else makes is still news.
//
// The board and the words are the whole match, because that is all the
// notice frame carries -- and the frame's shape is pinned on purpose. An id
// on the wire would be a new field on every notice for the sake of one
// tab's echo.
//
// The trade, named: a row settled by SOMEBODY ELSE on the same board within
// ten seconds of a decision made here is silent. Ten seconds is about one
// refresh cycle, so that change still arrives -- as the projection swap that
// takes its card out of the queue -- it just arrives without a sentence. The
// alternative was leaving every operator's own decision reported back to
// them a second after they made it, over the card they are reading next.
const ECHO_WINDOW = 10000;
const OWN_WORDS = ['Attention resolved', 'Attention reopened'];
const ownChanges = new Map();
const noteOwnChange = board => { if (board) ownChanges.set(board, Date.now()); };
const ownEcho = notice => {
  if (!notice.board || notice.type !== 'notice' || !OWN_WORDS.includes(notice.what)) return false;
  const at = ownChanges.get(notice.board);
  return typeof at === 'number' && Date.now() - at < ECHO_WINDOW;
};
function applyNotice(notice) {
  if (!notice || !notice.key || seenNotices.has(notice.key)) return false;
  seenNotices.add(notice.key);
  // Remembered, then dropped: an echo that is redelivered must not be
  // rendered the second time either.
  if (ownEcho(notice)) return false;
  while (seenNotices.size > NOTICE_MEMORY) seenNotices.delete(seenNotices.values().next().value);
  const row = document.createElement('p');
  row.className = notice.type === 'notice' ? 'notice' : 'notice summary';
  row.dataset.key = notice.key;
  if (notice.board) {
    const board = document.createElement('span');
    board.className = 'notice-board';
    board.textContent = notice.board;
    row.append(board);
  }
  const what = document.createElement('span');
  what.className = 'notice-what';
  what.textContent = notice.what || '';
  row.append(what);
  if (notice.task) {
    const link = document.createElement('a');
    link.href = `/task/${encodeURIComponent(notice.board)}/${encodeURIComponent(notice.task)}`;
    link.textContent = notice.title ? `${notice.task} ${notice.title}` : notice.task;
    row.append(link);
  }
  const dismiss = document.createElement('button');
  dismiss.type = 'button';
  dismiss.className = 'dismiss';
  dismiss.textContent = 'Dismiss';
  row.append(dismiss);
  const strip = noticeStrip();
  strip.prepend(row);
  while (strip.children.length > NOTICE_SHOWN) strip.lastElementChild.remove();
  toast(row);
  return true;
}
// A toast stays twenty seconds (George, 2026-09-17: "the toast is too
// fast"), and the clock stops while the pointer is on it or the keyboard is
// in it -- a notice that vanished out from under the eye reading it would be
// worse than one that stayed. It goes on a click anywhere on it, or on
// `Escape`; three stay on screen at once, newest first. `kb ev` is the log,
// and the list at `/all` keeps its strip without a clock at all.
const TOAST_LIFE = 20000;
function toast(row) {
  if (!document.querySelector('[data-deck]')) return;
  let timer = setTimeout(() => row.remove(), TOAST_LIFE);
  const hold = () => { if (timer !== null) { clearTimeout(timer); timer = null; } };
  const release = () => { if (timer === null) timer = setTimeout(() => row.remove(), TOAST_LIFE); };
  row.addEventListener('mouseenter', hold);
  row.addEventListener('mouseleave', release);
  row.addEventListener('focusin', hold);
  row.addEventListener('focusout', release);
}
// Every toast at once, because `Escape` is not aimed at one of them.
const dismissToasts = () => {
  const rows = [...document.querySelectorAll('[data-notices] .notice')];
  rows.forEach(row => row.remove());
  return rows.length > 0;
};
function connectLive() {
  const scheme = location.protocol === 'https:' ? 'wss' : 'ws';
  const socket = new WebSocket(`${scheme}://${location.host}/live`);
  socket.onopen = () => { liveSocketUp = true; setLive('live'); };
  socket.onmessage = event => {
    let frame;
    try { frame = JSON.parse(event.data); } catch (_) { return; }
    if (frame.type === 'notice' || frame.type === 'behind') { applyNotice(frame); return; }
    if (frame.type === 'ready') {
      // The server starts every connection at the head and says so. Only a
      // RECONNECT needs saying out loud: the operator who just lost a socket
      // is the one who would otherwise wonder what they missed.
      if (liveConnects++ > 0 && frame.noticesFrom === 'now') {
        applyNotice({type: 'reconnected', key: `reconnected-${liveConnects}`, what: 'Reconnected. Showing changes from now.'});
      }
      return;
    }
    // A refresh that did not land leaves the line saying what the socket
    // is, which is the only thing this line is allowed to say.
    if (frame.type === 'refresh') refreshProjection().catch(settleLive);
  };
  socket.onclose = () => { liveSocketUp = false; setLive('reconnecting'); setTimeout(connectLive, 1500); };
  socket.onerror = () => socket.close();
}
// A click anywhere on a notice takes it away: the dismiss button is the
// keyboard's way in and the label that says so, and the row itself is the
// thumb's.
document.addEventListener('click', event => {
  const row = event.target.closest ? event.target.closest('.notice') : null;
  if (row) row.remove();
});
// The undo button on a receipt (Needs you) and the submit on a decided row's
// form (Recent decisions) are the same action through the same route; the
// form keeps its real POST so a browser without script still undoes.
document.addEventListener('click', event => {
  const undo = event.target.closest('[data-undo]');
  if (!undo) return;
  const row = undo.closest('[data-receipt], article.decided');
  if (row) { event.preventDefault(); undoDecision(row); }
});
document.addEventListener('submit', event => {
  const form = event.target.closest('form.undo');
  if (!form) return;
  const row = form.closest('article.decided');
  if (row) { event.preventDefault(); undoDecision(row); }
});
// --- hover previews ---------------------------------------------------------
// Every `a[data-ref]` shows what it points at on hover, and the fragment it
// fetches can itself carry `data-ref` anchors, so previews nest: hovering a
// reference inside a preview opens the next one beside it. All listening is
// delegated to the document, which is what makes that true for content that
// did not exist when the page loaded. Focus opens the same preview, so the
// keyboard path is not a second-class citizen.
const PREVIEW_DELAY = 120;
const previewCache = new Map();
const openPreviews = [];
let previewTimer = null;
let previewCloseTimer = null;
const previewUrl = anchor => {
  try {
    const url = new URL(anchor.href, location.href);
    if (url.origin !== location.origin) return null;
    return `/preview${url.pathname}`;
  } catch (_) { return null; }
};
// Pop every popup that is not an ancestor of `keep`: hovering a new anchor
// in the page closes everything, hovering one inside a popup closes only the
// popups deeper than that one.
const closePreviews = keep => {
  while (openPreviews.length && !(openPreviews[openPreviews.length - 1].contains(keep) || openPreviews[openPreviews.length - 1] === keep)) {
    openPreviews.pop().remove();
  }
};
const closeAllPreviews = () => { while (openPreviews.length) openPreviews.pop().remove(); };
function positionPreview(popup, anchor) {
  const rect = anchor.getBoundingClientRect();
  const margin = 8;
  popup.style.visibility = 'hidden';
  document.body.append(popup);
  const box = popup.getBoundingClientRect();
  let left = Math.min(rect.right + margin, window.innerWidth - box.width - margin);
  left = Math.max(margin, left);
  const below = rect.bottom + margin + box.height < window.innerHeight;
  popup.style.top = below ? `${rect.bottom + margin}px` : `${Math.max(margin, rect.top - box.height - margin)}px`;
  popup.style.left = `${left}px`;
  popup.style.visibility = 'visible';
}
async function showPreview(anchor) {
  const url = previewUrl(anchor);
  if (!url) return;
  closePreviews(anchor);
  if (openPreviews.some(popup => popup.dataset.previewFor === url && popup.contains(anchor))) return;
  const popup = document.createElement('div');
  popup.className = 'preview-pop';
  popup.dataset.previewFor = url;
  popup.setAttribute('role', 'tooltip');
  const loading = document.createElement('p');
  loading.className = 'meta';
  loading.textContent = 'Loading…';
  popup.append(loading);
  openPreviews.push(popup);
  positionPreview(popup, anchor);
  try {
    let html = previewCache.get(url);
    if (html === undefined) {
      const response = await fetch(url, {credentials: 'same-origin'});
      if (!response.ok) throw new Error(`preview ${response.status}`);
      html = await response.text();
      previewCache.set(url, html);
    }
    if (!popup.isConnected) return;
    popup.innerHTML = html;
    positionPreview(popup, anchor);
  } catch (error) {
    if (popup.isConnected) {
      popup.innerHTML = '';
      const failed = document.createElement('p');
      failed.className = 'meta';
      failed.textContent = 'The preview did not load. Click the link to open the item.';
      popup.append(failed);
    }
  }
}
const schedulePreview = anchor => {
  clearTimeout(previewCloseTimer);
  clearTimeout(previewTimer);
  previewTimer = setTimeout(() => showPreview(anchor), PREVIEW_DELAY);
};
const scheduleCloseAll = () => {
  clearTimeout(previewTimer);
  clearTimeout(previewCloseTimer);
  previewCloseTimer = setTimeout(closeAllPreviews, PREVIEW_DELAY * 2);
};
document.addEventListener('mouseover', event => {
  const anchor = event.target.closest ? event.target.closest('a[data-ref]') : null;
  if (anchor) { schedulePreview(anchor); return; }
  if (!event.target.closest || !event.target.closest('.preview-pop')) scheduleCloseAll();
});
document.addEventListener('focusin', event => {
  const anchor = event.target.closest ? event.target.closest('a[data-ref]') : null;
  if (anchor) schedulePreview(anchor);
});
// 1-4 answer the card that has focus, in the order it lists them, so 1 is
// always the recommendation -- the muscle memory that makes a long list
// tractable. `c` reaches that card's reply field instead.
//
// The digits are inert while no card has focus, and inert on a card that is
// COMPOSING ITS OWN ANSWER, which is decided by the card and not by what
// happens to have focus: one Tab from the verdict picker lands on the
// submit, and from there `1` used to click the recommendation and record it
// -- dropping the operator's picked verdict on the way, because the route
// forwards no picker value onto an authored key. A picked verdict is that
// signal. A typed reply is NOT: it rides with whichever choice is clicked,
// which is the contract the reply field itself states.
//
// Enter from the verdict picker has to be aimed for the same reason: the
// browser's own implicit submission would pick the form's first submit
// button, which is that same recommendation. It goes to the card's own
// submit instead, which records the free-text answer or refuses it in the
// board's words.
//
// `u` brings back the last decided item (George, 2026-09-11): the receipt
// under focus on Needs you, the decided row under focus on Recent decisions,
// and otherwise the newest receipt on the page -- which is the one the last
// keypress just made. It is inert while composing, like every other key.
//
// `Escape` is the release, and it works while composing because that is
// where it is needed: a picked verdict holds the live projection and HTML
// has no other way to un-check a radio group. An `Escape` that is ending an
// IME composition is the input method's, not ours.
document.addEventListener('keydown', event => {
  if (event.metaKey || event.ctrlKey || event.altKey || event.isComposing) return;
  const target = event.target;
  // Focus that is INSIDE the card's composer is composing, whatever kind of
  // element the focus happens to be on: the note field, the verdict picker,
  // the fold's own summary, the submit, the release. It is not about being a
  // text field -- it is about the operator working the composer, where a
  // digit is a digit and not a decision.
  //
  // A `summary` is a TAB STOP, and the fold's summary is the next one after
  // the note field. So `Tab` then `1` reached the choice shortcut and
  // recorded the recommendation over a note the operator was still writing --
  // the same hole `a_digit_after_tabbing_off_a_picked_verdict...` found on
  // the submit, reopened by folding the answer, and closed here by the kind
  // of place focus is rather than by a verdict happening to be picked.
  // Every `summary` counts, the long form's included: focus on a disclosure
  // control is focus on that control.
  const composing = Boolean(target && target.matches
    && target.matches('textarea, input, summary, details[data-custom] *'));
  if (event.key === 'Escape') {
    if (openPreviews.length) { event.preventDefault(); closeAllPreviews(); return; }
    // The menu and the side history are the outermost things Escape closes:
    // whatever is over the page goes first, and only then the verdict. A
    // toast is over the page too, and it is the thing an operator reaches
    // for Escape about most often, so it goes first of all.
    if (dismissToasts()) { event.preventDefault(); return; }
    if (drawerOpen() || historyOpen()) { event.preventDefault(); setDrawer(false); setHistory(false); return; }
    const escaped = cardOf(target) || cardOf(document.activeElement);
    const form = escaped && escaped.querySelector('form.decide');
    if (form) { event.preventDefault(); clearVerdict(form); }
    return;
  }
  if (composing) {
    if (event.key === 'Enter' && target.matches('input[name=outcome]') && target.form) {
      const record = target.form.querySelector('button.record');
      if (record) { event.preventDefault(); record.click(); }
    }
    return;
  }
  if (event.key === 'u') {
    const row = (target.closest && target.closest('[data-receipt], article.decided'))
      || (lastDecided && lastDecided.isConnected ? lastDecided : null);
    if (row) { event.preventDefault(); undoDecision(row); }
    return;
  }
  // `m` is the menu, which is now the only way to the other pages: eight
  // links across the top of a phone were eight links in the way.
  //
  // Every key below calls `preventDefault`, and not only to stop the
  // browser's own default. A keydown this page leaves unhandled arrives
  // again, and again: the driver the browser tests run through delivers one
  // keypress as thousands of keydowns (measured 2026-09-17: 18,000 in three
  // seconds) until one of them is prevented. A skip that fired on every one
  // of those would walk the whole queue on one tap of `s`.
  if (event.key === 'm') { event.preventDefault(); setDrawer(!drawerOpen()); return; }
  // The deck's own keys: the card that cannot be answered yet goes to the
  // back of the queue, and the arrows walk it without recording anything.
  if (deckNode()) {
    if (event.key === 's') { event.preventDefault(); deckSkip(); return; }
    if (event.key === 'ArrowRight') { event.preventDefault(); deckSkip(); return; }
    if (event.key === 'ArrowLeft') { event.preventDefault(); deckMove(-1); return; }
  }
  // The deck answers for the card it is showing even when focus has been
  // put somewhere that is not a card at all -- a tap on the backdrop, a
  // link followed back -- because on a deck there is exactly one card a
  // digit could mean.
  const card = cardOf(document.activeElement) || (deckNode() ? currentCard() : null);
  if (!card) return;
  const digit = CHOICE_KEYS.indexOf(event.key);
  if (digit >= 0) {
    if (card.querySelector('form.decide input[name=outcome]:checked')) return;
    const choices = card.querySelectorAll('form.decide button.choice');
    if (digit < choices.length) { event.preventDefault(); choices[digit].click(); }
    return;
  }
  // `c` is the way into the answer nobody authored a button for, and that
  // answer is folded: the key unfolds it and lands on the field, so one
  // keystroke still reaches the whole path rather than half of it.
  if (event.key === 'c') {
    const custom = card.querySelector('details[data-custom]');
    const text = card.querySelector('textarea[name=reply]');
    if (custom) custom.open = true;
    if (text) { event.preventDefault(); text.focus(); }
  }
});
// Which destination this is, marked by a rule rather than by a fill: the
// drawer's own current-page mark. It is read off the address bar rather than
// rendered, because one shell serves every page and the page it is serving
// is exactly what `location` says.
function markCurrentPage() {
  const here = location.pathname.replace(/\/+$/, '') || '/';
  document.querySelectorAll('[data-drawer] .nav-links a[href]').forEach(link => {
    const target = new URL(link.href, location.href).pathname.replace(/\/+$/, '') || '/';
    if (target === here) link.setAttribute('aria-current', 'page');
    else link.removeAttribute('aria-current');
  });
}
markCurrentPage();
bindCards();
// The deck starts on the first card with the keyboard already on it, so the
// first thing the page is good for is answering it.
bindDeck();
focusCurrent();
connectLive();
"#;
/// One designed system, in one inline stylesheet (ADR-046, spec WEB).
///
/// Two desk surfaces and nothing else: `--base` is the page, `--mantle` is
/// the answer panel, the side column and the drawer. Hierarchy is carried by
/// fill, size and type — there is no outline but the focus ring, one hairline
/// between a card's body and its answers, one row separator, and two radii
/// with one job each (8px for a control, 999px for a pill). A colour is an
/// outcome: green, red, yellow and blue mean approve, reject, defer and
/// other, and nothing decorative is ever coloured. The outcome hue travels
/// on `--hue`, set by the one class that knows the outcome, so exactly one
/// rule fills the recommended answer and exactly one rule rules a receipt.
///
/// One serif, and it is the question: the system serif, sized by the
/// viewport between 1.375rem and 1.75rem, is the largest thing on the screen
/// and the only thing set in it. Mono is reserved for what is literally code
/// — an id, a key map, a numeric column. One motion exists, the card
/// advance, at 140ms each way; a notice, a receipt and a toast simply appear.
///
/// Every rule that lays the deck out is scoped to `html.js`, because the deck
/// is one card only while there is a script to hide the others: without one
/// the same markup has to stay the plain scrolling list it is served as.
const CSS: &str = "\
*{box-sizing:border-box}\
:root{color-scheme:dark;\
--base:#1e1e2e;--mantle:#181825;--surface0:#313244;--surface1:#45475a;\
--text:#cdd6f4;--subtext:#a6adc8;--overlay:#9399b2;\
--green:#a6e3a1;--red:#f38ba8;--yellow:#f9e2af;--blue:#89b4fa;\
--link:#89b4fa;--focus:#b4befe;\
--serif:ui-serif,'New York','Iowan Old Style',Charter,Georgia,serif;\
--sans:-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,'Helvetica Neue',Arial,sans-serif;\
--mono:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}\
body{margin:0;min-height:100vh;color:var(--text);background:var(--base);\
font-family:var(--sans);font-size:1rem;line-height:1.55;\
scrollbar-color:var(--surface1) var(--base)}\
main{max-width:66rem;margin:0 auto;padding:clamp(1rem,3vw,2rem);overflow-x:auto}\
footer{max-width:66rem;margin:0 auto;\
padding:1.2rem clamp(1rem,3vw,2rem) calc(1.2rem + env(safe-area-inset-bottom));\
color:var(--overlay);font-size:.8125rem}\
h1{margin:.2rem 0 1rem;font-family:var(--serif);font-size:1.5rem;line-height:1.2;\
font-weight:600;color:var(--text)}\
h2{margin:1.6rem 0 .5rem;font-size:1.0625rem;line-height:1.3;font-weight:600;color:var(--subtext)}\
a{color:var(--link);text-decoration:none}\
a:hover{text-decoration:underline}\
code{padding:.12em .3em;color:var(--text);background:var(--surface0);border-radius:8px;\
font-family:var(--mono);font-size:.92em}\
pre{margin:.6rem 0;padding:.8rem;background:var(--surface0);border-radius:8px;\
overflow-x:auto;white-space:pre-wrap;word-break:break-word;font-size:.85rem}\
input,textarea{min-height:2.75rem;padding:.6rem .7rem;font-family:var(--sans);font-size:1rem;\
line-height:1.55;color:var(--text);background:var(--surface0);border:0;border-radius:8px}\
input::placeholder,textarea::placeholder{color:var(--subtext)}\
button{min-height:2.75rem;padding:.6rem .8rem;font-family:var(--sans);font-size:1rem;\
line-height:1.2;font-weight:600;color:var(--text);background:var(--surface0);border:0;\
border-radius:8px;cursor:pointer}\
*:focus-visible{outline:2px solid var(--focus);outline-offset:2px}\
/* ...except the card the deck is showing. The deck moves focus to it so\
   `1` decides without a click first, which means the ring would be drawn\
   around the whole card permanently -- a box around everything, saying\
   nothing (WEB-13). Focus is still visible on every control inside it, and\
   on the same card in the plain list, where it is reached by Tab. */\
html.js .deck article.item[data-current]:focus-visible{outline:0}\
/* --- the shell ----------------------------------------------------------- */\
nav{z-index:10;display:flex;align-items:center;gap:.8rem;\
padding:.45rem max(1rem,env(safe-area-inset-right)) .45rem max(1rem,env(safe-area-inset-left));\
background:var(--mantle);position:sticky;top:0}\
/* Two letters, and a 44px hit box around them: the home link is a control\
   in the bar, and WEB-45's floor is every control in it. */\
.brand{justify-content:center;min-width:2.75rem;padding:0;background:none;color:var(--link);\
font-weight:600}\
.nav-links{display:flex;align-items:center;gap:.15rem}\
nav a{display:flex;align-items:center;min-height:2.75rem;padding:0 .7rem;color:var(--subtext);\
white-space:nowrap}\
nav form{margin-left:auto;min-width:8rem}nav form input{width:100%}\
.menu{display:flex;align-items:center;justify-content:center;width:2.75rem;min-height:2.75rem;\
padding:0;background:none}\
.menu .bars{display:block;width:1.1rem;height:10px;\
background:repeating-linear-gradient(var(--text) 0 2px,transparent 2px 4px)}\
.backdrop{position:fixed;inset:0;z-index:40;background:var(--base);opacity:.72}\
.drawer{position:fixed;left:0;top:0;bottom:0;z-index:50;display:flex;flex-direction:column;\
gap:.6rem;width:min(17rem,82vw);padding:.8rem max(.7rem,env(safe-area-inset-left));\
overflow-y:auto;background:var(--mantle)}\
.drawer form{margin:0}.drawer form input{width:100%}\
.drawer .nav-links{display:flex;flex-direction:column;align-items:stretch;gap:0}\
.drawer .nav-links a{min-height:2.75rem;padding:0 .7rem;color:var(--text);\
border-bottom:1px solid var(--surface0)}\
.drawer .nav-links a[aria-current=page]{background:var(--mantle);border-left:2px solid var(--text)}\
/* `hidden` is a display rule, and so is the one above it -- but only a page\
   with a script has a button to open the drawer with, so only there is the\
   drawer allowed to be shut. Without one it is a block of links under the\
   top bar, which is what this shell was before the menu. */\
html.js .drawer[hidden]{display:none}\
/* `!important`, because the UA rule for `[hidden]` is one too: a page with\
   no script has no button to open this with, so the attribute the script\
   manages has to lose to the layout that does not need it. */\
html:not(.js) .drawer[hidden],html:not(.js) .drawer{display:flex!important}\
html:not(.js) .drawer{position:static;width:auto;\
padding:.5rem max(1rem,env(safe-area-inset-left))}\
html:not(.js) .drawer .nav-links{flex-direction:row;flex-wrap:wrap}\
html:not(.js) .drawer .nav-links a{border-bottom:0}\
html:not(.js) .menu{display:none}\
/* --- rows, tables and pills ---------------------------------------------- */\
table{width:100%;border-collapse:collapse;font-size:.88rem}\
th,td{padding:.45rem .6rem;text-align:left;vertical-align:top;\
border-bottom:1px solid var(--surface0)}\
th{font-size:.75rem;font-weight:600;color:var(--overlay)}\
td.n,th.n{text-align:right;font-family:var(--mono);font-variant-numeric:tabular-nums}\
td.waiting{font-weight:600}\
td.when{white-space:nowrap;color:var(--subtext)}\
td.payload{color:var(--subtext);font-size:.85rem;word-break:break-word}\
ul.rows,ul.children{list-style:none;margin:.3rem 0;padding:0}\
ul.rows li,ul.children li{padding:.4rem 0;border-bottom:1px solid var(--surface0)}\
/* A row's title leads it, so it carries the row's weight; the sentence\
   under it is the quiet `.meta` every page already declares. */\
ul.rows li>a,ul.children li>a,.title{font-weight:500}\
.title{display:inline-block;color:var(--text)}\
dl{display:grid;grid-template-columns:max-content 1fr;gap:.15rem .8rem;margin:.4rem 0}\
dt{color:var(--overlay);font-size:.8125rem}\
dd{margin:0;font-size:.9375rem;word-break:break-word}\
.pill{display:inline-block;padding:.1em .4em;color:var(--subtext);background:var(--surface0);\
font-size:.75rem;border-radius:999px}\
.pill.status-done,.pill.status-approve,.pill.status-succeeded{color:var(--green)}\
.pill.status-in_progress,.pill.status-other{color:var(--blue)}\
.pill.status-blocked,.pill.status-reject,.pill.status-failed{color:var(--red)}\
.pill.status-review,.pill.status-defer{color:var(--yellow)}\
.priority{font-family:var(--mono);color:var(--overlay)}\
.priority-p0{color:var(--red)}\
.meta{margin:.2rem 0;color:var(--overlay);font-size:.8125rem;line-height:1.4}\
.count,.empty{color:var(--subtext);font-size:.8125rem;font-weight:400}\
.attention-count{margin-left:.4rem;color:var(--overlay);font-size:.8125rem}\
.citation{margin:.4rem 0 0;color:var(--overlay)}\
.cmd{margin:.5rem 0 0;font-size:.8125rem}\
.success{margin:0 0 1rem;color:var(--subtext)}\
.error{color:var(--red)}\
.live{color:var(--overlay);font-size:.75rem}\
/* The connection line is the shell's, not any one page's: it says whether\
   this document is still hearing from the boards, so it is announced once\
   per page from the bar that is on every page. */\
nav>.live{margin-left:auto}\
.heading{display:flex;align-items:center;flex-wrap:wrap;gap:.6rem}\
.search-page{display:flex;gap:.5rem}.search-page input{flex:1}\
.search-result h2{margin:.1rem 0}\
.sprint-version{font-size:1.15rem}.sprint-history{margin-top:1.5rem}\
.plan-body{max-height:28rem;overflow-y:auto}\
.queued{margin-top:.25rem;color:var(--subtext);font-size:.8125rem}\
.queued .retrying,.queued .dead{color:var(--text);font-weight:700}\
/* --- the card (ADR-042 §5) ----------------------------------------------- */\
.item,.note,.plan,.search-result,.card,.decided{margin:1.4rem 0;padding:0}\
.eyebrow{max-width:70ch;margin:0 0 .5rem;color:var(--overlay);font-size:.8125rem;line-height:1.4}\
.item>h2{max-width:70ch;margin:0 0 .7rem;font-family:var(--serif);\
font-size:clamp(1.375rem,1.1rem + 1vw,1.75rem);line-height:1.15;font-weight:600;\
color:var(--text)}\
.context{max-width:70ch;margin:0 0 1rem;color:var(--subtext)}\
.explain{max-width:70ch;margin:0 0 1rem;color:var(--subtext)}\
.consequence{max-width:70ch;margin:.35rem 0 0;color:var(--subtext);font-size:.9375rem}\
.body{max-width:70ch;margin:.5rem 0}\
.decide{margin:0}\
fieldset{min-width:0;margin:0;padding:0;border:0}\
legend{padding:0;color:var(--overlay);font-size:.8125rem}\
.choice{display:block;width:100%;margin:0;padding:.7rem .9rem;text-align:left;\
white-space:normal;background:var(--surface0)}\
.choice .key{display:inline-block;min-width:1.4em;margin-right:.5em;font-family:var(--mono);\
font-size:.75rem;font-weight:400}\
.choice.outcome-approve{--hue:var(--green);color:var(--hue)}\
.choice.outcome-reject{--hue:var(--red);color:var(--hue)}\
.choice.outcome-defer{--hue:var(--yellow);color:var(--hue)}\
.choice.outcome-other{--hue:var(--blue);color:var(--hue)}\
/* The one rule that fills an answer, and it is the recommendation: the hue\
   rides in on `--hue` from the class that knows the outcome, so adding a\
   fifth outcome is a token line rather than a fifth fill. */\
.recommended .choice{color:var(--base);background:var(--hue,var(--surface1))}\
/* One answer per row, at every width. Two-up put a 37-character label on\
   three or four wrapped lines in a 177px button on a 390px phone and read\
   as a squeezed table of fragments (George, 2026-09-18: \"it's up but it's\
   squished and not mobile responsive\"). A choice is a sentence, so it gets\
   the card's whole width and its consequence sits under it. */\
.alternatives{display:grid;grid-template-columns:1fr;gap:.6rem .8rem;margin:1rem 0 0}\
.alternative .consequence{margin-top:.3rem}\
.reply{margin-top:1rem}\
.reply>label{display:block;margin:0 0 .35rem;color:var(--subtext);font-size:.8125rem}\
.reply textarea{display:block;width:100%;resize:vertical}\
.custom{margin-top:1rem}\
.custom>summary{display:flex;align-items:center;min-height:2.75rem;color:var(--overlay);\
font-size:.8125rem;cursor:pointer;list-style:none}\
.custom>summary::-webkit-details-marker{display:none}\
.custom>summary::after{content:'\\25be';margin-left:.4em}\
.custom[open]>summary::after{content:'\\25b4'}\
.picks{display:flex;flex-wrap:wrap;gap:.5rem;margin:.4rem 0 .7rem}\
.picks label{display:inline-flex;align-items:center;gap:.4rem;min-height:2.75rem;\
padding:.25rem .8rem;color:var(--subtext);background:var(--surface0);border-radius:999px;\
cursor:pointer}\
.picks label:has(input:checked){color:var(--text);background:var(--surface1)}\
.picks input{width:.9rem;height:.9rem;min-height:auto;margin:0;padding:0;background:none;\
accent-color:var(--focus)}\
.actions{display:flex;flex-wrap:wrap;align-items:center;gap:.6rem;margin-top:.7rem}\
.full{margin:1.2rem 0 0}\
.full summary{color:var(--subtext);cursor:pointer}\
.full .body{max-height:24rem;margin:.6rem 0 0;overflow-y:auto;color:var(--subtext)}\
p.keys{margin:.9rem 0 0;color:var(--overlay);font-family:var(--mono);font-size:.75rem}\
/* What a click is doing, said on the pressed answer's own fill: nothing is\
   disabled -- a disabled button is the one control that cannot report its\
   own refusal -- so the OTHER answers step back instead. */\
.item[data-state=sending] .choice:not([data-pressed]),\
.item[data-state=sending] .record:not([data-pressed]){opacity:.5}\
.decided h2{margin:0 0 .35rem;color:var(--text)}\
.decided .decision{margin:.2rem 0;color:var(--text)}\
.decided .note{margin:.3rem 0;color:var(--subtext);font-style:italic}\
.undo-button{min-height:2.25rem;margin-top:.4rem;padding:.2rem .7rem;color:var(--subtext);\
background:none;font-size:.8125rem;font-weight:400}\
/* --- what arrives: notices, toasts, receipts ----------------------------- */\
.notices{display:grid;gap:.35rem;margin:0 0 1.1rem}\
.notice{display:flex;align-items:center;flex-wrap:wrap;gap:.5rem;margin:0;padding:.5rem .65rem;\
background:var(--surface0);border-radius:8px;font-size:.8125rem}\
.notice-board{color:var(--subtext)}\
.notice.summary .notice-what{font-weight:600}\
.notice .dismiss{min-height:auto;margin-left:auto;padding:.1rem .5rem;color:var(--subtext);\
background:none;font-size:.75rem;font-weight:400}\
.preview-pop{position:fixed;z-index:300;max-width:26rem;max-height:22rem;overflow-y:auto;\
padding:.8rem .9rem;color:var(--text);background:var(--surface0);border-radius:8px}\
.preview-card h3{margin:.1rem 0 .4rem;color:var(--text);font-size:.95rem;font-weight:600}\
.preview-card h3 a{color:inherit}\
.preview-card .body{margin:.4rem 0 0;color:var(--subtext);font-size:.88rem}\
.body.md>*:first-child{margin-top:0}.body.md>*:last-child{margin-bottom:0}\
.body.md p{margin:.5rem 0;max-width:70ch}\
.body.md h1,.body.md h2,.body.md h3,.body.md h4{margin:.9rem 0 .35rem;color:var(--text);\
font-weight:600}\
.body.md h1{font-size:1.1rem}.body.md h2{font-size:1.05rem}\
.body.md h3{font-size:1rem}.body.md h4{font-size:.95rem}\
.body.md ul,.body.md ol{margin:.5rem 0;padding-left:1.4rem;max-width:70ch}\
.body.md li{margin:.2rem 0}\
.body.md ul{list-style:disc}.body.md ol{list-style:decimal}\
.body.md blockquote{margin:.6rem 0;padding:.2rem .9rem;color:var(--subtext)}\
.body.md table{margin:.6rem 0}\
.body.md hr{height:1px;margin:1rem 0;background:var(--surface0);border:0}\
/* --- the deck: one card, and its own column the one thing that scrolls -- */\
/* Every rule here is scoped to `html.js`, and that scope is load-bearing:\
   the deck is one card because a script hides the others, so without a\
   script the same markup has to stay what it is -- a plain scrolling list\
   of cards whose forms post on their own. A page laid out as a deck with\
   nothing to drive it would show one crushed card and no way past it.\
\
   The answers have to be where the thumb is, so the card is a column: what\
   the item is at the top, the long form in the middle taking whatever is\
   left, and the form at the bottom of the screen. The form is rendered\
   before the long form (ADR-042 §5 keeps the answers above the detail in\
   the markup, and that is the order a scriptless browser reads), so the\
   deck orders the column visually and leaves the document alone. */\
html.js:has(main[data-deck]){height:100%}\
html.js body:has(main[data-deck]){height:100%;overflow:hidden;display:flex;flex-direction:column}\
html.js body:has(main[data-deck])>nav[data-primary-nav]{flex:none}\
html.js main[data-deck]~footer{display:none}\
html.js main[data-deck]{position:relative;flex:1;min-height:0;width:100%;display:flex;\
flex-direction:column;gap:.45rem;overflow:hidden;padding:.6rem clamp(.7rem,3vw,1.2rem) 0}\
html.js main[data-deck]>.heading,html.js main[data-deck]>.success{flex:none}\
/* Everything that is not the card gives up its room to the card: on a\
   phone every line of page furniture is a line the long form or an answer\
   does not get. */\
html.js main[data-deck]>.heading h1{margin:0;font-size:1rem}\
html.js main[data-deck]>p.keys{flex:none;margin:0;\
padding:.3rem 0 calc(.3rem + env(safe-area-inset-bottom))}\
html.js .progress{margin:0;color:var(--overlay);font-size:.8125rem;\
font-variant-numeric:tabular-nums}\
html.js .deck{position:relative;flex:1;min-height:0;display:flex}\
html.js .deck .item{flex:1;min-height:0;display:flex;flex-direction:column;margin:0;\
overflow:hidden auto;overscroll-behavior:contain}\
/* `hidden` is a display rule and the rule above is one too, so the card the\
   deck is not showing needs saying twice to stay off the screen. */\
html.js .deck .item[hidden]{display:none}\
html.js .deck .item>.eyebrow{order:1;flex:none;margin-bottom:.25rem}\
html.js .deck .item>h2{order:2;flex:none}\
/* A raiser's context is the FIRST block of the long form on the deck, and\
   it is the DECK that puts it there: the served markup keeps the ADR-042\
   §5 order (question, context, answers, folded body), and `bindDeck` moves\
   the paragraph into the body region so the two share one soft-edged,\
   capped scroller. It used to be a clipped region of its own at the top of\
   the card, which cut the paragraph mid-sentence with the priority pill\
   over it (George, 2026-09-17, on the deck screenshots); then it was a\
   free block in the card's column, which pushed the recommended answer off\
   the first screen of a phone whenever a raiser wrote 790 characters of\
   context (George, 2026-09-18). Now it scrolls inside the body region with\
   the long form it introduces, under the same fade, and the answers are on\
   the screen the card opens on.\
\
   The rule below is what the paragraph is while it is still a child of the\
   card -- one frame at most, before the deck's script has moved it, and\
   every frame on a page whose script never ran. */\
html.js .deck .item>.context{order:3;flex:none;margin-bottom:.4rem}\
html.js .deck .item .body>.context{margin:0 0 .8rem}\
/* The long form is the card's body and clips: a region that overflowed\
   instead would paint the body straight over the answers. It keeps a floor\
   so that a card with a long context and four answers still shows some of\
   what it is about, and a 40vh cap so it scrolls inside itself rather than\
   pushing the answers down the card. */\
html.js .deck .item>.full{order:4;flex:1 1 0;min-height:8rem;overflow:hidden;\
display:flex;flex-direction:column;margin:0}\
/* The disclosure control goes: on the deck the long form is not folded, so\
   a control that says `show the full item` above an item already shown is\
   one line of a phone spent saying nothing. */\
html.js .deck .item>.full>summary{display:none}\
/* Chrome wraps what a `<details>` reveals in `::details-content`, so the\
   long form is a grandchild of the card's column and not a child: the\
   wrapper has to carry the column through, or the body sizes itself to its\
   content and paints over the answers. The viewport bound behind it is for\
   a browser without that pseudo-element, where the body is bounded\
   directly rather than by what the column has left. */\
html.js .deck .item>.full::details-content{flex:1 1 0;min-height:0;overflow:hidden;\
display:flex;flex-direction:column}\
/* The foot of the long form fades, because a hard cut at the panel's edge\
   says the item ended there. Two rem of it is the plan's figure, and it is\
   the only fade on the page. */\
html.js .deck .item>.full .body{flex:1;min-height:0;max-height:40vh;overflow-y:auto;\
padding-right:.3rem;\
-webkit-mask-image:linear-gradient(to bottom,var(--text) calc(100% - 2rem),transparent);\
mask-image:linear-gradient(to bottom,var(--text) calc(100% - 2rem),transparent)}\
/* The trailing meta line does not trail on the deck: the script moves its\
   contents onto the end of the eyebrow, above the question, because trailing\
   put the priority pill directly under the dissolving last line of the long\
   form (George, 2026-09-18, on the v6 screenshots) -- the pill over clipped\
   text, which is the thing it was moved out of the card's head to avoid.\
   The rule below is what the paragraph is before the script has moved it,\
   and on every card the deck is not showing: at the foot of the body region\
   rather than floating in the middle of a short card. */\
html.js .deck .item>.meta{order:5;flex:none;margin:auto 0 0}\
/* The panel: the answers and the note, on the desk surface at the foot of\
   the card. It is sized to its content and scrolls nothing of its own.\
\
   It used to be capped at three fifths of the card and scroll its answers\
   past that cap. That put the note field at y=961 on a 390x844 phone --\
   off the screen, inside a nested scroller with no affordance -- and cut\
   the fourth answer at the cap (George, 2026-09-18, on the served deck:\
   \"it's up but it's squished and not mobile responsive\"; \"the web version\
   looks mushed up ... (iPad has it squished up)\"). So the cap is gone and\
   with it the panel's own overflow: the card's column above is the ONE\
   scroller, and the note is reached by scrolling the card the reader is\
   already scrolling.\
\
   Its top two rem is the fade, because a line of the raiser's paragraph\
   meeting an opaque edge mid-sentence reads as text that was cut off. The\
   gradient's last stop is the desk's second surface, so the panel below\
   the band is `--mantle` and the band lets the text above it dissolve into\
   it. It is a gradient and not a second mask: the one mask on the page is\
   the long form's own foot. */\
html.js .deck .item>.decide{order:6;flex:none;\
margin:.45rem calc(-1 * clamp(.7rem,3vw,1.2rem)) 0;\
padding:0 clamp(.7rem,3vw,1.2rem) calc(.5rem + env(safe-area-inset-bottom));\
background:linear-gradient(to bottom,transparent,var(--mantle) 2rem)}\
/* The one hairline the design keeps, between the card's body and its\
   answers -- at the foot of the band and not its head, because a rule\
   drawn across text that is still dissolving reads as a strikethrough.\
   It is the panel's own first block, two rem tall, carrying the same ramp.\
   It is a plain block and no longer a sticky one: the panel scrolls\
   nothing, so `position:sticky` here would resolve against the CARD's\
   scrollport and float the band over the long form. */\
html.js .deck .item>.decide::before{content:'';display:block;\
height:2rem;box-sizing:border-box;pointer-events:none;\
background:linear-gradient(to bottom,transparent,var(--mantle) 2rem);\
border-bottom:1px solid var(--surface1)}\
html.js .deck .item>.decide .consequence{font-size:.875rem}\
html.js .deck .item>.decide .reply{margin-top:.7rem}\
html.js .deck .item>.decide .custom{margin-top:.5rem}\
html.js .deck .item>.decide textarea{min-height:2.75rem;max-height:7.5rem;overflow-y:auto;\
resize:none}\
/* The one motion: the card that was answered leaves to the left, the next\
   one arrives from the right, 140ms each way. */\
html.js .deck .item.entering{animation:deck-in .14s ease-out}\
html.js .deck .item.leaving,html.js .deck .item.leaving-back{position:absolute;left:0;right:0;\
top:0;bottom:0;pointer-events:none;animation:deck-out .14s ease-out forwards}\
html.js .deck .item.leaving-back{animation-name:deck-out-back}\
@keyframes deck-in{from{opacity:0;transform:translateX(16px)}to{opacity:1;transform:none}}\
@keyframes deck-out{to{opacity:0;transform:translateX(-16px)}}\
@keyframes deck-out-back{to{opacity:0;transform:translateX(16px)}}\
/* --- the side: toasts above, this sitting's decisions below -------------- */\
/* A list, not a stack of tiles: one heading, then one row per decision on\
   the desk surface. A receipt's own left rule is its outcome, which is the\
   one thing a reader needs from a decision already made. */\
html.js .side{display:flex;flex-direction:column;gap:.7rem;min-height:0}\
html.js .toasts{display:grid;gap:.35rem;margin:0}\
html.js .history{display:flex;flex-direction:column;gap:.5rem;min-height:0;overflow-y:auto}\
html.js .history h2{margin:0;padding:0;font-size:.75rem;font-weight:600;color:var(--overlay)}\
html.js .history .receipt,html.js .history .pending{margin:0;padding:.45rem .65rem;\
color:var(--subtext);background:var(--surface0);font-size:.8125rem}\
html.js .history .receipt{border-left:3px solid var(--hue,var(--surface1))}\
html.js .receipt.outcome-approve{--hue:var(--green)}\
html.js .receipt.outcome-reject{--hue:var(--red)}\
html.js .receipt.outcome-defer{--hue:var(--yellow)}\
html.js .receipt.outcome-other{--hue:var(--blue)}\
html.js .history-toggle{min-height:2.75rem;padding:.2rem .7rem;color:var(--subtext);\
font-size:.8125rem;font-weight:400}\
html.js .history-toggle [data-history-count]{margin-left:.4rem;color:var(--text)}\
/* The desk on a Mac: the deck column left, the side column right on its own\
   surface, holding the toasts above this sitting's decisions. */\
@media(min-width:900px){html.js main[data-deck]{padding-right:calc(22rem + 1.2rem)}\
html.js .deck{max-width:44rem}\
html.js main[data-deck]>.heading h1{font-size:1.5rem}\
html.js main[data-deck]>.side{position:absolute;top:0;right:0;bottom:0;width:22rem;\
padding:.8rem 1rem calc(.8rem + env(safe-area-inset-bottom));background:var(--mantle)}\
html.js .history-toggle{display:none}}\
/* Below the desktop column the side is a drawer, and the toasts come out of\
   it: news has to arrive whether or not the history is open. */\
@media(max-width:899px){html.js main[data-deck]>.side{display:contents}\
/* A toast is IN the column on a phone, between the heading and the card: one\
   line that pushes the card down while it is there. Floated over the card it\
   covered the eyebrow and the question -- the two lines that say what is\
   being decided -- which is a notice getting in the way of the decision. */\
html.js main[data-deck]>.heading,html.js main[data-deck]>.success{order:0}\
html.js main[data-deck] .toasts{order:1;position:static;margin:0;pointer-events:auto}\
html.js main[data-deck]>.deck{order:3}\
html.js main[data-deck]>p.keys{order:4}\
html.js main[data-deck] .toasts .notice{flex-wrap:nowrap;overflow:hidden}\
html.js main[data-deck] .toasts .notice .notice-what,\
html.js main[data-deck] .toasts .notice a{overflow:hidden;white-space:nowrap;\
text-overflow:ellipsis}\
/* One at a time: the strip is a line of the screen, and the newest notice is\
   the one worth that line. */\
html.js main[data-deck] .toasts .notice~.notice{display:none}\
html.js main[data-deck] .history{position:fixed;top:0;right:0;bottom:0;z-index:50;\
width:min(19rem,86vw);\
padding:.8rem max(.7rem,env(safe-area-inset-right)) calc(.8rem + env(safe-area-inset-bottom));\
background:var(--mantle);transform:translateX(100%);transition:transform .14s ease-out}\
html.js body[data-history-open] main[data-deck] .history{transform:none}}\
@media(max-width:700px){nav{align-items:stretch;flex-wrap:wrap}\
.brand{flex:0 0 2.5rem}\
.nav-links{flex:1;overflow-x:auto;scrollbar-width:none}\
.nav-links::-webkit-scrollbar{display:none}\
nav form{order:3;flex:1 0 100%;margin:0}\
.actions button{flex:1}\
.picks{display:grid;grid-template-columns:1fr 1fr}\
table{min-width:38rem}\
.drawer{flex-wrap:nowrap}\
.drawer .nav-links{flex:none;overflow:visible}\
.drawer nav form,.drawer form{order:0;flex:none}}\
/* A short screen -- a laptop window, a phone in landscape -- lets the long\
   form give up its floor, so the answers keep their room and what does not\
   fit scrolls with the card rather than being cut off. */\
@media(max-height:620px){html.js .deck .item>.full{min-height:0}}\
/* The operator who asked for the page not to move gets the same page with\
   the one motion left out: the queue still advances, in no time at all. */\
@media(prefers-reduced-motion:reduce){*{scroll-behavior:auto!important}\
html.js .deck .item.entering{animation:none}\
html.js .deck .item.leaving,html.js .deck .item.leaving-back{animation:none;opacity:0}\
html.js main[data-deck] .history{transition:none}}\
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authz::AuthzContext;
    use crate::model::{
        AddSubscription, AddTask, AttentionDecision, DecisionCard, DeployIdentity,
        FinishDeployment, NewSprint, StartDeployment,
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

    /// A restricted row says so, and the lease says which model holds it.
    ///
    /// Both are conditional: an unrestricted row must carry no `allowed
    /// models` line at all, and a claim taken without `--model` must not
    /// render an empty `model` row, because a blank fact reads as a fact.
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
        assert!(
            !facts("px", &task).contains("allowed models"),
            "an unrestricted row must carry no allow-list row"
        );
        task.allowed_models = vec!["Astra".to_owned(), "Blender-1".to_owned()];
        assert!(
            facts("px", &task).contains("<dt>allowed models</dt><dd>Astra, Blender-1</dd>"),
            "{}",
            facts("px", &task)
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
        let without = held_by(&claim);
        assert!(!without.contains("<dt>model</dt>"), "{without}");
        claim.model = Some("Astra".to_owned());
        let with = held_by(&claim);
        assert!(with.contains("<dt>model</dt><dd>Astra</dd>"), "{with}");
        assert!(
            !with.contains("secret-token"),
            "the lease token must never reach the page: {with}"
        );
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

    /// Every selector in the stylesheet, comma-separated parts split out and
    /// keyframe stops dropped: enough to ask what a rule is scoped to.
    fn css_selectors(css: &str) -> Vec<String> {
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
        let mut selectors = Vec::new();
        for chunk in stripped.split('{') {
            let tail = chunk.rsplit('}').next().unwrap_or("").trim();
            if tail.is_empty() || tail.starts_with('@') {
                continue;
            }
            for part in tail.split(',') {
                let part = part.trim();
                if part.is_empty() || part == "from" || part == "to" || part.ends_with('%') {
                    continue;
                }
                selectors.push(part.to_owned());
            }
        }
        selectors
    }

    /// The deck's layout is scoped to a page whose script ran, and nothing
    /// else is allowed to claim it.
    ///
    /// The deck is one card because the script hides the others. The same
    /// markup with no script has to stay the plain scrolling list it is
    /// served as: a viewport-height column with `overflow:hidden` and a flex
    /// row of cards, applied with nothing to drive it, is one crushed card
    /// and no way past it — every open item unreachable on the page whose
    /// whole job is to reach them. So every rule that mentions the deck, or
    /// styles the parts only the deck renders, MUST be behind `html.js`.
    #[test]
    fn every_deck_rule_is_scoped_to_a_page_whose_script_ran() {
        let deck_parts = [
            "data-deck",
            ".deck",
            ".progress",
            ".side",
            ".history",
            ".pending",
            ".toasts",
        ];
        let mut unscoped = Vec::new();
        for selector in css_selectors(CSS) {
            let mentions_deck = deck_parts.iter().any(|part| selector.contains(part));
            if mentions_deck && !selector.starts_with("html.js") {
                unscoped.push(selector);
            }
        }
        assert!(
            unscoped.is_empty(),
            "these deck rules would apply to a page with no script: {unscoped:?}"
        );
        // And the script says so before it can throw: the class is added on
        // the first line, not after the page is wired up.
        let marker = "document.documentElement.classList.add('js');";
        let at = JS.find(marker).expect("the script marks the document");
        assert!(
            JS[..at]
                .lines()
                .all(|line| line.trim().is_empty() || line.trim_start().starts_with("//")),
            "something runs before the document is marked as scripted"
        );
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
        for rule in css_rules(CSS) {
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
        for rule in css_rules(CSS) {
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
        css_rule_body_in(CSS, selector)
    }

    /// The same, in a named stylesheet: the served `CSS`, or the bundle's own
    /// (SPA-56 runs ADR-046's token proofs over both).
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
        token_contrast_in(CSS, foreground, background)
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
        assert!(
            CSS.contains("--serif:ui-serif,'New York','Iowan Old Style',Charter,Georgia,serif"),
            "{CSS}"
        );
        let mut serif = css_selectors_declaring("var(--serif)");
        serif.sort();
        assert_eq!(serif, vec![".item>h2".to_owned(), "h1".to_owned()]);
        // Mono is for an identifier, a key map and a numeric column, and the
        // allowlist is the spec's (WEB-05). A selector is judged by the
        // element it lands on, so `.choice .key` is `.key`.
        let allowed = ["code", ".id", ".key", ".priority", "p.keys", "td.n", "th.n"];
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
            (".meta", vec!["font-size:.8125rem", "line-height:1.4"]),
            (
                ".choice .key",
                vec!["font-size:.75rem", "font-family:var(--mono)"],
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
        let stripped = css_without_comments(CSS);
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
                // the drawer's current destination
                || (property == "border-left"
                    && value == "2px solid var(--text)"
                    && selector.contains("aria-current"));
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
        for rule in css_rules(CSS) {
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
        let stripped = css_without_comments(CSS);
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
        for rule in css_rules(CSS) {
            if rule.body.contains("mask-image") {
                faded.push(rule);
            }
        }
        assert_eq!(faded.len(), 1, "more than one element fades");
        let fade = &faded[0];
        assert_eq!(
            fade.selectors,
            vec!["html.js .deck .item>.full .body".to_owned()]
        );
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
        let panel = css_rule_body("html.js .deck .item>.decide");
        assert!(panel.contains("flex:none"), "{panel}");
        for banned in ["max-height", "overflow"] {
            assert!(
                !panel.contains(banned),
                "the answer panel still declares {banned}, so it is a nested \
                 scroller again: {panel}"
            );
        }
        // The card's own column is the one scroller the deck has.
        let card = css_rule_body("html.js .deck .item");
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
        for rule in css_rules(CSS) {
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

    /// The bundle's stylesheet, as the browser receives it.
    fn bundle_stylesheet() -> String {
        let asset = crate::bundle::asset(crate::bundle::STYLESHEET)
            .expect("the bundle's stylesheet is embedded under the name the shell links");
        String::from_utf8(asset.bytes.to_vec()).expect("the stylesheet is UTF-8")
    }

    /// SPA-56 — ADR-046's token proofs, re-run over the bundle's own CSS.
    ///
    /// The served `CSS` const and the bundle's stylesheet are two stylesheets
    /// for one product, and the cutover moves rules from the first to the
    /// second one page at a time. A token block that drifted while it moved
    /// would be a different palette wearing the same names, so the bundle's
    /// block must be the served one exactly, the contrast arithmetic is
    /// re-run on it, and no colour may be written outside it.
    ///
    /// What this does NOT yet re-run, because the rules it judges have not
    /// moved: `.pill`'s quiet-text rule, and the type-scale, serif/mono,
    /// prose-width and glyph-prefix proofs, all of which select elements the
    /// deck brings with it (`t-1f495a7f`, `t-bf255880`).
    #[test]
    fn the_bundle_stylesheet_keeps_the_token_block_and_its_contrast_unit() {
        let bundle = bundle_stylesheet();
        let colours = |css: &str| -> Vec<(String, String)> {
            css_declarations(&css_rule_body_in(css, ":root"))
                .into_iter()
                .filter(|(_, value)| value.starts_with('#'))
                .collect()
        };
        assert_eq!(
            colours(&bundle),
            colours(CSS),
            "the bundle's token block is not the served stylesheet's"
        );

        // No hex outside the token block, in the bundle as in the document.
        let stripped = css_without_comments(&bundle);
        let root_at = stripped.find(":root").expect("the token block");
        let root_end = root_at + stripped[root_at..].find('}').expect("the block closes");
        let outside = format!("{}{}", &stripped[..root_at], &stripped[root_end..]);
        assert!(
            !outside.contains('#'),
            "the bundle writes a hex literal outside its token block: {outside}"
        );

        for (foreground, background) in AA_TOKEN_PAIRS {
            let ratio = token_contrast_in(&bundle, foreground, background);
            assert!(
                ratio >= 4.5,
                "in the bundle, {foreground} on {background} is {ratio:.2}:1, below AA"
            );
        }
        for ground in ["--base", "--mantle"] {
            let ratio = token_contrast_in(&bundle, "--focus", ground);
            assert!(
                ratio >= 3.0,
                "the bundle's focus ring is {ratio:.2}:1 on {ground}"
            );
        }
        for hue in ["--green", "--red", "--yellow", "--blue"] {
            let ratio = token_contrast_in(&bundle, hue, "--base");
            assert!(
                ratio >= 3.0,
                "a filled answer in {hue} is {ratio:.2}:1 against the bundle's page"
            );
        }
        for rule in css_rules(&bundle) {
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

    /// One board with one fully carded open item, three days old.
    ///
    /// Opened directly rather than through the registry: the card renderers
    /// need a store for a task reference and nothing else, so the copy this
    /// slice is about is readable without a spawned process.
    struct CardFixture {
        _dir: TempDataDir,
        store: Store,
        board: String,
        item: Attention,
    }

    fn card_fixture(label: &str) -> CardFixture {
        let dir = TempDataDir::new(label);
        let path = dir.path().join("px.db");
        let mut store = Store::open(&path).expect("open the card board");
        store
            .initialize("px", "seed")
            .expect("initialize the board");
        let card = DecisionCard {
            question: Some("Ship the sprint tonight, or wait for Monday?".to_owned()),
            context: Some("Gate green at 353/0. Rollback is one command.".to_owned()),
            choices: vec![
                AttentionChoice {
                    key: "ship".to_owned(),
                    label: "Ship tonight".to_owned(),
                    consequence: "The release goes out now.".to_owned(),
                    outcome: "approve".to_owned(),
                    recommended: true,
                },
                AttentionChoice {
                    key: "wait".to_owned(),
                    label: "Wait for Monday".to_owned(),
                    consequence: "Nothing goes out today.".to_owned(),
                    outcome: "defer".to_owned(),
                    recommended: false,
                },
                AttentionChoice {
                    key: "stop".to_owned(),
                    label: "Stop the release".to_owned(),
                    consequence: "The sprint does not ship at all.".to_owned(),
                    outcome: "reject".to_owned(),
                    recommended: false,
                },
            ],
        };
        let mut item = store
            .raise_attention(
                "The long form of the ask, as a raiser writes it.",
                "decision",
                "codex@driver",
                None,
                0,
                &[],
                &card,
            )
            .expect("raise a carded item");
        item.created_at = now_ms() - 3 * 24 * 60 * 60_000;
        CardFixture {
            _dir: dir,
            store,
            board: "px".to_owned(),
            item,
        }
    }

    /// Everything between `<p class=X>` and its close, for every occurrence.
    fn rendered_regions(html: &str, class: &str) -> Vec<String> {
        let mut out = Vec::new();
        for tag in ["p", "div"] {
            let open = format!("<{tag} class={class}>");
            let close = format!("</{tag}>");
            let mut rest = html;
            while let Some(at) = rest.find(&open) {
                let body = &rest[at + open.len()..];
                let end = body.find(&close).expect("the region closes");
                out.push(body[..end].to_owned());
                rest = &body[end..];
            }
        }
        out
    }

    /// WEB-23 — the eyebrow names who asked, where, and when, in one
    /// sentence.
    #[test]
    fn the_eyebrow_names_raiser_board_and_age_unit() {
        let fixture = card_fixture("eyebrow");
        let card = decision_card(&fixture.board, &fixture.store, &fixture.item);
        assert!(
            card.contains(
                "<p class=eyebrow>codex@driver asked on \
                 <a href=\"/board/px\" data-ref target=_blank rel=noopener>px</a>, 3 days ago</p>"
            ),
            "{card}"
        );
        let eyebrow = rendered_regions(&card, "eyebrow");
        assert_eq!(eyebrow.len(), 1, "{eyebrow:?}");
        for absent in ["kind", "waiting", "priority", "decision"] {
            assert!(
                !eyebrow[0].contains(absent),
                "the eyebrow still carries {absent}: {}",
                eyebrow[0]
            );
        }
    }

    /// WEB-22 — meta is a sentence, never a chain.
    #[test]
    fn rendered_meta_is_a_sentence_with_no_dot_chain_unit() {
        let fixture = card_fixture("meta");
        let mut settled = fixture.item.clone();
        settled.status = "resolved".to_owned();
        settled.resolved_by = Some(OPERATOR_ACTOR.to_owned());
        settled.resolved_at = Some(now_ms());
        settled.decision = Some(AttentionDecision {
            choice: "ship".to_owned(),
            outcome: "approve".to_owned(),
            note: Some("go".to_owned()),
            by: OPERATOR_ACTOR.to_owned(),
            at: now_ms(),
        });
        let surfaces = [
            decision_card(&fixture.board, &fixture.store, &fixture.item),
            decided_row(&fixture.board, &fixture.store, &settled),
            attention_section(
                &fixture.board,
                "Open attention",
                std::slice::from_ref(&fixture.item),
            ),
        ];
        for surface in &surfaces {
            for class in ["eyebrow", "meta", "decision"] {
                for region in rendered_regions(surface, class) {
                    for chain in [" · ", " | "] {
                        assert!(
                            !region.contains(chain),
                            "a {class} line reads as a chain: {region}"
                        );
                    }
                    assert!(!region.trim_end().ends_with('→'), "{region}");
                }
            }
        }
        // A receipt is built by the script, and its sentences are written
        // there: no chain, and no arrow. The ONE separator the script writes
        // is the deck's own eyebrow join (WEB-22, superseded 2026-09-18):
        // the card's meta line moves onto the end of the eyebrow so the
        // priority pill stops sitting under text dissolving into the fade,
        // and one line of orientation carries who asked, where, when and how
        // urgent. Counted rather than banned, so a second chain written
        // anywhere in the script still fails here.
        assert_eq!(
            JS.matches(" · ").count(),
            1,
            "the script writes a dot chain somewhere other than the eyebrow join"
        );
        assert!(
            JS.contains("eyebrow.append(' · ')"),
            "the one separator the script writes is not the eyebrow join"
        );
        assert!(!JS.contains('→'), "the script writes an arrow");
    }

    /// WEB-24, WEB-25 — the note field's label, and only its label; and the
    /// custom answer's button says what happens.
    #[test]
    fn the_note_field_is_labelled_add_a_note_with_no_hint_unit() {
        let fixture = card_fixture("note");
        let card = decision_card(&fixture.board, &fixture.store, &fixture.item);
        let id = url_encode(&fixture.item.id);
        assert!(
            card.contains(&format!(
                "<div class=reply><label for=\"answer-{id}\">Add a note</label>"
            )),
            "{card}"
        );
        assert!(!card.contains("Add a note (optional)"), "{card}");
        assert!(!card.contains("aria-describedby=\"reply-hint"), "{card}");
        assert!(
            !card.contains("Sent with whichever answer you pick"),
            "the note kept its hint paragraph: {card}"
        );
    }

    /// WEB-25 — the custom answer's button says what happens.
    #[test]
    fn the_custom_answer_button_says_record_my_answer_unit() {
        let fixture = card_fixture("record");
        let card = decision_card(&fixture.board, &fixture.store, &fixture.item);
        assert!(
            card.contains(
                "<button type=submit class=record name=decision value=custom \
                 data-testid=deck-record>Record my answer</button>"
            ),
            "{card}"
        );
        assert!(!card.contains("Record this answer"), "{card}");
    }

    /// WEB-26 — the empty deck is an answer, not an absence.
    #[test]
    fn the_empty_deck_copy_and_its_link_are_exact_unit() {
        assert!(
            EMPTY_QUEUE.contains(
                "<p class=empty>Nothing is waiting. \
                 Every question an agent raised has an answer.</p>"
            ),
            "{EMPTY_QUEUE}"
        );
        assert!(
            EMPTY_QUEUE.contains("<a href=\"/decided\">See what was decided</a>"),
            "{EMPTY_QUEUE}"
        );
        assert!(
            !EMPTY_QUEUE.contains("every raised item has been settled"),
            "the absence wording is still served: {EMPTY_QUEUE}"
        );
    }

    /// WEB-27 — one quiet keyboard line, and the digits live on the buttons.
    ///
    /// The DECK's line moved into the bundle with the deck itself
    /// (`t-1f495a7f`), where it is observed in the browser by
    /// `the_deck_keeps_one_quiet_keys_line_in_real_chrome`; what is still
    /// served here is the plain list's, and the digits on the card are the
    /// half both surfaces share.
    #[test]
    fn one_quiet_keys_line_carries_no_kbd_badges_unit() {
        assert_eq!(LIST_KEYS, "<p class=keys>1–4 answer · u undo · c own</p>");
        assert!(!LIST_KEYS.contains("<kbd>"), "{LIST_KEYS}");
        assert_eq!(LIST_KEYS.matches("<p class=keys").count(), 1, "{LIST_KEYS}");
        let body = css_rule_body("p.keys");
        assert!(body.contains("font-size:.75rem"), "{body}");
        assert!(body.contains("color:var(--overlay)"), "{body}");
        assert!(body.contains("font-family:var(--mono)"), "{body}");
        // And the digits are on the answers themselves -- which is why the
        // legend over the recommendation says the one word and not the key
        // a second time (George, 2026-09-17, on the deck screenshots).
        let fixture = card_fixture("keys");
        let card = decision_card(&fixture.board, &fixture.store, &fixture.item);
        for digit in ["1", "2", "3"] {
            assert!(
                card.contains(&format!("<span class=key>{digit}</span>")),
                "{card}"
            );
        }
        assert!(
            card.contains("<legend>Recommended</legend>"),
            "the recommendation's legend is not the one word: {card}"
        );
        assert!(
            !card.contains("press 1"),
            "the legend still repeats the digit the button carries: {card}"
        );
        assert!(!card.contains("<kbd>"), "{card}");
    }

    /// WEB-03 — no glyph prefix anywhere.
    #[test]
    fn no_heading_or_link_carries_a_glyph_prefix_unit() {
        let stripped = css_without_comments(CSS);
        assert!(!stripped.contains("content:'>'"), "{stripped}");
        assert!(!stripped.contains("content:'> '"), "{stripped}");
        let fixture = card_fixture("glyph");
        let card = decision_card(&fixture.board, &fixture.store, &fixture.item);
        let rendered = page("Needs you", &format!("<h1>Needs you</h1>{card}"));
        // Every heading and every link text the renderers produce, read out
        // of the served bytes.
        for opener in ["<h1>", "<h2 ", "<h2>", "<a "] {
            let mut rest = rendered.as_str();
            while let Some(at) = rest.find(opener) {
                let after = &rest[at + opener.len()..];
                let text_at = match after.find('>') {
                    Some(close) if opener.ends_with(' ') => close + 1,
                    _ => 0,
                };
                let text = &after[text_at..];
                let end = text.find('<').unwrap_or(text.len());
                let text = text[..end].trim();
                assert!(
                    !text.starts_with('>'),
                    "{opener} text starts with >: {text}"
                );
                assert!(
                    !text.ends_with('→'),
                    "{opener} text ends with an arrow: {text}"
                );
                rest = after;
            }
        }
    }

    /// WEB-29 — a board refusal is the board's own sentence.
    #[test]
    fn a_refusal_is_the_boards_sentence_in_red_unit() {
        // The two channels are distinguished by attribute and never share a
        // node: the composer's own sentence is `incomplete`, the board's is
        // `board`.
        assert!(
            JS.contains("refusal.setAttribute('data-refusal', kind)"),
            "{JS}"
        );
        assert!(JS.contains("[data-refusal=board]"), "{JS}");
        assert!(
            JS.contains("showRefusal(form, INCOMPLETE_ANSWER, 'incomplete')"),
            "{JS}"
        );
        assert!(
            JS.contains("showRefusal(form, refused ? refused.textContent : `The board refused this decision (${response.status}).`, 'board')"),
            "{JS}"
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

    /// WEB-39 — search is the first thing in the drawer.
    #[test]
    fn the_drawer_puts_search_before_every_destination_unit() {
        let rendered = page("Boards", "<h1>Boards</h1>");
        let drawer_at = rendered.find("<nav class=drawer").expect("the drawer");
        let drawer = &rendered[drawer_at..];
        let search = drawer.find("data-nav-search").expect("the search form");
        for destination in [
            "needs-you",
            "all",
            "decided",
            "lanes",
            "boards",
            "sprints",
            "plans",
            "deployments",
            "subscriptions",
        ] {
            let at = drawer
                .find(&format!("data-nav={destination}"))
                .unwrap_or_else(|| panic!("no {destination} anchor in the drawer"));
            assert!(
                search < at,
                "the search field comes after {destination} in the drawer"
            );
        }
    }

    /// WEB-52 — two announcement channels, each with one role, and every
    /// field is named.
    ///
    /// Asserted here on the shell and the card, which is every input and
    /// textarea this slice renders; the same helper runs over every in-scope
    /// route inside the render fixture, where the registry exists.
    #[test]
    fn every_field_is_labelled_and_status_is_announced_once_unit() {
        let fixture = card_fixture("labels");
        let card = decision_card(&fixture.board, &fixture.store, &fixture.item);
        let deck = page(
            "Needs you",
            &format!(
                "<div class=heading><h1>Needs you</h1></div>\
                 {card}\
                 <aside class=side data-side>\
                 <div class=toasts data-notices role=log aria-live=polite></div></aside>"
            ),
        );
        assert_announcement_channels(&deck, "/");
        assert_every_field_is_named(&deck, "/");
        // The card's own name is its question.
        let id = escape(&fixture.item.id);
        assert!(
            card.contains(&format!("aria-labelledby=\"q-{id}\"")),
            "{card}"
        );
        assert!(card.contains(&format!("<h2 id=\"q-{id}\">")), "{card}");
        // The markup is only half of it: the script builds nodes too, and a
        // refusal that carried `role=alert` was a third announcement channel
        // that shouted whatever the board said. Every role the script writes
        // is named here, so adding one is a decision rather than an
        // accident: the toast log it creates when a page has none, and the
        // hover preview's tooltip, which announces nothing.
        let mut roles = Vec::new();
        let mut rest = JS;
        while let Some(at) = rest.find("setAttribute('role', '") {
            let after = &rest[at + "setAttribute('role', '".len()..];
            let end = after.find('\'').expect("the role closes");
            roles.push(&after[..end]);
            rest = &after[end..];
        }
        assert_eq!(
            roles,
            ["log", "tooltip"],
            "the script writes a role that is neither the toast log nor a hover preview"
        );
        assert!(
            !JS.contains("'alert'") && !JS.contains("\"alert\""),
            "the script raises an alert region: a refusal is described text, not an announcement"
        );
        assert_eq!(
            JS.matches("aria-live").count(),
            1,
            "the script writes aria-live somewhere other than the toast log it creates"
        );
        // ...and the refusal it does build is tied to what it focuses.
        assert!(
            JS.contains("describeRefusal(missing, refusal)")
                && JS.contains("setAttribute('aria-describedby', refusal.id)"),
            "a refusal no longer describes the control the operator is sent to"
        );
    }

    /// WEB-44 — every route the server answers is a route the sweep loads.
    ///
    /// The sweep is a list of URLs in `tests/e2e.rs`, and a list can fall
    /// behind the match it is a list of. So `ROUTE_SHAPES` is the registry
    /// both read, and this counts `render`'s arms in the source to hold the
    /// registry to the match: adding an arm without declaring its shape
    /// fails here, and declaring a shape the sweep does not load fails in
    /// the sweep.
    #[test]
    fn render_answers_exactly_the_declared_shapes_unit() {
        let source = include_str!("serve.rs");
        let body = source
            .split_once("fn render(url: &str) -> Result<String> {")
            .expect("render is declared")
            .1
            .split_once("\nfn post(")
            .expect("render ends before post")
            .0;
        let arms = body.matches("=>").count();
        assert_eq!(
            arms,
            ROUTE_SHAPES.len() + 1,
            "render answers {arms} arms (the last being not-found) but {} shapes are \
             declared: declare the new shape in ROUTE_SHAPES and load it in \
             no_route_overflows_sideways_at_three_widths_in_real_chrome",
            ROUTE_SHAPES.len()
        );
    }

    /// Every badge class the restyle retired, in the two spellings the
    /// renderers used.
    const RETIRED_BADGES: [&str; 8] = [
        "class=tag",
        "class=\"tag",
        "class=kind",
        "class=\"kind",
        "class=type",
        "class=\"type",
        "class=lane",
        "class=\"lane",
    ];

    /// WEB-41 — one pill style everywhere.
    ///
    /// The stylesheet is asked what it DECLARES: exactly one `.pill` rule,
    /// status modifiers that only recolour it, and no surviving rule that
    /// makes a tag, a kind, a type or a priority into a second badge. The
    /// renderers are asked what they EMIT, because "no served markup uses a
    /// second badge class" is a claim about every page rather than about the
    /// pages one fixture happens to seed.
    #[test]
    fn exactly_one_pill_style_exists_unit() {
        let pill_rules = css_rules(CSS)
            .into_iter()
            .filter(|rule| rule.selectors == vec![".pill".to_owned()])
            .collect::<Vec<_>>();
        assert_eq!(
            pill_rules.len(),
            1,
            "the stylesheet declares {} rules that select exactly .pill",
            pill_rules.len()
        );
        let body = &pill_rules[0].body;
        for declaration in [
            "background:var(--surface0)",
            "font-size:.75rem",
            "border-radius:999px",
        ] {
            assert!(
                body.contains(declaration),
                "the pill does not declare {declaration}: {body}"
            );
        }
        for rule in css_rules(CSS) {
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
        for rule in css_rules(CSS) {
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
        // What the renderers emit. The source is read rather than one page,
        // because the claim is about every arm of `render`.
        const SOURCE: &str = include_str!("serve.rs");
        let renderers = SOURCE
            .split_once("#[cfg(test)]\nmod tests {")
            .expect("the module has tests")
            .0;
        for badge in RETIRED_BADGES {
            assert!(
                !renderers.contains(badge),
                "a renderer still emits the retired badge {badge}"
            );
        }
        assert!(
            !renderers.contains("class=status") && !renderers.contains("class=\"status"),
            "a renderer still emits a bare status badge instead of the one pill"
        );
    }

    /// WEB-42 — tables lose their borders and keep the hairline.
    #[test]
    fn tables_declare_only_the_row_hairline_unit() {
        let parts = ["table", "th", "td", "th.n", "td.n"];
        for rule in css_rules(CSS) {
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
            head.contains("font-size:.75rem") && head.contains("color:var(--overlay)"),
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
                "/preview/{kind}/{project}/{id}",
                "/preview/board/{project}",
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
             | [\"plan\", _, _, \"open\"] | [\"subscription\", _, _, \"pause\" | \"resume\"]",
            "the write surface moved"
        );
    }

    /// One `role=status`, one `role=log`, and no third live region.
    ///
    /// Read off the MARKUP: the script is inlined into the same document and
    /// its comments name the roles it manages, which is prose about the
    /// contract rather than a region claiming one.
    fn assert_announcement_channels(html: &str, route: &str) {
        let markup = html.split("<script>").next().unwrap_or(html);
        assert_eq!(
            markup.matches("role=status").count(),
            1,
            "{route} does not carry exactly one role=status"
        );
        assert!(
            markup.matches("role=log").count() <= 1,
            "{route} carries more than one role=log"
        );
        assert_eq!(
            markup.matches("aria-live").count(),
            markup.matches("role=status").count() + markup.matches("role=log").count(),
            "{route} has a live region that is neither the status nor the log"
        );
    }

    /// Every `input` and `textarea` is named by a label or an aria-label.
    fn assert_every_field_is_named(html: &str, route: &str) {
        for opener in ["<input ", "<textarea "] {
            let mut rest = html;
            while let Some(at) = rest.find(opener) {
                let after = &rest[at..];
                let end = after.find('>').expect("the element closes");
                let element = &after[..end];
                rest = &after[end..];
                if element.contains("type=hidden") {
                    continue;
                }
                if element.contains("aria-label") {
                    continue;
                }
                let id = element
                    .split_once("id=\"")
                    .map(|(_, tail)| tail.split('"').next().unwrap_or("").to_owned())
                    .or_else(|| {
                        element.split_once("id=").map(|(_, tail)| {
                            tail.split_whitespace().next().unwrap_or("").to_owned()
                        })
                    })
                    .unwrap_or_default();
                assert!(
                    !id.is_empty(),
                    "{route} renders an unnamed field with no id: {element}"
                );
                assert!(
                    html.contains(&format!("for=\"{id}\"")) || html.contains(&format!("for={id}")),
                    "{route} renders {element} with no label naming {id}"
                );
            }
        }
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
                    )
                    .expect("raise an open item");
                if board == "atmux" {
                    decided.push((project.board_path.clone(), item.id));
                }
            }
        }

        // Twelve open across two boards, said as a count on the list and as
        // the length of the queue the deck reads. The deck's own `12 left`
        // is rendered by the bundle now (`t-1f495a7f`) and is observed in
        // the browser; what the SERVER owes both surfaces is one queue, so
        // that is what is counted here.
        let list = render("/all").expect("render the plain list");
        assert_html_contains(
            &list,
            "<p class=count><span data-open-count>12</span> open across 2 boards</p>",
        );
        assert_eq!(queue_length(), 12, "the deck's queue is not the list's");

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
                )
                .expect("settle an atmux row");
        }
        assert_eq!(queue_length(), 3, "settled rows are still in the queue");
        let list = render("/all").expect("re-render the plain list");
        assert_html_contains(
            &list,
            "<p class=count><span data-open-count>3</span> open across 1 board</p>",
        );

        // Every in-scope SERVER-RENDERED route keeps both announcement
        // channels and reaches no third party (WEB-52, WEB-54), which is a
        // claim about the routes and so is made where the routes can be
        // rendered. `/`, `/search` and `/task/{project}/{id}` are not in
        // the list because they are no longer among them: they answer the
        // application shell, whose own shape is held by
        // `the_app_shell_closes_its_head_exactly_once_unit` (it links the
        // two embedded assets and nothing else) and whose rendered page is
        // judged in the browser. `/decided`, `/boards`, `/board/{project}`,
        // `/lanes`, `/deployments`, `/deployment/{project}/{id}`,
        // `/subscriptions`, `/sprints`, `/sprints/{project}`,
        // `/sprint/{project}/{id}` and `/plans` left the list the same way
        // with `t-bf255880` wave 1.
        for route in ["/all", "/no/such/page"] {
            let html = render(route).unwrap_or_else(|error| panic!("render {route}: {error}"));
            assert_announcement_channels(&html, route);
            assert_every_field_is_named(&html, route);
            assert_no_third_party(&html, route);
        }
    }

    /// WEB-54 — the document reaches no third party.
    #[test]
    fn the_document_references_no_third_party_unit() {
        let fixture = card_fixture("third-party");
        let card = decision_card(&fixture.board, &fixture.store, &fixture.item);
        assert_no_third_party(&page("Needs you", &card), "/");
    }

    fn assert_no_third_party(html: &str, route: &str) {
        for forbidden in ["<link ", "<img ", "<iframe ", "<script src"] {
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

    #[test]
    fn operator_shell_keeps_phone_touch_and_live_status_contract() {
        let rendered = page(
            "Needs you",
            "<span class=live data-live role=status aria-live=polite>live</span>",
        );
        assert!(rendered.contains("width=device-width,initial-scale=1"));
        assert!(rendered.contains("role=status aria-live=polite"));
        // 44 CSS px is the floor a thumb needs (spec WEB-45, Apple's HIG
        // minimum), and 2.75rem is that at the root size this page sets.
        assert!(CSS.contains("min-height:2.75rem"));
        assert!(!CSS.contains("min-height:2.6rem"), "{CSS}");
        assert!(CSS.contains("env(safe-area-inset-bottom)"));
        assert!(CSS.contains(".attention-count"));
        assert!(CSS.contains("@media(max-width:700px)"));
        assert!(CSS.contains(":focus-visible"));
        assert!(JS.contains("input[name=outcome]:checked"));
        assert!(JS.contains("dataset.cardBound"));
        assert!(JS.contains("data-receipt"));
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

        // `/all` is the queue as one plain list, and the card is the card:
        // the page explains itself at length here, where there is room for
        // a paragraph.
        let list = render("/all").expect("render all open");
        assert_page_title(&list, "Needs you");
        assert_html_contains(&list, "Needs you");
        assert_html_contains(&list, "Please review before release");
        assert_html_contains(
            &list,
            "<p class=explain>Each card is one question an agent is waiting on.",
        );
        assert_html_contains(
            &list,
            "<p class=count><span data-open-count>1</span> open across",
        );
        // The page script is the same on every page, and it names the deck's
        // hooks; what says this page is not a deck is its `<main>`.
        assert_html_contains(&list, "<main id=main>");
        // Orientation above the question, in one sentence naming who asked,
        // where, and when -- and the free-text answer folded out of the way
        // beneath the choices.
        assert_html_contains(&list, "<p class=eyebrow>geoyws asked on ");
        assert_html_contains(
            &list,
            "<details class=custom data-custom data-testid=deck-custom>\
             <summary>Answer in my own words</summary>",
        );
        assert_html_contains(&list, "<legend>recorded as</legend>");
        // No recommendation is authored on this row, so no recommended
        // fieldset is rendered for it; the carded case is pinned in the e2e
        // suite.
        assert!(!list.contains("class=recommended"), "{list}");
        assert_html_contains(&list, "<div class=reply><label for=\"answer-");
        assert_html_contains(&list, ">Add a note</label>");
        assert!(
            !list.contains("Sent with whichever answer you pick"),
            "the note field grew a hint paragraph again: {list}"
        );
        assert_html_contains(&list, "value=\"approve\" data-label=\"Approve - proceed\"");
        // The submit is live in the markup: a rendered `disabled` made the
        // click do nothing at all, and the native POST has to reach the
        // route's own validation.
        assert_html_contains(
            &list,
            "<button type=submit class=record name=decision value=custom \
             data-testid=deck-record>Record my answer</button>",
        );
        // Two voices. The card refuses in the page's language, naming both
        // halves the way the card itself names them, and nothing served here
        // speaks CLI flags: the route's wording is quoted only when the route
        // has actually said it.
        assert_html_contains(
            &list,
            "Your own answer needs both halves: pick a verdict \
             (approve, reject, defer or other) and write your reply.",
        );
        assert!(
            !list.contains("--outcome"),
            "the card is refusing in command-line flags to somebody on a phone: {list}"
        );
        // The release for a picked verdict, rendered away until there is one
        // to release. It is a control and not only a keystroke because this
        // shell is phone-first and a phone has no Escape key.
        assert_html_contains(
            &list,
            "<button type=button class=clear data-clear data-testid=deck-clear hidden>\
             Clear verdict</button>",
        );
        assert!(
            !list.contains("value=custom disabled"),
            "the free-text submit is rendered disabled, so a click reports nothing: {list}"
        );
        assert_html_contains(&list, "/board/SERVE-RENDER");
        assert!(!list.contains("<strong>before release</strong>"));
        let replied_list =
            render(&format!("/all?replied={}", fixture.epic_id)).expect("render replied");
        assert_html_contains(
            &replied_list,
            "<code>e-serve-render</code> is decided and the board has it.",
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
        assert_eq!(age(now - 3 * 24 * 60 * 60_000), "3 days");
        assert_eq!(age(now - 25 * 60 * 60_000), "1 day");
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
        assert_eq!(ago(now - 3 * 24 * 60 * 60_000), "3 days ago");
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

    fn subscription_fixture(id: &str) -> Subscription {
        Subscription {
            id: id.to_owned(),
            protocol_version: 1,
            subject_task_id: None,
            relations: Vec::new(),
            kinds: Vec::new(),
            prior_statuses: Vec::new(),
            current_statuses: Vec::new(),
            tags: Vec::new(),
            consumer_id: "codex.queue".to_owned(),
            action_id: "enqueue-turn".to_owned(),
            timeout_ms: 30_000,
            max_retries: 3,
            rate_per_minute: 60,
            max_concurrency: 1,
            start_event_seq: 4,
            secret_ref: None,
            status: "active".to_owned(),
            created_at: 1_787_529_600_000,
            created_by: OPERATOR_ACTOR.to_owned(),
            updated_at: 1_787_529_600_000,
            updated_by: OPERATOR_ACTOR.to_owned(),
            paused_at: None,
            paused_by: None,
        }
    }

    fn subscription_view(
        board: &str,
        subscription: Subscription,
        head_event_seq: i64,
        position: SubscriptionPosition,
    ) -> SubscriptionView {
        SubscriptionView {
            board: board.to_owned(),
            subscription,
            position,
            dead_letter_codes: Vec::new(),
            head_event_seq,
        }
    }

    /// A caught-up subscription whose only queued state is dead letters
    /// carrying the given codes, with the count derived from them so a
    /// fixture cannot claim a total its own codes contradict.
    fn dead_lettered_view(id: &str, codes: &[(&str, i64)]) -> SubscriptionView {
        SubscriptionView {
            board: "PX".to_owned(),
            subscription: subscription_fixture(id),
            position: SubscriptionPosition {
                acked_through_seq: Some(12),
                dead_letter: codes.iter().map(|(_, deliveries)| deliveries).sum(),
                ..SubscriptionPosition::default()
            },
            dead_letter_codes: codes
                .iter()
                .map(|(code, deliveries)| DeadLetterCode {
                    code: (*code).to_owned(),
                    deliveries: *deliveries,
                })
                .collect(),
            head_event_seq: 12,
        }
    }

    #[test]
    fn a_watch_sentence_names_every_narrowing_in_plain_words() {
        // Nothing narrowing it means it really does watch everything, and
        // saying so is the difference between "all events" and six empty
        // columns an operator has to interpret.
        assert_eq!(
            watch_sentence(&subscription_fixture("sub-bare")),
            "Every event on the board."
        );

        let mut narrowed = subscription_fixture("sub-narrowed");
        narrowed.kinds = vec!["task_moved".to_owned()];
        narrowed.current_statuses = vec!["done".to_owned()];
        assert_eq!(
            watch_sentence(&narrowed),
            "Every task_moved event arriving at done."
        );

        let mut every = subscription_fixture("sub-every");
        every.kinds = vec!["note_added".to_owned(), "checkpoint_added".to_owned()];
        every.subject_task_id = Some("t-1".to_owned());
        every.relations = vec!["parent:t-9".to_owned()];
        every.prior_statuses = vec!["todo".to_owned()];
        every.current_statuses = vec!["in_progress".to_owned(), "review".to_owned()];
        every.tags = vec!["ops".to_owned(), "release".to_owned(), "infra".to_owned()];
        assert_eq!(
            watch_sentence(&every),
            "Every note_added or checkpoint_added event about task t-1, related through \
             parent:t-9, leaving todo, arriving at in_progress or review, tagged ops, release, \
             or infra."
        );
    }

    #[test]
    fn a_position_reads_as_caught_up_only_when_the_head_is_reached() {
        assert_eq!(position_sentence(12, 12), "Caught up with head seq 12.");
        assert_eq!(
            position_sentence(12, 11),
            "1 board event behind head seq 12."
        );
        assert_eq!(
            position_sentence(12, 8),
            "4 board events behind head seq 12."
        );
        // An ack that reads past the head is not a negative backlog.
        assert_eq!(position_sentence(12, 14), "Caught up with head seq 12.");
    }

    #[test]
    fn an_empty_subscriptions_page_invites_the_command_that_creates_one() {
        let html = subscriptions_body(&[], false, None);
        assert_html_contains(&html, "Nothing is subscribed yet.");
        assert_html_contains(&html, "kb subscription add --consumer NAME --action NAME");
        assert_html_contains(&html, &format!("--as {OPERATOR_ACTOR}"));
        assert!(!html.contains("<table"), "{html}");
    }

    #[test]
    fn a_subscription_row_carries_its_sentence_delivery_state_position_and_limits() {
        let mut subscription = subscription_fixture("sub-one");
        subscription.kinds = vec!["task_moved".to_owned()];
        subscription.current_statuses = vec!["done".to_owned()];
        let html = subscriptions_body(
            &[subscription_view(
                "PX",
                subscription,
                12,
                SubscriptionPosition {
                    acked_through_seq: Some(8),
                    pending: 2,
                    leased: 1,
                    retry_wait: 0,
                    dead_letter: 0,
                },
            )],
            false,
            None,
        );
        assert_html_contains(&html, "<code>sub-one</code>");
        assert_html_contains(
            &html,
            "<a href=\"/board/PX\" data-ref target=_blank rel=noopener>PX</a>",
        );
        assert_html_contains(&html, "Every task_moved event arriving at done.");
        assert_html_contains(&html, "<code>codex.queue</code>");
        assert_html_contains(&html, "action <code>enqueue-turn</code> and");
        assert_html_contains(&html, "4 board events behind head seq 12.");
        assert_html_contains(&html, "started at seq 4, acked through seq 8, 1 in flight");
        assert_html_contains(&html, "30000 ms timeout, 3 retries, 60/min, 1 at a time");
        assert_html_contains(&html, "action=\"/subscription/PX/sub-one/pause\"");
        assert_html_contains(&html, "Pause delivery");
        assert_html_contains(&html, "2 pending");
    }

    #[test]
    fn subscription_positions_keep_each_sentence_and_order() {
        let views = [
            subscription_view(
                "PX",
                subscription_fixture("sub-one"),
                20,
                SubscriptionPosition {
                    acked_through_seq: Some(20),
                    ..SubscriptionPosition::default()
                },
            ),
            subscription_view(
                "PX",
                subscription_fixture("sub-two"),
                20,
                SubscriptionPosition {
                    acked_through_seq: Some(15),
                    pending: 5,
                    ..SubscriptionPosition::default()
                },
            ),
            subscription_view(
                "KB",
                subscription_fixture("sub-three"),
                20,
                SubscriptionPosition::default(),
            ),
        ];
        let html = subscriptions_body(&views, false, None);
        assert_html_contains(&html, "Caught up with head seq 20.");
        assert_html_contains(&html, "5 board events behind head seq 20.");
        // Nothing acked leaves a subscription on its start anchor, seq 4 here,
        // rather than at seq 0 — which would report 20 events of phantom lag.
        assert_html_contains(&html, "16 board events behind head seq 20.");
        assert_html_contains(&html, "nothing acked yet");
        let placed = ["sub-one", "sub-two", "sub-three"].map(|id| {
            html.find(id)
                .unwrap_or_else(|| panic!("missing {id} in {html}"))
        });
        assert!(placed[0] < placed[1] && placed[1] < placed[2], "{html}");
    }

    #[test]
    fn a_dead_lettered_delivery_is_flagged_for_the_operator_and_a_retrying_one_is_not() {
        let retrying = subscriptions_body(
            &[subscription_view(
                "PX",
                subscription_fixture("sub-retry"),
                12,
                SubscriptionPosition {
                    acked_through_seq: Some(12),
                    retry_wait: 2,
                    ..SubscriptionPosition::default()
                },
            )],
            false,
            None,
        );
        assert_html_contains(&retrying, "<span class=retrying>2 retrying</span>");
        // Zero is silence, not a rendered nought: nothing pending and nothing
        // dead-lettered must not appear at all.
        assert!(
            !retrying.contains("pending") && !retrying.contains("dead-lettered"),
            "an empty count must be silent, not a zero: {retrying}"
        );
        assert!(
            !retrying.contains("class=dead"),
            "a retry needs no operator: {retrying}"
        );

        let dead = subscriptions_body(
            &[dead_lettered_view(
                "sub-dead",
                &[("opencode_endpoint_unreachable", 3)],
            )],
            false,
            None,
        );
        // The count says a person is needed; the code says whether to go and
        // look at a port or at a payload.
        assert_html_contains(
            &dead,
            "<span class=dead>3 dead-lettered, all opencode_endpoint_unreachable</span>",
        );
        assert!(
            !dead.contains("pending") && !dead.contains("retrying"),
            "an empty count must be silent, not a zero: {dead}"
        );
        // Nothing queued renders no queued line at all, which is the whole
        // reason these three stopped being columns: at rest the cell is the
        // position sentence and nothing else.
        let quiet = subscriptions_body(
            &[subscription_view(
                "PX",
                subscription_fixture("sub-quiet"),
                12,
                SubscriptionPosition {
                    acked_through_seq: Some(12),
                    ..SubscriptionPosition::default()
                },
            )],
            false,
            None,
        );
        assert!(
            !quiet.contains("class=queued"),
            "a subscription with nothing waiting must render no queued line: {quiet}"
        );

        // A colour is an outcome now (spec WEB-11): the four hues mean
        // approve, reject, defer and other, and nothing else is coloured. So
        // the queued line is quiet prose and a retry or a dead letter is
        // marked by WEIGHT and by naming itself in words -- which is what
        // the rest of this case reads -- rather than by an amber or a red
        // that would make a colour mean two things.
        assert!(
            CSS.contains(".queued{margin-top:.25rem;color:var(--subtext);font-size:.8125rem}"),
            "{CSS}"
        );
        assert!(
            CSS.contains(".queued .retrying,.queued .dead{color:var(--text);font-weight:700}"),
            "{CSS}"
        );
        assert!(
            CSS.contains("td.waiting{font-weight:600}"),
            "a board with items waiting is marked by weight, not by a hue: {CSS}"
        );
    }

    #[test]
    fn mixed_dead_letter_codes_stay_apart_and_a_summarised_tail_still_adds_up() {
        // Two adapters refusing for two reasons is two pieces of work. A
        // single count, or one code standing in for the set, sends an
        // operator to check one thing and leaves the other one broken.
        let mixed = subscriptions_body(
            &[dead_lettered_view(
                "sub-mixed",
                &[
                    ("opencode_endpoint_unreachable", 3),
                    ("kimi_frame_oversized", 1),
                ],
            )],
            false,
            None,
        );
        assert_html_contains(
            &mixed,
            "<span class=dead>4 dead-lettered: 3 opencode_endpoint_unreachable, \
             1 kimi_frame_oversized</span>",
        );
        assert!(
            !mixed.contains(", all "),
            "\"all\" said about a mixed set is a false diagnosis: {mixed}"
        );

        // Past three codes the tail is summarised, and summarised with its
        // own numbers: 4 + 3 + 2 named plus 1 in the tail is the 10 printed
        // beside them, so the named codes cannot read as the whole story.
        let tail = subscriptions_body(
            &[dead_lettered_view(
                "sub-tail",
                &[
                    ("cursor_worker_busy", 4),
                    ("opencode_request_rejected", 3),
                    ("kimi_peer_unanswered", 2),
                    ("kimi_identity_mismatch", 1),
                ],
            )],
            false,
            None,
        );
        assert_html_contains(
            &tail,
            "<span class=dead>10 dead-lettered: 4 cursor_worker_busy, \
             3 opencode_request_rejected, 2 kimi_peer_unanswered, \
             and 1 more code across 1 delivery</span>",
        );
        assert!(
            !tail.contains("kimi_identity_mismatch"),
            "the tail is summarised, not listed: {tail}"
        );

        let longer_tail = subscriptions_body(
            &[dead_lettered_view(
                "sub-longer-tail",
                &[
                    ("cursor_worker_unavailable", 5),
                    ("opencode_deadline_exceeded", 4),
                    ("opencode_response_invalid", 3),
                    ("kimi_frame_malformed", 2),
                    ("kimi_request_rejected", 1),
                ],
            )],
            false,
            None,
        );
        assert_html_contains(
            &longer_tail,
            "15 dead-lettered: 5 cursor_worker_unavailable, 4 opencode_deadline_exceeded, \
             3 opencode_response_invalid, and 2 more codes across 3 deliveries</span>",
        );

        // A code is ledger text rendered on an operator page, so it is
        // escaped like every other value: a delivery cannot smuggle markup
        // into the one line on this page that is meant to be believed.
        let hostile = subscriptions_body(
            &[dead_lettered_view(
                "sub-hostile",
                &[("<script>steal()</script>", 2)],
            )],
            false,
            None,
        );
        assert!(!hostile.contains("<script>"), "{hostile}");
        assert_html_contains(
            &hostile,
            "2 dead-lettered, all &lt;script&gt;steal()&lt;/script&gt;",
        );

        // A dead letter with no code is a state the delivery table's CHECK
        // forbids. If one ever appears, the page reports the count it has
        // instead of inventing an attribution for it.
        assert_eq!(
            queued_state(
                SubscriptionPosition {
                    dead_letter: 2,
                    ..SubscriptionPosition::default()
                },
                &[],
            ),
            "<div class=queued><span class=dead>2 dead-lettered</span></div>"
        );
    }

    #[test]
    fn a_paused_subscription_is_listed_only_when_the_url_asks_for_it() {
        let mut paused = subscription_fixture("sub-halted");
        paused.status = "paused".to_owned();
        paused.paused_at = Some(now_ms() - 90 * 60_000);
        paused.paused_by = Some(OPERATOR_ACTOR.to_owned());
        let views = [
            subscription_view(
                "PX",
                subscription_fixture("sub-live"),
                12,
                SubscriptionPosition {
                    acked_through_seq: Some(12),
                    ..SubscriptionPosition::default()
                },
            ),
            subscription_view(
                "PX",
                paused,
                12,
                SubscriptionPosition {
                    acked_through_seq: Some(6),
                    pending: 4,
                    ..SubscriptionPosition::default()
                },
            ),
        ];

        let default_view = subscriptions_body(&views, false, None);
        assert_html_contains(&default_view, "sub-live");
        assert!(!default_view.contains("sub-halted"), "{default_view}");
        assert_html_contains(&default_view, "1 paused subscription is hidden.");
        assert_html_contains(
            &default_view,
            "<a href=\"/subscriptions?show=all\">Show paused subscriptions</a>",
        );

        let everything = subscriptions_body(&views, true, None);
        assert_html_contains(&everything, "sub-halted");
        assert_html_contains(
            &everything,
            &format!("paused by {OPERATOR_ACTOR} 1h30m ago"),
        );
        assert_html_contains(&everything, "Resume delivery");
        assert_html_contains(
            &everything,
            "action=\"/subscription/PX/sub-halted/resume?show=all\"",
        );
        assert!(
            !everything.contains("paused subscription is hidden"),
            "{everything}"
        );

        // Hiding every row is not the same fact as having no subscriptions,
        // and the page must not report the filter as an absence.
        let hidden_only = subscriptions_body(&views[1..], false, None);
        assert_html_contains(&hidden_only, "Every subscription here is paused right now.");
        assert!(
            !hidden_only.contains("Nothing is subscribed yet"),
            "{hidden_only}"
        );
    }

    #[test]
    fn a_configured_secret_is_reported_without_naming_it() {
        let mut configured = subscription_fixture("sub-secret");
        configured.secret_ref = Some("codex_queue_token".to_owned());
        let html = subscriptions_body(
            &[subscription_view(
                "PX",
                configured,
                4,
                SubscriptionPosition::default(),
            )],
            false,
            None,
        );
        assert_html_contains(&html, "a secret is configured");
        assert!(!html.contains("codex_queue_token"), "{html}");

        let without = subscriptions_body(
            &[subscription_view(
                "PX",
                subscription_fixture("sub-plain"),
                4,
                SubscriptionPosition::default(),
            )],
            false,
            None,
        );
        assert_html_contains(&without, "no secret configured");
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
        let summary = sprint_summary(&managed, "SPRINT-AUTHZ", &row, true)
            .expect("render visibility-safe summary");
        assert_html_contains(&summary, "data-sprint-open>1");
        assert_html_contains(&summary, "data-sprint-done>1");
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
            )
            .expect("the principal holds this row");
        assert_eq!(
            second.resolved_by.as_deref(),
            Some("other@edge.test"),
            "the second audit identity must be recorded verbatim too"
        );
    }
}
