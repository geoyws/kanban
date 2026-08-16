use crate::model::{TASK_STATUSES, TASK_TYPES};
use crate::registry::now_ms;
use crate::store::Store;
use anyhow::{Context, Result, bail};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::Path;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DanglingDependency {
    #[serde(rename = "taskID")]
    pub task_id: String,
    #[serde(rename = "dependencyID")]
    pub dependency_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissingParent {
    #[serde(rename = "taskID")]
    pub task_id: String,
    #[serde(rename = "parentID")]
    pub parent_id: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NonterminalCompletion {
    #[serde(rename = "taskID")]
    pub task_id: String,
    pub status: String,
    pub completed_at: i64,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportWarnings {
    pub dangling_dependencies: Vec<DanglingDependency>,
    pub missing_parents: Vec<MissingParent>,
    pub nonterminal_completions: Vec<NonterminalCompletion>,
}

#[derive(Debug, Serialize)]
pub struct ImportCounts {
    pub epics: usize,
    pub stories: usize,
    pub tasks: usize,
}

#[derive(Debug, Serialize)]
pub struct ImportReceipt {
    pub source: String,
    pub counts: ImportCounts,
    pub warnings: ImportWarnings,
    pub imported: usize,
    pub created: usize,
    pub updated: usize,
}

struct Input {
    id: String,
    task_type: String,
    parent_id: Option<String>,
    title: String,
    body: Option<String>,
    assignee: Option<String>,
    lane: Option<String>,
    deliverable: Option<String>,
    stale_minutes: Option<i64>,
    driver_only: bool,
    status: String,
    priority: i64,
    created_at: i64,
    updated_at: i64,
    completed_at: Option<i64>,
    metadata: Value,
    dependencies: Vec<String>,
    note: Option<String>,
}

fn text(row: &Map<String, Value>, key: &str) -> Option<String> {
    row.get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

fn integer(row: &Map<String, Value>, key: &str) -> Option<i64> {
    row.get(key).and_then(Value::as_i64)
}

fn boolean(row: &Map<String, Value>, key: &str) -> bool {
    row.get(key)
        .and_then(Value::as_bool)
        .unwrap_or_else(|| integer(row, key) == Some(1))
}

fn epoch(value: Option<i64>, fallback: i64) -> i64 {
    value
        .map(|v| if v < 100_000_000_000 { v * 1000 } else { v })
        .unwrap_or(fallback)
}

fn parse_json(value: Option<&Value>, fallback: Value) -> Value {
    match value {
        Some(Value::String(text)) if !text.trim().is_empty() => {
            serde_json::from_str(text).unwrap_or(Value::String(text.clone()))
        }
        Some(value) if !value.is_null() => value.clone(),
        _ => fallback,
    }
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    parse_json(value, json!([]))
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn status(value: Option<String>, workflow: bool) -> Result<String> {
    let raw = value.unwrap_or_else(|| {
        if workflow {
            "planning".into()
        } else {
            "todo".into()
        }
    });
    let mapped = if workflow {
        match raw.as_str() {
            "planning" => "backlog",
            "ready" => "todo",
            "in-progress" => "in_progress",
            "testing" | "review" | "merging" => "review",
            "done" => "done",
            _ => bail!("unsupported atmux status {raw}"),
        }
    } else {
        match raw.as_str() {
            "backlog" => "backlog",
            "todo" => "todo",
            "in-progress" | "in_progress" => "in_progress",
            "blocked" => "blocked",
            "review" => "review",
            "done" => "done",
            "cancelled" | "wontfix" => "cancelled",
            _ => bail!("unsupported atmux status {raw}"),
        }
    };
    Ok(mapped.into())
}

fn epic(row: Map<String, Value>, imported_at: i64, source: &str) -> Result<Input> {
    let id = text(&row, "id").context("atmux epic has no id")?;
    let created = epoch(
        integer(&row, "createdAt").or_else(|| integer(&row, "created_at")),
        imported_at,
    );
    let completed = integer(&row, "completedAt")
        .or_else(|| integer(&row, "completed_at"))
        .map(|v| epoch(Some(v), created));
    let workflow = text(&row, "status").unwrap_or_else(|| "planning".into());
    let dependencies = string_array(row.get("dependsOn").or_else(|| row.get("depends_on")));
    Ok(Input {
        id: id.clone(),
        task_type: "epic".into(),
        parent_id: None,
        title: text(&row, "title").unwrap_or(id),
        body: text(&row, "body"),
        assignee: None,
        lane: None,
        deliverable: None,
        stale_minutes: None,
        driver_only: false,
        status: status(Some(workflow.clone()), true)?,
        priority: 1,
        created_at: created,
        updated_at: completed.unwrap_or(created),
        completed_at: completed,
        metadata: json!({"importedFrom":source,"workflowStatus":workflow,"driverRef":row.get("driverRef").or_else(||row.get("driver_ref")).cloned().unwrap_or(Value::Null),"isReady":boolean(&row,"isReady")||boolean(&row,"is_ready"),"spawnedAt":row.get("spawnedAt").or_else(||row.get("spawned_at")).cloned().unwrap_or(Value::Null),"legacyStories":parse_json(row.get("stories"),json!([])),"atmuxExtra":parse_json(row.get("extra"),Value::Object(row.clone()))}),
        dependencies,
        note: None,
    })
}

fn story(row: Map<String, Value>, imported_at: i64, source: &str) -> Result<Input> {
    let id = text(&row, "id").context("atmux story has no id")?;
    let created = epoch(
        integer(&row, "createdAt").or_else(|| integer(&row, "created_at")),
        imported_at,
    );
    let completed = integer(&row, "completedAt")
        .or_else(|| integer(&row, "completed_at"))
        .map(|v| epoch(Some(v), created));
    let advanced = epoch(
        integer(&row, "advancedAt").or_else(|| integer(&row, "advanced_at")),
        created,
    );
    let workflow = text(&row, "status").unwrap_or_else(|| "planning".into());
    Ok(Input {
        id: id.clone(),
        task_type: "story".into(),
        parent_id: text(&row, "epic"),
        title: text(&row, "title").unwrap_or(id),
        body: text(&row, "body"),
        assignee: None,
        lane: None,
        deliverable: None,
        stale_minutes: None,
        driver_only: false,
        status: status(Some(workflow.clone()), true)?,
        priority: 2,
        created_at: created,
        updated_at: completed.unwrap_or(advanced),
        completed_at: completed,
        metadata: json!({"importedFrom":source,"workflowStatus":workflow,"acceptanceCriteria":row.get("acceptanceCriteria").or_else(||row.get("acceptance_criteria")).cloned().unwrap_or(Value::Null),"reviewSignoff":boolean(&row,"reviewSignoff")||boolean(&row,"review_signoff"),"mergeTaskID":row.get("mergeTaskId").or_else(||row.get("merge_task_id")).cloned().unwrap_or(Value::Null),"mergeMode":text(&row,"mergeMode").or_else(||text(&row,"merge_mode")).unwrap_or_else(||"feature-branch".into()),"advancedAt":advanced,"atmuxExtra":parse_json(row.get("extra"),Value::Object(row.clone()))}),
        dependencies: vec![],
        note: None,
    })
}

fn task(row: Map<String, Value>, imported_at: i64, source: &str) -> Result<Input> {
    let id = text(&row, "id").context("atmux task has no id")?;
    let created = epoch(
        integer(&row, "createdAt").or_else(|| integer(&row, "created_at")),
        imported_at,
    );
    let claimed = epoch(
        integer(&row, "claimedAt").or_else(|| integer(&row, "claimed_at")),
        created,
    );
    let completed = integer(&row, "completedAt")
        .or_else(|| integer(&row, "completed_at"))
        .map(|v| epoch(Some(v), claimed));
    let parent = text(&row, "story").or_else(|| text(&row, "epic"));
    let dependencies = string_array(row.get("deps"));
    let metadata = json!({"importedFrom":source,"claimedFrom":parse_json(row.get("claimedFrom").or_else(||row.get("claimed_from")),Value::Null),"createdFrom":parse_json(row.get("createdFrom").or_else(||row.get("created_from")),Value::Null),"legacyEpic":row.get("epic").cloned().unwrap_or(Value::Null),"legacyStory":row.get("story").cloned().unwrap_or(Value::Null),"atmuxExtra":parse_json(row.get("extra"),Value::Object(row.clone()))});
    Ok(Input {
        id: id.clone(),
        task_type: "task".into(),
        parent_id: parent,
        title: text(&row, "subject").unwrap_or(id),
        body: text(&row, "body"),
        assignee: text(&row, "owner"),
        lane: text(&row, "lane"),
        deliverable: text(&row, "deliverable"),
        stale_minutes: integer(&row, "staleMin").or_else(|| integer(&row, "stale_min")),
        driver_only: boolean(&row, "driverOnly") || boolean(&row, "driver_only"),
        status: status(text(&row, "status"), false)?,
        priority: integer(&row, "priority").unwrap_or(3),
        created_at: created,
        updated_at: completed.unwrap_or(claimed),
        completed_at: completed,
        metadata,
        dependencies,
        note: text(&row, "note"),
    })
}

fn sqlite_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(v) => json!(v),
        ValueRef::Real(v) => json!(v),
        ValueRef::Text(v) => Value::String(String::from_utf8_lossy(v).into_owned()),
        ValueRef::Blob(v) => Value::String(format!("<{} bytes>", v.len())),
    }
}

fn query_rows(connection: &Connection, table: &str) -> Result<Vec<Map<String, Value>>> {
    let mut statement = connection.prepare(&format!("SELECT * FROM {table} ORDER BY id"))?;
    let names = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let rows = statement.query_map([], |row| {
        let mut object = Map::new();
        for (index, name) in names.iter().enumerate() {
            object.insert(name.clone(), sqlite_value(row.get_ref(index)?));
        }
        Ok(object)
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn normalize_and_insert(
    store: &mut Store,
    mut inputs: Vec<Input>,
    actor: &str,
    source: String,
    counts: ImportCounts,
    reconcile: bool,
) -> Result<ImportReceipt> {
    if actor.trim().is_empty() {
        bail!("actor is required");
    }
    let known = inputs
        .iter()
        .map(|input| input.id.clone())
        .collect::<HashSet<_>>();
    let mut warnings = ImportWarnings::default();
    for input in &mut inputs {
        if let Some(parent) = input.parent_id.clone()
            && !known.contains(&parent)
        {
            warnings.missing_parents.push(MissingParent {
                task_id: input.id.clone(),
                parent_id: parent.clone(),
            });
            input.parent_id = None;
            if let Some(metadata) = input.metadata.as_object_mut() {
                metadata.insert("legacyMissingParent".into(), Value::String(parent));
            }
        }
        let mut valid = Vec::new();
        let mut dangling = Vec::new();
        for dependency in &input.dependencies {
            if known.contains(dependency) {
                valid.push(dependency.clone())
            } else {
                dangling.push(dependency.clone());
                warnings.dangling_dependencies.push(DanglingDependency {
                    task_id: input.id.clone(),
                    dependency_id: dependency.clone(),
                });
            }
        }
        input.dependencies = valid;
        if !dangling.is_empty()
            && let Some(metadata) = input.metadata.as_object_mut()
        {
            metadata.insert("legacyDanglingDependencies".into(), json!(dangling));
        }
        if let Some(value) = input.completed_at
            && input.status != "done"
        {
            warnings
                .nonterminal_completions
                .push(NonterminalCompletion {
                    task_id: input.id.clone(),
                    status: input.status.clone(),
                    completed_at: value,
                });
            if let Some(metadata) = input.metadata.as_object_mut() {
                metadata.insert("legacyCompletedAt".into(), json!(value));
            }
        }
    }
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing = {
        let mut statement = transaction.prepare("SELECT id FROM tasks")?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?
    };
    let overlap = inputs
        .iter()
        .filter(|input| existing.contains(&input.id))
        .map(|input| input.id.as_str())
        .collect::<Vec<_>>();
    if !reconcile && !overlap.is_empty() {
        bail!(
            "import overlaps {} existing task(s), including {}; rerun with --reconcile only after source writers are stopped",
            overlap.len(),
            overlap
                .iter()
                .take(5)
                .copied()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    for input in &inputs {
        if !TASK_TYPES.contains(&input.task_type.as_str())
            || !TASK_STATUSES.contains(&input.status.as_str())
        {
            bail!("invalid imported task {}", input.id);
        }
        let completed = if input.status == "done" {
            input.completed_at.or(Some(input.updated_at))
        } else {
            None
        };
        transaction.execute("INSERT INTO tasks(id,type,parent_id,title,body,assignee,lane,deliverable,stale_minutes,driver_only,status,priority,created_at,updated_at,completed_at,metadata) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET type=excluded.type,parent_id=NULL,title=excluded.title,body=excluded.body,assignee=excluded.assignee,lane=excluded.lane,deliverable=excluded.deliverable,stale_minutes=excluded.stale_minutes,driver_only=excluded.driver_only,status=excluded.status,priority=excluded.priority,created_at=excluded.created_at,updated_at=excluded.updated_at,completed_at=excluded.completed_at,metadata=excluded.metadata",params![input.id,input.task_type,Option::<String>::None,input.title,input.body,input.assignee,input.lane,input.deliverable,input.stale_minutes,input.driver_only as i64,input.status,input.priority,input.created_at,input.updated_at,completed,input.metadata.to_string()])?;
        if reconcile && existing.contains(&input.id) {
            transaction.execute("DELETE FROM task_claims WHERE task_id=?", [&input.id])?;
            transaction.execute("DELETE FROM task_dependencies WHERE task_id=?", [&input.id])?;
        }
        if !existing.contains(&input.id)
            && let Some(note) = &input.note
        {
            transaction.execute(
                "INSERT INTO task_notes(task_id,author,kind,body,created_at) VALUES(?,?,?,?,?)",
                params![
                    input.id,
                    "atmux/import",
                    "progress",
                    note,
                    input.completed_at.unwrap_or(input.updated_at)
                ],
            )?;
        }
    }
    for input in &inputs {
        if let Some(parent) = &input.parent_id {
            transaction.execute(
                "UPDATE tasks SET parent_id=? WHERE id=?",
                params![parent, input.id],
            )?;
        }
        for dependency in &input.dependencies {
            transaction.execute(
                "INSERT INTO task_dependencies(task_id,depends_on) VALUES(?,?)",
                params![input.id, dependency],
            )?;
        }
    }
    transaction.execute("INSERT INTO events(task_id,kind,actor,payload,created_at) VALUES(NULL,'tasks_imported',?,?,?)",params![actor,json!({"taskIDs":inputs.iter().map(|i|&i.id).collect::<Vec<_>>()}).to_string(),now_ms()])?;
    transaction.commit()?;
    Ok(ImportReceipt {
        source,
        counts,
        warnings,
        imported: inputs.len(),
        created: inputs
            .iter()
            .filter(|input| !existing.contains(&input.id))
            .count(),
        updated: inputs
            .iter()
            .filter(|input| existing.contains(&input.id))
            .count(),
    })
}

pub fn import_json(
    store: &mut Store,
    path: &Path,
    actor: &str,
    reconcile: bool,
) -> Result<ImportReceipt> {
    let parsed: Value = serde_json::from_slice(&fs::read(path)?)?;
    let epics = parsed
        .get("epics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let stories = parsed
        .get("stories")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let tasks = parsed
        .get("tasks")
        .and_then(Value::as_array)
        .context("kanban.json has no tasks array")?
        .clone();
    let imported_at = now_ms();
    let source = "atmux/kanban.json";
    let mut inputs = Vec::new();
    for row in &epics {
        inputs.push(epic(
            row.as_object().context("invalid epic")?.clone(),
            imported_at,
            source,
        )?);
    }
    for row in &stories {
        inputs.push(story(
            row.as_object().context("invalid story")?.clone(),
            imported_at,
            source,
        )?);
    }
    for row in &tasks {
        inputs.push(task(
            row.as_object().context("invalid task")?.clone(),
            imported_at,
            source,
        )?);
    }
    normalize_and_insert(
        store,
        inputs,
        actor,
        path.to_string_lossy().into_owned(),
        ImportCounts {
            epics: epics.len(),
            stories: stories.len(),
            tasks: tasks.len(),
        },
        reconcile,
    )
}

pub fn import_sqlite(
    store: &mut Store,
    path: &Path,
    actor: &str,
    reconcile: bool,
) -> Result<ImportReceipt> {
    let source_db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut table_statement =
        source_db.prepare("SELECT name FROM sqlite_master WHERE type='table'")?;
    let tables = table_statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    drop(table_statement);
    if !tables.contains("tasks") {
        bail!("atmux state.db has no tasks table");
    }
    let epics = if tables.contains("epics") {
        query_rows(&source_db, "epics")?
    } else {
        vec![]
    };
    let stories = if tables.contains("stories") {
        query_rows(&source_db, "stories")?
    } else {
        vec![]
    };
    let tasks = query_rows(&source_db, "tasks")?;
    let imported_at = now_ms();
    let source = "atmux/state.db";
    let mut inputs = Vec::new();
    for row in &epics {
        inputs.push(epic(row.clone(), imported_at, source)?);
    }
    for row in &stories {
        inputs.push(story(row.clone(), imported_at, source)?);
    }
    for row in &tasks {
        inputs.push(task(row.clone(), imported_at, source)?);
    }
    normalize_and_insert(
        store,
        inputs,
        actor,
        path.to_string_lossy().into_owned(),
        ImportCounts {
            epics: epics.len(),
            stories: stories.len(),
            tasks: tasks.len(),
        },
        reconcile,
    )
}
