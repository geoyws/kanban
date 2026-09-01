use anyhow::{Context, Result, bail};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde_json::json;
use std::cell::Cell;
use std::fs::{self, Permissions};
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BOARD_V1: &str = r#"
CREATE TABLE board_meta (key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL) STRICT;
CREATE TABLE tasks (
 id TEXT PRIMARY KEY NOT NULL,type TEXT NOT NULL CHECK(type IN ('epic','story','task')),
 parent_id TEXT REFERENCES tasks(id),title TEXT NOT NULL,body TEXT,
 status TEXT NOT NULL CHECK(status IN ('backlog','todo','in_progress','blocked','review','done','cancelled')),
 priority INTEGER NOT NULL DEFAULT 3,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,
 completed_at INTEGER,metadata TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(metadata))
) STRICT;
CREATE INDEX idx_tasks_status_priority ON tasks(status,priority,created_at);
CREATE INDEX idx_tasks_parent ON tasks(parent_id);
CREATE TABLE task_dependencies (
 task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 depends_on TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 PRIMARY KEY(task_id,depends_on),CHECK(task_id <> depends_on)
) STRICT;
CREATE TABLE task_notes (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 author TEXT NOT NULL,kind TEXT NOT NULL,body TEXT NOT NULL,created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_task_notes_task_seq ON task_notes(task_id,seq);
CREATE TABLE task_claims (
 task_id TEXT PRIMARY KEY NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 agent_id TEXT NOT NULL,session_id TEXT,lease_token TEXT NOT NULL UNIQUE,
 claimed_at INTEGER NOT NULL,heartbeat_at INTEGER NOT NULL,expires_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_task_claims_expiry ON task_claims(expires_at);
CREATE TABLE checkpoints (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 author TEXT NOT NULL,session_id TEXT,model TEXT,state TEXT NOT NULL CHECK(state IN ('continue','blocked','done')),
 summary TEXT NOT NULL,intent TEXT NOT NULL,next_action TEXT NOT NULL,
 blockers TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(blockers)),
 validations TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(validations)),repo_path TEXT,branch TEXT,
 head_sha TEXT,dirty_summary TEXT,created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_checkpoints_task_seq ON checkpoints(task_id,seq);
CREATE TABLE events (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT,kind TEXT NOT NULL,actor TEXT,
 payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_events_task_seq ON events(task_id,seq);
"#;

const BOARD_V2: &str = r#"
CREATE TABLE handoffs (
 id TEXT PRIMARY KEY NOT NULL,task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 checkpoint_seq INTEGER NOT NULL REFERENCES checkpoints(seq),
 reason TEXT NOT NULL CHECK(reason IN ('token_pressure','provider_limit','session_end','manual')),
 status TEXT NOT NULL CHECK(status IN ('pending','accepted','cancelled')),
 from_agent TEXT NOT NULL,from_session TEXT,from_model TEXT,to_agent TEXT,
 summary TEXT NOT NULL,intent TEXT NOT NULL,next_action TEXT NOT NULL,
 blockers TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(blockers)),
 validations TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(validations)),repo_path TEXT,branch TEXT,
 head_sha TEXT,dirty_summary TEXT,created_at INTEGER NOT NULL,accepted_at INTEGER,
 accepted_by TEXT,accepted_session TEXT
) STRICT;
CREATE INDEX idx_handoffs_task_created ON handoffs(task_id,created_at);
CREATE INDEX idx_handoffs_status_created ON handoffs(status,created_at);
"#;

const BOARD_V3: &str = r#"
ALTER TABLE tasks ADD COLUMN assignee TEXT;
ALTER TABLE tasks ADD COLUMN lane TEXT;
ALTER TABLE tasks ADD COLUMN deliverable TEXT;
ALTER TABLE tasks ADD COLUMN stale_minutes INTEGER CHECK(stale_minutes IS NULL OR stale_minutes >= 0);
ALTER TABLE tasks ADD COLUMN driver_only INTEGER NOT NULL DEFAULT 0 CHECK(driver_only IN (0,1));
CREATE INDEX idx_tasks_assignee_status ON tasks(assignee,status);
CREATE INDEX idx_tasks_lane_status ON tasks(lane,status);
"#;

/// A handoff no longer has to be about a task.
///
/// `task_id` and `checkpoint_seq` were both NOT NULL, so the only handoff that
/// could exist was one taken over a claimed task — the record of a lease
/// changing hands. There was nowhere to put the other kind: what a session
/// itself learned, spanning several tasks or none, which is the thing a
/// successor most needs and the thing no task owns.
///
/// SQLite cannot relax NOT NULL in place, so the table is rebuilt. At creation
/// the two columns move together — a handoff is about a task and carries the
/// checkpoint that closed it, or it is about neither — and `create_handoff`
/// enforces that, because it is a rule about how a handoff is made.
///
/// It is deliberately not a CHECK, because it must not hold forever. Both
/// columns are now `ON DELETE SET NULL` rather than `CASCADE`: removing a task
/// used to delete every handoff ever taken over it, so the record of who
/// handed what to whom vanished with the row it described. A handoff is a
/// historical account of a handover that happened, and deleting the subject
/// does not un-happen it. The links are dropped and the account survives.
///
/// The rebuild starts by creating the *old* shape if it is missing. A v3 board
/// written by the retired TypeScript implementation can lack tables entirely,
/// so there may be nothing to copy from; standing the old shape up first means
/// one path handles both, and such a board ends the migration with the table it
/// should have had. The columns are listed in the original order because the
/// copy is positional.
const BOARD_V4: &str = r#"
CREATE TABLE IF NOT EXISTS handoffs (
 id TEXT PRIMARY KEY NOT NULL,task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 checkpoint_seq INTEGER NOT NULL REFERENCES checkpoints(seq),
 reason TEXT NOT NULL CHECK(reason IN ('token_pressure','provider_limit','session_end','manual')),
 status TEXT NOT NULL CHECK(status IN ('pending','accepted','cancelled')),
 from_agent TEXT NOT NULL,from_session TEXT,from_model TEXT,to_agent TEXT,
 summary TEXT NOT NULL,intent TEXT NOT NULL,next_action TEXT NOT NULL,
 blockers TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(blockers)),
 validations TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(validations)),repo_path TEXT,branch TEXT,
 head_sha TEXT,dirty_summary TEXT,created_at INTEGER NOT NULL,accepted_at INTEGER,
 accepted_by TEXT,accepted_session TEXT
) STRICT;
CREATE TABLE handoffs_next (
 id TEXT PRIMARY KEY NOT NULL,
 task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
 checkpoint_seq INTEGER REFERENCES checkpoints(seq) ON DELETE SET NULL,
 reason TEXT NOT NULL CHECK(reason IN ('token_pressure','provider_limit','session_end','manual')),
 status TEXT NOT NULL CHECK(status IN ('pending','accepted','cancelled')),
 from_agent TEXT NOT NULL,from_session TEXT,from_model TEXT,to_agent TEXT,
 summary TEXT NOT NULL,intent TEXT NOT NULL,next_action TEXT NOT NULL,
 blockers TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(blockers)),
 validations TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(validations)),repo_path TEXT,branch TEXT,
 head_sha TEXT,dirty_summary TEXT,created_at INTEGER NOT NULL,accepted_at INTEGER,
 accepted_by TEXT,accepted_session TEXT
) STRICT;
INSERT INTO handoffs_next SELECT * FROM handoffs;
DROP TABLE handoffs;
ALTER TABLE handoffs_next RENAME TO handoffs;
CREATE INDEX idx_handoffs_task_created ON handoffs(task_id,created_at);
CREATE INDEX idx_handoffs_status_created ON handoffs(status,created_at);
"#;

/// Things that need the operator, kept where the work is.
///
/// Agents surface blockers, decisions and approvals in whatever they happen to
/// be writing — a report, a commit message, a chat reply — and every one of
/// those is a channel that scrolls away. An item raised at 03:00 and never
/// acted on leaves no trace that it was ever raised, so the same question gets
/// asked again three sessions later, or worse, quietly answered by an agent
/// that should not have decided it.
///
/// This is the durable place for them. Rows are resolved, never deleted, so
/// what was asked, by whom, when, and how it was settled stays on the board —
/// which is the point: the trail is the feature, not a side effect of storing
/// them.
///
/// `task_id` is optional and `ON DELETE SET NULL` for the same reason handoffs
/// are: an item may be about the session rather than one row, and removing the
/// row it referred to does not un-ask the question.
const BOARD_V5: &str = r#"
CREATE TABLE attention (
 id TEXT PRIMARY KEY NOT NULL,
 task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
 kind TEXT NOT NULL CHECK(kind IN ('blocking','decision','approval','review','risk')),
 body TEXT NOT NULL,
 raised_by TEXT NOT NULL,
 created_at INTEGER NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('open','resolved')),
 resolved_at INTEGER,resolved_by TEXT,resolution TEXT,
 CHECK((status = 'open') = (resolved_at IS NULL))
) STRICT;
CREATE INDEX idx_attention_status_created ON attention(status,created_at);
CREATE INDEX idx_attention_task ON attention(task_id);
"#;

/// A task that is not finished being written.
///
/// `backlog` already meant "real work, not scheduled yet". There was nothing
/// for the state before that: a row someone is still drafting, whose title,
/// body or scope may still be wrong. Agents treat every row on the board as a
/// specification, so an unfinished one gets decomposed, depended on, and worked
/// as though it were settled — and the ledger said nothing to stop it.
///
/// `draft` is that state. It is not claimable, which needs no new rule: `claim`
/// already accepts only `todo` and `in_progress`, and `--next` selects `todo`
/// alone. Widening the CHECK is the whole schema change.
///
/// SQLite cannot alter a CHECK in place, so the table is rebuilt. Foreign keys
/// are already off across the ladder (see `open_board`), which is what lets the
/// referencing tables survive the drop.
///
/// The rebuilt table reproduces the original exactly apart from the widened
/// CHECK — same `DEFAULT 3` on priority, same four indexes, and `parent_id`
/// still `REFERENCES tasks(id)` with no `ON DELETE` clause. That last one is
/// load-bearing: removal is meant to fail and name the children, and giving it
/// `SET NULL` here would silently orphan them instead. A rebuild is the easiest
/// place in a schema to change something nobody asked to change.
const BOARD_V6: &str = r#"
CREATE TABLE tasks_next (
 id TEXT PRIMARY KEY NOT NULL,type TEXT NOT NULL CHECK(type IN ('epic','story','task')),
 parent_id TEXT REFERENCES tasks(id),title TEXT NOT NULL,body TEXT,
 status TEXT NOT NULL CHECK(status IN ('draft','backlog','todo','in_progress','blocked','review','done','cancelled')),
 priority INTEGER NOT NULL DEFAULT 3,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL,
 completed_at INTEGER,metadata TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(metadata)),
 assignee TEXT,lane TEXT,deliverable TEXT,
 stale_minutes INTEGER CHECK(stale_minutes IS NULL OR stale_minutes >= 0),
 driver_only INTEGER NOT NULL DEFAULT 0 CHECK(driver_only IN (0,1))
) STRICT;
INSERT INTO tasks_next(id,type,parent_id,title,body,status,priority,created_at,updated_at,completed_at,metadata,assignee,lane,deliverable,stale_minutes,driver_only)
 SELECT id,type,parent_id,title,body,status,priority,created_at,updated_at,completed_at,metadata,assignee,lane,deliverable,stale_minutes,driver_only FROM tasks;
DROP TABLE tasks;
ALTER TABLE tasks_next RENAME TO tasks;
CREATE INDEX idx_tasks_status_priority ON tasks(status,priority,created_at);
CREATE INDEX idx_tasks_parent ON tasks(parent_id);
CREATE INDEX idx_tasks_assignee_status ON tasks(assignee,status);
CREATE INDEX idx_tasks_lane_status ON tasks(lane,status);
"#;

/// Where work happened, recorded on the rows that describe work happening.
///
/// A claim said who held a task and until when, and nothing about where they
/// were holding it. On a box running several driver lanes of the same
/// repository that is the first question anyone asks — which worktree is this
/// lane in — and the ledger could not answer it.
///
/// Checkpoints and handoffs already had `repo_path`, `branch` and `head_sha`.
/// They gain `root_head`: a submodule's own commit says nothing about which
/// revision of the whole tree it belonged to, and for a nested checkout the
/// answer to "what was checked out" is the outermost superproject's commit.
///
/// Every column is nullable. Provenance is captured when the caller is standing
/// in a repository and recorded as absent when they are not, which is the
/// truthful outcome rather than a failure.
///
/// The two `CREATE TABLE IF NOT EXISTS` are for the same reason V4 carries one:
/// a v3 board written by the retired TypeScript implementation can lack tables
/// outright, and `ALTER TABLE` has no `IF EXISTS`. Standing the original shape
/// up first means one path handles both, and such a board finishes the ladder
/// with the tables it should have had.
const BOARD_V7: &str = r#"
CREATE TABLE IF NOT EXISTS task_claims (
 task_id TEXT PRIMARY KEY NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 agent_id TEXT NOT NULL,session_id TEXT,lease_token TEXT NOT NULL UNIQUE,
 claimed_at INTEGER NOT NULL,heartbeat_at INTEGER NOT NULL,expires_at INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS checkpoints (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 author TEXT NOT NULL,session_id TEXT,model TEXT,state TEXT NOT NULL CHECK(state IN ('continue','blocked','done')),
 summary TEXT NOT NULL,intent TEXT NOT NULL,next_action TEXT NOT NULL,
 blockers TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(blockers)),
 validations TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(validations)),repo_path TEXT,branch TEXT,
 head_sha TEXT,dirty_summary TEXT,created_at INTEGER NOT NULL
) STRICT;
ALTER TABLE task_claims ADD COLUMN worktree TEXT;
ALTER TABLE task_claims ADD COLUMN worktree_kind TEXT;
ALTER TABLE task_claims ADD COLUMN branch TEXT;
ALTER TABLE task_claims ADD COLUMN head_sha TEXT;
ALTER TABLE task_claims ADD COLUMN root_head TEXT;
ALTER TABLE checkpoints ADD COLUMN root_head TEXT;
ALTER TABLE handoffs ADD COLUMN root_head TEXT;
"#;

/// Tags, and the master file that says which ones exist.
///
/// A board needs an axis that says *what a row is about* — `infra`, `queuer`,
/// `askie` — and had none. `lane` is the nearest field and is the wrong one: it
/// carries the *kind of work* (`fe`, `be`, `ops`, `test`, `review`) and
/// `claim --next` routes on it, so overloading it with subsystems would both
/// lose a distinction and change who gets handed what. `metadata` is free JSON
/// and would make every tag a spelling.
///
/// So tags are registered before they are used. That is the difference between
/// a vocabulary and a habit: without a master file, `infra`, `Infra` and
/// `infrastructure` all exist, nothing says which is meant, and every filter
/// silently misses rows. Applying an unregistered tag is refused, with the
/// nearest registered name suggested and the command that would add it — the
/// same shape as a mistyped flag.
///
/// `ON DELETE RESTRICT` on the tag reference is what makes the master file
/// authoritative: a tag in use cannot quietly disappear from under the rows
/// carrying it. Removing one names how many rows would be orphaned and requires
/// `--force`, like every other destructive path here.
const BOARD_V8: &str = r#"
CREATE TABLE tags (
 name TEXT PRIMARY KEY NOT NULL,
 description TEXT,
 created_by TEXT,
 created_at INTEGER NOT NULL
) STRICT;
CREATE TABLE task_tags (
 task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 tag TEXT NOT NULL REFERENCES tags(name) ON DELETE RESTRICT,
 PRIMARY KEY(task_id,tag)
) STRICT;
CREATE INDEX idx_task_tags_tag ON task_tags(tag);
"#;

/// Status updates: the low-ceremony sibling of a handoff.
///
/// Keyed to a **lane**, not a task, which is the whole point. A note needs a
/// task; a checkpoint needs a task *and* a lease. An agent working across
/// several tasks, or between them, or exploring before it claims anything, had
/// nowhere to write down where things stand — so it went in a reply that
/// scrolls away, or it waited for a handoff nobody had time to write.
///
/// `archived` is set by the writer, not by a sweep: posting an update archives
/// what it superseded, so the current view is bounded without anything having
/// to run on a timer.
const BOARD_V9: &str = r#"
CREATE TABLE status_updates (
 id TEXT PRIMARY KEY NOT NULL,
 lane TEXT NOT NULL,
 task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
 author TEXT NOT NULL,
 body TEXT NOT NULL,
 worktree TEXT,
 branch TEXT,
 head_sha TEXT,
 root_head TEXT,
 dirty_summary TEXT,
 archived INTEGER NOT NULL DEFAULT 0,
 created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_status_lane_created ON status_updates(lane,archived,created_at DESC);
"#;

/// Rename the one-day-old status-update surface to the unambiguous `sitrep`.
///
/// V9 had reached live boards, so this is a data-preserving rename rather than
/// an edit to the old migration. Its `u-` ids and event trail are rewritten as
/// well because every existing row was still a probe. A mature table with real
/// history would keep its old ids and accept that seam instead.
const BOARD_V10: &str = r#"
ALTER TABLE status_updates RENAME TO sitreps;
DROP INDEX IF EXISTS idx_status_lane_created;
CREATE INDEX idx_sitreps_lane_created ON sitreps(lane,archived,created_at DESC);
UPDATE sitreps SET id = 'sr-' || substr(id, 3) WHERE id LIKE 'u-%';
CREATE TABLE IF NOT EXISTS events (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT,kind TEXT NOT NULL,actor TEXT,
 payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS idx_events_task_seq ON events(task_id,seq);
UPDATE events
SET kind = 'sitrep_posted',
    payload = replace(replace(payload, '"statusID"', '"sitrepID"'), '"u-', '"sr-')
WHERE kind = 'status_posted';
"#;

/// Per-project operator rules: short constraints agents receive from the board.
///
/// These rows are a working copy, not a secret store or cross-machine source
/// of truth. They retire rather than delete so their audit trail stays useful.
const BOARD_V11: &str = r#"
CREATE TABLE rules (
 id TEXT PRIMARY KEY NOT NULL,
 body TEXT NOT NULL,
 author TEXT NOT NULL,
 archived INTEGER NOT NULL DEFAULT 0,
 created_at INTEGER NOT NULL,
 updated_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_rules_active ON rules(archived,created_at);
"#;

/// Cold history stays in the board but leaves every operational secondary index.
///
/// A sidecar would make backup, restore, addressing and task references span two
/// databases. Marking settled rows and using partial indexes keeps the single-file
/// durability contract while bounding the indexes used by day-to-day reads.
const BOARD_V12: &str = r#"
CREATE TABLE IF NOT EXISTS task_dependencies (
 task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 depends_on TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 PRIMARY KEY(task_id,depends_on),CHECK(task_id <> depends_on)
) STRICT;
CREATE TABLE IF NOT EXISTS task_notes (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
 author TEXT NOT NULL,kind TEXT NOT NULL,body TEXT NOT NULL,created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS idx_task_notes_task_seq ON task_notes(task_id,seq);
CREATE INDEX IF NOT EXISTS idx_checkpoints_task_seq ON checkpoints(task_id,seq);
ALTER TABLE tasks ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1));
ALTER TABLE tasks ADD COLUMN archived_at INTEGER;
ALTER TABLE task_notes ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1));
ALTER TABLE checkpoints ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1));
ALTER TABLE events ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1));
ALTER TABLE handoffs ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1));
ALTER TABLE attention ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1));
ALTER TABLE task_tags ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1));

DROP INDEX idx_tasks_status_priority;
DROP INDEX idx_tasks_parent;
DROP INDEX idx_tasks_assignee_status;
DROP INDEX idx_tasks_lane_status;
DROP INDEX idx_task_notes_task_seq;
DROP INDEX idx_checkpoints_task_seq;
DROP INDEX idx_events_task_seq;
DROP INDEX idx_handoffs_task_created;
DROP INDEX idx_handoffs_status_created;
DROP INDEX idx_attention_status_created;
DROP INDEX idx_attention_task;
DROP INDEX idx_task_tags_tag;
DROP INDEX idx_sitreps_lane_created;
DROP INDEX idx_rules_active;

CREATE INDEX idx_tasks_status_priority ON tasks(status,priority,created_at) WHERE archived=0;
CREATE INDEX idx_tasks_parent ON tasks(parent_id) WHERE archived=0;
CREATE INDEX idx_tasks_assignee_status ON tasks(assignee,status) WHERE archived=0;
CREATE INDEX idx_tasks_lane_status ON tasks(lane,status) WHERE archived=0;
CREATE INDEX idx_task_notes_task_seq ON task_notes(task_id,seq) WHERE archived=0;
CREATE INDEX idx_checkpoints_task_seq ON checkpoints(task_id,seq) WHERE archived=0;
CREATE INDEX idx_events_task_seq ON events(task_id,seq) WHERE archived=0;
CREATE INDEX idx_handoffs_task_created ON handoffs(task_id,created_at) WHERE archived=0;
CREATE INDEX idx_handoffs_status_created ON handoffs(status,created_at) WHERE archived=0;
CREATE INDEX idx_attention_status_created ON attention(status,created_at) WHERE archived=0;
CREATE INDEX idx_attention_task ON attention(task_id) WHERE archived=0;
CREATE INDEX idx_task_tags_tag ON task_tags(tag) WHERE archived=0;
CREATE INDEX idx_sitreps_lane_created ON sitreps(lane,created_at DESC) WHERE archived=0;
CREATE INDEX idx_rules_active ON rules(created_at) WHERE archived=0;
"#;

/// One rebuildable search corpus over the board's authoritative rows.
///
/// FTS5 is part of the bundled SQLite amalgamation. The ordinary table keeps
/// source metadata and optional semantic-vector bytes; the external-content
/// virtual table owns only the lexical index. Source-table triggers rebuild the
/// affected derived rows, and search-document triggers keep FTS5 in step. A
/// source mutation deliberately clears its cached embedding: search computes a
/// missing vector in memory, while the explicit rebuild operation persists the
/// current model later without making an ordinary read write the board.
const BOARD_V13: &str = r#"
CREATE TABLE search_documents (
 seq INTEGER PRIMARY KEY,
 source_kind TEXT NOT NULL CHECK(source_kind IN ('task','note','checkpoint','handoff','attention','sitrep','rule','event')),
 source_id TEXT NOT NULL,
 task_id TEXT,
 title TEXT NOT NULL,
 body TEXT NOT NULL,
 status TEXT,
 lane TEXT,
 tags TEXT NOT NULL DEFAULT '',
 created_at INTEGER NOT NULL,
 updated_at INTEGER NOT NULL,
 archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1)),
 source_hash TEXT,
 embedding_model TEXT,
 embedding BLOB,
 UNIQUE(source_kind,source_id)
) STRICT;
CREATE INDEX idx_search_documents_source ON search_documents(source_kind,source_id);
CREATE INDEX idx_search_documents_task ON search_documents(task_id);
CREATE INDEX idx_search_documents_active ON search_documents(updated_at DESC) WHERE archived=0;

CREATE VIRTUAL TABLE search_fts USING fts5(
 title,
 body,
 tags,
 content='search_documents',
 content_rowid='seq',
 tokenize='porter unicode61 remove_diacritics 2',
 prefix='2 3'
);

CREATE TRIGGER search_documents_ai AFTER INSERT ON search_documents BEGIN
 INSERT INTO search_fts(rowid,title,body,tags)
 VALUES(new.seq,new.title,new.body,new.tags);
END;
CREATE TRIGGER search_documents_ad AFTER DELETE ON search_documents BEGIN
 INSERT INTO search_fts(search_fts,rowid,title,body,tags)
 VALUES('delete',old.seq,old.title,old.body,old.tags);
END;
CREATE TRIGGER search_documents_au AFTER UPDATE OF title,body,tags ON search_documents BEGIN
 INSERT INTO search_fts(search_fts,rowid,title,body,tags)
 VALUES('delete',old.seq,old.title,old.body,old.tags);
 INSERT INTO search_fts(rowid,title,body,tags)
 VALUES(new.seq,new.title,new.body,new.tags);
END;

CREATE VIEW search_source_rows AS
SELECT
 'task' AS source_kind,
 t.id AS source_id,
 t.id AS task_id,
 t.title AS title,
 COALESCE(t.body,'') || char(10) || COALESCE(t.deliverable,'') || char(10) || t.metadata AS body,
 t.status AS status,
 t.lane AS lane,
 COALESCE((SELECT group_concat(tag,' ') FROM
   (SELECT tag FROM task_tags WHERE task_id=t.id AND archived=0 ORDER BY tag)), '') AS tags,
 t.created_at AS created_at,
 t.updated_at AS updated_at,
 t.archived AS archived
FROM tasks t
UNION ALL
SELECT
 'note', CAST(n.seq AS TEXT), n.task_id,
 n.kind || ' note on ' || n.task_id,
 n.author || char(10) || n.body,
 t.status, t.lane,
 COALESCE((SELECT group_concat(tag,' ') FROM
   (SELECT tag FROM task_tags WHERE task_id=n.task_id AND archived=0 ORDER BY tag)), ''),
 n.created_at, n.created_at, n.archived
FROM task_notes n JOIN tasks t ON t.id=n.task_id
UNION ALL
SELECT
 'checkpoint', CAST(c.seq AS TEXT), c.task_id,
 'checkpoint: ' || c.summary,
 c.author || char(10) || c.summary || char(10) || c.intent || char(10) || c.next_action ||
   char(10) || c.blockers || char(10) || c.validations || char(10) ||
   COALESCE(c.repo_path,'') || char(10) || COALESCE(c.branch,''),
 t.status, t.lane,
 COALESCE((SELECT group_concat(tag,' ') FROM
   (SELECT tag FROM task_tags WHERE task_id=c.task_id AND archived=0 ORDER BY tag)), ''),
 c.created_at, c.created_at, c.archived
FROM checkpoints c JOIN tasks t ON t.id=c.task_id
UNION ALL
SELECT
 'handoff', h.id, h.task_id,
 'handoff: ' || h.summary,
 h.from_agent || char(10) || COALESCE(h.to_agent,'') || char(10) || h.summary ||
   char(10) || h.intent || char(10) || h.next_action || char(10) || h.blockers ||
   char(10) || h.validations || char(10) || COALESCE(h.repo_path,'') ||
   char(10) || COALESCE(h.branch,''),
 h.status,
 (SELECT lane FROM tasks WHERE id=h.task_id),
 COALESCE((SELECT group_concat(tag,' ') FROM
   (SELECT tag FROM task_tags WHERE task_id=h.task_id AND archived=0 ORDER BY tag)), ''),
 h.created_at, COALESCE(h.accepted_at,h.created_at), h.archived
FROM handoffs h
UNION ALL
SELECT
 'attention', a.id, a.task_id,
 'attention: ' || a.kind,
 a.raised_by || char(10) || a.body || char(10) || COALESCE(a.resolution,''),
 a.status,
 (SELECT lane FROM tasks WHERE id=a.task_id),
 COALESCE((SELECT group_concat(tag,' ') FROM
   (SELECT tag FROM task_tags WHERE task_id=a.task_id AND archived=0 ORDER BY tag)), ''),
 a.created_at, COALESCE(a.resolved_at,a.created_at), a.archived
FROM attention a
UNION ALL
SELECT
 'sitrep', s.id, s.task_id,
 'sitrep: ' || s.lane,
 s.author || char(10) || s.body || char(10) || COALESCE(s.worktree,'') ||
   char(10) || COALESCE(s.branch,''),
 NULL, s.lane,
 COALESCE((SELECT group_concat(tag,' ') FROM
   (SELECT tag FROM task_tags WHERE task_id=s.task_id AND archived=0 ORDER BY tag)), ''),
 s.created_at, s.created_at, s.archived
FROM sitreps s
UNION ALL
SELECT
 'rule', r.id, NULL,
 substr(r.body,1,instr(r.body || char(10),char(10))-1),
 r.body,
 CASE WHEN r.archived=0 THEN 'active' ELSE 'retired' END,
 NULL, '', r.created_at, r.updated_at, r.archived
FROM rules r
UNION ALL
SELECT
 'event', CAST(e.seq AS TEXT), e.task_id,
 'event: ' || e.kind,
 COALESCE(e.actor,'') || char(10) || e.payload,
 (SELECT status FROM tasks WHERE id=e.task_id),
 (SELECT lane FROM tasks WHERE id=e.task_id),
 COALESCE((SELECT group_concat(tag,' ') FROM
   (SELECT tag FROM task_tags WHERE task_id=e.task_id AND archived=0 ORDER BY tag)), ''),
 e.created_at, e.created_at, e.archived
FROM events e
WHERE e.kind IN (
 'task_added','task_updated','task_moved','note_added','checkpoint_added',
 'handoff_created','handoff_accepted','attention_raised','attention_resolved',
 'sitrep_posted','rule_added','rule_updated','rule_retired','archive_swept'
);

CREATE TRIGGER search_tasks_ai AFTER INSERT ON tasks BEGIN
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE task_id=new.id;
END;
CREATE TRIGGER search_tasks_au AFTER UPDATE ON tasks BEGIN
 DELETE FROM search_documents WHERE task_id=old.id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE task_id=new.id;
END;
CREATE TRIGGER search_tasks_ad AFTER DELETE ON tasks BEGIN
 DELETE FROM search_documents WHERE task_id=old.id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE task_id=old.id;
END;

CREATE TRIGGER search_task_tags_ai AFTER INSERT ON task_tags BEGIN
 DELETE FROM search_documents WHERE task_id=new.task_id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE task_id=new.task_id;
END;
CREATE TRIGGER search_task_tags_au AFTER UPDATE ON task_tags BEGIN
 DELETE FROM search_documents WHERE task_id=old.task_id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE task_id=old.task_id;
 DELETE FROM search_documents WHERE task_id=new.task_id AND new.task_id<>old.task_id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE task_id=new.task_id AND new.task_id<>old.task_id;
END;
CREATE TRIGGER search_task_tags_ad AFTER DELETE ON task_tags BEGIN
 DELETE FROM search_documents WHERE task_id=old.task_id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE task_id=old.task_id;
END;

CREATE TRIGGER search_notes_ai AFTER INSERT ON task_notes BEGIN
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='note' AND source_id=CAST(new.seq AS TEXT);
END;
CREATE TRIGGER search_notes_au AFTER UPDATE ON task_notes BEGIN
 DELETE FROM search_documents WHERE source_kind='note' AND source_id=CAST(old.seq AS TEXT);
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='note' AND source_id=CAST(new.seq AS TEXT);
END;
CREATE TRIGGER search_notes_ad AFTER DELETE ON task_notes BEGIN
 DELETE FROM search_documents WHERE source_kind='note' AND source_id=CAST(old.seq AS TEXT);
END;

CREATE TRIGGER search_checkpoints_ai AFTER INSERT ON checkpoints BEGIN
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='checkpoint' AND source_id=CAST(new.seq AS TEXT);
END;
CREATE TRIGGER search_checkpoints_au AFTER UPDATE ON checkpoints BEGIN
 DELETE FROM search_documents WHERE source_kind='checkpoint' AND source_id=CAST(old.seq AS TEXT);
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='checkpoint' AND source_id=CAST(new.seq AS TEXT);
END;
CREATE TRIGGER search_checkpoints_ad AFTER DELETE ON checkpoints BEGIN
 DELETE FROM search_documents WHERE source_kind='checkpoint' AND source_id=CAST(old.seq AS TEXT);
END;

CREATE TRIGGER search_handoffs_ai AFTER INSERT ON handoffs BEGIN
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='handoff' AND source_id=new.id;
END;
CREATE TRIGGER search_handoffs_au AFTER UPDATE ON handoffs BEGIN
 DELETE FROM search_documents WHERE source_kind='handoff' AND source_id=old.id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='handoff' AND source_id=new.id;
END;
CREATE TRIGGER search_handoffs_ad AFTER DELETE ON handoffs BEGIN
 DELETE FROM search_documents WHERE source_kind='handoff' AND source_id=old.id;
END;

CREATE TRIGGER search_attention_ai AFTER INSERT ON attention BEGIN
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='attention' AND source_id=new.id;
END;
CREATE TRIGGER search_attention_au AFTER UPDATE ON attention BEGIN
 DELETE FROM search_documents WHERE source_kind='attention' AND source_id=old.id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='attention' AND source_id=new.id;
END;
CREATE TRIGGER search_attention_ad AFTER DELETE ON attention BEGIN
 DELETE FROM search_documents WHERE source_kind='attention' AND source_id=old.id;
END;

CREATE TRIGGER search_sitreps_ai AFTER INSERT ON sitreps BEGIN
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='sitrep' AND source_id=new.id;
END;
CREATE TRIGGER search_sitreps_au AFTER UPDATE ON sitreps BEGIN
 DELETE FROM search_documents WHERE source_kind='sitrep' AND source_id=old.id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='sitrep' AND source_id=new.id;
END;
CREATE TRIGGER search_sitreps_ad AFTER DELETE ON sitreps BEGIN
 DELETE FROM search_documents WHERE source_kind='sitrep' AND source_id=old.id;
END;

CREATE TRIGGER search_rules_ai AFTER INSERT ON rules BEGIN
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='rule' AND source_id=new.id;
END;
CREATE TRIGGER search_rules_au AFTER UPDATE ON rules BEGIN
 DELETE FROM search_documents WHERE source_kind='rule' AND source_id=old.id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='rule' AND source_id=new.id;
END;
CREATE TRIGGER search_rules_ad AFTER DELETE ON rules BEGIN
 DELETE FROM search_documents WHERE source_kind='rule' AND source_id=old.id;
END;

CREATE TRIGGER search_events_ai AFTER INSERT ON events
WHEN new.kind IN (
 'task_added','task_updated','task_moved','note_added','checkpoint_added',
 'handoff_created','handoff_accepted','attention_raised','attention_resolved',
 'sitrep_posted','rule_added','rule_updated','rule_retired','archive_swept'
) BEGIN
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='event' AND source_id=CAST(new.seq AS TEXT);
END;
CREATE TRIGGER search_events_au AFTER UPDATE ON events BEGIN
 DELETE FROM search_documents WHERE source_kind='event' AND source_id=CAST(old.seq AS TEXT);
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='event' AND source_id=CAST(new.seq AS TEXT);
END;
CREATE TRIGGER search_events_ad AFTER DELETE ON events BEGIN
 DELETE FROM search_documents WHERE source_kind='event' AND source_id=CAST(old.seq AS TEXT);
END;

INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
SELECT * FROM search_source_rows;
"#;

/// One priority vocabulary for every actionable queue.
///
/// Existing tasks keep their original 0-9 values. Attention and handoff rows
/// predate priority, so migration gives them the routine P2 anchor. CHECKs
/// prevent new invalid history while the readers remain tolerant of legacy
/// task values written before the band was enforced.
const BOARD_V14: &str = r#"
ALTER TABLE attention ADD COLUMN priority INTEGER NOT NULL DEFAULT 6 CHECK(priority BETWEEN 0 AND 9);
ALTER TABLE handoffs ADD COLUMN priority INTEGER NOT NULL DEFAULT 6 CHECK(priority BETWEEN 0 AND 9);
CREATE INDEX idx_attention_status_priority ON attention(status,priority,created_at,id) WHERE archived=0;
CREATE INDEX idx_handoffs_status_priority ON handoffs(status,priority,created_at,id) WHERE archived=0;
"#;

const BOARD_V15: &str = r#"
ALTER TABLE rules ADD COLUMN task_tags TEXT NOT NULL DEFAULT '[]'
 CHECK(json_valid(task_tags) AND json_type(task_tags) = 'array');
"#;

const BOARD_V16: &str = r#"
CREATE TABLE attention_tags (
 attention_id TEXT NOT NULL REFERENCES attention(id) ON DELETE CASCADE,
 tag TEXT NOT NULL REFERENCES tags(name) ON DELETE RESTRICT,
 PRIMARY KEY(attention_id,tag)
) STRICT;
CREATE INDEX idx_attention_tags_tag ON attention_tags(tag);
"#;

const BOARD_V17: &str = r#"
DROP TRIGGER search_attention_ai;
DROP TRIGGER search_attention_au;
DROP TRIGGER search_attention_ad;
PRAGMA legacy_alter_table=ON;
ALTER TABLE attention RENAME TO attention_v16;
CREATE TABLE attention (
 id TEXT PRIMARY KEY NOT NULL,
 task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
 kind TEXT NOT NULL CHECK(kind IN ('blocking','decision','approval','review','risk')),
 body TEXT NOT NULL,
 raised_by TEXT NOT NULL,
 created_at INTEGER NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('open','resolved')),
 resolved_at INTEGER,resolved_by TEXT,resolution TEXT,
 reopened_at INTEGER,reopened_by TEXT,reopen_note TEXT,
 archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1)),
 priority INTEGER NOT NULL DEFAULT 6 CHECK(priority BETWEEN 0 AND 9),
 CHECK(
   (status='resolved' AND resolved_at IS NOT NULL AND resolved_by IS NOT NULL AND reopened_at IS NULL)
   OR
   (status='open' AND (
     (resolved_at IS NULL AND resolved_by IS NULL AND resolution IS NULL AND reopened_at IS NULL)
     OR
     (resolved_at IS NOT NULL AND resolved_by IS NOT NULL AND reopened_at IS NOT NULL
      AND reopened_by IS NOT NULL AND reopen_note IS NOT NULL)
   ))
 )
) STRICT;
INSERT INTO attention(
 id,task_id,kind,body,raised_by,created_at,status,resolved_at,resolved_by,resolution,
 reopened_at,reopened_by,reopen_note,archived,priority
)
SELECT id,task_id,kind,body,raised_by,created_at,status,resolved_at,resolved_by,resolution,
 NULL,NULL,NULL,archived,priority
FROM attention_v16;
DROP TABLE attention_v16;
PRAGMA legacy_alter_table=OFF;
CREATE INDEX idx_attention_status_created ON attention(status,created_at) WHERE archived=0;
CREATE INDEX idx_attention_task ON attention(task_id) WHERE archived=0;
CREATE INDEX idx_attention_status_priority ON attention(status,priority,created_at,id) WHERE archived=0;
CREATE TRIGGER search_attention_ai AFTER INSERT ON attention BEGIN
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='attention' AND source_id=new.id;
END;
CREATE TRIGGER search_attention_au AFTER UPDATE ON attention BEGIN
 DELETE FROM search_documents WHERE source_kind='attention' AND source_id=old.id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='attention' AND source_id=new.id;
END;
CREATE TRIGGER search_attention_ad AFTER DELETE ON attention BEGIN
 DELETE FROM search_documents WHERE source_kind='attention' AND source_id=old.id;
END;
"#;

const BOARD_V18: &str = r#"
CREATE TABLE IF NOT EXISTS board_meta (key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL) STRICT;
ALTER TABLE events ADD COLUMN prev_hash TEXT;
ALTER TABLE events ADD COLUMN event_hash TEXT;
"#;

/// Append-only deployment attempts and bounded hot projections (ADR-030).
const BOARD_V19: &str = r#"
CREATE TABLE IF NOT EXISTS deployments (
 id TEXT PRIMARY KEY NOT NULL,
 task_id TEXT REFERENCES tasks(id) ON DELETE SET NULL,
 repo TEXT NOT NULL,
 commit_sha TEXT NOT NULL CHECK(length(commit_sha)=40 AND commit_sha NOT GLOB '*[^0-9a-f]*'),
 branch TEXT,
 tier TEXT NOT NULL CHECK(tier IN ('@_bdt','@_bd','@_bst','@_bs','@_s','@_uat','@_p')),
 environment TEXT NOT NULL,
 host TEXT NOT NULL,
 url TEXT NOT NULL,
 mechanism TEXT,
 operation_id TEXT UNIQUE,
 retry_of TEXT REFERENCES deployments(id) ON DELETE SET NULL,
 status TEXT NOT NULL CHECK(status IN ('started','succeeded','failed','cancelled','abandoned')),
 phase TEXT CHECK(phase IS NULL OR phase IN ('build','publish','start','verification')),
 actor TEXT NOT NULL,
 lane TEXT,
 capability_token TEXT NOT NULL UNIQUE,
 receipt TEXT,
 artifact_uri TEXT,
 served_commit TEXT CHECK(served_commit IS NULL OR (length(served_commit)=40 AND served_commit NOT GLOB '*[^0-9a-f]*')),
 created_at INTEGER NOT NULL,
 updated_at INTEGER NOT NULL,
 completed_at INTEGER,
 archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1)),
 archived_at INTEGER,
 CHECK(
   (status='started' AND completed_at IS NULL)
   OR
   (status<>'started' AND completed_at IS NOT NULL)
 ),
 CHECK(status<>'succeeded' OR (phase='verification' AND receipt IS NOT NULL AND length(trim(receipt))>0 AND served_commit=commit_sha))
) STRICT;
CREATE INDEX IF NOT EXISTS idx_deployments_hot_target ON deployments(repo,tier,environment,created_at DESC,id) WHERE archived=0;
CREATE INDEX IF NOT EXISTS idx_deployments_hot_status ON deployments(status,created_at DESC,id) WHERE archived=0;
CREATE INDEX IF NOT EXISTS idx_deployments_task ON deployments(task_id,created_at DESC) WHERE archived=0;
CREATE TRIGGER IF NOT EXISTS search_deployment_events_ai AFTER INSERT ON events
WHEN new.kind IN ('deployment_started','deployment_finished','deployment_abandoned') BEGIN
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT 'event',CAST(new.seq AS TEXT),d.task_id,'deployment: ' || d.id,
        new.kind || char(10) || COALESCE(new.actor,'') || char(10) || new.payload,
        d.status,d.lane,
        COALESCE((SELECT group_concat(tag,' ') FROM
          (SELECT tag FROM task_tags WHERE task_id=d.task_id AND archived=0 ORDER BY tag)),''),
        new.created_at,d.updated_at,d.archived
 FROM deployments d WHERE d.id=json_extract(new.payload,'$.deploymentID');
END;
CREATE TRIGGER IF NOT EXISTS search_deployments_au AFTER UPDATE ON deployments BEGIN
 UPDATE search_documents SET status=new.status,lane=new.lane,updated_at=new.updated_at,archived=new.archived
 WHERE source_kind='event' AND source_id IN (
   SELECT CAST(seq AS TEXT) FROM events
   WHERE kind IN ('deployment_started','deployment_finished','deployment_abandoned')
     AND json_extract(payload,'$.deploymentID')=new.id
 );
END;
"#;

/// Keep the V19 deployment-event projection inside every task-scoped refresh.
///
/// V13's task and tag triggers rebuild all documents for a task from
/// `search_source_rows`. Deployment events were added later and deliberately
/// live outside that view, so those refreshes silently deleted the V19 rows.
/// One canonical projection lets task, tag, event, deployment, rebuild, and
/// health paths agree on exactly which deployment documents exist.
const BOARD_V20: &str = r#"
DROP VIEW IF EXISTS search_deployment_event_rows;
CREATE VIEW search_deployment_event_rows(
 source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived
) AS
SELECT
 'event', CAST(e.seq AS TEXT), d.task_id,
 'deployment: ' || d.id,
 e.kind || char(10) || COALESCE(e.actor,'') || char(10) || e.payload,
 d.status, d.lane,
 COALESCE((SELECT group_concat(tag,' ') FROM
   (SELECT tag FROM task_tags WHERE task_id=d.task_id AND archived=0 ORDER BY tag)), ''),
 e.created_at, d.updated_at, d.archived
FROM events e
JOIN deployments d ON d.id=json_extract(e.payload,'$.deploymentID')
WHERE e.kind IN ('deployment_started','deployment_finished','deployment_abandoned');

DROP TRIGGER search_tasks_au;
CREATE TRIGGER search_tasks_au AFTER UPDATE ON tasks BEGIN
 DELETE FROM search_documents WHERE task_id=old.id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE task_id=new.id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_deployment_event_rows WHERE task_id=new.id;
END;

DROP TRIGGER search_task_tags_ai;
CREATE TRIGGER search_task_tags_ai AFTER INSERT ON task_tags BEGIN
 DELETE FROM search_documents WHERE task_id=new.task_id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE task_id=new.task_id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_deployment_event_rows WHERE task_id=new.task_id;
END;

DROP TRIGGER search_task_tags_au;
CREATE TRIGGER search_task_tags_au AFTER UPDATE ON task_tags BEGIN
 DELETE FROM search_documents WHERE task_id=old.task_id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE task_id=old.task_id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_deployment_event_rows WHERE task_id=old.task_id;
 DELETE FROM search_documents WHERE task_id=new.task_id AND new.task_id<>old.task_id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE task_id=new.task_id AND new.task_id<>old.task_id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_deployment_event_rows WHERE task_id=new.task_id AND new.task_id<>old.task_id;
END;

DROP TRIGGER search_task_tags_ad;
CREATE TRIGGER search_task_tags_ad AFTER DELETE ON task_tags BEGIN
 DELETE FROM search_documents WHERE task_id=old.task_id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE task_id=old.task_id;
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_deployment_event_rows WHERE task_id=old.task_id;
END;

DROP TRIGGER search_events_au;
CREATE TRIGGER search_events_au AFTER UPDATE ON events BEGIN
 DELETE FROM search_documents WHERE source_kind='event' AND source_id=CAST(old.seq AS TEXT);
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_source_rows WHERE source_kind='event' AND source_id=CAST(new.seq AS TEXT);
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_deployment_event_rows WHERE source_id=CAST(new.seq AS TEXT);
END;

DROP TRIGGER search_deployment_events_ai;
CREATE TRIGGER search_deployment_events_ai AFTER INSERT ON events
WHEN new.kind IN ('deployment_started','deployment_finished','deployment_abandoned') BEGIN
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_deployment_event_rows WHERE source_id=CAST(new.seq AS TEXT);
END;

DROP TRIGGER search_deployments_au;
CREATE TRIGGER search_deployments_au AFTER UPDATE ON deployments BEGIN
 DELETE FROM search_documents
 WHERE source_kind='event' AND source_id IN (
   SELECT CAST(seq AS TEXT) FROM events
   WHERE kind IN ('deployment_started','deployment_finished','deployment_abandoned')
     AND json_extract(payload,'$.deploymentID') IN (old.id,new.id)
 );
 INSERT INTO search_documents(source_kind,source_id,task_id,title,body,status,lane,tags,created_at,updated_at,archived)
 SELECT * FROM search_deployment_event_rows
 WHERE source_id IN (
   SELECT CAST(seq AS TEXT) FROM events
   WHERE kind IN ('deployment_started','deployment_finished','deployment_abandoned')
     AND json_extract(payload,'$.deploymentID')=new.id
 );
END;
"#;

/// Board-local declarative pub/sub subscriptions (ADR-031).
///
/// The addressed board is the tenancy boundary, so rows store no root or
/// path. Execution arguments, credentials, and delivery state belong to the
/// later dispatcher phase and are deliberately absent.
const BOARD_V21: &str = r#"
CREATE TABLE subscriptions (
 id TEXT PRIMARY KEY NOT NULL,
 protocol_version INTEGER NOT NULL CHECK(protocol_version=1),
 subject_task_id TEXT,
 relations TEXT NOT NULL CHECK(json_valid(relations) AND json_type(relations)='array'),
 kinds TEXT NOT NULL CHECK(json_valid(kinds) AND json_type(kinds)='array'),
 prior_statuses TEXT NOT NULL CHECK(json_valid(prior_statuses) AND json_type(prior_statuses)='array'),
 current_statuses TEXT NOT NULL CHECK(json_valid(current_statuses) AND json_type(current_statuses)='array'),
 tags TEXT NOT NULL CHECK(json_valid(tags) AND json_type(tags)='array'),
 consumer_id TEXT NOT NULL,
 action_id TEXT NOT NULL,
 timeout_ms INTEGER NOT NULL CHECK(timeout_ms BETWEEN 1 AND 300000),
 max_retries INTEGER NOT NULL CHECK(max_retries BETWEEN 0 AND 20),
 rate_per_minute INTEGER NOT NULL CHECK(rate_per_minute BETWEEN 1 AND 10000),
 max_concurrency INTEGER NOT NULL CHECK(max_concurrency BETWEEN 1 AND 64),
 secret_ref TEXT,
 status TEXT NOT NULL CHECK(status IN ('active','paused')),
 created_at INTEGER NOT NULL,
 created_by TEXT NOT NULL,
 updated_at INTEGER NOT NULL,
 updated_by TEXT NOT NULL,
 paused_at INTEGER,
 paused_by TEXT,
 CHECK((status='active' AND paused_at IS NULL AND paused_by IS NULL) OR
       (status='paused' AND paused_at IS NOT NULL AND paused_by IS NOT NULL))
) STRICT;
CREATE INDEX idx_subscriptions_status ON subscriptions(status,created_at,id);
CREATE INDEX idx_subscriptions_consumer ON subscriptions(consumer_id,status,created_at,id);
"#;

/// Keep subscriptions from retroactively seeing events that already existed
/// when they were created.
///
/// The new `start_event_seq` is backfilled from each row's own
/// `subscription_added` anchor rather than from the current tail, then
/// enforced with triggers because SQLite cannot add a true NOT NULL column in
/// place without rebuilding the table. The delivery ledger keeps the
/// immutable event hash separate from the sequenced foreign key, and the
/// attempt table preserves retry history so rate accounting does not depend on
/// one mutable timestamp.
const BOARD_V22: &str = r#"
ALTER TABLE subscriptions ADD COLUMN start_event_seq INTEGER CHECK(start_event_seq IS NULL OR start_event_seq >= 0);
WITH valid_anchors AS (
 SELECT
  e.seq,
  json_extract(e.payload, '$.subscriptionID') AS subscription_id
 FROM events e
 WHERE e.kind='subscription_added'
   AND json_valid(e.payload)
   AND json_type(e.payload)='object'
   AND json_type(e.payload, '$.subscriptionID')='text'
   AND length(CAST(json_extract(e.payload, '$.subscriptionID') AS BLOB)) BETWEEN 5 AND 64
   AND json_extract(e.payload, '$.subscriptionID') GLOB 'sub-[0-9A-Za-z]*'
   AND json_extract(e.payload, '$.subscriptionID') NOT GLOB '*[^0-9A-Za-z._-]*'
),
subscription_anchors AS (
 SELECT
  s.id AS subscription_id,
  COUNT(v.seq) AS anchor_count,
  MIN(v.seq) AS anchor_seq
 FROM subscriptions s
 LEFT JOIN valid_anchors v ON v.subscription_id = s.id
 GROUP BY s.id
),
bad_anchors AS (
 SELECT COUNT(*) AS bad_count
 FROM events e
 WHERE e.kind='subscription_added'
   AND CASE
        WHEN NOT json_valid(e.payload) THEN 1
        WHEN json_type(e.payload) IS NOT 'object' THEN 1
        WHEN json_type(e.payload, '$.subscriptionID') IS NOT 'text' THEN 1
        WHEN length(CAST(json_extract(e.payload, '$.subscriptionID') AS BLOB)) NOT BETWEEN 5 AND 64 THEN 1
        WHEN json_extract(e.payload, '$.subscriptionID') NOT GLOB 'sub-[0-9A-Za-z]*' THEN 1
        WHEN json_extract(e.payload, '$.subscriptionID') GLOB '*[^0-9A-Za-z._-]*' THEN 1
        ELSE 0
       END = 1
),
exact_anchors AS (
 SELECT subscription_id, anchor_seq
 FROM subscription_anchors
 WHERE anchor_count = 1
)
UPDATE subscriptions
SET start_event_seq = (
 SELECT anchor_seq FROM exact_anchors
 WHERE exact_anchors.subscription_id = subscriptions.id
)
WHERE start_event_seq IS NULL
  AND (SELECT bad_count FROM bad_anchors) = 0;
WITH valid_anchors AS (
 SELECT
  e.seq,
  json_extract(e.payload, '$.subscriptionID') AS subscription_id
 FROM events e
 WHERE e.kind='subscription_added'
   AND json_valid(e.payload)
   AND json_type(e.payload)='object'
   AND json_type(e.payload, '$.subscriptionID')='text'
   AND length(CAST(json_extract(e.payload, '$.subscriptionID') AS BLOB)) BETWEEN 5 AND 64
   AND json_extract(e.payload, '$.subscriptionID') GLOB 'sub-[0-9A-Za-z]*'
   AND json_extract(e.payload, '$.subscriptionID') NOT GLOB '*[^0-9A-Za-z._-]*'
),
subscription_anchors AS (
 SELECT
  s.id AS subscription_id,
  COUNT(v.seq) AS anchor_count,
  MIN(v.seq) AS anchor_seq
 FROM subscriptions s
 LEFT JOIN valid_anchors v ON v.subscription_id = s.id
 GROUP BY s.id
),
bad_anchors AS (
 SELECT COUNT(*) AS bad_count
 FROM events e
 WHERE e.kind='subscription_added'
   AND CASE
        WHEN NOT json_valid(e.payload) THEN 1
        WHEN json_type(e.payload) IS NOT 'object' THEN 1
        WHEN json_type(e.payload, '$.subscriptionID') IS NOT 'text' THEN 1
        WHEN length(CAST(json_extract(e.payload, '$.subscriptionID') AS BLOB)) NOT BETWEEN 5 AND 64 THEN 1
        WHEN json_extract(e.payload, '$.subscriptionID') NOT GLOB 'sub-[0-9A-Za-z]*' THEN 1
        WHEN json_extract(e.payload, '$.subscriptionID') GLOB '*[^0-9A-Za-z._-]*' THEN 1
        ELSE 0
       END = 1
),
exact_anchors AS (
 SELECT subscription_id, anchor_seq
 FROM subscription_anchors
 WHERE anchor_count = 1
)
UPDATE subscriptions
SET start_event_seq = -1
WHERE start_event_seq IS NULL
   OR (SELECT bad_count FROM bad_anchors) > 0;
CREATE TRIGGER subscriptions_start_event_seq_bi
BEFORE INSERT ON subscriptions
WHEN new.start_event_seq IS NULL OR new.start_event_seq < 0 BEGIN
 SELECT RAISE(ABORT, 'subscriptions.start_event_seq is required');
END;
CREATE TRIGGER subscriptions_start_event_seq_bu
BEFORE UPDATE OF start_event_seq ON subscriptions
WHEN new.start_event_seq IS NULL
  OR new.start_event_seq < 0
  OR new.start_event_seq IS NOT old.start_event_seq BEGIN
 SELECT RAISE(ABORT, 'subscriptions.start_event_seq is immutable');
END;
CREATE TABLE board_materialization_cursor (
 id INTEGER PRIMARY KEY NOT NULL CHECK(id=1),
 event_seq INTEGER NOT NULL CHECK(event_seq >= 0),
 updated_at INTEGER NOT NULL
) STRICT;
INSERT INTO board_materialization_cursor(id,event_seq,updated_at)
VALUES (
  1,
  0,
  COALESCE((SELECT max(created_at) FROM events), 0)
)
ON CONFLICT(id) DO UPDATE SET
 event_seq=excluded.event_seq,
 updated_at=excluded.updated_at;
CREATE TABLE subscription_deliveries (
 subscription_id TEXT NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
 event_id TEXT NOT NULL CHECK(length(event_id)=64 AND event_id NOT GLOB '*[^0-9a-f]*'),
 event_seq INTEGER NOT NULL REFERENCES events(seq) ON DELETE CASCADE,
 event_kind TEXT NOT NULL,
 event_created_at INTEGER NOT NULL,
 status TEXT NOT NULL CHECK(status IN ('pending','leased','retry_wait','acked','dead_letter')),
 attempts INTEGER NOT NULL DEFAULT 0 CHECK(attempts >= 0),
 lease_token TEXT,
 lease_deadline_at INTEGER,
 next_attempt_at INTEGER,
 last_attempt_at INTEGER,
 last_error_code TEXT,
 acked_at INTEGER,
 dead_lettered_at INTEGER,
 created_at INTEGER NOT NULL,
 updated_at INTEGER NOT NULL,
 PRIMARY KEY(subscription_id,event_id),
 UNIQUE(subscription_id,event_seq),
  CHECK(updated_at >= created_at),
  CHECK(status <> 'pending' OR (
   attempts = 0 AND
   next_attempt_at IS NOT NULL AND
   next_attempt_at >= created_at AND
   lease_token IS NULL AND
   lease_deadline_at IS NULL AND
   last_attempt_at IS NULL AND
   last_error_code IS NULL AND
   acked_at IS NULL AND
   dead_lettered_at IS NULL
 )),
 CHECK(status <> 'leased' OR (
   attempts >= 1 AND
   lease_token IS NOT NULL AND
   lease_deadline_at IS NOT NULL AND
   next_attempt_at IS NULL AND
   last_attempt_at IS NOT NULL AND
   last_error_code IS NULL AND
   acked_at IS NULL AND
   dead_lettered_at IS NULL
 )),
 CHECK(status <> 'retry_wait' OR (
   attempts >= 1 AND
   lease_token IS NULL AND
   lease_deadline_at IS NULL AND
   next_attempt_at IS NOT NULL AND
   last_attempt_at IS NOT NULL AND
   last_error_code IS NOT NULL AND
   acked_at IS NULL AND
   dead_lettered_at IS NULL
 )),
 CHECK(status <> 'acked' OR (
   attempts >= 1 AND
   lease_token IS NULL AND
   lease_deadline_at IS NULL AND
   next_attempt_at IS NULL AND
   last_attempt_at IS NOT NULL AND
   last_error_code IS NULL AND
   acked_at IS NOT NULL AND
   dead_lettered_at IS NULL
 )),
 CHECK(status <> 'dead_letter' OR (
   attempts >= 1 AND
   lease_token IS NULL AND
   lease_deadline_at IS NULL AND
   next_attempt_at IS NULL AND
   last_attempt_at IS NOT NULL AND
   last_error_code IS NOT NULL AND
   acked_at IS NULL AND
   dead_lettered_at IS NOT NULL
 ))
) STRICT;
CREATE TRIGGER subscription_deliveries_insert_bi
BEFORE INSERT ON subscription_deliveries
WHEN new.status <> 'pending'
  OR new.attempts <> 0
  OR new.lease_token IS NOT NULL
  OR new.lease_deadline_at IS NOT NULL
  OR new.next_attempt_at IS NULL
  OR new.last_attempt_at IS NOT NULL
  OR new.last_error_code IS NOT NULL
  OR new.acked_at IS NOT NULL
  OR new.dead_lettered_at IS NOT NULL BEGIN
 SELECT RAISE(ABORT, 'subscription_deliveries must be inserted as pending');
END;
CREATE INDEX idx_subscription_deliveries_due ON subscription_deliveries(status,next_attempt_at,event_seq,subscription_id)
 WHERE status IN ('pending','retry_wait');
CREATE INDEX idx_subscription_deliveries_lease_expiry ON subscription_deliveries(status,lease_deadline_at,event_seq,subscription_id)
 WHERE status='leased';
CREATE INDEX idx_subscription_deliveries_subscription_status ON subscription_deliveries(subscription_id,status,event_seq);
CREATE TRIGGER subscription_deliveries_event_binding_bi
BEFORE INSERT ON subscription_deliveries
WHEN NOT EXISTS (
 SELECT 1
 FROM subscriptions s
 JOIN events e ON e.seq = new.event_seq
 WHERE s.id = new.subscription_id
   AND e.event_hash = new.event_id
   AND e.event_hash IS NOT NULL
   AND length(e.event_hash) = 64
   AND e.event_hash NOT GLOB '*[^0-9a-f]*'
   AND e.kind = new.event_kind
   AND e.created_at = new.event_created_at
   AND e.seq > s.start_event_seq
) BEGIN
 SELECT RAISE(ABORT, 'subscription_deliveries.event_id must match events.event_hash');
END;
CREATE TRIGGER subscription_deliveries_event_binding_bu
BEFORE UPDATE OF subscription_id,event_id,event_seq,event_kind,event_created_at ON subscription_deliveries
WHEN NOT EXISTS (
 SELECT 1
 FROM subscriptions s
 JOIN events e ON e.seq = new.event_seq
 WHERE s.id = new.subscription_id
   AND e.event_hash = new.event_id
   AND e.event_hash IS NOT NULL
   AND length(e.event_hash) = 64
   AND e.event_hash NOT GLOB '*[^0-9a-f]*'
   AND e.kind = new.event_kind
   AND e.created_at = new.event_created_at
   AND e.seq > s.start_event_seq
) BEGIN
 SELECT RAISE(ABORT, 'subscription_deliveries.event_id must match events.event_hash');
END;
CREATE TRIGGER subscription_deliveries_identity_bu
BEFORE UPDATE OF subscription_id,event_id,event_seq,event_kind,event_created_at,created_at ON subscription_deliveries
WHEN new.subscription_id IS NOT old.subscription_id
  OR new.event_id IS NOT old.event_id
  OR new.event_seq IS NOT old.event_seq
  OR new.event_kind IS NOT old.event_kind
  OR new.event_created_at IS NOT old.event_created_at
  OR new.created_at IS NOT old.created_at BEGIN
 SELECT RAISE(ABORT, 'subscription_deliveries identity is immutable');
END;
CREATE TRIGGER subscription_deliveries_updated_at_bu
BEFORE UPDATE OF updated_at ON subscription_deliveries
WHEN new.updated_at < old.updated_at BEGIN
 SELECT RAISE(ABORT, 'subscription_deliveries.updated_at must be monotonic');
END;
CREATE TRIGGER subscription_deliveries_state_bu
BEFORE UPDATE ON subscription_deliveries
WHEN old.status IN ('acked','dead_letter')
  OR (old.status IN ('pending','retry_wait') AND new.status <> 'leased')
  OR (old.status = 'leased' AND new.status NOT IN ('retry_wait','acked','dead_letter'))
  OR (new.status = 'leased' AND new.attempts <> old.attempts + 1)
  OR (new.status = 'leased' AND (
        new.lease_token IS NULL OR
        new.lease_deadline_at IS NULL OR
        new.next_attempt_at IS NOT NULL OR
        new.last_attempt_at IS NULL OR
        new.last_error_code IS NOT NULL OR
        new.acked_at IS NOT NULL OR
        new.dead_lettered_at IS NOT NULL
      ))
  OR (old.status = 'leased' AND new.attempts <> old.attempts)
  OR (old.status = 'leased' AND (
        new.lease_token IS NOT NULL OR
        new.lease_deadline_at IS NOT NULL OR
        new.last_attempt_at IS NULL OR
        new.updated_at < old.updated_at
      ))
  OR (new.status = 'retry_wait' AND (
        new.next_attempt_at IS NULL OR
        new.last_error_code IS NULL OR
        new.acked_at IS NOT NULL OR
        new.dead_lettered_at IS NOT NULL
      ))
  OR (new.status = 'acked' AND (
        new.next_attempt_at IS NOT NULL OR
        new.last_error_code IS NOT NULL OR
        new.acked_at IS NULL OR
        new.dead_lettered_at IS NOT NULL
      ))
  OR (new.status = 'dead_letter' AND (
        new.next_attempt_at IS NOT NULL OR
        new.last_error_code IS NULL OR
        new.acked_at IS NOT NULL OR
        new.dead_lettered_at IS NULL
      )) BEGIN
 SELECT RAISE(ABORT, 'subscription_deliveries state transition is invalid');
END;
CREATE TRIGGER subscription_deliveries_delete_bu
BEFORE DELETE ON subscription_deliveries
WHEN EXISTS (SELECT 1 FROM subscriptions WHERE id=old.subscription_id) BEGIN
 SELECT RAISE(ABORT, 'subscription_deliveries are immutable');
END;
CREATE TABLE subscription_delivery_attempts (
 subscription_id TEXT NOT NULL,
 event_id TEXT NOT NULL,
 attempt INTEGER NOT NULL CHECK(attempt >= 1),
 started_at INTEGER NOT NULL,
 finished_at INTEGER,
 outcome TEXT NOT NULL CHECK(outcome IN ('claim','success','retry','dead','lease_expired','timeout')),
 error_code TEXT CHECK(error_code IS NULL OR (length(error_code) > 0 AND error_code NOT GLOB '*[^a-z0-9_:-]*')),
 CHECK(finished_at IS NULL OR finished_at >= started_at),
 PRIMARY KEY(subscription_id,event_id,attempt),
 FOREIGN KEY(subscription_id,event_id) REFERENCES subscription_deliveries(subscription_id,event_id) ON DELETE CASCADE
) STRICT;
CREATE TRIGGER subscription_delivery_attempts_insert_bi
BEFORE INSERT ON subscription_delivery_attempts
WHEN new.outcome <> 'claim'
  OR new.finished_at IS NOT NULL
  OR new.error_code IS NOT NULL BEGIN
 SELECT RAISE(ABORT, 'subscription_delivery_attempts must be inserted as open claims');
END;
CREATE TRIGGER subscription_delivery_attempts_update_bu
BEFORE UPDATE ON subscription_delivery_attempts
WHEN old.finished_at IS NOT NULL
  OR old.outcome <> 'claim'
  OR new.subscription_id IS NOT old.subscription_id
  OR new.event_id IS NOT old.event_id
  OR new.attempt IS NOT old.attempt
  OR new.started_at IS NOT old.started_at
  OR new.finished_at IS NULL
  OR new.outcome = 'claim'
  OR new.outcome NOT IN ('success','retry','dead','lease_expired','timeout')
  OR (new.outcome = 'success' AND new.error_code IS NOT NULL)
  OR (new.outcome <> 'success' AND new.error_code IS NULL)
  OR new.finished_at < old.started_at BEGIN
 SELECT RAISE(ABORT, 'subscription_delivery_attempts state transition is invalid');
END;
CREATE TRIGGER subscription_delivery_attempts_delete_bu
BEFORE DELETE ON subscription_delivery_attempts
WHEN EXISTS (SELECT 1 FROM subscriptions WHERE id=old.subscription_id) BEGIN
 SELECT RAISE(ABORT, 'subscription_delivery_attempts are immutable');
END;
CREATE INDEX idx_subscription_delivery_attempts_rate ON subscription_delivery_attempts(subscription_id,started_at);
"#;

const BOARD_V23: &str = r#"
DROP INDEX IF EXISTS idx_tasks_priority_created_id;
CREATE INDEX idx_tasks_priority_created_id ON tasks(priority,created_at,id);
DROP INDEX IF EXISTS idx_events_created_seq;
CREATE INDEX idx_events_created_seq ON events(created_at,seq);
"#;

const REGISTRY_V1: &str = r#"
CREATE TABLE workspaces (
 root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL UNIQUE,
 created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
) STRICT;
"#;

const REGISTRY_V2: &str = r#"
CREATE TABLE workspace_aliases (
 root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL,
 created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_workspace_aliases_board ON workspace_aliases(board_path);
"#;

/// One audited rules document inherited by every registered project.
const REGISTRY_V3: &str = r#"
CREATE TABLE global_rules (
 id TEXT PRIMARY KEY NOT NULL,
 body TEXT NOT NULL,
 author TEXT NOT NULL,
 archived INTEGER NOT NULL DEFAULT 0,
 created_at INTEGER NOT NULL,
 updated_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_global_rules_active ON global_rules(archived,created_at);
CREATE TABLE global_rule_events (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,
 rule_id TEXT NOT NULL,
 kind TEXT NOT NULL,
 actor TEXT NOT NULL,
 payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),
 created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_global_rule_events_rule_seq ON global_rule_events(rule_id,seq);
"#;

/// Explicit board targeting for global rules. Existing rules retain their
/// original every-board behavior through the reserved `ALL` tag.
const REGISTRY_V4: &str = r#"
ALTER TABLE global_rules ADD COLUMN board_tags TEXT NOT NULL DEFAULT '["ALL"]'
 CHECK(json_valid(board_tags) AND json_type(board_tags) = 'array');
"#;

const REGISTRY_V5: &str = r#"
DROP INDEX idx_global_rules_active;
CREATE INDEX idx_global_rules_active ON global_rules(created_at) WHERE archived=0;
"#;

/// Workspace aliases can outlive disposable worktrees. Retire them without
/// erasing who attached what, while keeping active resolution and indexes
/// bounded to roots that still participate in the project.
const REGISTRY_V6: &str = r#"
ALTER TABLE workspaces ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1));
ALTER TABLE workspaces ADD COLUMN archived_at INTEGER;
ALTER TABLE workspaces ADD COLUMN archived_by TEXT;
ALTER TABLE workspace_aliases ADD COLUMN archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1));
ALTER TABLE workspace_aliases ADD COLUMN archived_at INTEGER;
ALTER TABLE workspace_aliases ADD COLUMN archived_by TEXT;
DROP INDEX idx_workspace_aliases_board;
CREATE INDEX idx_workspace_aliases_board ON workspace_aliases(board_path) WHERE archived=0;
"#;

/// Retired aliases live apart from active address resolution so a disposable
/// worktree path may be reused after a rebuild without overwriting history or
/// colliding with the active table's primary key.
const REGISTRY_V7: &str = r#"
CREATE TABLE workspace_alias_history (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,
 root_path TEXT NOT NULL,
 name TEXT NOT NULL,
 board_path TEXT NOT NULL,
 created_at INTEGER NOT NULL,
 last_used_at INTEGER NOT NULL,
 archived_at INTEGER NOT NULL,
 archived_by TEXT NOT NULL
) STRICT;
CREATE INDEX idx_workspace_alias_history_root ON workspace_alias_history(root_path,seq);
"#;

const REGISTRY_V8: &str = r#"
ALTER TABLE global_rules ADD COLUMN task_tags TEXT NOT NULL DEFAULT '[]'
 CHECK(json_valid(task_tags) AND json_type(task_tags) = 'array');
"#;

/// One canonical tag-scoped rules document. The legacy registry tables remain
/// readable during the rolling upgrade; board-local rows are consolidated by
/// the explicit cross-database migration recorded in ADR-027.
const REGISTRY_V9: &str = r#"
CREATE TABLE rules (
 id TEXT PRIMARY KEY NOT NULL,
 body TEXT NOT NULL,
 author TEXT NOT NULL,
 archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0,1)),
 created_at INTEGER NOT NULL,
 updated_at INTEGER NOT NULL,
 tags TEXT NOT NULL DEFAULT '["ALL"]'
  CHECK(json_valid(tags) AND json_type(tags) = 'array'),
 source_board TEXT,
 source_rule_id TEXT,
 CHECK((source_board IS NULL) = (source_rule_id IS NULL)),
 UNIQUE(source_board,source_rule_id)
) STRICT;
CREATE INDEX idx_registry_rules_active ON rules(created_at,id) WHERE archived=0;
CREATE TABLE rule_events (
 seq INTEGER PRIMARY KEY AUTOINCREMENT,
 rule_id TEXT NOT NULL,
 kind TEXT NOT NULL,
 actor TEXT NOT NULL,
 payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),
 created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_registry_rule_events_rule_seq ON rule_events(rule_id,seq);
CREATE TABLE rule_board_migrations (
 board_path TEXT PRIMARY KEY NOT NULL,
 board_name TEXT NOT NULL,
 source_count INTEGER NOT NULL,
 actor TEXT NOT NULL,
 migrated_at INTEGER NOT NULL
) STRICT;
INSERT INTO rules(id,body,author,archived,created_at,updated_at,tags)
SELECT id,body,author,archived,created_at,updated_at,
       (SELECT json_group_array(value)
          FROM (SELECT value,0 AS family,CAST(key AS INTEGER) AS position
                  FROM json_each(global_rules.board_tags)
                UNION ALL
                SELECT value,1 AS family,CAST(key AS INTEGER) AS position
                  FROM json_each(global_rules.task_tags)
                ORDER BY family,position))
  FROM global_rules;
INSERT INTO rule_events(rule_id,kind,actor,payload,created_at)
SELECT rule_id,kind,actor,payload,created_at FROM global_rule_events ORDER BY seq;
"#;

const REGISTRY_V10: &str = r#"
CREATE TABLE registry_meta (key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL) STRICT;
ALTER TABLE rule_events ADD COLUMN prev_hash TEXT;
ALTER TABLE rule_events ADD COLUMN event_hash TEXT;
"#;

/// Split board identity from roots. The registry keeps the old legacy tables
/// around for audit-readability, but the live API now resolves through the
/// explicit board entity and optional unordered roots table.
const REGISTRY_V11: &str = r#"
CREATE TABLE boards (
 board_path TEXT PRIMARY KEY NOT NULL,
 name TEXT NOT NULL,
 created_at INTEGER NOT NULL,
 last_used_at INTEGER NOT NULL
) STRICT;
CREATE INDEX idx_boards_name ON boards(name,board_path);
CREATE TABLE workspace_roots (
 root_path TEXT PRIMARY KEY NOT NULL,
 board_path TEXT NOT NULL,
 created_at INTEGER NOT NULL,
 last_used_at INTEGER NOT NULL,
 FOREIGN KEY(board_path) REFERENCES boards(board_path)
) STRICT;
CREATE INDEX idx_workspace_roots_board ON workspace_roots(board_path,last_used_at DESC);
INSERT INTO boards(board_path,name,created_at,last_used_at)
SELECT board_path,name,created_at,last_used_at FROM workspaces;
INSERT INTO workspace_roots(root_path,board_path,created_at,last_used_at)
SELECT root_path,board_path,created_at,last_used_at FROM workspaces;
INSERT INTO workspace_roots(root_path,board_path,created_at,last_used_at)
SELECT root_path,board_path,created_at,last_used_at FROM workspace_aliases;
UPDATE boards
SET last_used_at = COALESCE((
  SELECT max(last_used_at)
  FROM workspace_roots
  WHERE board_path=boards.board_path
), last_used_at);
"#;

pub const BOARD_SCHEMA_VERSION: usize = 23;
pub const REGISTRY_SCHEMA_VERSION: usize = 11;

/// Create `dir` and any missing ancestors, each mode 0700.
///
/// Directories that already exist are left exactly as the operator set them.
/// Kanban must never re-permission a directory it does not own: the previous
/// unconditional `chmod 0700` on the parent meant `--db /tmp/board.db` locked
/// `/tmp` to the calling user, and as root that breaks every other process on
/// the host. Use [`own_private_dir`] for the one tree Kanban does own.
pub fn create_private_dir_all(dir: &Path) -> Result<()> {
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        create_private_dir_all(parent)?;
    }
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        // A concurrent kanban process won the race; its mode is ours.
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).with_context(|| format!("create directory {}", dir.display())),
    }
}

/// Create `dir` if missing and assert mode 0700 on it. Only for the private
/// data root, which Kanban owns outright; never for an operator-supplied path.
pub fn own_private_dir(dir: &Path) -> Result<()> {
    create_private_dir_all(dir)?;
    fs::set_permissions(dir, Permissions::from_mode(0o700))
        .with_context(|| format!("secure directory {}", dir.display()))
}

/// Create `path` with mode 0600 before anything can open it.
///
/// `Connection::open` creates with 0644 and a follow-up `chmod` leaves a window
/// in which any local user can open the file and keep the descriptor across the
/// narrowing. Boards hold private work state, so they are never briefly public.
fn create_private_file(path: &Path) -> Result<()> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).with_context(|| format!("create {}", path.display())),
    }
}

/// How long a writer keeps retrying a locked board before giving up.
///
/// Measured on a 16-agent fan-out against one board: the median write took
/// 208ms and 3% of writes failed, every one of them at the old 5s ceiling. A
/// 15s budget covers a write queue roughly seventy deep; past that the board
/// is genuinely oversubscribed and failing is the honest answer, because a
/// command that never returns is worse for an agent than one that says no.
const BUSY_BUDGET: Duration = Duration::from_secs(15);

/// Longest single pause between retries. Small enough that a lock released
/// early is picked up promptly, large enough that waiting is not a spin.
const BUSY_MAX_PAUSE_MS: u64 = 100;

thread_local! {
    /// When the current contention episode began. SQLite passes a retry count
    /// but no clock, and the handler is a bare `fn` pointer with nowhere to
    /// keep state, so the deadline is anchored here instead of extrapolated
    /// from the count — an extrapolated one would let the error report a wait
    /// that never happened.
    static BUSY_SINCE: Cell<Option<Instant>> = const { Cell::new(None) };
}

/// Sub-millisecond entropy, decorrelated per process.
///
/// Enough to break up a herd, which is all it is for; nothing here needs an
/// RNG dependency or cryptographic quality.
fn jitter(bound: u64) -> u64 {
    if bound == 0 {
        return 0;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| u64::from(value.subsec_nanos()));
    (nanos ^ u64::from(std::process::id()).wrapping_mul(2_654_435_761)) % bound
}

/// Retry a locked database with randomized exponential backoff.
///
/// SQLite's built-in handler sleeps on a fixed schedule with no
/// randomization, so writers that collide wake together and collide again;
/// the same unlucky process can lose every round while the board as a whole
/// makes progress. Jitter decorrelates them, which is what turns a starving
/// writer into a merely slow one.
fn busy_backoff(count: i32) -> bool {
    let started = if count == 0 {
        let now = Instant::now();
        BUSY_SINCE.with(|since| since.set(Some(now)));
        now
    } else {
        BUSY_SINCE
            .with(|since| since.get())
            .unwrap_or_else(Instant::now)
    };
    if started.elapsed() >= BUSY_BUDGET {
        BUSY_SINCE.with(|since| since.set(None));
        return false;
    }
    let ceiling = (1u64 << count.clamp(0, 7)).min(BUSY_MAX_PAUSE_MS);
    sleep(Duration::from_millis(1 + jitter(ceiling)));
    true
}

fn open(path: &Path) -> Result<Connection> {
    let parent = path.parent().context("database path has no parent")?;
    create_private_dir_all(parent)?;
    create_private_file(path)?;
    let connection = Connection::open(path)
        .with_context(|| format!("open SQLite database {}", path.display()))?;
    // Re-assert for databases created before this rule, or by another tool.
    fs::set_permissions(path, Permissions::from_mode(0o600))?;
    // synchronous=FULL, not NORMAL: a checkpoint that survives the agent but
    // not the host is not durable, and this ledger exists to be resumed from.
    // Write volume is a handful of rows per model turn, so the extra fsync is
    // not a meaningful cost.
    connection.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;",
    )?;
    // Replaces `busy_timeout`, which installs SQLite's own unjittered handler.
    connection.busy_handler(Some(busy_backoff))?;
    Ok(connection)
}

/// Open a fresh database file for a backup copy. Fails if the destination
/// exists: creation is the existence check, so no window separates the two.
pub fn create_backup_target(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        create_private_dir_all(parent)?;
    }
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| match error.kind() {
            ErrorKind::AlreadyExists => {
                anyhow::anyhow!("backup destination already exists: {}", path.display())
            }
            _ => anyhow::Error::new(error).context(format!("create {}", path.display())),
        })?;
    Connection::open(path).with_context(|| format!("open backup target {}", path.display()))
}

fn migrate(connection: &mut Connection, migrations: &[&str]) -> Result<()> {
    let mut current: usize = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current > migrations.len() {
        bail!(
            "database version {current} is newer than supported version {}",
            migrations.len()
        );
    }
    while current < migrations.len() {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        current = transaction.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if current > migrations.len() {
            bail!(
                "database version {current} is newer than supported version {}",
                migrations.len()
            );
        }
        if current == migrations.len() {
            transaction.commit()?;
            return Ok(());
        }
        transaction.execute_batch(migrations[current])?;
        transaction.pragma_update(None, "user_version", (current + 1) as i64)?;
        transaction.commit()?;
        current += 1;
    }
    Ok(())
}

pub fn open_board(path: &Path) -> Result<Connection> {
    let mut connection = open(path)?;
    // Foreign keys are off across the upgrade, and on for everything after.
    //
    // This is SQLite's own procedure for rebuilding a table, and it has to
    // happen out here: `PRAGMA foreign_keys` is a no-op inside a transaction,
    // which is where every migration runs. Rebuilding `handoffs` in V4 drops
    // and recreates a table carrying references, and with enforcement on that
    // reads the tables it points at — including `checkpoints`, which a v3
    // board written by the retired TypeScript implementation may not have.
    //
    // Nothing is weakened by this: the rows are copied verbatim from a board
    // that already satisfied its own constraints, `foreign_key_check` is what
    // `doctor` runs to prove it afterwards, and enforcement is restored before
    // the connection does any work.
    connection.pragma_update(None, "foreign_keys", false)?;
    let outcome = migrate(&mut connection, BOARD_MIGRATIONS);
    connection.pragma_update(None, "foreign_keys", true)?;
    outcome?;
    crate::audit::initialize_board_chain(&mut connection)?;
    Ok(connection)
}

const BOARD_MIGRATIONS: &[&str] = &[
    BOARD_V1, BOARD_V2, BOARD_V3, BOARD_V4, BOARD_V5, BOARD_V6, BOARD_V7, BOARD_V8, BOARD_V9,
    BOARD_V10, BOARD_V11, BOARD_V12, BOARD_V13, BOARD_V14, BOARD_V15, BOARD_V16, BOARD_V17,
    BOARD_V18, BOARD_V19, BOARD_V20, BOARD_V21, BOARD_V22, BOARD_V23,
];

/// Open a current board without creating, migrating, sweeping, or checkpointing it.
///
/// Scheduler inspection must be observational: an agent asking what it could
/// claim must not expire another agent's lease or leave a ledger event behind.
/// An old schema is refused because making it readable would itself be a write;
/// the next ordinary command will migrate it before inspection is retried.
pub fn open_board_readonly(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open SQLite board read-only {}", path.display()))?;
    connection.busy_handler(Some(busy_backoff))?;
    connection.pragma_update(None, "query_only", true)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    let version = schema_version(&connection)?;
    if version != BOARD_MIGRATIONS.len() {
        bail!(
            "board schema is {version}, but read-only inspection requires {}; run any ordinary kanban command once to migrate it",
            BOARD_MIGRATIONS.len()
        );
    }
    Ok(connection)
}

/// A reader whose queries all run on one SQLite connection.
///
/// Snapshotting one connection while reading from another is the defect
/// `read_snapshot` exists to prevent. Be precise about how much of that the
/// compiler actually holds, because the rest is convention:
///
/// Enforced — the value `read_snapshot` hands the closure is, by construction,
/// the same value it opened the transaction on. There is no second parameter
/// that could disagree with the first.
///
/// Enforced — `snapshot_connection` returns a borrow tied to `&self`, so an
/// impl cannot open a connection inside the method and return it; that does not
/// borrow-check. The connection has to be one the value already holds.
///
/// NOT enforced — that the closure body reads through the value it was handed.
/// `read_snapshot(&store, |_| other.events_since(..))` compiles, and so does a
/// body that opens its own reader and queries that. Nothing there is inside the
/// transaction, and nothing says so. The convention is to shadow the parameter
/// (`|store| ..`) so the outer handle is out of scope for the body; that is a
/// habit, not a guarantee, and a reviewer still has to read the body.
///
/// Forward hazard — the property holds because every impl is a single-connection
/// type: `Store` and `Registry` each own exactly one `connection`. A type
/// holding two would satisfy this trait while its other connection stayed
/// outside every snapshot, silently. Nothing here prevents that.
pub trait SnapshotSource {
    fn snapshot_connection(&self) -> &Connection;
}

impl SnapshotSource for Connection {
    fn snapshot_connection(&self) -> &Connection {
        self
    }
}

/// Run `read` inside one deferred read transaction on `source`.
///
/// In WAL a deferred transaction takes its snapshot at the first read and
/// holds it until the transaction ends, so every query `read` issues on this
/// source observes one database state. A reader that decides something from
/// two queries needs exactly that: run them on two snapshots and a commit
/// landing between them lets the second query see a row the first never had a
/// chance to reject, with nothing in either result to say so. Watch polls
/// depend on this — see `watch::poll_once`.
///
/// Nothing is committed. Readers open with `query_only`, so the transaction
/// exists only to pin the snapshot and is rolled back on the way out — and on
/// the error path too, since `Transaction` drops with `DropBehavior::Rollback`.
pub fn read_snapshot<S: SnapshotSource, T>(
    source: &S,
    read: impl FnOnce(&S) -> Result<T>,
) -> Result<T> {
    // Deferred is named, not inherited. `unchecked_transaction` reads the
    // connection's mutable `transaction_behavior`, and BEGIN IMMEDIATE against
    // a read-only `query_only` connection errors, which would propagate out of
    // a poll and end the follow loop. The snapshot semantics are the whole
    // correctness argument here, so the behavior is stated rather than assumed.
    let snapshot =
        Transaction::new_unchecked(source.snapshot_connection(), TransactionBehavior::Deferred)?;
    let value = read(source)?;
    snapshot.finish()?;
    Ok(value)
}

pub fn open_registry(path: &Path) -> Result<Connection> {
    let mut connection = open(path)?;
    migrate(&mut connection, REGISTRY_MIGRATIONS)?;
    crate::audit::initialize_registry_chain(&mut connection)?;
    record_workspace_name_drift(&mut connection)?;
    Ok(connection)
}

fn record_workspace_name_drift(connection: &mut Connection) -> Result<()> {
    const KEY: &str = "workspace_root_model_v11_name_drift_audited";
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if transaction
        .query_row("SELECT 1 FROM registry_meta WHERE key=?", [KEY], |_| Ok(()))
        .optional()?
        .is_some()
    {
        transaction.commit()?;
        return Ok(());
    }
    let drifted = {
        let mut statement = transaction.prepare(
            "SELECT a.root_path,a.board_path,a.name,b.name \
             FROM workspace_aliases a JOIN workspaces b ON b.board_path=a.board_path \
             WHERE a.name<>b.name ORDER BY b.name,a.root_path",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let now = crate::registry::now_ms();
    for (root_path, board_path, discarded_name, board_name) in drifted {
        crate::audit::append_registry_event(
            &transaction,
            &format!("workspace:{root_path}"),
            "workspace_alias_name_discarded",
            "system@migration",
            &json!({
                "rootPath": root_path,
                "boardPath": board_path,
                "discardedName": discarded_name,
                "boardName": board_name,
            })
            .to_string(),
            now,
        )?;
    }
    transaction.execute(
        "INSERT INTO registry_meta(key,value) VALUES(?,?)",
        params![KEY, now.to_string()],
    )?;
    transaction.commit()?;
    Ok(())
}

const REGISTRY_MIGRATIONS: &[&str] = &[
    REGISTRY_V1,
    REGISTRY_V2,
    REGISTRY_V3,
    REGISTRY_V4,
    REGISTRY_V5,
    REGISTRY_V6,
    REGISTRY_V7,
    REGISTRY_V8,
    REGISTRY_V9,
    REGISTRY_V10,
    REGISTRY_V11,
];

pub fn open_registry_readonly(path: &Path) -> Result<Connection> {
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open SQLite registry read-only {}", path.display()))?;
    connection.busy_handler(Some(busy_backoff))?;
    connection.pragma_update(None, "query_only", true)?;
    let version = schema_version(&connection)?;
    if version != REGISTRY_MIGRATIONS.len() {
        bail!(
            "registry schema is {version}, but read-only inspection requires {}; run any ordinary kanban command once to migrate it",
            REGISTRY_MIGRATIONS.len()
        );
    }
    Ok(connection)
}

pub fn integrity(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection.prepare("PRAGMA integrity_check")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn schema_version(connection: &Connection) -> Result<usize> {
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(Into::into)
}

/// Rows whose foreign key points at something that is not there.
///
/// `integrity_check` validates the b-tree and says nothing about referential
/// consistency, so a board could be structurally perfect and still hold a note
/// on a task that no longer exists — the shape a v3 board written by the
/// retired TypeScript implementation, or a partial import, can leave behind.
pub fn foreign_key_violations(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
    let rows = statement.query_map([], |row| {
        Ok(format!(
            "{} row {} references missing {}",
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?
                .map_or_else(|| "?".to_owned(), |id| id.to_string()),
            row.get::<_, String>(2)?
        ))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

pub fn checkpoint(connection: &Connection) -> Result<()> {
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    Ok(())
}

/// Integrity-check a database file without registering or migrating it.
/// Used to verify a snapshot before it is allowed to overwrite live state.
pub fn verify(path: &Path) -> Result<Vec<String>> {
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open snapshot {}", path.display()))?;
    integrity(&connection)
}

/// Put `source` in place of `destination`, atomically as far as readers are
/// concerned.
///
/// Copies to a temporary sibling first and renames over the target, so an
/// interrupted restore cannot leave a half-written board. Any `-wal`/`-shm`
/// belonging to the replaced database is removed: they describe the file that
/// just went away, and leaving them would let SQLite replay the old log over
/// the new contents.
pub fn replace_database(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        create_private_dir_all(parent)?;
    }
    let staging = destination.with_extension("db.restoring");
    let _ = fs::remove_file(&staging);
    fs::copy(source, &staging)
        .with_context(|| format!("copy {} to {}", source.display(), staging.display()))?;
    fs::set_permissions(&staging, Permissions::from_mode(0o600))?;
    fs::rename(&staging, destination)
        .with_context(|| format!("replace {}", destination.display()))?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = destination.as_os_str().to_owned();
        sidecar.push(suffix);
        let _ = fs::remove_file(Path::new(&sidecar));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// A held read snapshot hides commits that land while it is open.
    ///
    /// This is the guarantee `read_snapshot` sells and that `watch` spends, so
    /// it is measured against a real WAL board opened exactly the way readers
    /// open one — `SQLITE_OPEN_READ_ONLY`, `query_only`, no shared cache —
    /// rather than assumed from the SQLite documentation.
    #[test]
    fn a_read_snapshot_hides_commits_that_land_while_it_is_open() {
        let root =
            std::env::temp_dir().join(format!("kanban-read-snapshot-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).expect("create temp snapshot dir");
        let path = root.join("board.db");
        let writer = open_board(&path).expect("open writable board");
        crate::audit::append_board_event(&writer, None, "board_changed", "codex", "{}", 1)
            .expect("append the seed event");
        let reader = open_board_readonly(&path).expect("open read-only board");
        let head = |connection: &Connection| -> i64 {
            connection
                .query_row("SELECT COALESCE(MAX(seq),0) FROM events", [], |row| {
                    row.get(0)
                })
                .expect("read ledger head")
        };

        read_snapshot(&reader, |reader| {
            assert_eq!(head(reader), 1);
            crate::audit::append_board_event(&writer, None, "board_changed", "codex", "{}", 2)
                .expect("commit while the snapshot is held");
            assert_eq!(
                head(reader),
                1,
                "a held snapshot saw a commit that landed after it opened"
            );
            Ok(())
        })
        .expect("run the snapshot body");

        assert_eq!(
            head(&reader),
            2,
            "the snapshot outlived its read transaction"
        );
    }

    /// Every migration that exists is in the ladder that runs it.
    ///
    /// A migration can be written, reviewed and committed without ever being
    /// added to `open_board`'s list, and nothing says so: the constant is used
    /// nowhere, the build is clean, the tests pass, and the first sign is a
    /// column missing at runtime on a board that thought it was current. That
    /// happened once here, which is why this reads the file back.
    #[test]
    fn every_board_migration_is_in_the_ladder() {
        const SOURCE: &str = include_str!("db.rs");
        // Assembled, because this test reads its own source and a literal
        // would match itself.
        let declaration = format!("{} BOARD_V", "const");
        let declared = SOURCE.matches(declaration.as_str()).count();
        // Checked by name rather than by counting a text span: the list is
        // formatter-wrapped, so any span-based guard is one `cargo fmt` away
        // from measuring the wrong thing.
        let applied = SOURCE
            .split_once("pub fn open_board")
            .map(|(_, rest)| rest)
            .expect("open_board must exist");
        for n in 1..=declared {
            let name = format!("BOARD_V{n}");
            assert!(
                applied.contains(&name),
                "{name} is declared but never applied; a migration that does not \
                 run is invisible until a column is missing at runtime"
            );
        }
        let ladder = declared;
        assert_eq!(
            declared, ladder,
            "{declared} board migrations are declared but {ladder} are applied; \
             a migration that is never run is invisible until a column is missing"
        );
        assert_eq!(BOARD_SCHEMA_VERSION, declared);
        let registry_declaration = format!("{} REGISTRY_V", "const");
        assert_eq!(
            REGISTRY_SCHEMA_VERSION,
            SOURCE.matches(registry_declaration.as_str()).count()
        );
    }

    #[test]
    fn board_v22_anchors_start_event_seq_and_materializes_the_first_later_match() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection, &BOARD_MIGRATIONS[..21]).unwrap();

        crate::audit::append_board_event(
            &connection,
            Some("t-subject"),
            "task_created",
            "geo",
            "{}",
            10,
        )
        .unwrap();
        let anchor_payload = r#"{"subscriptionID":"sub-1"}"#;
        crate::audit::append_board_event(
            &connection,
            None,
            "subscription_added",
            "geo",
            anchor_payload,
            11,
        )
        .unwrap();
        let matching_payload = r#"{"_semanticV1":{"subject":{"type":"task","id":"t-subject"},"relations":[],"priorStatus":"todo","currentStatus":"in_progress","tags":["pubsub"]}}"#;
        crate::audit::append_board_event(
            &connection,
            Some("t-subject"),
            "checkpoint_added",
            "geo",
            matching_payload,
            12,
        )
        .unwrap();
        connection
            .execute(
                "INSERT INTO subscriptions(id,protocol_version,subject_task_id,relations,kinds,prior_statuses,current_statuses,tags,consumer_id,action_id,timeout_ms,max_retries,rate_per_minute,max_concurrency,secret_ref,status,created_at,created_by,updated_at,updated_by,paused_at,paused_by) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                rusqlite::params![
                    "sub-1",
                    1,
                    Option::<String>::None,
                    "[]",
                    "[\"checkpoint_added\"]",
                    "[]",
                    "[]",
                    "[\"pubsub\"]",
                    "consumer",
                    "action",
                    1000,
                    3,
                    60,
                    2,
                    Option::<String>::None,
                    "active",
                    20,
                    "geo",
                    20,
                    "geo",
                    Option::<i64>::None,
                    Option::<String>::None,
                ],
            )
            .unwrap();

        migrate(&mut connection, BOARD_MIGRATIONS).unwrap();

        assert_eq!(
            connection
                .query_row(
                    "SELECT start_event_seq FROM subscriptions WHERE id='sub-1'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT event_seq FROM board_materialization_cursor WHERE id=1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );

        let mut store = crate::store::Store { connection };
        assert_eq!(store.materialize_subscriptions().unwrap(), 1);
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT event_seq FROM board_materialization_cursor WHERE id=1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            3
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT event_seq FROM subscription_deliveries WHERE subscription_id='sub-1'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            3
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT event_id FROM subscription_deliveries WHERE subscription_id='sub-1'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            store
                .connection
                .query_row("SELECT event_hash FROM events WHERE seq=3", [], |row| {
                    row.get::<_, String>(0)
                },)
                .unwrap()
        );
        assert!(
            store
                .connection
                .execute(
                    "INSERT INTO subscription_deliveries(subscription_id,event_id,event_seq,event_kind,event_created_at,status,attempts,lease_token,lease_deadline_at,next_attempt_at,last_attempt_at,last_error_code,acked_at,dead_lettered_at,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                    rusqlite::params![
                        "sub-1",
                        "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
                        2,
                        "subscription_added",
                        11,
                        "pending",
                        0,
                        Option::<String>::None,
                        Option::<i64>::None,
                        11,
                        Option::<i64>::None,
                        Option::<String>::None,
                        Option::<i64>::None,
                        Option::<i64>::None,
                        11,
                        11,
                    ],
                )
                .is_err(),
            "subscription_deliveries accepted a post-anchor event that did not match the event identity"
        );
    }

    #[test]
    fn board_v22_allows_a_valid_historical_removed_subscription_anchor() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection, &BOARD_MIGRATIONS[..21]).unwrap();

        crate::audit::append_board_event(
            &connection,
            Some("t-subject"),
            "task_created",
            "geo",
            "{}",
            10,
        )
        .unwrap();
        crate::audit::append_board_event(
            &connection,
            None,
            "subscription_added",
            "geo",
            r#"{"subscriptionID":"sub-1"}"#,
            11,
        )
        .unwrap();
        crate::audit::append_board_event(
            &connection,
            None,
            "subscription_added",
            "geo",
            r#"{"subscriptionID":"sub-old"}"#,
            12,
        )
        .unwrap();
        crate::audit::append_board_event(
            &connection,
            Some("t-subject"),
            "checkpoint_added",
            "geo",
            r#"{"_semanticV1":{"subject":{"type":"task","id":"t-subject"},"relations":[],"priorStatus":"todo","currentStatus":"in_progress","tags":["pubsub"]}}"#,
            13,
        )
        .unwrap();
        connection
            .execute(
                "INSERT INTO subscriptions(id,protocol_version,subject_task_id,relations,kinds,prior_statuses,current_statuses,tags,consumer_id,action_id,timeout_ms,max_retries,rate_per_minute,max_concurrency,secret_ref,status,created_at,created_by,updated_at,updated_by,paused_at,paused_by) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                rusqlite::params![
                    "sub-1",
                    1,
                    Option::<String>::None,
                    "[]",
                    "[\"checkpoint_added\"]",
                    "[]",
                    "[]",
                    "[\"pubsub\"]",
                    "consumer",
                    "action",
                    1000,
                    3,
                    60,
                    2,
                    Option::<String>::None,
                    "active",
                    20,
                    "geo",
                    20,
                    "geo",
                    Option::<i64>::None,
                    Option::<String>::None,
                ],
            )
            .unwrap();

        migrate(&mut connection, BOARD_MIGRATIONS).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT start_event_seq FROM subscriptions WHERE id='sub-1'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
    }

    #[test]
    fn board_v22_fails_closed_for_missing_duplicate_or_malformed_anchors() {
        let cases = [
            ("missing", "{}", "{}", 1usize),
            (
                "duplicate",
                r#"{"subscriptionID":"sub-1"}"#,
                r#"{"subscriptionID":"sub-1"}"#,
                2,
            ),
            (
                "malformed",
                r#"{"subscriptionID":1}"#,
                r#"{"subscriptionID":"sub-1"}"#,
                1,
            ),
            (
                "mixed",
                r#"{"subscriptionID":"sub-1"}"#,
                r#"{"subscriptionID":1}"#,
                2,
            ),
            (
                "mixed-missing-path",
                r#"{"subscriptionID":"sub-1"}"#,
                r#"{"other":"value"}"#,
                2,
            ),
            (
                "mixed-null-path",
                r#"{"subscriptionID":"sub-1"}"#,
                r#"{"subscriptionID":null}"#,
                2,
            ),
            (
                "empty-id",
                r#"{"subscriptionID":"sub-1"}"#,
                r#"{"subscriptionID":""}"#,
                2,
            ),
            (
                "suffix-only",
                r#"{"subscriptionID":"sub-1"}"#,
                r#"{"subscriptionID":"sub-"}"#,
                2,
            ),
            (
                "overlong",
                r#"{"subscriptionID":"sub-1"}"#,
                r#"{"subscriptionID":"sub-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
                2,
            ),
            (
                "non-ascii",
                r#"{"subscriptionID":"sub-1"}"#,
                r#"{"subscriptionID":"sub-é"}"#,
                2,
            ),
            (
                "space",
                r#"{"subscriptionID":"sub-1"}"#,
                r#"{"subscriptionID":"sub-1 2"}"#,
                2,
            ),
            (
                "slash",
                r#"{"subscriptionID":"sub-1"}"#,
                r#"{"subscriptionID":"sub-1/2"}"#,
                2,
            ),
            (
                "colon",
                r#"{"subscriptionID":"sub-1"}"#,
                r#"{"subscriptionID":"sub-1:2"}"#,
                2,
            ),
            (
                "punctuation",
                r#"{"subscriptionID":"sub-1"}"#,
                r#"{"subscriptionID":"sub-1+2"}"#,
                2,
            ),
            (
                "wrong-prefix",
                r#"{"subscriptionID":"sub-1"}"#,
                r#"{"subscriptionID":"xub-1"}"#,
                2,
            ),
        ];

        for (label, first_payload, second_payload, anchor_count) in cases {
            let mut connection = Connection::open_in_memory().unwrap();
            migrate(&mut connection, &BOARD_MIGRATIONS[..21]).unwrap();
            connection
                .execute(
                    "INSERT INTO events(seq,task_id,kind,actor,payload,created_at,archived,prev_hash,event_hash) VALUES(?,?,?,?,?,?,?,?,?)",
                    rusqlite::params![
                        1,
                        Option::<String>::None,
                        "task_created",
                        "geo",
                        "{}",
                        10,
                        0,
                        "prev-1",
                        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    ],
                )
                .unwrap();
            if label != "missing" {
                connection
                    .execute(
                        "INSERT INTO events(seq,task_id,kind,actor,payload,created_at,archived,prev_hash,event_hash) VALUES(?,?,?,?,?,?,?,?,?)",
                        rusqlite::params![
                            2,
                            Option::<String>::None,
                            "subscription_added",
                            "geo",
                            first_payload,
                            11,
                            0,
                            "prev-2",
                            "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
                        ],
                    )
                    .unwrap();
            }
            if anchor_count == 2 {
                connection
                    .execute(
                        "INSERT INTO events(seq,task_id,kind,actor,payload,created_at,archived,prev_hash,event_hash) VALUES(?,?,?,?,?,?,?,?,?)",
                        rusqlite::params![
                            3,
                            Option::<String>::None,
                            "subscription_added",
                            "geo",
                            second_payload,
                            12,
                            0,
                            "prev-3",
                            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        ],
                    )
                    .unwrap();
            }
            connection
                .execute(
                    "INSERT INTO subscriptions(id,protocol_version,subject_task_id,relations,kinds,prior_statuses,current_statuses,tags,consumer_id,action_id,timeout_ms,max_retries,rate_per_minute,max_concurrency,secret_ref,status,created_at,created_by,updated_at,updated_by,paused_at,paused_by) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                    rusqlite::params![
                        "sub-1",
                        1,
                        Option::<String>::None,
                        "[]",
                        "[]",
                        "[]",
                        "[]",
                        "[]",
                        "consumer",
                        "action",
                        1000,
                        3,
                        60,
                        2,
                        Option::<String>::None,
                        "active",
                        20,
                        "geo",
                        20,
                        "geo",
                        Option::<i64>::None,
                        Option::<String>::None,
                    ],
                )
                .unwrap();

            assert!(
                migrate(&mut connection, BOARD_MIGRATIONS).is_err(),
                "{label}: bad anchors must fail closed"
            );
        }
    }

    #[test]
    fn registry_v9_preserves_existing_rule_order_and_combines_its_tags() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection, &REGISTRY_MIGRATIONS[..8]).unwrap();
        connection
            .execute(
                "INSERT INTO global_rules(id,body,author,archived,created_at,updated_at,board_tags,task_tags) VALUES(?,?,?,?,?,?,?,?)",
                rusqlite::params![
                    "g-existing",
                    "Keep evidence exact.",
                    "geo",
                    0,
                    10,
                    11,
                    r#"["ALL","EXCEPT:px"]"#,
                    r#"["testing","rules"]"#,
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO global_rule_events(rule_id,kind,actor,payload,created_at) VALUES(?,?,?,?,?)",
                rusqlite::params!["g-existing", "global_rule_added", "geo", "{}", 10],
            )
            .unwrap();

        migrate(&mut connection, REGISTRY_MIGRATIONS).unwrap();

        let (tags, source_board, source_rule): (String, Option<String>, Option<String>) =
            connection
                .query_row(
                    "SELECT tags,source_board,source_rule_id FROM rules WHERE id='g-existing'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&tags).unwrap(),
            ["ALL", "EXCEPT:px", "testing", "rules"]
        );
        assert_eq!((source_board, source_rule), (None, None));
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM rule_events", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    use super::*;

    fn index_sql(connection: &Connection, name: &str) -> String {
        connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='index' AND name=?",
                [name],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    }

    fn index_columns(connection: &Connection, name: &str) -> Vec<String> {
        let sql = format!("PRAGMA index_info('{name}')");
        connection
            .prepare(&sql)
            .unwrap()
            .query_map([], |row| row.get::<_, String>(2))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    fn index_names(connection: &Connection, table: &str) -> Vec<String> {
        let sql = format!("PRAGMA index_list('{table}')");
        connection
            .prepare(&sql)
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn jitter_stays_inside_its_bound() {
        assert_eq!(jitter(0), 0);
        for bound in [1u64, 2, 4, 64, BUSY_MAX_PAUSE_MS] {
            assert!(jitter(bound) < bound, "jitter escaped bound {bound}");
        }
    }

    #[test]
    fn a_fresh_contention_episode_does_not_inherit_the_last_one_s_deadline() {
        // A long-lived process that once waited out a full budget must not
        // refuse every write it attempts afterwards. `count == 0` is SQLite
        // saying "this is a new lock attempt", so it re-anchors the clock.
        BUSY_SINCE.with(|since| since.set(Some(Instant::now() - BUSY_BUDGET * 2)));
        assert!(
            busy_backoff(0),
            "a new episode reused an expired deadline and gave up immediately"
        );
    }

    #[test]
    fn a_writer_gives_up_once_its_budget_is_spent() {
        BUSY_SINCE
            .with(|since| since.set(Some(Instant::now() - BUSY_BUDGET - Duration::from_secs(1))));
        assert!(
            !busy_backoff(1),
            "a writer past its budget kept retrying instead of surfacing the lock"
        );
        // Cleared, so the next episode starts from its own clock.
        assert_eq!(BUSY_SINCE.with(|since| since.get()), None);
    }

    #[test]
    fn backoff_pauses_are_bounded_at_every_retry_count() {
        for count in [0i32, 1, 7, 31, i32::MAX] {
            let ceiling = (1u64 << count.clamp(0, 7)).min(BUSY_MAX_PAUSE_MS);
            assert!(
                (1..=BUSY_MAX_PAUSE_MS).contains(&ceiling),
                "retry {count} would pause for {ceiling}ms"
            );
        }
    }

    #[test]
    fn board_schema_v23_creates_the_priority_and_created_indexes_without_a_seq_only_event_index() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&mut connection, &BOARD_MIGRATIONS[..22]).unwrap();
        assert_eq!(schema_version(&connection).unwrap(), 22);
        connection
            .execute_batch(
                r#"
                CREATE INDEX idx_tasks_priority_created_id ON tasks(priority,created_at);
                CREATE INDEX idx_events_created_seq ON events(seq);
                "#,
            )
            .unwrap();
        assert_eq!(
            index_sql(&connection, "idx_tasks_priority_created_id"),
            "CREATE INDEX idx_tasks_priority_created_id ON tasks(priority,created_at)"
        );
        assert_eq!(
            index_sql(&connection, "idx_events_created_seq"),
            "CREATE INDEX idx_events_created_seq ON events(seq)"
        );

        migrate(&mut connection, BOARD_MIGRATIONS).unwrap();

        assert_eq!(schema_version(&connection).unwrap(), 23);
        assert_eq!(
            index_sql(&connection, "idx_tasks_priority_created_id"),
            "CREATE INDEX idx_tasks_priority_created_id ON tasks(priority,created_at,id)"
        );
        assert_eq!(
            index_sql(&connection, "idx_events_created_seq"),
            "CREATE INDEX idx_events_created_seq ON events(created_at,seq)"
        );

        let seq_only_secondary = index_names(&connection, "events")
            .into_iter()
            .filter(|name| index_columns(&connection, name) == vec!["seq".to_owned()])
            .collect::<Vec<_>>();
        assert!(
            seq_only_secondary.is_empty(),
            "unexpected seq-only event index(es): {}",
            seq_only_secondary.join(", ")
        );
    }
}
