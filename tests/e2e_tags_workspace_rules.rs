//! Compiled-binary E2E: drafts and plans, provenance, tags, `workspace adopt`,
//! the symbol inventory, registry migrations and rules.
//!
//! One of the `e2e_*` area targets (t-2aeec40c). Each is serial inside
//! (`--test-threads=1`); `scripts/release-gate.sh` runs the areas side by
//! side because every case owns its fixture under a pid-unique temp root.
//! Helpers more than one area uses live in `tests/e2e_support/`.

// Each area uses only some of the shared helpers and imports.
#[allow(dead_code, unused_imports)]
mod e2e_support;
use e2e_support::*;

const MAX_CFG_ATOMS: usize = 12;

fn rule_transfer_item_fingerprint(rule: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(rule["sourceRegistryUuid"].as_str().unwrap().as_bytes());
    hasher.update([0]);
    hasher.update(rule["sourceRuleId"].as_str().unwrap().as_bytes());
    hasher.update([0]);
    hasher.update(rule["body"].as_str().unwrap().as_bytes());
    hasher.update([0]);
    hasher.update(rule["author"].as_str().unwrap().as_bytes());
    hasher.update([0]);
    hasher.update([rule["archived"].as_bool().unwrap() as u8]);
    hasher.update([0]);
    hasher.update(rule["createdAt"].as_i64().unwrap().to_le_bytes());
    hasher.update([0]);
    hasher.update(rule["updatedAt"].as_i64().unwrap().to_le_bytes());
    hasher.update([0]);
    let tags = rule["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tag| tag.as_str().unwrap())
        .collect::<Vec<_>>()
        .join("\u{1f}");
    hasher.update(tags.as_bytes());
    Sha256::digest(hasher.finalize())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn external_source_board(fixture: &Fixture, label: &str, name: &str) -> PathBuf {
    let data = fixture.root.join(format!("{label}-data"));
    let cwd = fixture.root.join(format!("{label}-cwd"));
    fs::create_dir_all(&cwd).unwrap();
    let output = fixture
        .command_with_data_dir(&cwd, &data)
        .args(["init", "--name", name, "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "source init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    PathBuf::from(value["boardPath"].as_str().unwrap())
}

fn adoption_marker_path(fixture: &Fixture) -> PathBuf {
    fixture.data.join(".workspace-adopt.json")
}

static WORKSPACE_ADOPT_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn workspace_adopt_test_guard() -> std::sync::MutexGuard<'static, ()> {
    // A panicking sibling must fail on its own assertion, not cascade a
    // PoisonError into every later adopt test that shares this lock.
    WORKSPACE_ADOPT_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const WORKSPACE_ADOPT_HELPER_ROOT_FD: i32 = 37;

const WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD: i32 = 38;

/// Occupy the helper's fixed fd numbers with close-on-exec `/dev/null`
/// handles, so the adopt binary starts with those numbers taken and then
/// freed at exec, exactly as a parent that happened to hold them would leave
/// it.
///
/// This runs in the FORKED CHILD, from `pre_exec`, never in the test process.
/// Every test in this binary is a thread of one process sharing one fd table,
/// and `dup2` onto a fixed low number there clobbers whatever a sibling thread
/// has on it mid-`spawn` — a pipe it is about to hand to its own child. Three
/// unrelated spawn-heavy tests failed `Command::spawn` with EBADF on
/// 2026-09-05, one per gate, once `Fixture::new` began spawning `git init`
/// twice per fixture and widened the window. Only async-signal-safe calls
/// belong here: `open`, `dup2`, `fcntl`, `close`.
fn occupy_helper_fds_in_child() -> std::io::Result<()> {
    unsafe {
        for target in [
            WORKSPACE_ADOPT_HELPER_ROOT_FD,
            WORKSPACE_ADOPT_HELPER_SNAPSHOT_FD,
        ] {
            let source = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
            if source < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(source, target) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if source != target && libc::close(source) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(target, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn wait_for_path(path: &Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {}", path.display());
}

#[cfg(debug_assertions)]
fn wait_for_json_file(path: &Path) -> Value {
    for _ in 0..200 {
        if let Ok(text) = fs::read_to_string(path)
            && let Ok(value) = serde_json::from_str(&text)
        {
            return value;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for valid JSON in {}", path.display());
}

fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(path) = stack.pop() {
        let metadata = fs::metadata(&path).unwrap();
        if metadata.is_dir() {
            let mut entries: Vec<_> = fs::read_dir(&path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect();
            entries.sort();
            stack.extend(entries.into_iter().rev());
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            sources.push(path);
        }
    }
    sources.sort();
    sources
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SymbolReferenceKind {
    Definition,
    Use,
}

fn symbol_references_in_source(source: &str, symbol: &str) -> Vec<SymbolReferenceKind> {
    let file = syn::parse_file(source).unwrap();
    struct Finder<'a> {
        symbol: &'a str,
        references: Vec<SymbolReferenceKind>,
    }

    #[derive(Clone)]
    enum CfgExpr {
        Atom(String),
        All(Vec<CfgExpr>),
        Any(Vec<CfgExpr>),
        Not(Box<CfgExpr>),
    }

    fn meta_path_to_string(path: &syn::Path) -> String {
        path.segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::")
    }

    fn expr_to_cfg_string(expr: &Expr) -> String {
        match expr {
            Expr::Lit(ExprLit { lit, .. }) => match lit {
                Lit::Str(value) => format!("{:?}", value.value()),
                Lit::ByteStr(value) => format!("{:?}", value.value()),
                Lit::Byte(value) => value.value().to_string(),
                Lit::Char(value) => value.value().to_string(),
                Lit::Int(value) => value.base10_digits().to_owned(),
                Lit::Float(value) => value.base10_digits().to_owned(),
                Lit::Bool(value) => value.value.to_string(),
                _ => "lit".to_owned(),
            },
            Expr::Path(expr_path) => meta_path_to_string(&expr_path.path),
            Expr::Paren(expr_paren) => expr_to_cfg_string(&expr_paren.expr),
            Expr::Group(expr_group) => expr_to_cfg_string(&expr_group.expr),
            Expr::Unary(expr_unary) => match &expr_unary.op {
                syn::UnOp::Neg(_) => format!("-{}", expr_to_cfg_string(&expr_unary.expr)),
                syn::UnOp::Not(_) => format!("!{}", expr_to_cfg_string(&expr_unary.expr)),
                _ => "unary".to_owned(),
            },
            Expr::Macro(expr_macro) => expr_macro.mac.tokens.to_string(),
            _ => "expr".to_owned(),
        }
    }

    fn meta_to_cfg_expr(meta: Meta) -> Option<CfgExpr> {
        match meta {
            Meta::Path(path) => Some(CfgExpr::Atom(meta_path_to_string(&path))),
            Meta::NameValue(name_value) => Some(CfgExpr::Atom(format!(
                "{}={}",
                meta_path_to_string(&name_value.path),
                expr_to_cfg_string(&name_value.value)
            ))),
            Meta::List(list) => {
                let tokens = list.tokens.clone();
                let items = Punctuated::<Meta, Token![,]>::parse_terminated
                    .parse2(tokens.clone())
                    .ok()?;
                let nested = items
                    .into_iter()
                    .map(meta_to_cfg_expr)
                    .collect::<Option<Vec<_>>>()?;
                match meta_path_to_string(&list.path).as_str() {
                    "all" => Some(CfgExpr::All(nested)),
                    "any" => Some(CfgExpr::Any(nested)),
                    "not" => {
                        if nested.len() != 1 {
                            return None;
                        }
                        Some(CfgExpr::Not(Box::new(nested.into_iter().next().unwrap())))
                    }
                    _ => Some(CfgExpr::Atom(format!(
                        "{}({})",
                        meta_path_to_string(&list.path),
                        tokens
                    ))),
                }
            }
        }
    }

    fn cfg_expr_atoms(expr: &CfgExpr, atoms: &mut Vec<String>) {
        match expr {
            CfgExpr::Atom(atom) => atoms.push(atom.clone()),
            CfgExpr::All(children) | CfgExpr::Any(children) => {
                for child in children {
                    cfg_expr_atoms(child, atoms);
                }
            }
            CfgExpr::Not(child) => cfg_expr_atoms(child, atoms),
        }
    }

    fn cfg_expr_matches(
        expr: &CfgExpr,
        assignment: &std::collections::HashMap<String, bool>,
    ) -> bool {
        match expr {
            CfgExpr::Atom(atom) => assignment.get(atom).copied().unwrap_or(false),
            CfgExpr::All(children) => children
                .iter()
                .all(|child| cfg_expr_matches(child, assignment)),
            CfgExpr::Any(children) => children
                .iter()
                .any(|child| cfg_expr_matches(child, assignment)),
            CfgExpr::Not(child) => !cfg_expr_matches(child, assignment),
        }
    }

    fn has_test_only_cfg(attrs: &[Attribute]) -> bool {
        attrs.iter().any(|attr| {
            if !attr.path().is_ident("cfg") {
                return false;
            }
            let Meta::List(list) = &attr.meta else {
                return false;
            };
            let Ok(items) =
                Punctuated::<Meta, Token![,]>::parse_terminated.parse2(list.tokens.clone())
            else {
                return false;
            };
            let Some(expr) = items
                .into_iter()
                .map(meta_to_cfg_expr)
                .collect::<Option<Vec<_>>>()
                .map(CfgExpr::All)
            else {
                return false;
            };
            let mut atoms = Vec::new();
            cfg_expr_atoms(&expr, &mut atoms);
            atoms.sort();
            atoms.dedup();
            atoms.retain(|atom| atom != "test");
            if atoms.len() > MAX_CFG_ATOMS {
                return false;
            }
            let mut assignment = std::collections::HashMap::new();
            let Some(limit) = 1usize.checked_shl(atoms.len() as u32) else {
                return false;
            };
            for mask in 0..limit {
                assignment.clear();
                assignment.insert("test".to_owned(), false);
                for (index, atom) in atoms.iter().enumerate() {
                    assignment.insert(atom.clone(), (mask & (1usize << index)) != 0);
                }
                if cfg_expr_matches(&expr, &assignment) {
                    return false;
                }
            }
            true
        })
    }

    trait HasAttrs {
        fn attrs(&self) -> &[Attribute];
    }

    impl HasAttrs for Item {
        fn attrs(&self) -> &[Attribute] {
            match self {
                Item::Const(item) => &item.attrs,
                Item::Enum(item) => &item.attrs,
                Item::ExternCrate(item) => &item.attrs,
                Item::Fn(item) => &item.attrs,
                Item::ForeignMod(item) => &item.attrs,
                Item::Impl(item) => &item.attrs,
                Item::Macro(item) => &item.attrs,
                Item::Mod(item) => &item.attrs,
                Item::Static(item) => &item.attrs,
                Item::Struct(item) => &item.attrs,
                Item::Trait(item) => &item.attrs,
                Item::TraitAlias(item) => &item.attrs,
                Item::Type(item) => &item.attrs,
                Item::Union(item) => &item.attrs,
                Item::Use(item) => &item.attrs,
                Item::Verbatim(_) => &[],
                _ => &[],
            }
        }
    }

    impl HasAttrs for Variant {
        fn attrs(&self) -> &[Attribute] {
            &self.attrs
        }
    }

    impl HasAttrs for TraitItemFn {
        fn attrs(&self) -> &[Attribute] {
            &self.attrs
        }
    }

    impl HasAttrs for ForeignItemFn {
        fn attrs(&self) -> &[Attribute] {
            &self.attrs
        }
    }

    impl HasAttrs for ImplItemFn {
        fn attrs(&self) -> &[Attribute] {
            &self.attrs
        }
    }

    impl Visit<'_> for Finder<'_> {
        fn visit_item(&mut self, node: &Item) {
            if has_test_only_cfg(node.attrs()) {
                return;
            }
            if let Item::Fn(item_fn) = node
                && item_fn.sig.ident == self.symbol
            {
                self.references.push(SymbolReferenceKind::Definition);
            }
            visit::visit_item(self, node);
        }

        fn visit_trait_item_fn(&mut self, node: &TraitItemFn) {
            if has_test_only_cfg(node.attrs()) {
                return;
            }
            if node.sig.ident == self.symbol {
                self.references.push(SymbolReferenceKind::Definition);
            }
            visit::visit_trait_item_fn(self, node);
        }

        fn visit_foreign_item_fn(&mut self, node: &ForeignItemFn) {
            if has_test_only_cfg(node.attrs()) {
                return;
            }
            if node.sig.ident == self.symbol {
                self.references.push(SymbolReferenceKind::Definition);
            }
            visit::visit_foreign_item_fn(self, node);
        }

        fn visit_impl_item_fn(&mut self, node: &ImplItemFn) {
            if has_test_only_cfg(node.attrs()) {
                return;
            }
            if node.sig.ident == self.symbol {
                self.references.push(SymbolReferenceKind::Definition);
            }
            visit::visit_impl_item_fn(self, node);
        }

        fn visit_field(&mut self, node: &syn::Field) {
            if has_test_only_cfg(&node.attrs) {
                return;
            }
            visit::visit_field(self, node);
        }

        fn visit_expr_method_call(&mut self, node: &ExprMethodCall) {
            if node.method == self.symbol {
                self.references.push(SymbolReferenceKind::Use);
            }
            visit::visit_expr_method_call(self, node);
        }

        fn visit_path(&mut self, node: &SynPath) {
            if node
                .segments
                .iter()
                .any(|segment| segment.ident == self.symbol)
            {
                self.references.push(SymbolReferenceKind::Use);
            }
            visit::visit_path(self, node);
        }

        fn visit_macro(&mut self, node: &syn::Macro) {
            if node.tokens.to_string().contains(self.symbol) {
                self.references.push(SymbolReferenceKind::Use);
            }
            visit::visit_macro(self, node);
        }

        fn visit_variant(&mut self, node: &Variant) {
            if has_test_only_cfg(node.attrs()) {
                return;
            }
            if node.ident == self.symbol {
                self.references.push(SymbolReferenceKind::Definition);
            }
            visit::visit_variant(self, node);
        }
    }
    let mut finder = Finder {
        symbol,
        references: Vec::new(),
    };
    finder.visit_file(&file);
    finder.references
}

/// `story advance` on an epic names the verb that does move it: an epic has no
/// gate, and the answer the claim refusal already gives — "claim one of its
/// children instead" — has no story-advance equivalent, so the discoverable
/// fix is `task move`. The old refusal said only "not a story".
#[test]
fn story_advance_on_an_epic_names_the_working_verb() {
    let fixture = Fixture::new("story-advance-epic");
    fixture.ok_json(&fixture.main, &["init", "--name", "STORY", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "the plan", "--type", "epic", "--id", "e-1", "--json",
        ],
    );
    let refused = fixture.run(
        &fixture.main,
        &["story", "advance", "e-1", "--as", "agent", "--json"],
    );
    let message = refusal_object(&refused);
    assert!(
        message.contains("is an epic") && message.contains("only a story advances"),
        "{message}"
    );
    assert!(
        message.contains("task move e-1 <status>"),
        "the refusal does not name the verb that moves an epic: {message}"
    );
    assert!(!message.contains("a epic"), "{message}");
}

#[test]
fn a_draft_task_is_not_offered_as_work_until_it_is_promoted() {
    // `backlog` already meant real work that is simply unscheduled. There was
    // nothing for the state before that -- a row still being written -- so an
    // unfinished one read as a specification and got claimed and worked.
    let fixture = Fixture::new("draft");
    fixture.ok_json(&fixture.main, &["init", "--name", "DRAFT", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Still being written",
            "--id",
            "t-draft",
            "--status",
            "draft",
            "--priority",
            "0",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Ready to work",
            "--id",
            "t-ready",
            "--priority",
            "9",
            "--json",
        ],
    );

    // The draft sorts ahead on every tiebreak --next uses and is still skipped:
    // being unfinished outranks being urgent.
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "--next", "--as", "worker", "--json"]
        )["taskID"],
        "t-ready"
    );

    // Naming it explicitly is refused, and the refusal says which state stopped it.
    let explicit = fixture.run(
        &fixture.main,
        &["claim", "t-draft", "--as", "worker", "--json"],
    );
    assert!(!explicit.status.success(), "a draft was handed out as work");
    let error = String::from_utf8_lossy(&explicit.stderr).to_string();
    assert!(error.contains("draft"), "{error}");
    assert!(error.contains("not claimable"), "{error}");

    // A draft is not touched by the refusal, and is not a stale-work candidate.
    let shown = fixture.ok_json(&fixture.main, &["task", "show", "t-draft", "--json"]);
    assert_eq!(shown["status"], "draft");
    assert!(shown["assignee"].is_null());
    assert!(
        fixture
            .ok_json(&fixture.main, &["stale", "--json"])
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Promoting it is an ordinary move, and then it is work like any other.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "t-draft", "todo", "--as", "geoyws", "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "t-draft", "--as", "worker", "--json"]
        )["taskID"],
        "t-draft"
    );

    // It is a first-class status: filterable, and counted where the others are.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Another draft",
            "--id",
            "t-d2",
            "--status",
            "draft",
            "--json",
        ],
    );
    let drafts = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--status", "draft", "--json"],
    );
    assert_eq!(drafts.as_array().unwrap().len(), 1);
    assert_eq!(drafts[0]["id"], "t-d2");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["dashboard", "--json"])[0]["taskCounts"]["draft"],
        1
    );

    // An invented status is still refused -- the set stays closed.
    let invented = fixture.run(
        &fixture.main,
        &["task", "add", "x", "--status", "nearly", "--json"],
    );
    assert!(!invented.status.success());
    assert!(String::from_utf8_lossy(&invented.stderr).contains("invalid task status"));
}

#[test]
fn the_draft_migration_preserves_the_table_it_rebuilds() {
    // Widening a CHECK means rebuilding the table, and a rebuild is the easiest
    // place in a schema to change something nobody asked to change. The one
    // that matters here: parent_id has no ON DELETE clause, so removing a
    // parent is meant to fail and name its children rather than orphan them.
    let fixture = Fixture::new("draft-migration");
    fixture.ok_json(&fixture.main, &["init", "--name", "MIGRATE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Epic", "--id", "e-1", "--type", "epic", "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Child", "--id", "t-1", "--parent", "e-1", "--json",
        ],
    );

    let refused = fixture.run(
        &fixture.main,
        &[
            "task", "remove", "e-1", "--as", "geoyws", "--force", "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "a parent with children was removed, orphaning them"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("t-1"),
        "the refusal must name the child"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"])["parentID"],
        "e-1",
        "the child lost its parent"
    );

    // Public writers now create routine work at P2 even on a board whose
    // preserved historical table default remains the old numeric 3.
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Plain", "--id", "t-2", "--json"],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-2", "--json"])["priority"],
        6,
        "the priority default was lost in the rebuild"
    );

    // Read the schema itself. The behaviour above is defended twice -- the
    // remove path names children in code before the foreign key is consulted --
    // so it cannot see a changed ON DELETE clause. The DDL can, and that clause
    // is the difference between a removal that refuses and one that silently
    // orphans, so it is asserted where it is actually written.
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();
    let connection = Connection::open(&board).unwrap();
    let schema: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='tasks'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        schema.contains("parent_id TEXT REFERENCES tasks(id),"),
        "parent_id gained an ON DELETE clause in the rebuild: {schema}"
    );
    assert!(
        !schema.contains("ON DELETE"),
        "the rebuilt tasks table carries a delete rule it never had: {schema}"
    );
    assert!(
        schema.contains("priority INTEGER NOT NULL DEFAULT 3"),
        "{schema}"
    );
    assert!(schema.contains("'draft'"), "the widened CHECK is missing");

    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='tasks'")
        .unwrap();
    let indexes = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for expected in [
        "idx_tasks_status_priority",
        "idx_tasks_parent",
        "idx_tasks_assignee_status",
        "idx_tasks_lane_status",
    ] {
        assert!(
            indexes.iter().any(|name| name == expected),
            "{expected} did not survive the rebuild: {indexes:?}"
        );
    }
    drop(statement);

    let doctor = fixture.ok_json(&fixture.main, &["doctor", "--json"]);
    assert_eq!(doctor["healthy"], true, "{doctor}");
}

#[test]
fn the_v10_sitrep_rename_preserves_v9_rows_and_their_trail() {
    let fixture = Fixture::new("sitrep-migration");
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "SITREP-MIGRATION", "--json"],
    );
    let board = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
        .as_str()
        .unwrap()
        .to_owned();

    // Recreate the exact V9 surface on an otherwise current fixture. Opening
    // it through the compiled binary below must run V10, not merely exercise
    // fresh-board behaviour.
    let connection = Connection::open(&board).unwrap();
    remove_v27_sprint_schema(&connection);
    remove_v21_subscription_schema(&connection);
    remove_v18_board_audit_schema(&connection);
    remove_v13_search_schema(&connection);
    connection
        .execute_batch(
            r#"
            DROP INDEX idx_sitreps_lane_created;
            ALTER TABLE sitreps RENAME TO status_updates;
            CREATE INDEX idx_status_lane_created ON status_updates(lane,archived,created_at DESC);
            DROP INDEX idx_rules_active;
            DROP TABLE rules;
            DROP INDEX idx_tasks_status_priority;
            DROP INDEX idx_tasks_parent;
            DROP INDEX idx_tasks_assignee_status;
            DROP INDEX idx_tasks_lane_status;
            DROP INDEX idx_task_notes_task_seq;
            DROP INDEX idx_checkpoints_task_seq;
            DROP INDEX idx_events_task_seq;
            DROP INDEX idx_handoffs_task_created;
            DROP INDEX idx_handoffs_status_created;
            DROP INDEX idx_handoffs_status_priority;
            DROP INDEX idx_attention_status_created;
            DROP INDEX idx_attention_task;
            DROP INDEX idx_attention_status_priority;
            DROP INDEX idx_task_tags_tag;
            DROP TABLE attention_tags;
            ALTER TABLE tasks DROP COLUMN archived_at;
            ALTER TABLE tasks DROP COLUMN archived;
            ALTER TABLE task_notes DROP COLUMN archived;
            ALTER TABLE checkpoints DROP COLUMN archived;
            ALTER TABLE events DROP COLUMN archived;
            ALTER TABLE handoffs DROP COLUMN priority;
            ALTER TABLE handoffs DROP COLUMN archived;
            ALTER TABLE attention DROP COLUMN priority;
            ALTER TABLE attention DROP COLUMN archived;
            ALTER TABLE task_tags DROP COLUMN archived;
            CREATE INDEX idx_tasks_status_priority ON tasks(status,priority,created_at);
            CREATE INDEX idx_tasks_parent ON tasks(parent_id);
            CREATE INDEX idx_tasks_assignee_status ON tasks(assignee,status);
            CREATE INDEX idx_tasks_lane_status ON tasks(lane,status);
            CREATE INDEX idx_task_notes_task_seq ON task_notes(task_id,seq);
            CREATE INDEX idx_checkpoints_task_seq ON checkpoints(task_id,seq);
            CREATE INDEX idx_events_task_seq ON events(task_id,seq);
            CREATE INDEX idx_handoffs_task_created ON handoffs(task_id,created_at);
            CREATE INDEX idx_handoffs_status_created ON handoffs(status,created_at);
            CREATE INDEX idx_attention_status_created ON attention(status,created_at);
            CREATE INDEX idx_attention_task ON attention(task_id);
            CREATE INDEX idx_task_tags_tag ON task_tags(tag);
            INSERT INTO status_updates VALUES(
              'u-11111111','driver-2',NULL,'claude@driver-2','first body',
              '/repo','main','abc123',NULL,'clean',1,1000
            );
            INSERT INTO status_updates VALUES(
              'u-22222222','driver-2',NULL,'codex@driver-2','second body',
              '/repo','main','def456',NULL,'1 file changed',0,2000
            );
            INSERT INTO events(task_id,kind,actor,payload,created_at) VALUES(
              NULL,'status_posted','claude@driver-2',
              '{"statusID":"u-11111111","lane":"driver-2","archived":0}',1000
            );
            INSERT INTO events(task_id,kind,actor,payload,created_at) VALUES(
              NULL,'status_posted','codex@driver-2',
              '{"statusID":"u-22222222","lane":"driver-2","archived":1}',2000
            );
            PRAGMA user_version=9;
            "#,
        )
        .unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM status_updates", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        2,
        "the V9 setup wrote no rows, so the migration test would prove nothing"
    );
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        9,
        "the fixture was not actually held at V9"
    );
    drop(connection);

    let rows = fixture.ok_json(
        &fixture.main,
        &["sitrep", "list", "--db", &board, "--all", "--json"],
    );
    assert_eq!(rows.as_array().unwrap().len(), 2);
    assert_eq!(rows[0]["id"], "sr-22222222");
    assert_eq!(rows[0]["body"], "second body");
    assert_eq!(rows[0]["archived"], false);
    assert_eq!(rows[1]["id"], "sr-11111111");
    assert_eq!(rows[1]["body"], "first body");
    assert_eq!(rows[1]["archived"], true);

    let connection = Connection::open(&board).unwrap();
    // An ordinary open of a registered board stops at the pre-CROSS schema;
    // the CROSS step is the owner's, taken by `init` (ADR-056 §5).
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        38
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM rules", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0,
        "V11 did not recreate the rules table after the V10 fixture migrated"
    );
    let mut statement = connection
        .prepare("SELECT kind,payload FROM events WHERE kind='sitrep_posted' ORDER BY seq")
        .unwrap();
    let trail = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(trail.len(), 2, "V10 lost trail entries: {trail:?}");
    assert!(trail.iter().all(|(kind, _)| kind == "sitrep_posted"));
    assert!(trail[0].1.contains("\"sitrepID\":\"sr-11111111\""));
    assert!(trail[1].1.contains("\"sitrepID\":\"sr-22222222\""));
    assert!(
        trail
            .iter()
            .all(|(_, payload)| !payload.contains("statusID"))
    );
}

#[test]
fn a_plan_is_an_epic_whose_body_survives_being_revised() {
    // A plan is an epic: its body is the plan, its children are the work it
    // became, and `draft` is a plan saved up but not ready to act on. Two
    // things had to be true for that to hold -- a body can come from a file,
    // and revising one does not destroy what it replaced.
    let fixture = Fixture::new("plan");
    fixture.ok_json(&fixture.main, &["init", "--name", "PLAN", "--json"]);

    let first = "# Q4 migration\n\n## Phase 1\nEnumerate every consumer.\n";
    let plan_file = fixture.root.join("plan.md");
    fs::write(&plan_file, first).unwrap();

    let plan = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Q4 migration",
            "--id",
            "e-plan",
            "--type",
            "epic",
            "--status",
            "draft",
            "--body-file",
            plan_file.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(plan["status"], "draft", "a plan saved up is not yet work");
    assert_eq!(plan["body"], first, "the body did not come from the file");

    // The work it becomes hangs beneath it, so "what did this plan produce" is
    // answered by the tree rather than by a link nobody maintains.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Phase 1", "--id", "e-phase1", "--type", "epic", "--parent", "e-plan",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Enumerate consumers",
            "--id",
            "t-enum",
            "--parent",
            "e-phase1",
            "--json",
        ],
    );

    // Revise it. The previous plan has to survive, or a revision is a deletion
    // with extra steps.
    let second = "# Q4 migration\n\n## Phase 1\nEnumerate every consumer, with receipts.\n";
    fs::write(&plan_file, second).unwrap();
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "update",
            "e-plan",
            "--as",
            "geoyws",
            "--body-file",
            plan_file.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "e-plan", "--json"])["body"],
        second
    );

    let events = fixture.ok_json(&fixture.main, &["events", "--task", "e-plan", "--json"]);
    let updated = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "task_updated")
        .expect("the revision was not recorded");
    assert_eq!(
        updated["payload"]["changed"],
        json!(["body"]),
        "the trail must name what moved, not just that something did"
    );
    assert_eq!(
        updated["payload"]["previousBody"], first,
        "the plan it replaced was destroyed"
    );

    // A change that is not the body records what moved and carries no body.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "update",
            "e-plan",
            "--as",
            "geoyws",
            "--title",
            "Q4 migration programme",
            "--json",
        ],
    );
    let latest = fixture.ok_json(&fixture.main, &["events", "--task", "e-plan", "--json"]);
    let title_only = latest
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "task_updated" && e["payload"]["changed"] == json!(["title"]))
        .expect("a title-only change was not recorded");
    assert!(
        title_only["payload"]["previousBody"].is_null(),
        "a body was recorded for a change that did not touch it"
    );

    // Two answers to one question is refused rather than ranked.
    let both = fixture.run(
        &fixture.main,
        &[
            "task",
            "add",
            "Ambiguous",
            "--body",
            "inline",
            "--body-file",
            plan_file.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        !both.status.success(),
        "--body and --body-file were both taken"
    );
    assert!(
        String::from_utf8_lossy(&both.stderr).contains("pass one"),
        "{}",
        String::from_utf8_lossy(&both.stderr)
    );

    // A body file that is not there is an error, not an empty plan.
    let missing = fixture.run(
        &fixture.main,
        &[
            "task",
            "add",
            "Ghost",
            "--body-file",
            "/nonexistent/plan.md",
            "--json",
        ],
    );
    assert!(!missing.status.success(), "a missing body file was ignored");
}

#[test]
fn where_work_happened_is_captured_rather_than_asked_for() {
    // The columns for this existed and were empty: measured across the live
    // boards, 0 of 20 checkpoints carried a HEAD sha, because filling them
    // meant passing --repo --branch --head by hand and nobody did.
    let fixture = Fixture::new("provenance");
    fs::create_dir_all(&fixture.main).unwrap();
    if !make_repo(&fixture.main) {
        eprintln!("git unavailable; skipping provenance assertions");
        return;
    }
    fixture.ok_json(&fixture.main, &["init", "--name", "PROV", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Work",
            "--id",
            "t-1",
            "--as",
            "claude@solo",
            "--json",
        ],
    );

    // A claim now says where it was taken, which on a box running several lanes
    // of one repository is the first question anyone asks.
    let claim = fixture.ok_json(&fixture.main, &["claim", "t-1", "--as", "worker", "--json"]);
    let worktree = claim["worktree"].as_str().expect("no worktree recorded");
    assert!(
        worktree.ends_with("main"),
        "the recorded worktree is not where the command ran: {worktree}"
    );
    assert_eq!(claim["branch"], "work");
    assert_eq!(claim["worktreeKind"], "main");
    let claimed_head = claim["headSha"].as_str().unwrap().to_owned();
    assert_eq!(
        claimed_head.len(),
        40,
        "a HEAD sha should be a full object id"
    );

    // A heartbeat is a fresh receipt, not merely a longer expiry stamped onto
    // the checkout where the claim was first taken.
    fs::write(fixture.main.join("after-claim.txt"), "new head").unwrap();
    assert!(commit_all(&fixture.main, "advance after claim"));
    let token = claim["leaseToken"].as_str().unwrap().to_owned();
    let heartbeat = fixture.ok_json(
        &fixture.main,
        &[
            "heartbeat",
            "t-1",
            "--lease",
            &token,
            "--lease-minutes",
            "30",
            "--json",
        ],
    );
    assert_ne!(heartbeat["headSha"], claimed_head);
    let current_head = Command::new("git")
        .arg("-C")
        .arg(&fixture.main)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert_eq!(
        heartbeat["headSha"],
        String::from_utf8(current_head.stdout).unwrap().trim()
    );

    // Creating a task is attributable. Every other event kind recorded who did
    // it; this one could not, because there was no --as to record.
    let events = fixture.ok_json(&fixture.main, &["events", "--task", "t-1", "--json"]);
    let added = events
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "task_added")
        .expect("no task_added event");
    assert_eq!(
        added["actor"], "claude@solo",
        "the creator was not recorded"
    );
    assert_eq!(added["payload"]["type"], "task");

    // A checkpoint fills the columns that used to be null.
    let checkpoint = fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-1",
            "--lease",
            &token,
            "--as",
            "worker",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--json",
        ],
    );
    assert_eq!(checkpoint["branch"], "work");
    assert!(!checkpoint["headSha"].is_null(), "head was not captured");
    assert!(
        !checkpoint["repoPath"].is_null(),
        "repo path was not captured"
    );
    assert_eq!(
        checkpoint["dirtySummary"], "clean",
        "a clean tree should say so"
    );

    // An explicit flag still wins: capture is a default, not an override.
    let explicit = fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-1",
            "--lease",
            &token,
            "--as",
            "worker",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--branch",
            "stated-by-hand",
            "--json",
        ],
    );
    assert_eq!(explicit["branch"], "stated-by-hand");
}

#[test]
fn a_command_outside_a_repository_records_no_provenance() {
    // Claim is best-effort: running outside a repository is not an error -- it
    // simply has no git context, and recording none is the truthful outcome.
    // (A checkpoint/handoff/sitrep would refuse here; a claim is legitimate.)
    let fixture = Fixture::new("provenance-none");
    // The fixture's cwds are git repositories now, so stand in a plain sibling
    // directory to observe the no-repository path.
    let plain = fixture.root.join("plain");
    fs::create_dir_all(&plain).unwrap();
    fixture.ok_json(&plain, &["init", "--name", "NONE", "--json"]);
    fixture.ok_json(&plain, &["task", "add", "Work", "--id", "t-1", "--json"]);

    let claim = fixture.ok_json(&plain, &["claim", "t-1", "--as", "worker", "--json"]);
    assert!(claim["worktree"].is_null(), "provenance was invented");
    assert!(claim["branch"].is_null());
    assert!(claim["headSha"].is_null());

    // And the command itself is unaffected.
    assert_eq!(claim["taskID"], "t-1");
    assert_eq!(
        fixture.ok_json(&plain, &["task", "show", "t-1", "--json"])["status"],
        "in_progress"
    );
}

#[test]
fn a_provenance_write_outside_a_checkout_is_refused_and_flags_are_validated() {
    // ADR-008: a field that says something and holds nothing is refused. Run
    // each provenance-bearing write from a plain (non-repository) directory
    // with no flags, and require a refusal that names every flag and kb-board.
    let fixture = Fixture::new("provenance-refused");
    let plain = fixture.root.join("plain");
    fs::create_dir_all(&plain).unwrap();
    fixture.ok_json(&plain, &["init", "--name", "REFUSED", "--json"]);
    fixture.ok_json(&plain, &["task", "add", "Work", "--id", "t-1", "--json"]);
    let claim = fixture.ok_json(&plain, &["claim", "t-1", "--as", "worker", "--json"]);
    let token = claim["leaseToken"].as_str().unwrap().to_owned();

    let refuses_blank = |args: &[&str]| {
        let output = fixture.run(&plain, args);
        assert!(
            !output.status.success(),
            "a provenance write from outside a checkout succeeded: {args:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        for needle in ["--repo", "--branch", "--head", "--dirty", "kb-board"] {
            assert!(
                stderr.contains(needle),
                "the refusal must name {needle}: {stderr}"
            );
        }
    };

    refuses_blank(&[
        "checkpoint",
        "t-1",
        "--lease",
        &token,
        "--as",
        "worker",
        "--summary",
        "s",
        "--intent",
        "i",
        "--next-action",
        "n",
        "--json",
    ]);
    refuses_blank(&[
        "handoff",
        "create",
        "--as",
        "worker",
        "--summary",
        "s",
        "--intent",
        "i",
        "--next-action",
        "n",
        "--json",
    ]);
    refuses_blank(&[
        "sitrep",
        "post",
        "Where I stand",
        "--as",
        "worker",
        "--lane",
        "driver-2",
        "--json",
    ]);

    // An explicit flag that smuggles garbage is refused by its shape, not
    // trusted: a HEAD must be hex (full 40 or at least 7), and --dirty must
    // read like `git status` (the exact wording kb-board writes).
    let head_shape = fixture.run(
        &plain,
        &[
            "checkpoint",
            "t-1",
            "--lease",
            &token,
            "--as",
            "worker",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--repo",
            "/tmp/r",
            "--branch",
            "b",
            "--head",
            "zzz",
            "--dirty",
            "clean",
            "--json",
        ],
    );
    assert!(
        !head_shape.status.success(),
        "a non-hex --head was accepted"
    );
    assert!(
        String::from_utf8_lossy(&head_shape.stderr).contains("--head"),
        "the shape refusal must name --head: {}",
        String::from_utf8_lossy(&head_shape.stderr)
    );

    let dirty_wording = fixture.run(
        &plain,
        &[
            "checkpoint",
            "t-1",
            "--lease",
            &token,
            "--as",
            "worker",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--repo",
            "/tmp/r",
            "--branch",
            "b",
            "--head",
            "0123456789abcdef0123456789abcdef01234567",
            "--dirty",
            "3 files",
            "--json",
        ],
    );
    assert!(
        !dirty_wording.status.success(),
        "a malformed --dirty was accepted"
    );
    assert!(
        String::from_utf8_lossy(&dirty_wording.stderr).contains("--dirty"),
        "the wording refusal must name --dirty: {}",
        String::from_utf8_lossy(&dirty_wording.stderr)
    );
}

#[test]
fn provenance_flags_round_trip_exactly_and_capture_matches_the_checkout() {
    // (b) Explicit flags are stored verbatim, so a caller whose checkout is not
    // the process cwd can still ship the truth across a process boundary.
    let fixture = Fixture::new("provenance-roundtrip");
    fixture.ok_json(&fixture.main, &["init", "--name", "RT", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Work", "--id", "t-1", "--json"],
    );
    let claim = fixture.ok_json(&fixture.main, &["claim", "t-1", "--as", "worker", "--json"]);
    let token = claim["leaseToken"].as_str().unwrap().to_owned();

    let head = "0123456789abcdef0123456789abcdef01234567";
    let checkpoint = fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-1",
            "--lease",
            &token,
            "--as",
            "worker",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--repo",
            "/tmp/example-repo",
            "--branch",
            "feature-x",
            "--head",
            head,
            "--dirty",
            "2 files changed",
            "--json",
        ],
    );
    assert_eq!(checkpoint["repoPath"], "/tmp/example-repo");
    assert_eq!(checkpoint["branch"], "feature-x");
    assert_eq!(checkpoint["headSha"], head);
    assert_eq!(checkpoint["dirtySummary"], "2 files changed");
    // Read back through the same surface a resuming agent would use.
    let shown = fixture.ok_json(&fixture.main, &["task", "show", "t-1", "--json"]);
    assert_eq!(shown["checkpoints"][0]["repoPath"], "/tmp/example-repo");
    assert_eq!(shown["checkpoints"][0]["headSha"], head);
    assert_eq!(shown["checkpoints"][0]["dirtySummary"], "2 files changed");

    fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "--as",
            "worker",
            "--to",
            "@:team/project/driver-2",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--repo",
            "/tmp/example-repo",
            "--branch",
            "feature-x",
            "--head",
            head,
            "--dirty",
            "2 files changed",
            "--json",
        ],
    );
    let listed = fixture.ok_json(&fixture.main, &["handoff", "list", "--json"]);
    let row = &listed.as_array().unwrap()[0];
    assert_eq!(row["repoPath"], "/tmp/example-repo");
    assert_eq!(row["branch"], "feature-x");
    assert_eq!(row["headSha"], head);
    assert_eq!(row["dirtySummary"], "2 files changed");

    let sitrep = fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "Where I stand",
            "--as",
            "worker",
            "--lane",
            "driver-2",
            "--repo",
            "/tmp/example-repo",
            "--branch",
            "feature-x",
            "--head",
            head,
            "--dirty",
            "2 files changed",
            "--json",
        ],
    );
    assert_eq!(sitrep["worktree"], "/tmp/example-repo");
    assert_eq!(sitrep["branch"], "feature-x");
    assert_eq!(sitrep["headSha"], head);
    assert_eq!(sitrep["dirtySummary"], "2 files changed");
    let sitrep_listed = fixture.ok_json(&fixture.main, &["sitrep", "list", "--json"]);
    assert_eq!(
        sitrep_listed.as_array().unwrap()[0]["worktree"],
        "/tmp/example-repo"
    );

    // (c) With no flags, capture resolves the checkout the command runs in.
    // `main` is a git repository (the fixture made it one), so the recorded
    // values equal that checkout's real HEAD, branch and dirty count.
    let real_head = Command::new("git")
        .arg("-C")
        .arg(&fixture.main)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(
        real_head.status.success(),
        "git rev-parse HEAD failed: {}",
        String::from_utf8_lossy(&real_head.stderr)
    );
    let real_head = String::from_utf8(real_head.stdout)
        .unwrap()
        .trim()
        .to_owned();

    let captured = fixture.ok_json(
        &fixture.main,
        &[
            "checkpoint",
            "t-1",
            "--lease",
            &token,
            "--as",
            "worker",
            "--summary",
            "s",
            "--intent",
            "i",
            "--next-action",
            "n",
            "--json",
        ],
    );
    assert!(
        captured["repoPath"].as_str().unwrap().ends_with("main"),
        "captured repoPath is not the checkout: {}",
        captured["repoPath"]
    );
    assert_eq!(captured["branch"], "work");
    assert_eq!(captured["headSha"], real_head);
    assert_eq!(captured["dirtySummary"], "clean");
}

#[test]
fn work_under_an_unopened_plan_is_not_handed_to_a_driver() {
    // A plan is an epic, so drafting a plan and hanging work under it produced
    // tasks that were immediately claimable: whether the plan was ready was
    // recorded on the plan and consulted by nobody. A draft protected the row
    // it sat on and nothing beneath it.
    let fixture = Fixture::new("draft-gate");
    fixture.ok_json(&fixture.main, &["init", "--name", "GATE", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Q4 plan", "--id", "e-plan", "--type", "epic", "--status", "draft",
            "--json",
        ],
    );
    // Two levels down, because a plan holds sub-plans and the gate has to reach
    // through them rather than only checking the immediate parent.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "Phase 1", "--id", "e-p1", "--type", "epic", "--parent", "e-plan",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "add", "The work", "--id", "t-work", "--parent", "e-p1", "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Unrelated", "--id", "t-free", "--json"],
    );

    // --next steps over the whole drafted tree and finds the open work instead.
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["claim", "--next", "--as", "driver-1", "--json"]
        )["taskID"],
        "t-free",
        "a driver was handed work from a plan nobody had opened"
    );

    // Naming it explicitly is refused, and the refusal names the plan and the
    // command that opens it -- an agent told only "no" has no next move.
    let refused = fixture.run(
        &fixture.main,
        &["claim", "t-work", "--as", "driver-2", "--json"],
    );
    assert!(!refused.status.success(), "work under a draft was claimed");
    let error = String::from_utf8_lossy(&refused.stderr).to_string();
    assert!(
        error.contains("e-plan"),
        "the draft ancestor is not named: {error}"
    );
    assert!(error.contains("draft"), "{error}");
    assert!(error.contains("task move e-plan todo"), "{error}");

    // Opening the plan makes its work available to any driver.
    fixture.ok_json(
        &fixture.main,
        &["task", "move", "e-plan", "todo", "--as", "geoyws", "--json"],
    );
    let claimed = fixture.ok_json(
        &fixture.main,
        &["claim", "t-work", "--as", "driver-2", "--json"],
    );
    assert_eq!(claimed["taskID"], "t-work");
    assert_eq!(claimed["agentID"], "driver-2");

    // And a second driver gets the ordinary already-claimed refusal, not the
    // draft one -- drivers are just identities competing for the same work.
    let contested = fixture.run(
        &fixture.main,
        &["claim", "t-work", "--as", "driver-3", "--json"],
    );
    assert!(!contested.status.success());
    assert!(
        String::from_utf8_lossy(&contested.stderr).contains("already claimed"),
        "{}",
        String::from_utf8_lossy(&contested.stderr)
    );

    // Every driver sees the same board, drafts included. Visibility was never
    // per-driver and is not being made so: a draft is hidden from the queue,
    // not from the reader.
    let listed = fixture.ok_json(&fixture.main, &["task", "list", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 4);
    assert_eq!(
        fixture
            .ok_json(
                &fixture.main,
                &["task", "list", "--status", "draft", "--json"]
            )
            .as_array()
            .unwrap()
            .len(),
        0,
        "e-plan was opened above, so nothing should still read as draft"
    );
}

#[test]
fn a_tag_is_a_master_file_entry_before_it_is_a_label() {
    // Free-text labels are how one subsystem ends up spelled four ways --
    // `infra`, `Infra`, `infrastructure`, `infra-` -- and a board that answers
    // "show me infra" with three of the four is worse than one with no tags at
    // all, because the answer looks complete. So a tag exists in a per-board
    // master file first, and only a registered tag can be attached.
    let fixture = Fixture::new("tags");
    fixture.ok_json(&fixture.main, &["init", "--name", "TAGS", "--json"]);

    let registered = fixture.ok_json(
        &fixture.main,
        &[
            "tag",
            "add",
            "geoyws/infra",
            "--description",
            "hosts, containers, deploys",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(registered["name"], "geoyws/infra");
    assert_eq!(registered["description"], "hosts, containers, deploys");
    assert_eq!(registered["createdBy"], "geoyws");
    assert_eq!(registered["uses"], 0);

    // Registering the same concept twice is the collision the file exists to
    // prevent, so it is refused rather than treated as an upsert -- a silent
    // second add would quietly discard the first one's description.
    let again = fixture.run(&fixture.main, &["tag", "add", "geoyws/infra", "--json"]);
    assert!(!again.status.success(), "a tag was registered twice");
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("already in the master file"),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );

    // The shape is fixed at the door: `Infra` is refused, not folded to `infra`,
    // because folding decides on the caller's behalf which spelling was meant.
    let shouted = fixture.run(&fixture.main, &["tag", "add", "Infra", "--json"]);
    assert!(!shouted.status.success(), "an uppercase tag was registered");
    assert!(
        String::from_utf8_lossy(&shouted.stderr).contains("one concept"),
        "{}",
        String::from_utf8_lossy(&shouted.stderr)
    );

    fixture.ok_json(&fixture.main, &["tag", "add", "geoyws/queuer", "--json"]);
    fixture.ok_json(&fixture.main, &["tag", "add", "geoyws/askie", "--json"]);

    // Every row type carries tags, because the axis is "which subsystem" and a
    // plan belongs to one as much as the task it produces does.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Queue rework",
            "--id",
            "e-plan",
            "--type",
            "epic",
            "--status",
            "draft",
            "--tag",
            "geoyws/queuer",
            "--tag",
            "geoyws/infra",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Retry backoff",
            "--id",
            "t-retry",
            "--parent",
            "e-plan",
            "--tag",
            "geoyws/queuer",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Chat replies",
            "--id",
            "t-chat",
            "--tag",
            "geoyws/askie",
            "--json",
        ],
    );

    let plan = fixture.ok_json(&fixture.main, &["task", "show", "e-plan", "--json"]);
    assert_eq!(
        plan["tags"],
        json!(["geoyws/infra", "geoyws/queuer"]),
        "a draft epic must carry its tags, and read back sorted"
    );

    // An unregistered tag is refused at the point of use, naming the nearest
    // registered one and the command that would make this one real -- the same
    // shape as a mistyped flag, because it is the same mistake.
    let typo = fixture.run(
        &fixture.main,
        &[
            "task",
            "update",
            "t-chat",
            "--tag",
            "geoyws/askiee",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(!typo.status.success(), "an unregistered tag was attached");
    let error = String::from_utf8_lossy(&typo.stderr).to_string();
    assert!(error.contains("master file"), "{error}");
    assert!(error.contains("did you mean geoyws/askie?"), "{error}");
    assert!(error.contains("tag add geoyws/askiee"), "{error}");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-chat", "--json"])["tags"],
        json!(["geoyws/askie"]),
        "a refused update must leave the existing tags alone"
    );

    // Listing narrows by tag, and the count is the tag's own answer to
    // "is anyone using this".
    let queuer = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--tag", "geoyws/queuer", "--json"],
    );
    let ids = queuer
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 2, "{ids:?}");
    assert!(
        ids.contains(&"e-plan") && ids.contains(&"t-retry"),
        "{ids:?}"
    );

    let listed = fixture.ok_json(&fixture.main, &["tag", "list", "--json"]);
    let uses = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|row| (row["name"].as_str().unwrap(), row["uses"].as_i64().unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(
        uses,
        vec![
            ("geoyws/askie", 1),
            ("geoyws/infra", 1),
            ("geoyws/queuer", 2)
        ],
        "tag list must report real use counts, sorted by name"
    );

    // Filtering by a tag nobody registered is refused rather than answered with
    // an empty list: an empty list reads as "nothing is tagged that", which is
    // exactly how a typo becomes a wrong answer somebody acts on.
    let ghost = fixture.run(
        &fixture.main,
        &["task", "list", "--tag", "geoyws/infr", "--json"],
    );
    assert!(!ghost.status.success(), "an unregistered filter answered");
    let ghost_error = String::from_utf8_lossy(&ghost.stderr).to_string();
    assert!(
        ghost_error.contains("did you mean geoyws/infra?"),
        "{ghost_error}"
    );
    assert!(ghost_error.contains("read like an answer"), "{ghost_error}");

    // --tag replaces wholesale rather than appending, and --clear-tags is the
    // way to say "none" -- passing both is two answers to one question.
    let both = fixture.run(
        &fixture.main,
        &[
            "task",
            "update",
            "e-plan",
            "--tag",
            "geoyws/infra",
            "--clear-tags",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !both.status.success(),
        "--tag and --clear-tags were both taken"
    );
    assert!(
        String::from_utf8_lossy(&both.stderr).contains("mutually exclusive"),
        "{}",
        String::from_utf8_lossy(&both.stderr)
    );

    let replaced = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "update",
            "e-plan",
            "--tag",
            "geoyws/infra",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(
        replaced["tags"],
        json!(["geoyws/infra"]),
        "--tag must replace, not append"
    );

    // Retiring a tag that rows still carry would strip them silently, so it is
    // refused and says how many -- the operator gets the number they need to
    // decide, not just a no.
    let in_use = fixture.run(&fixture.main, &["tag", "remove", "geoyws/queuer", "--json"]);
    assert!(!in_use.status.success(), "an in-use tag was retired");
    let in_use_error = String::from_utf8_lossy(&in_use.stderr).to_string();
    assert!(in_use_error.contains("carried by 1 row"), "{in_use_error}");
    assert!(in_use_error.contains("--force"), "{in_use_error}");

    fixture.ok_json(
        &fixture.main,
        &[
            "tag",
            "remove",
            "geoyws/queuer",
            "--force",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-retry", "--json"])["tags"],
        json!([]),
        "forcing a removal must strip the tag from the rows that carried it"
    );

    // Both halves of the master file land in the audit trail, because a tag
    // vanishing from every row it labelled is exactly the change someone will
    // later need explained.
    let events = fixture.ok_json(&fixture.main, &["events", "--limit", "50", "--json"]);
    let kinds = events
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(kinds.contains(&"tag_added"), "{kinds:?}");
    assert!(kinds.contains(&"tag_removed"), "{kinds:?}");
    let removal = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "tag_removed")
        .expect("the removal must be recorded");
    assert_eq!(removal["payload"]["tag"], "geoyws/queuer");
    assert_eq!(
        removal["payload"]["strippedFrom"], 1,
        "the trail must say how many rows lost the tag"
    );

    // And the master file is per board: a second project starts empty rather
    // than inheriting a vocabulary that was never about it.
    fixture.ok_json(&fixture.worktree, &["init", "--name", "OTHER", "--json"]);
    assert_eq!(
        fixture
            .ok_json(&fixture.worktree, &["tag", "list", "--json"])
            .as_array()
            .unwrap()
            .len(),
        0,
        "tags must not leak between boards"
    );
}

/// CLI-01 — `tag add` refuses a bare name with the board's own namespaced
/// form, on every mapped estate, and writes nothing.
#[test]
fn tag_add_refuses_a_bare_name_with_the_boards_mapped_estate() {
    for (board, estate) in [
        ("prjx", "ifca"),
        ("kanban", "geoyws"),
        ("memberx", "unum"),
        ("unum-ledger", "unum"),
    ] {
        let fixture = Fixture::new("tag-namespace-refusal");
        fixture.ok_json(&fixture.main, &["init", "--name", board, "--json"]);
        let refused = fixture.run(&fixture.main, &["tag", "add", "assistant", "--json"]);
        assert!(
            !refused.status.success(),
            "a bare tag was registered on {board}"
        );
        assert_eq!(
            refusal_object(&refused),
            format!(
                "tag assistant is not namespaced: use {estate}/assistant (estates: ifca, unum, geoyws)"
            ),
            "wrong repair on {board}"
        );
        assert_eq!(
            fixture.ok_json(&fixture.main, &["tag", "list", "--json"]),
            json!([]),
            "a refused registration must leave the master file empty on {board}"
        );
        let kinds = fixture
            .ok_json(&fixture.main, &["events", "--limit", "50", "--json"])
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["kind"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert!(
            !kinds.contains(&"tag_added".to_owned()),
            "a refused registration must append no event on {board}: {kinds:?}"
        );
    }
    // A7 — the shape and estate checks run before the namespace check, with
    // their own baseline sentences, on the mapped board too.
    let shaped = Fixture::new("tag-namespace-shape-first");
    shaped.ok_json(&shaped.main, &["init", "--name", "prjx", "--json"]);
    for (name, sentence) in [
        (
            "Assistant",
            "tag Assistant is not a usable name: lowercase letters, digits and inner \
             hyphens only, so one concept cannot arrive under two spellings",
        ),
        (
            "ifac/assistant",
            "tag ifac/assistant names estate ifac, which is not registered: a namespaced tag \
             is filed under one of ifca, unum, geoyws, so one subsystem cannot arrive under \
             two owners",
        ),
    ] {
        let refused = shaped.run(&shaped.main, &["tag", "add", name, "--json"]);
        assert!(!refused.status.success(), "a malformed tag was registered");
        let message = refusal_object(&refused);
        assert_eq!(message, sentence, "wrong baseline sentence for {name}");
        assert!(
            !message.contains("not namespaced"),
            "the shape check must not speak of namespaces: {message}"
        );
    }
    assert_eq!(
        shaped.ok_json(&shaped.main, &["tag", "list", "--json"]),
        json!([]),
        "a refused registration must leave the master file empty"
    );
}

/// CLI-01 — a batched bare registration is refused against the batch's
/// board, not the cwd's. An item argv carries no board selector
/// (`plan_transact` refuses one), so resolving the board from the workspace
/// would name the wrong estate — or bail on a retired cwd board that has
/// nothing to do with the batch.
#[test]
fn tag_add_in_transact_refuses_against_the_batch_board_not_the_cwd() {
    let fixture = Fixture::new("tag-namespace-transact");
    fixture.ok_json(&fixture.main, &["init", "--name", "prjx", "--json"]);
    fixture.ok_json(&fixture.worktree, &["init", "--name", "kanban", "--json"]);
    let items = serde_json::to_string(&[serde_json::json!({
        "name": "tag_add",
        "arguments": {"name": "assistant"},
    })])
    .unwrap();
    // From the geoyws board's own checkout, against the ifca board.
    let batch = fixture.run(
        &fixture.worktree,
        &["transact", "--project", "prjx", "--items", &items, "--json"],
    );
    assert!(!batch.status.success(), "a batched bare tag was registered");
    let stdout = String::from_utf8_lossy(&batch.stdout).to_string();
    assert!(
        stdout.contains("not namespaced: use ifca/assistant"),
        "the refusal named the wrong board's estate: {stdout}"
    );
    assert!(
        !stdout.contains("geoyws/assistant"),
        "the refusal named the cwd board's estate: {stdout}"
    );
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &["tag", "list", "--project", "prjx", "--json"]
        ),
        json!([]),
        "a refused batch must land nothing on the batch board"
    );
}

/// CLI-03 — an unmapped board is refused with the estate list and no single
/// suggestion, because there is no board truth to build one from.
#[test]
fn tag_add_refuses_a_bare_name_on_an_unmapped_board_with_the_estate_list_only() {
    let fixture = Fixture::new("tag-namespace-unmapped");
    fixture.ok_json(&fixture.main, &["init", "--name", "SCRATCH", "--json"]);
    let refused = fixture.run(&fixture.main, &["tag", "add", "assistant", "--json"]);
    assert!(!refused.status.success(), "a bare tag was registered");
    let message = refusal_object(&refused);
    assert!(
        message.contains("(estates: ifca, unum, geoyws)"),
        "the refusal must carry the estate list: {message}"
    );
    for estate in ["ifca", "unum", "geoyws"] {
        assert!(
            !message.contains(&format!("use {estate}/")),
            "an unmapped board must suggest no single form: {message}"
        );
    }
    assert_eq!(
        fixture.ok_json(&fixture.main, &["tag", "list", "--json"]),
        json!([]),
        "a refused registration must leave the master file empty"
    );
}

/// CLI-07 — attaching an unregistered tag refuses with the board's estate
/// form, and the named repair is a working one: it registers, and the tag
/// then attaches. An unmapped board carries the estate list and suggests no
/// single form.
#[test]
fn tag_attach_refusal_names_the_boards_estate_form() {
    // Mapped board: the refusal names `tag add ifca/assistant`.
    let fixture = Fixture::new("tag-attach-repair");
    fixture.ok_json(&fixture.main, &["init", "--name", "prjx", "--json"]);
    let refused = fixture.run(
        &fixture.main,
        &[
            "task",
            "add",
            "Chat replies",
            "--id",
            "t-chat",
            "--tag",
            "assistant",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "an unregistered tag was attached"
    );
    assert_eq!(
        refusal_object(&refused),
        "tag assistant is not in this board's master file — \
         register it first with `tag add ifca/assistant`",
        "the attach refusal must name the estate form"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "list", "--json"]),
        json!([]),
        "a refused attach must write no row"
    );
    // The suggested repair registers, and the tag then attaches.
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "ifca/assistant", "--as", "geoyws", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Chat replies",
            "--id",
            "t-chat",
            "--tag",
            "ifca/assistant",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-chat", "--json"])["tags"],
        json!(["ifca/assistant"]),
        "the repaired tag must attach and read back"
    );

    // Unmapped board: the estate list, no single suggestion.
    let unmapped = Fixture::new("tag-attach-repair-unmapped");
    unmapped.ok_json(&unmapped.main, &["init", "--name", "SCRATCH", "--json"]);
    let refused = unmapped.run(
        &unmapped.main,
        &[
            "task",
            "add",
            "Chat replies",
            "--id",
            "t-chat",
            "--tag",
            "assistant",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "an unregistered tag was attached"
    );
    let message = refusal_object(&refused);
    assert!(
        message.contains("register it first with `tag add <estate>/assistant`"),
        "the unmapped attach refusal must name the estate placeholder: {message}"
    );
    assert!(
        message.contains("(estates: ifca, unum, geoyws)"),
        "the unmapped attach refusal must carry the estate list: {message}"
    );
    for estate in ["ifca", "unum", "geoyws"] {
        assert!(
            !message.contains(&format!("tag add {estate}/assistant")),
            "an unmapped board must suggest no single form: {message}"
        );
    }
    assert_eq!(
        unmapped.ok_json(&unmapped.main, &["task", "list", "--json"]),
        json!([]),
        "a refused attach must write no row"
    );
}

/// CLI-08 — the lease holder moves its own row without `--force`; a bystander
/// is still refused, and `task remove` keeps the guard for the holder too.
#[test]
fn task_move_lets_the_lease_holder_move_its_own_row_and_still_refuses_a_bystander() {
    let fixture = Fixture::new("holder-move");
    fixture.ok_json(&fixture.main, &["init", "--name", "HOLDER", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "held", "--id", "t-held", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["claim", "t-held", "--as", "worker", "--json"],
    );
    let events_of = |kind: &str| {
        fixture
            .ok_json(&fixture.main, &["events", "--kind", kind, "--json"])
            .as_array()
            .unwrap()
            .clone()
    };
    let show = || fixture.ok_json(&fixture.main, &["task", "show", "t-held", "--json"]);
    let expires = show()["claim"]["expiresAt"].as_i64().unwrap();

    // A bystander is refused exactly as before and writes nothing.
    let refused = fixture.run(
        &fixture.main,
        &[
            "task",
            "move",
            "t-held",
            "todo",
            "--as",
            "bystander",
            "--json",
        ],
    );
    assert!(!refused.status.success(), "a bystander voided a live lease");
    assert_eq!(
        refusal_object(&refused),
        format!(
            "task t-held is leased by worker until {expires} (session -); \
             rerun with --force to move it anyway"
        )
    );
    let after_refusal = show();
    assert_eq!(after_refusal["status"], "in_progress");
    assert_eq!(after_refusal["claim"]["agentID"], "worker");
    assert!(events_of("lease_seized").is_empty());

    // The holder's move to in_progress keeps its claim and releases nothing.
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "move",
            "t-held",
            "in_progress",
            "--as",
            "worker",
            "--json",
        ],
    );
    assert_eq!(show()["claim"]["agentID"], "worker");
    assert!(events_of("claim_released").is_empty());

    // `task remove` keeps the guard, even for the holder.
    let remove = fixture.run(
        &fixture.main,
        &["task", "remove", "t-held", "--as", "worker", "--json"],
    );
    assert!(
        !remove.status.success(),
        "the holder removed without --force"
    );
    assert!(
        refusal_object(&remove).ends_with("rerun with --force to remove it anyway"),
        "{}",
        refusal_object(&remove)
    );

    // The holder moves its own row without --force: no seizure, one release.
    fixture.ok_json(
        &fixture.main,
        &[
            "task", "move", "t-held", "review", "--as", "worker", "--json",
        ],
    );
    let moved = show();
    assert_eq!(moved["status"], "review");
    assert!(
        moved["claim"].is_null(),
        "the holder's move left a claim: {moved}"
    );
    assert!(events_of("lease_seized").is_empty());
    let released = events_of("claim_released");
    assert_eq!(released.len(), 1);
    assert_eq!(released[0]["actor"], "worker");
    assert_eq!(released[0]["taskID"], "t-held");
    let task_moved = events_of("task_moved");
    assert_eq!(task_moved[0]["payload"]["status"], "review");
    assert!(task_moved[0]["payload"]["seizedFrom"].is_null());
}

/// CLI-04 — a namespaced name registers exactly as before: it lists, and a
/// row carries it.
#[test]
fn tag_add_registers_a_namespaced_tag() {
    let fixture = Fixture::new("tag-namespace-success");
    fixture.ok_json(&fixture.main, &["init", "--name", "prjx", "--json"]);
    let registered = fixture.ok_json(
        &fixture.main,
        &["tag", "add", "ifca/assistant", "--as", "geoyws", "--json"],
    );
    assert_eq!(registered["name"], "ifca/assistant");
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Chat replies",
            "--id",
            "t-chat",
            "--tag",
            "ifca/assistant",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-chat", "--json"])["tags"],
        json!(["ifca/assistant"]),
        "a registered namespaced tag must attach and read back"
    );
}

/// CLI-09 (A11) — a tag name is at most 64 bytes whole: `tag add` refuses 67
/// bytes with the length sentence and writes nothing, accepts exactly 64, and
/// `tag rename` refuses a 67-byte NEW name, leaving the old name in place.
#[test]
fn tag_add_and_rename_refuse_a_name_over_64_bytes() {
    let fixture = Fixture::new("tag-name-bound");
    fixture.ok_json(&fixture.main, &["init", "--name", "kanban", "--json"]);
    let at_bound = format!("geoyws/{}", "a".repeat(57));
    let over = format!("geoyws/{}", "a".repeat(60));
    assert_eq!((at_bound.len(), over.len()), (64, 67));
    let sentence = format!(
        "tag {over} is not a usable name: at most 64 bytes in all, every segment and slash \
         counted, so a tag stays one readable handle"
    );
    let names = |fixture: &Fixture| -> Vec<String> {
        fixture
            .ok_json(&fixture.main, &["tag", "list", "--json"])
            .as_array()
            .expect("tag list is an array")
            .iter()
            .map(|tag| tag["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let tag_added = |fixture: &Fixture| -> usize {
        fixture
            .ok_json(&fixture.main, &["events", "--kind", "tag_added", "--json"])
            .as_array()
            .expect("events is an array")
            .len()
    };
    let before = (names(&fixture), tag_added(&fixture));

    let refused = fixture.run(
        &fixture.main,
        &["tag", "add", &over, "--as", "geoyws", "--json"],
    );
    assert_eq!(refusal_object(&refused), sentence);
    assert_eq!(
        (names(&fixture), tag_added(&fixture)),
        before,
        "a refused add wrote"
    );

    let added = fixture.ok_json(
        &fixture.main,
        &["tag", "add", &at_bound, "--as", "geoyws", "--json"],
    );
    assert_eq!(added["name"], at_bound.as_str());
    assert!(names(&fixture).contains(&at_bound));

    let renamed = fixture.run(
        &fixture.main,
        &[
            "tag", "rename", &at_bound, &over, "--as", "geoyws", "--json",
        ],
    );
    assert_eq!(refusal_object(&renamed), sentence);
    let after = names(&fixture);
    assert!(
        after.contains(&at_bound),
        "the refused rename moved the tag: {after:?}"
    );
    assert!(!after.contains(&over), "{after:?}");
}

/// CLI-05 — the `--tag` filters refuse unknown names exactly as today: the
/// master-file sentence on the two listings, the registry sentence on rules,
/// and never the `tag add` namespace sentence, which registers rather than
/// reads.
#[test]
fn tag_filters_refuse_unknown_names_exactly_as_before() {
    let fixture = Fixture::new("tag-filter-refusal");
    fixture.ok_json(&fixture.main, &["init", "--name", "FILTERS", "--json"]);
    let listed = fixture.run(&fixture.main, &["task", "list", "--tag", "nope", "--json"]);
    assert!(!listed.status.success(), "an unknown tag filtered");
    let message = refusal_object(&listed);
    assert!(
        message.contains("is not in this board's master file"),
        "{message}"
    );
    assert!(message.contains("filter to nothing"), "{message}");
    assert!(!message.contains("not namespaced"), "{message}");
    let attention = fixture.run(
        &fixture.main,
        &["attention", "list", "--tag", "nope", "--json"],
    );
    assert!(!attention.status.success(), "an unknown tag filtered");
    let message = refusal_object(&attention);
    assert!(
        message.contains("is not in this board's master file"),
        "{message}"
    );
    assert!(message.contains("filter to nothing"), "{message}");
    assert!(!message.contains("not namespaced"), "{message}");
    let rule = fixture.run(
        &fixture.main,
        &[
            "rule",
            "add",
            "A shared rule.",
            "--tag",
            "nope",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(!rule.status.success(), "an unknown rule tag was accepted");
    let message = refusal_object(&rule);
    assert!(
        message.contains("is not registered on any active board"),
        "{message}"
    );
    assert!(!message.contains("not namespaced"), "{message}");
}

/// CLI-06 — `task add --id` validates the id against the board's own id
/// shape for the kind being created and refuses anything else with the
/// expected shape, writing nothing. A well-formed explicit id keeps working
/// (the watcher's idempotent `t-<8 hex>`), and a duplicate is still refused
/// by the primary key as before.
#[test]
fn task_add_refuses_a_misshaped_id_with_the_kinds_expected_shape() {
    let fixture = Fixture::new("task-id-shape");
    fixture.ok_json(&fixture.main, &["init", "--name", "IDSHAPE", "--json"]);
    // `bogus id!`: whitespace and `!` are outside the shape, and the refusal
    // names the shape the id should have had.
    let refused = fixture.run(
        &fixture.main,
        &[
            "task",
            "add",
            "title",
            "--id",
            "bogus id!",
            "--as",
            "X",
            "--status",
            "draft",
            "--json",
        ],
    );
    assert!(!refused.status.success(), "a spaced id was filed");
    assert_eq!(
        refusal_object(&refused),
        "invalid task id \"bogus id!\": expected t-<suffix> with 1-62 lowercase letters, digits, dot, underscore, or hyphen (at most 64 characters total)",
        "wrong repair for a misshaped id"
    );
    // A wrong-kind prefix: an epic id filed as a task, and a task id filed
    // as an epic. Both name the kind's own prefix.
    let epic_as_task = fixture.run(
        &fixture.main,
        &[
            "task",
            "add",
            "title",
            "--id",
            "e-1234abcd",
            "--as",
            "X",
            "--status",
            "draft",
            "--json",
        ],
    );
    assert!(
        !epic_as_task.status.success(),
        "an epic id was filed as a task"
    );
    assert_eq!(
        refusal_object(&epic_as_task),
        "invalid task id \"e-1234abcd\": expected t-<suffix> with 1-62 lowercase letters, digits, dot, underscore, or hyphen (at most 64 characters total)",
        "wrong repair for a wrong-kind prefix"
    );
    let task_as_epic = fixture.run(
        &fixture.main,
        &[
            "task",
            "add",
            "plan",
            "--id",
            "t-1234abcd",
            "--type",
            "epic",
            "--as",
            "X",
            "--status",
            "draft",
            "--json",
        ],
    );
    assert!(
        !task_as_epic.status.success(),
        "a task id was filed as an epic"
    );
    assert_eq!(
        refusal_object(&task_as_epic),
        "invalid epic id \"t-1234abcd\": expected e-<suffix> with 1-62 lowercase letters, digits, dot, underscore, or hyphen (at most 64 characters total)",
        "wrong repair for a wrong-kind prefix"
    );
    // Every refusal above wrote nothing: the board is still empty, and no
    // refusal appended an event — the trail still holds only the init row.
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "list", "--json"]),
        json!([]),
        "a refused id must leave the board empty"
    );
    let kinds = fixture
        .ok_json(&fixture.main, &["events", "--json"])
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["kind"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        vec!["board_initialized"],
        "a refusal appended an event: {kinds:?}"
    );
    // The same id offered as a `transact` `task_add` item rolls the batch
    // back with the same sentence: the check lives in the store, which every
    // surface reaches.
    let envelope = transact_results(
        &fixture,
        &fixture.main,
        &[
            serde_json::json!({ "name": "task_add", "arguments": {"title": "title", "id": "bogus id!"} }),
        ],
    );
    assert_eq!(envelope["ok"], false, "{envelope}");
    assert_eq!(envelope["rolledBack"], true, "{envelope}");
    assert!(
        envelope["results"][0]["error"].as_str().is_some_and(
            |error| error.contains("invalid task id \"bogus id!\": expected t-<suffix>")
        ),
        "{envelope}"
    );
    // A well-formed explicit id is accepted: the deterministic `t-<8 hex>`
    // a watcher derives to make filing idempotent.
    let created = fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "title",
            "--id",
            "t-1234abcd",
            "--as",
            "X",
            "--status",
            "draft",
            "--json",
        ],
    );
    assert_eq!(created["id"], "t-1234abcd");
    // The duplicate is still refused as before, by the primary key.
    let duplicate = fixture.run(
        &fixture.main,
        &[
            "task",
            "add",
            "title",
            "--id",
            "t-1234abcd",
            "--as",
            "X",
            "--status",
            "draft",
            "--json",
        ],
    );
    assert!(
        !duplicate.status.success(),
        "a duplicate id was filed twice"
    );
    assert!(
        refusal_object(&duplicate).contains("task t-1234abcd already exists"),
        "the duplicate must still be refused as before: {}",
        String::from_utf8_lossy(&duplicate.stdout)
    );
}

#[test]
fn tag_rename_rewrites_every_table_in_one_transaction_and_the_chain_verifies() {
    // A tag is carried by rows in three places and scoped by rules in a
    // fourth, so "rename" is only a rename if every one of them moves. Doing
    // it by hand -- add the new name, retag, retire the old -- leaves a window
    // where filters answer with half the rows and reads like a complete
    // answer, which is the failure the master file exists to prevent.
    let fixture = Fixture::new("tag-rename");
    fixture.ok_json(&fixture.main, &["init", "--name", "TAGRENAME", "--json"]);
    fixture.ok_json(
        &fixture.worktree,
        &["init", "--name", "OTHERBOARD", "--json"],
    );
    for cwd in [&fixture.main, &fixture.worktree] {
        fixture.ok_json(
            cwd,
            &["tag", "add", "geoyws/infra", "--as", "geoyws", "--json"],
        );
    }

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Live work",
            "--id",
            "t-live",
            "--tag",
            "geoyws/infra",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Old work",
            "--id",
            "t-old",
            "--tag",
            "geoyws/infra",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &["task", "move", "t-old", "done", "--as", "geoyws", "--json"],
    );
    // Archiving needs a completion old enough to sweep; the clock is the only
    // thing being faked here.
    let board_path = board_path_for_project(&fixture, &fixture.main, "TAGRENAME");
    Connection::open(&board_path)
        .unwrap()
        .execute(
            "UPDATE tasks SET completed_at=1,updated_at=1 WHERE id='t-old'",
            [],
        )
        .unwrap();
    assert_eq!(
        fixture.ok_json(
            &fixture.main,
            &[
                "archive",
                "--older-than-days",
                "1",
                "--as",
                "geoyws",
                "--json"
            ],
        )["tasks"],
        1
    );

    fixture.ok_json(
        &fixture.main,
        &[
            "attention",
            "raise",
            "Which queue owns retries?",
            "--as",
            "geoyws",
            "--tag",
            "geoyws/infra",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Everything infra ships behind a flag.",
            "--as",
            "geoyws",
            "--tag",
            "geoyws/infra",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Other board infra rule.",
            "--as",
            "geoyws",
            "--board",
            "OTHERBOARD",
            "--tag",
            "geoyws/infra",
            "--json",
        ],
    );

    let renamed = fixture.ok_json(
        &fixture.main,
        &[
            "tag",
            "rename",
            "geoyws/infra",
            "ifca/infra",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(
        renamed,
        json!({
            "old": "geoyws/infra",
            "new": "ifca/infra",
            "tasks": 1,
            "archivedTasks": 1,
            "attention": 1,
            "rules": 1,
        }),
        "the receipt must count every table it moved"
    );

    // The live row, the archived row and the attention row all carry the new
    // spelling, and the namespaced name is a usable filter -- a rename that
    // produced a name no filter accepts would have moved the rows out of
    // reach.
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-live", "--json"])["tags"],
        json!(["ifca/infra"])
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-old", "--json"])["tags"],
        json!(["ifca/infra"]),
        "history kept the old spelling: readable and unfindable"
    );
    let listed = fixture.ok_json(
        &fixture.main,
        &["task", "list", "--tag", "ifca/infra", "--json"],
    );
    assert_eq!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["t-live"]
    );
    assert_eq!(
        fixture
            .ok_json(
                &fixture.main,
                &["attention", "list", "--tag", "ifca/infra", "--json"],
            )
            .as_array()
            .unwrap()
            .len(),
        1,
        "an attention filter must take the namespaced form verbatim"
    );

    let master = fixture.ok_json(&fixture.main, &["tag", "list", "--json"]);
    let entry = master.as_array().unwrap();
    assert_eq!(entry.len(), 1, "{master}");
    assert_eq!(entry[0]["name"], "ifca/infra");
    assert_eq!(entry[0]["renamedFrom"], "geoyws/infra");
    assert_eq!(entry[0]["createdBy"], "geoyws", "provenance is preserved");
    assert_eq!(entry[0]["uses"], 3);

    // The rule scoped to this board moved; the one scoped to another board is
    // another board's vocabulary and is left exactly as it was.
    let rules = fixture.ok_json(&fixture.main, &["rule", "list", "--full", "--json"]);
    let tags = rules
        .as_array()
        .unwrap()
        .iter()
        .map(|rule| {
            (
                rule["body"].as_str().unwrap().to_owned(),
                rule["tags"].clone(),
            )
        })
        .collect::<Vec<_>>();
    assert!(
        tags.iter()
            .any(|(body, tags)| body.starts_with("Everything infra ships")
                && tags == &json!(["ALL", "ifca/infra"])),
        "{tags:?}"
    );
    assert!(
        tags.iter()
            .any(|(body, tags)| body.starts_with("Other board infra rule")
                && tags == &json!(["ONLY:OTHERBOARD", "geoyws/infra"])),
        "another board's rule must be untouched: {tags:?}"
    );

    let events = fixture.ok_json(&fixture.main, &["events", "--limit", "50", "--json"]);
    let rename_event = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "tag_renamed")
        .expect("the rename must be in the ledger");
    assert_eq!(rename_event["payload"]["old"], "geoyws/infra");
    assert_eq!(rename_event["payload"]["new"], "ifca/infra");
    assert_eq!(rename_event["payload"]["tasks"], 1);
    assert_eq!(rename_event["payload"]["archivedTasks"], 1);
    assert_eq!(rename_event["payload"]["attention"], 1);
    assert_eq!(rename_event["actor"], "geoyws");

    assert_eq!(
        fixture.ok_json(&fixture.main, &["audit", "verify", "--json"])["healthy"],
        json!(true),
        "a rename must leave the hash chain verifiable"
    );
}

#[test]
fn tag_rename_refuses_unknown_existing_and_malformed_names() {
    // Each refusal names the one move that makes it work: a rename is a
    // destructive rewrite over rows nobody is looking at, so "no" without a
    // next step is how an operator reaches for raw SQL instead.
    let fixture = Fixture::new("tag-rename-refusals");
    fixture.ok_json(&fixture.main, &["init", "--name", "REFUSALS", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "geoyws/infra", "--as", "geoyws", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "geoyws/queuer", "--as", "geoyws", "--json"],
    );

    let unknown = fixture.run(
        &fixture.main,
        &["tag", "rename", "ghost", "ifca/ghost", "--as", "geoyws"],
    );
    assert!(!unknown.status.success(), "an unknown tag was renamed");
    let unknown = String::from_utf8_lossy(&unknown.stderr).to_string();
    assert!(
        unknown.contains(
            "tag ghost is not in this board's master file, so there is nothing to rename \
             — `kanban tag list` names the ones that are"
        ),
        "{unknown}"
    );

    let taken = fixture.run(
        &fixture.main,
        &[
            "tag",
            "rename",
            "geoyws/infra",
            "geoyws/queuer",
            "--as",
            "geoyws",
        ],
    );
    assert!(!taken.status.success(), "a rename merged two tags");
    let taken = String::from_utf8_lossy(&taken.stderr).to_string();
    assert!(
        taken.contains(
            "tag geoyws/queuer is already in the master file and a rename does not merge two tags \
             into one — pick a free name, or retire one of them with `kanban tag remove` first"
        ),
        "{taken}"
    );

    let shouted = fixture.run(
        &fixture.main,
        &[
            "tag",
            "rename",
            "geoyws/infra",
            "Ifca/infra",
            "--as",
            "geoyws",
        ],
    );
    assert!(!shouted.status.success(), "a malformed name was accepted");
    let shouted = String::from_utf8_lossy(&shouted.stderr).to_string();
    assert!(
        shouted.contains(
            "tag Ifca/infra is not a usable name: lowercase letters, digits and inner \
             hyphens only, so one concept cannot arrive under two spellings"
        ),
        "{shouted}"
    );

    let estate = fixture.run(
        &fixture.main,
        &[
            "tag",
            "rename",
            "geoyws/infra",
            "acme/infra",
            "--as",
            "geoyws",
        ],
    );
    assert!(!estate.status.success(), "an unregistered estate was taken");
    let estate = String::from_utf8_lossy(&estate.stderr).to_string();
    assert!(
        estate.contains(
            "tag acme/infra names estate acme, which is not registered: a namespaced tag \
             is filed under one of ifca, unum, geoyws, so one subsystem cannot arrive under \
             two owners"
        ),
        "{estate}"
    );

    // Every refusal left the master file exactly as it was.
    assert_eq!(
        fixture
            .ok_json(&fixture.main, &["tag", "list", "--json"])
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        vec!["geoyws/infra".to_owned(), "geoyws/queuer".to_owned()]
    );
}

#[test]
fn tag_rename_is_admissible_inside_transact() {
    // A rename is a write like any other, so it belongs in the one batch that
    // is all-or-nothing (ADR-041). The registry half is the part that has to
    // be proved: it cannot join the board's transaction, so it is held until
    // the batch commits -- a rolled-back batch that had already rewritten the
    // rules would leave them pointing at a tag no board has.
    let fixture = Fixture::new("tag-rename-transact");
    fixture.ok_json(&fixture.main, &["init", "--name", "TRANSACTED", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "geoyws/infra", "--as", "geoyws", "--json"],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Ship it",
            "--id",
            "t-ship",
            "--tag",
            "geoyws/infra",
            "--json",
        ],
    );
    fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Infra ships behind a flag.",
            "--as",
            "geoyws",
            "--tag",
            "geoyws/infra",
            "--json",
        ],
    );

    let rule_tags = |fixture: &Fixture| {
        fixture.ok_json(&fixture.main, &["rule", "list", "--full", "--json"])[0]["tags"].clone()
    };

    // A second item that cannot land: the rename must go back with it.
    let rolled_back = transact_results(
        &fixture,
        &fixture.main,
        &[
            json!({
                "name": "tag_rename",
                "arguments": { "old": "geoyws/infra", "new": "ifca/infra", "as": "geoyws" },
            }),
            json!({
                "name": "task_update",
                "arguments": { "id": "t-missing", "title": "no such task", "as": "geoyws" },
            }),
        ],
    );
    assert_eq!(rolled_back["ok"], false, "{rolled_back}");
    assert_eq!(rolled_back["failedIndex"], 1, "{rolled_back}");
    assert_eq!(rolled_back["rolledBack"], true, "{rolled_back}");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["tag", "list", "--json"])[0]["name"],
        "geoyws/infra",
        "the rename survived a rolled-back batch"
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-ship", "--json"])["tags"],
        json!(["geoyws/infra"])
    );
    assert_eq!(
        rule_tags(&fixture),
        json!(["ALL", "geoyws/infra"]),
        "the registry rewrite must wait for the board's commit"
    );

    // The same batch with a second item that works: both land.
    let landed = transact_results(
        &fixture,
        &fixture.main,
        &[
            json!({
                "name": "tag_rename",
                "arguments": { "old": "geoyws/infra", "new": "ifca/infra", "as": "geoyws" },
            }),
            json!({
                "name": "task_update",
                "arguments": { "id": "t-ship", "title": "Ship it now", "as": "geoyws" },
            }),
        ],
    );
    assert_eq!(landed["ok"], true, "{landed}");
    assert_eq!(landed["results"][0]["result"]["rules"], 1, "{landed}");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-ship", "--json"])["tags"],
        json!(["ifca/infra"])
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["task", "show", "t-ship", "--json"])["title"],
        "Ship it now"
    );
    assert_eq!(rule_tags(&fixture), json!(["ALL", "ifca/infra"]));
    assert_eq!(
        fixture.ok_json(&fixture.main, &["audit", "verify", "--json"])["healthy"],
        json!(true)
    );
}

#[test]
fn a_project_whose_tree_moved_is_reported_rather_than_silently_unreachable() {
    // Registration canonicalises, so a stored root is right when written and
    // can only go wrong afterwards. This repository is the worked example: it
    // was moved into the dotfiles and a symlink left at the old path, and from
    // that moment no directory inside it resolved to its own board. The
    // database was perfect throughout and `doctor` said healthy.
    let fixture = Fixture::new("moved-tree");
    let original = fixture.root.join("project");
    let lane = original.join("lane");
    let moved = fixture.root.join("elsewhere");
    fs::create_dir_all(&lane).unwrap();

    fixture.ok_json(&original, &["init", "--name", "MOVED", "--json"]);
    let board_path =
        fixture.ok_json(&fixture.main, &["workspace", "list", "--json"])[0]["boardPath"]
            .as_str()
            .unwrap()
            .to_owned();
    fixture.ok_json(
        &lane,
        &[
            "workspace",
            "attach",
            "--to",
            original.to_str().unwrap(),
            "--json",
        ],
    );
    fixture.ok_json(
        &original,
        &["task", "add", "Real work", "--id", "t-1", "--json"],
    );

    // The move: the tree goes elsewhere and a symlink stands where it was, so
    // every path anyone has written down still works at the shell.
    fs::rename(&original, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &original).unwrap();
    assert!(original.join("lane").is_dir(), "the symlink must be usable");

    // And yet the board is gone from the inside: the caller's cwd resolves to
    // the new physical path, and the registry only knows the old spelling.
    let lost = fixture.run(&original, &["task", "list", "--json"]);
    assert!(
        !lost.status.success(),
        "the board resolved by cwd, so this test is no longer testing the defect"
    );
    assert!(
        String::from_utf8_lossy(&lost.stderr).contains("no Kanban project contains"),
        "{}",
        String::from_utf8_lossy(&lost.stderr)
    );

    // Doctor reports the stale discovery hints, but a root is not board
    // identity: the healthy board remains reachable by its global name.
    let sick = fixture.run(&original, &["doctor", "--json"]);
    assert!(
        sick.status.success(),
        "an unreachable discovery hint failed board integrity: {}",
        String::from_utf8_lossy(&sick.stderr)
    );
    let report: Value = serde_json::from_slice(&sick.stdout).unwrap();
    assert_eq!(report["healthy"], true);
    let roots = report["unreachableRoots"].as_array().unwrap();
    assert_eq!(
        roots.len(),
        2,
        "the project root and the lane beneath it both broke: {roots:?}"
    );
    let project_root = roots
        .iter()
        .find(|item| item["boardPath"] == board_path)
        .expect("the project root must be named");
    assert_eq!(project_root["name"], "MOVED");
    assert_eq!(
        project_root["resolvesTo"],
        moved.canonicalize().unwrap().to_string_lossy().into_owned(),
        "the report must say where the path leads now, not merely that it is wrong"
    );

    // Repointing takes every broken row by default, because one tree moving
    // breaks its root and each lane beneath it at once.
    let fixed = fixture.ok_json(&original, &["workspace", "repoint", "--json"]);
    assert_eq!(fixed.as_array().unwrap().len(), 2);
    let active = fixture.ok_json(&original, &["workspace", "list", "--json"]);
    let mut moved_roots = active
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["name"] == "MOVED")
        .map(|row| {
            assert_eq!(
                row["boardPath"], board_path,
                "repoint changed board identity"
            );
            assert_eq!(row["archived"], false);
            row["rootPath"].as_str().unwrap().to_owned()
        })
        .collect::<Vec<_>>();
    moved_roots.sort();
    let mut expected_roots = vec![
        moved.canonicalize().unwrap().to_string_lossy().into_owned(),
        moved
            .join("lane")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    ];
    expected_roots.sort();
    assert_eq!(
        moved_roots, expected_roots,
        "repoint did not preserve exactly the intended canonical roots"
    );

    // The board is reachable from the inside again, and it is the same board --
    // repointing changes one path's spelling and nothing about identity.
    let rows = fixture.ok_json(&original, &["task", "list", "--json"]);
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["id"], "t-1");
    assert_eq!(
        fixture.ok_json(&lane, &["task", "list", "--json"])[0]["id"],
        "t-1",
        "the lane alias must resolve to the same board it always did"
    );
    assert!(
        fixture.ok_json(&original, &["doctor", "--json"])["unreachableRoots"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // A second repoint has nothing to do and says so rather than reporting a
    // successful no-op, which would read as a repair that happened.
    let again = fixture.run(&original, &["workspace", "repoint", "--json"]);
    assert!(!again.status.success());
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("nothing to repoint"),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );

    // A root that is simply gone has nowhere to be repointed to, and guessing
    // would be worse than the gap.
    fs::remove_file(&original).unwrap();
    fs::rename(&moved, fixture.root.join("gone")).unwrap();
    let vanished = fixture.run(&fixture.root, &["workspace", "repoint", "--json"]);
    assert!(!vanished.status.success(), "a deleted root was repointed");
    assert!(
        String::from_utf8_lossy(&vanished.stderr).contains("nowhere to repoint"),
        "{}",
        String::from_utf8_lossy(&vanished.stderr)
    );
}

#[test]
fn an_intentionally_retired_worktree_leaves_auditable_registry_history() {
    let fixture = Fixture::new("detach-worktree");
    let project = fixture.root.join("project");
    let retired = fixture.root.join("retired-lane");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&retired).unwrap();

    let registered = fixture.ok_json(&project, &["init", "--name", "DETACH", "--json"]);
    let project_root = registered["workspaceRoots"][0]
        .as_str()
        .expect("registered project root")
        .to_owned();
    let attached = fixture.ok_json(
        &retired,
        &["workspace", "attach", "--to", &project_root, "--json"],
    );
    let retired_root = attached["rootPath"]
        .as_str()
        .expect("attached root path")
        .to_owned();
    fixture.ok_json(
        &project,
        &["task", "add", "Kept work", "--id", "t-kept", "--json"],
    );
    fs::remove_dir_all(&retired).unwrap();

    let detached = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "detach",
            "--root",
            &retired_root,
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(detached["rootPath"], retired_root);
    assert_eq!(detached["archived"], true);
    assert_eq!(detached["archivedBy"], "geoyws");
    assert!(detached["archivedAt"].as_i64().is_some());
    let lifecycle = fixture.ok_json(
        &fixture.root,
        &[
            "events",
            "--registry",
            "--kind",
            "workspace_detached",
            "--json",
        ],
    );
    assert_eq!(lifecycle[0]["actor"], "geoyws");
    assert_eq!(lifecycle[0]["payload"]["rootPath"], retired_root);

    let active = fixture.ok_json(&fixture.root, &["workspace", "list", "--json"]);
    assert!(
        active
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["rootPath"] != retired_root)
    );
    let all = fixture.ok_json(&fixture.root, &["workspace", "list", "--all", "--json"]);
    assert!(
        all.as_array()
            .unwrap()
            .iter()
            .any(|row| { row["rootPath"] == retired_root && row["archived"] == true })
    );
    fs::create_dir_all(&retired).unwrap();
    let recreated_alias = fixture.run(&retired, &["task", "show", "t-kept", "--json"]);
    assert!(
        !recreated_alias.status.success(),
        "recreating a detached alias silently reattached it"
    );
    let recreated_stderr = String::from_utf8_lossy(&recreated_alias.stderr);
    assert!(
        recreated_stderr.contains("no Kanban project contains"),
        "{recreated_stderr}"
    );
    assert!(!String::from_utf8_lossy(&recreated_alias.stdout).contains("t-kept"));
    assert_eq!(
        fixture.ok_json(&project, &["task", "show", "t-kept", "--json"])["id"],
        "t-kept",
        "detaching an alias changed or orphaned its board"
    );
    assert!(
        fixture.ok_json(&project, &["doctor", "--json"])["unreachableRoots"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let detached_root = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "detach",
            "--root",
            &project_root,
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(detached_root["rootPath"], project_root);
    assert_eq!(detached_root["archived"], true);
    assert_eq!(detached_root["archivedBy"], "geoyws");
    let doctor = fixture.ok_json(&fixture.root, &["doctor", "--json"]);
    let detached_project = doctor["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "DETACH")
        .expect("the detached board must still be listed");
    assert_eq!(detached_project["rootless"], true);
    assert!(
        detached_project["workspaceRoots"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    for cwd in [&project, &retired] {
        let bare = fixture.run(cwd, &["task", "show", "t-kept", "--json"]);
        assert!(
            !bare.status.success(),
            "{} still resolved the board after its final root was detached",
            cwd.display()
        );
        let stderr = String::from_utf8_lossy(&bare.stderr);
        assert!(
            stderr.contains("no Kanban project contains"),
            "{}: {stderr}",
            cwd.display()
        );
        assert!(!String::from_utf8_lossy(&bare.stdout).contains("t-kept"));
    }
    assert_eq!(
        fixture.ok_json(
            &fixture.root,
            &["task", "show", "t-kept", "--project", "DETACH", "--json"]
        )["id"],
        "t-kept",
        "a board with no roots must remain reachable by name"
    );

    let twice = fixture.run(
        &fixture.root,
        &[
            "workspace",
            "detach",
            "--root",
            &retired_root,
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(!twice.status.success());
    assert!(String::from_utf8_lossy(&twice.stderr).contains("already detached"));

    // Disposable lane paths are reused after rebuilds. The retired row must
    // not occupy the active table's primary key forever.
    fs::create_dir_all(&retired).unwrap();
    let reattached = fixture.ok_json(
        &retired,
        &["workspace", "attach", "--to", "DETACH", "--json"],
    );
    assert_eq!(reattached["rootPath"], retired_root);
    assert_eq!(reattached["archived"], false);
    let history = fixture.ok_json(&fixture.root, &["workspace", "list", "--all", "--json"]);
    assert_eq!(
        history
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["rootPath"] == retired_root)
            .count(),
        2,
        "reattaching a reused path erased or replaced its retired history"
    );
}

#[test]
fn workspace_adopt_copies_a_source_board_from_another_registry_and_preserves_it() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt");
    let source_data = fixture.root.join("source-data");
    let source_cwd = fixture.root.join("source-cwd");
    fs::create_dir_all(&source_cwd).unwrap();
    fs::create_dir_all(&source_data).unwrap();

    let source_init = fixture
        .command_with_data_dir(&source_cwd, &source_data)
        .args(["init", "--name", "Alpha", "--json"])
        .output()
        .unwrap();
    assert!(
        source_init.status.success(),
        "source init failed: {}\nstderr: {}",
        String::from_utf8_lossy(&source_init.stdout),
        String::from_utf8_lossy(&source_init.stderr)
    );
    let source_init_json: Value = serde_json::from_slice(&source_init.stdout).unwrap();
    let source_board = PathBuf::from(source_init_json["boardPath"].as_str().unwrap());

    let source_task = fixture
        .command_with_data_dir(&source_cwd, &source_data)
        .args(["task", "add", "keep this state", "--id", "t-live", "--json"])
        .output()
        .unwrap();
    assert!(
        source_task.status.success(),
        "source task add failed: {}\nstderr: {}",
        String::from_utf8_lossy(&source_task.stdout),
        String::from_utf8_lossy(&source_task.stderr)
    );

    let source_board = source_board.canonicalize().unwrap();
    let source_bytes = fs::read(&source_board).unwrap();

    let second_root = fixture.root.join("adopted-sibling");
    let neighbor = fixture.root.join("registered-neighbor");
    fs::create_dir_all(&second_root).unwrap();
    fs::create_dir_all(&neighbor).unwrap();
    fixture.ok_json(&neighbor, &["init", "--name", "Neighbor", "--json"]);
    fixture.ok_json(
        &neighbor,
        &[
            "task",
            "add",
            "neighbor sentinel",
            "--id",
            "t-neighbor",
            "--json",
        ],
    );

    let adopt_root = fixture.root.join("adopted");
    fs::create_dir_all(&adopt_root).unwrap();
    let receipt = fixture.ok_json(
        &fixture.main,
        &[
            "workspace",
            "adopt",
            "--from-board",
            source_board.to_str().unwrap(),
            "--name",
            "Alpha",
            "--workspace",
            adopt_root.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );

    let adopt_root = adopt_root
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert_eq!(receipt["name"], "Alpha");
    assert_eq!(receipt["rootPath"], adopt_root.as_str());
    assert_eq!(
        receipt["sourceBoardPath"],
        source_board.to_string_lossy().as_ref()
    );
    assert_eq!(receipt["workspaceRoots"], json!([adopt_root.clone()]));
    let adopted_board = PathBuf::from(receipt["boardPath"].as_str().unwrap());
    assert_eq!(
        adopted_board.parent(),
        Some(fixture.data.join("boards").as_path()),
        "adopted destination escaped registry-owned boards storage"
    );
    assert_eq!(
        adopted_board.extension().and_then(|value| value.to_str()),
        Some("db")
    );
    assert!(
        adopted_board
            .file_stem()
            .and_then(|value| value.to_str())
            .is_some_and(|value| uuid::Uuid::parse_str(value).is_ok()),
        "adopted destination is not UUID-named: {}",
        adopted_board.display()
    );
    let adopted_bytes = fs::read(&adopted_board).unwrap();
    assert_eq!(
        receipt["sourceSha256"],
        format!("{:x}", Sha256::digest(&adopted_bytes)),
        "receipt hash did not describe the exact registered bytes"
    );
    assert_eq!(receipt["sourceBytes"], json!(adopted_bytes.len() as u64));

    let adopted_task = fixture.ok_json(
        Path::new(&adopt_root),
        &["task", "show", "t-live", "--json"],
    );
    assert_eq!(adopted_task["id"], "t-live");
    assert_eq!(adopted_task["title"], "keep this state");

    let attached = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "attach",
            "--to",
            "Alpha",
            "--workspace",
            second_root.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(
        attached["boardPath"],
        adopted_board.to_string_lossy().as_ref()
    );
    let second_root = second_root
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let from_second = fixture.ok_json(
        Path::new(&second_root),
        &["task", "show", "t-live", "--json"],
    );
    assert_eq!(from_second["id"], "t-live");
    assert_eq!(from_second["title"], "keep this state");
    let by_project = fixture.ok_json(
        &neighbor,
        &["task", "show", "t-live", "--project", "Alpha", "--json"],
    );
    assert_eq!(by_project["id"], "t-live");
    assert_eq!(by_project["title"], "keep this state");

    fixture.ok_json(
        Path::new(&second_root),
        &[
            "task",
            "add",
            "written from adopted sibling",
            "--id",
            "t-adopted-sibling",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(
            Path::new(&adopt_root),
            &["task", "show", "t-adopted-sibling", "--json"],
        )["title"],
        "written from adopted sibling"
    );
    let neighbor_tasks = fixture.ok_json(&neighbor, &["task", "list", "--json"]);
    assert_eq!(neighbor_tasks.as_array().unwrap().len(), 1);
    assert_eq!(neighbor_tasks[0]["id"], "t-neighbor");

    let listed = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    let mut alpha_roots = listed
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["name"] == "Alpha")
        .map(|row| {
            assert_eq!(
                row["boardPath"],
                adopted_board.to_string_lossy().as_ref(),
                "an adopted root points at a different board"
            );
            assert_eq!(row["archived"], false);
            row["rootPath"].as_str().unwrap().to_owned()
        })
        .collect::<Vec<_>>();
    alpha_roots.sort();
    let mut expected_roots = vec![adopt_root.clone(), second_root.clone()];
    expected_roots.sort();
    assert_eq!(alpha_roots, expected_roots);

    let events = fixture.ok_json(
        &fixture.main,
        &["events", "--registry", "--kind", "board_adopted", "--json"],
    );
    assert_eq!(events[0]["actor"], "geoyws");
    assert_eq!(events[0]["payload"]["name"], "Alpha");
    assert_eq!(events[0]["payload"]["rootPath"], adopt_root.as_str());
    assert_eq!(
        events[0]["payload"]["sourceBoardPath"],
        source_board.to_string_lossy().as_ref()
    );
    assert_eq!(
        events[0]["payload"]["sourceSha256"],
        receipt["sourceSha256"]
    );
    assert_eq!(events[0]["payload"]["sourceBytes"], receipt["sourceBytes"]);
    assert_eq!(fs::read(&source_board).unwrap(), source_bytes);
    assert!(!fixture.data.join(".workspace-adopt.json").exists());
}

#[test]
fn workspace_adopt_requires_an_explicit_actor_before_opening_registry_state() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-missing-actor");
    let source_data = fixture.root.join("source-data");
    let source_cwd = fixture.root.join("source-cwd");
    fs::create_dir_all(&source_cwd).unwrap();
    fs::create_dir_all(&source_data).unwrap();
    let source_init = fixture
        .command_with_data_dir(&source_cwd, &source_data)
        .args(["init", "--name", "Alpha", "--json"])
        .output()
        .unwrap();
    assert!(
        source_init.status.success(),
        "{}",
        String::from_utf8_lossy(&source_init.stderr)
    );
    let source_init_json: Value = serde_json::from_slice(&source_init.stdout).unwrap();
    let source_board = source_init_json["boardPath"].as_str().unwrap();

    let adopt = fixture.run(
        &fixture.main,
        &[
            "workspace",
            "adopt",
            "--from-board",
            source_board,
            "--name",
            "Alpha",
            "--rootless",
            "--json",
        ],
    );
    assert!(!adopt.status.success(), "adopt without --as succeeded");
    assert!(
        String::from_utf8_lossy(&adopt.stderr).contains("--as is required"),
        "{}",
        String::from_utf8_lossy(&adopt.stderr)
    );
    assert!(
        !fixture.data.join("registry.db").exists(),
        "missing-actor refusal opened or created the registry database"
    );
    assert!(
        !fixture.data.join("boards").exists(),
        "missing-actor refusal created registry-owned board storage"
    );
}

#[test]
fn workspace_adopt_missing_or_invalid_source_creates_no_live_registry_state() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    for (label, source) in [
        ("missing", None),
        ("invalid", Some(b"not a sqlite database".as_slice())),
        ("large", Some(vec![b'x'; 2 * 1024 * 1024].leak())),
    ] {
        let fixture = Fixture::new(&format!("workspace-adopt-preflight-{label}"));
        let source_path = fixture.root.join(format!("{label}.db"));
        if let Some(bytes) = source {
            fs::write(&source_path, bytes).unwrap();
        }
        let output = fixture.run(
            &fixture.main,
            &[
                "workspace",
                "adopt",
                "--from-board",
                source_path.to_str().unwrap(),
                "--name",
                "Alpha",
                "--rootless",
                "--as",
                "geoyws",
                "--json",
            ],
        );
        assert!(
            !output.status.success(),
            "{label} source unexpectedly adopted"
        );
        assert!(
            !fixture.data.exists(),
            "{label} source created live registry root before preflight refusal"
        );
    }
}

#[test]
fn workspace_adopt_helper_stays_hidden_from_help_schema_and_mcp() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-helper-hidden");
    let help = fixture.run(&fixture.main, &["--help"]);
    assert!(help.status.success());
    let help_text = String::from_utf8(help.stdout).unwrap();
    assert!(
        !help_text.contains("__workspace-adopt-helper"),
        "helper leaked into the public help surface: {help_text}"
    );

    let schema = fixture.ok_json(&fixture.main, &["schema", "--json"]);
    let operations = schema["operations"].as_array().unwrap();
    assert!(
        operations
            .iter()
            .all(|operation| operation["name"] != "__workspace_adopt_helper"),
        "helper leaked into the generated schema: {schema}"
    );

    let mut session = Session::start(
        Path::new(env!("CARGO_BIN_EXE_kanban")),
        &fixture.main,
        &fixture.data,
    );
    let _ = session.ask(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {} }
    }));
    let listed = session.ask(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert!(
        tools
            .iter()
            .all(|tool| tool["name"] != "__workspace_adopt_helper"),
        "helper leaked into the MCP tool list: {listed}"
    );
}

// `KANBAN_TEST_WORKSPACE_ADOPT_HOOK` is honoured only by the debug pause seam
// `workspace_adopt_test_hook` in rust/registry.rs (`#[cfg(debug_assertions)]`).
// A release binary completes adoption instead of pausing, so this test exists
// only in debug test binaries; see
// `release_binary_ignores_workspace_adopt_pause_hook` for the release-side proof.
#[cfg(debug_assertions)]
#[test]
fn workspace_adopt_rejects_a_concurrent_adopter_and_recovers_after_a_precommit_crash() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-crash-before-commit");
    let source = external_source_board(&fixture, "source", "Alpha");
    let marker = adoption_marker_path(&fixture);
    let mut first = fixture.command(&fixture.main);
    first
        .args([
            "workspace",
            "adopt",
            "--from-board",
            source.to_str().unwrap(),
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ])
        .env("KANBAN_TEST_WORKSPACE_ADOPT_HOOK", "after_marker");
    let mut first = first.spawn().unwrap();
    wait_for_path(&marker);
    let marker_json: Value = wait_for_json_file(&marker);
    let staging_dir = PathBuf::from(marker_json["stagingDir"].as_str().unwrap());

    let second = fixture.run(
        &fixture.main,
        &[
            "workspace",
            "adopt",
            "--from-board",
            source.to_str().unwrap(),
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !second.status.success(),
        "concurrent adopt unexpectedly won"
    );
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("another kanban process is using"),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );

    first.kill().unwrap();
    let first = first.wait_with_output().unwrap();
    assert!(
        !first.status.success(),
        "paused adopt unexpectedly completed"
    );

    let boards = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        boards
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "Alpha"),
        "crash recovery left a board registered unexpectedly: {boards}"
    );
    assert!(!marker.exists());
    assert!(!staging_dir.exists());
    assert_eq!(
        fs::read_dir(fixture.data.join("boards")).unwrap().count(),
        0
    );
}

#[test]
fn workspace_adopt_handles_helper_fd_collisions_and_cloexec() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-helper-fd-collision");
    let source = external_source_board(&fixture, "source", "Alpha");

    let mut command = fixture.command(&fixture.main);
    command.args([
        "workspace",
        "adopt",
        "--from-board",
        source.to_str().unwrap(),
        "--name",
        "Alpha",
        "--rootless",
        "--as",
        "geoyws",
        "--json",
    ]);
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(&mut command, occupy_helper_fds_in_child);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "fd collision and cloexec handoff failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// Debug-only: depends on the `after_marker` pause seam (see the note on
// `workspace_adopt_rejects_a_concurrent_adopter_and_recovers_after_a_precommit_crash`).
#[cfg(debug_assertions)]
#[test]
fn workspace_adopt_refuses_while_the_canonical_data_root_lock_is_held() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-lock-held");
    let source = external_source_board(&fixture, "source", "Alpha");
    let marker = adoption_marker_path(&fixture);

    let mut holder = fixture.command(&fixture.main);
    holder
        .args([
            "workspace",
            "adopt",
            "--from-board",
            source.to_str().unwrap(),
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ])
        .env("KANBAN_TEST_WORKSPACE_ADOPT_HOOK", "after_marker");
    let mut holder = holder.spawn().unwrap();

    wait_for_path(&marker);

    let blocked = fixture.run(
        &fixture.main,
        &[
            "workspace",
            "adopt",
            "--from-board",
            source.to_str().unwrap(),
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !blocked.status.success(),
        "second adopt unexpectedly succeeded while the canonical data-root lock was held"
    );
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    assert!(
        stderr.contains("another kanban process is using"),
        "canonical lock refusal missing from stderr: {stderr}"
    );
    assert!(
        !stderr.contains("database is locked"),
        "raw SQLite contention leaked through instead of the canonical lock refusal: {stderr}"
    );

    holder.kill().unwrap();
    let holder = holder.wait_with_output().unwrap();
    assert!(
        !holder.status.success(),
        "paused adopt unexpectedly completed while testing lock refusal"
    );
}

// Debug-only: depends on the `after_publish` pause seam (see the note on
// `workspace_adopt_rejects_a_concurrent_adopter_and_recovers_after_a_precommit_crash`).
#[cfg(debug_assertions)]
#[test]
fn workspace_adopt_recovers_after_publishing_before_commit() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-crash-after-rename");
    let source = external_source_board(&fixture, "source", "Alpha");
    let marker = adoption_marker_path(&fixture);
    let mut child = fixture.command(&fixture.main);
    child
        .args([
            "workspace",
            "adopt",
            "--from-board",
            source.to_str().unwrap(),
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ])
        .env("KANBAN_TEST_WORKSPACE_ADOPT_HOOK", "after_publish");
    let mut child = child.spawn().unwrap();
    // wait_for_path only proves the file exists; the writing process may still
    // be mid-write, and reading it then fails with "EOF while parsing a value"
    // (observed under a loaded gate on 2026-09-05). wait_for_json_file retries
    // until the content parses.
    let marker_json: Value = wait_for_json_file(&marker);
    let board_path = PathBuf::from(marker_json["boardPath"].as_str().unwrap());
    wait_for_path(&board_path);

    child.kill().unwrap();
    let child = child.wait_with_output().unwrap();
    assert!(
        !child.status.success(),
        "paused adopt unexpectedly completed"
    );

    let boards = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        boards
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "Alpha"),
        "recovery left a board registered unexpectedly: {boards}"
    );
    assert!(!marker.exists());
    assert!(!board_path.exists());
    assert_eq!(
        fs::read_dir(fixture.data.join("boards")).unwrap().count(),
        0
    );
}

// Release-side counterpart of the three `#[cfg(debug_assertions)]` adopt tests
// above. A release binary compiles `workspace_adopt_test_hook` down to `Ok(())`,
// so the pause env var must be inert: adoption completes, the marker is gone,
// and the board is registered. This is what a release run reports instead of
// silently running three fewer tests.
#[cfg(not(debug_assertions))]
#[test]
fn release_binary_ignores_workspace_adopt_pause_hook() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-release-hook-inert");
    let source = external_source_board(&fixture, "source", "Alpha");
    let marker = adoption_marker_path(&fixture);
    let mut command = fixture.command(&fixture.main);
    command
        .args([
            "workspace",
            "adopt",
            "--from-board",
            source.to_str().unwrap(),
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ])
        .env("KANBAN_TEST_WORKSPACE_ADOPT_HOOK", "after_marker");
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "release binary honoured the debug-only pause hook; the debug-only tests \
         workspace_adopt_rejects_a_concurrent_adopter_and_recovers_after_a_precommit_crash, \
         workspace_adopt_refuses_while_the_canonical_data_root_lock_is_held and \
         workspace_adopt_recovers_after_publishing_before_commit are intentionally \
         excluded from release test binaries (cfg(debug_assertions)); run `cargo test` \
         without --release to exercise the seam. stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !marker.exists(),
        "adoption marker lingered after a completed adopt"
    );
    let boards = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        boards
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "Alpha"),
        "release adopt did not register the board: {boards}"
    );
}

#[test]
fn workspace_adopt_rejects_boards_symlink_without_external_write_lock_or_event() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-boards-symlink");
    let source = external_source_board(&fixture, "source", "Alpha");
    let external = fixture.root.join("external");
    fs::create_dir_all(&fixture.data).unwrap();
    fs::create_dir(&external).unwrap();
    symlink(&external, fixture.data.join("boards")).unwrap();

    let output = fixture.run(
        &fixture.main,
        &[
            "workspace",
            "adopt",
            "--from-board",
            source.to_str().unwrap(),
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !output.status.success(),
        "boards symlink unexpectedly followed"
    );
    assert_eq!(
        fs::read_dir(&external).unwrap().count(),
        0,
        "external target was written"
    );
    assert!(
        !fixture.data.join(".lock").exists(),
        "symlink refusal created the live lock"
    );
    assert!(
        !fixture.data.join("registry.db").exists(),
        "symlink refusal created registry state"
    );
    assert!(
        fs::symlink_metadata(fixture.data.join("boards"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn workspace_adopt_compiled_process_refuses_source_symlink_traversal_fk_audit_and_newer_schema() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-fail-closed-sources");
    let valid = external_source_board(&fixture, "valid", "Alpha");
    let link = fixture.root.join("source-link.db");
    symlink(&valid, &link).unwrap();
    let traversal_dir = valid.parent().unwrap().join("traversal");
    fs::create_dir(&traversal_dir).unwrap();
    let traversal = traversal_dir.join("..").join(valid.file_name().unwrap());

    let fk = external_source_board(&fixture, "fk", "Alpha");
    let task = fixture.run(
        &fixture.main,
        &[
            "task",
            "add",
            "orphan",
            "--id",
            "t-orphan",
            "--db",
            fk.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        task.status.success(),
        "failed to seed FK fixture: {}",
        String::from_utf8_lossy(&task.stderr)
    );
    let fk_connection = Connection::open(&fk).unwrap();
    fk_connection
        .pragma_update(None, "foreign_keys", false)
        .unwrap();
    fk_connection
        .execute(
            "UPDATE tasks SET parent_id='t-missing' WHERE id='t-orphan'",
            [],
        )
        .unwrap();
    drop(fk_connection);

    let audit = external_source_board(&fixture, "audit", "Alpha");
    let audit_connection = Connection::open(&audit).unwrap();
    audit_connection
        .execute(
            "UPDATE events SET event_hash='bad' WHERE seq=(SELECT max(seq) FROM events)",
            [],
        )
        .unwrap();
    drop(audit_connection);

    let newer = external_source_board(&fixture, "newer", "Alpha");
    let newer_connection = Connection::open(&newer).unwrap();
    let source_version: i64 = newer_connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    newer_connection
        .pragma_update(None, "user_version", source_version + 1)
        .unwrap();
    drop(newer_connection);

    for (label, path, expected) in [
        ("symlink", link, "symlink"),
        ("traversal", traversal, "parent traversal"),
        ("foreign key", fk, "foreign key violations"),
        ("audit", audit, "invalid audit chain"),
        ("newer schema", newer, "newer than supported"),
    ] {
        let output = fixture.run(
            &fixture.main,
            &[
                "workspace",
                "adopt",
                "--from-board",
                path.to_str().unwrap(),
                "--name",
                "Alpha",
                "--rootless",
                "--as",
                "geoyws",
                "--json",
            ],
        );
        assert!(
            !output.status.success(),
            "{label} source unexpectedly adopted"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{label}: {stderr}");
        assert!(
            !fixture.data.exists(),
            "{label} refusal created live registry state"
        );
    }
}

#[test]
fn workspace_adopt_rejects_a_duplicate_active_board_name_across_processes() {
    let _adopt_test_guard = workspace_adopt_test_guard();
    let fixture = Fixture::new("workspace-adopt-duplicate");
    let source_data = fixture.root.join("source-data");
    let source_cwd = fixture.root.join("source-cwd");
    fs::create_dir_all(&source_cwd).unwrap();
    fs::create_dir_all(&source_data).unwrap();

    fixture.ok_json(&fixture.main, &["init", "--name", "Alpha", "--json"]);
    let source_init = fixture
        .command_with_data_dir(&source_cwd, &source_data)
        .args(["init", "--name", "Alpha", "--json"])
        .output()
        .unwrap();
    assert!(
        source_init.status.success(),
        "{}",
        String::from_utf8_lossy(&source_init.stderr)
    );
    let source_init_json: Value = serde_json::from_slice(&source_init.stdout).unwrap();
    let source_board = source_init_json["boardPath"].as_str().unwrap();

    let adopt = fixture.run(
        &fixture.main,
        &[
            "workspace",
            "adopt",
            "--from-board",
            source_board,
            "--name",
            "Alpha",
            "--rootless",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(!adopt.status.success(), "adopt unexpectedly succeeded");
    assert!(
        String::from_utf8_lossy(&adopt.stderr).contains("already named Alpha"),
        "{}",
        String::from_utf8_lossy(&adopt.stderr)
    );
}

#[test]
fn rust_sources_walk_nested_directories() {
    let root = std::env::temp_dir().join(format!("kanban-rust-source-walk-{}", std::process::id()));
    let nested = root.join("bin");
    fs::create_dir_all(&nested).unwrap();
    let nested_file = nested.join("tool.rs");
    fs::write(&nested_file, "fn main() {}\n").unwrap();

    let sources = rust_sources(&root);
    assert_eq!(sources, vec![nested_file]);

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn symbol_inventory_counts_associated_function_and_function_pointer_uses() {
    let associated_function = r#"
        struct Store;
        impl Store {
            fn resolve_attention_from_trusted_edge() {}
        }

        fn exercise() {
            let _ = Store::resolve_attention_from_trusted_edge;
        }
    "#;
    assert_eq!(
        symbol_references_in_source(associated_function, "resolve_attention_from_trusted_edge"),
        vec![SymbolReferenceKind::Definition, SymbolReferenceKind::Use]
    );

    let function_pointer = r#"
        fn resolve_attention_from_trusted_edge() {}

        fn exercise() {
            let _handler = resolve_attention_from_trusted_edge;
        }
    "#;
    assert_eq!(
        symbol_references_in_source(function_pointer, "resolve_attention_from_trusted_edge"),
        vec![SymbolReferenceKind::Definition, SymbolReferenceKind::Use]
    );
}

#[test]
fn symbol_inventory_skips_test_only_struct_type_and_static_items() {
    let source = r#"
        mod helper {
            pub struct resolve_attention_from_trusted_edge;
        }

        fn resolve_attention_from_trusted_edge() {}

        fn exercise() {
            let _ = resolve_attention_from_trusted_edge;
        }

        #[cfg(test)]
        struct TestOnlyStruct {
            field: helper::resolve_attention_from_trusted_edge,
        }

        #[cfg(all(test, feature = "inventory"))]
        type TestOnlyType = helper::resolve_attention_from_trusted_edge;

        #[cfg(any(
            all(test, feature = "inventory"),
            all(test, feature = "alternate")
        ))]
        static TEST_ONLY_STATIC: helper::resolve_attention_from_trusted_edge =
            helper::resolve_attention_from_trusted_edge;

        #[cfg(test)]
        struct TestOnlyStructField {
            field: helper::resolve_attention_from_trusted_edge,
        }

        #[cfg(test)]
        enum TestOnlyVariant {
            Hidden(helper::resolve_attention_from_trusted_edge),
        }

        #[cfg(all(feature = "alpha", not(feature = "beta")))]
        struct LiveFeatureField {
            field: helper::resolve_attention_from_trusted_edge,
        }
    "#;
    assert_eq!(
        symbol_references_in_source(source, "resolve_attention_from_trusted_edge"),
        vec![
            SymbolReferenceKind::Definition,
            SymbolReferenceKind::Use,
            SymbolReferenceKind::Use,
        ]
    );
}

#[test]
fn symbol_inventory_keeps_malformed_not_cfg_live() {
    let source = r#"
        fn resolve_attention_from_trusted_edge() {}

        #[cfg(not(test, feature = "inventory"))]
        struct MalformedNotField {
            field: resolve_attention_from_trusted_edge,
        }

        #[cfg(not())]
        enum EmptyNotVariant {
            Visible(resolve_attention_from_trusted_edge),
        }
    "#;
    assert_eq!(
        symbol_references_in_source(source, "resolve_attention_from_trusted_edge"),
        vec![
            SymbolReferenceKind::Definition,
            SymbolReferenceKind::Use,
            SymbolReferenceKind::Use,
        ]
    );
}

#[test]
fn symbol_inventory_treats_over_cap_cfg_as_live() {
    let cfg_atoms = (0..=MAX_CFG_ATOMS)
        .map(|index| format!("atom{index}"))
        .collect::<Vec<_>>();
    let source = format!(
        r#"
        fn resolve_attention_from_trusted_edge() {{}}

        #[cfg(all({cfg}))]
        struct OverCapLiveField {{
            field: resolve_attention_from_trusted_edge,
        }}
    "#,
        cfg = cfg_atoms.join(", ")
    );
    assert_eq!(
        symbol_references_in_source(&source, "resolve_attention_from_trusted_edge"),
        vec![SymbolReferenceKind::Definition, SymbolReferenceKind::Use]
    );
}

#[test]
fn symbol_inventory_catches_trait_foreign_and_macro_token_references() {
    let source = r#"
        trait Audit {
            fn resolve_attention_from_trusted_edge();
        }

        extern "C" {
            fn resolve_attention_from_trusted_edge();
        }

        macro_rules! capture {
            ($name:ident) => {
                $name
            };
        }

        fn resolve_attention_from_trusted_edge() {}

        fn exercise() {
            capture!(resolve_attention_from_trusted_edge);
        }
    "#;
    assert_eq!(
        symbol_references_in_source(source, "resolve_attention_from_trusted_edge"),
        vec![
            SymbolReferenceKind::Definition,
            SymbolReferenceKind::Definition,
            SymbolReferenceKind::Definition,
            SymbolReferenceKind::Use,
        ]
    );
}

#[test]
fn a_reader_that_hangs_up_ends_the_command_quietly() {
    // `kb task list --json | head` printed a Rust panic and a backtrace note
    // over the output it had just produced, and exited non-zero. Every other
    // Unix tool ends quietly when its reader leaves.
    //
    // The reader is closed here directly rather than through a shell pipeline,
    // because a pipeline's exit status is the LAST command's -- `| head` exits
    // 0 whatever happened upstream, so a shell test would have proved nothing
    // about kanban's own status. That is not hypothetical: the first version of
    // this test passed with the fix removed.
    let fixture = Fixture::new("broken-pipe");
    fixture.ok_json(&fixture.main, &["init", "--name", "PIPE", "--json"]);
    // Comfortably past a 64 KiB pipe buffer, so the writer is certain to still
    // be writing when the reader goes away. Bulk comes from long titles rather
    // than many rows: each row costs a process spawn, and 600 of them made this
    // the slowest test in the suite for no extra coverage.
    let padding = "x".repeat(2_000);
    for index in 0..60 {
        fixture.ok_json(
            &fixture.main,
            &["task", "add", &format!("Row {index} {padding}"), "--json"],
        );
    }

    let mut child = fixture
        .command(&fixture.main)
        .args(["task", "list", "--json"])
        .spawn()
        .unwrap();
    // Close the read end while the child is mid-write. This is exactly what
    // `head` does once it has what it wants.
    drop(child.stdout.take());
    let mut stderr = String::new();
    if let Some(mut handle) = child.stderr.take() {
        use std::io::Read as _;
        let _ = handle.read_to_string(&mut stderr);
    }
    let status = child.wait().unwrap();

    assert!(
        !stderr.contains("panicked"),
        "a closed pipe panicked: {stderr}"
    );
    assert!(
        !stderr.contains("Broken pipe"),
        "a closed pipe was reported as an error: {stderr}"
    );
    assert_eq!(
        status.code(),
        Some(0),
        "a closed pipe must exit 0, got {status:?} with stderr: {stderr}"
    );

    // A real error still fails, and still says so: the quiet exit is scoped to
    // the reader leaving, not to errors in general.
    let broken = fixture.run(&fixture.main, &["task", "show", "t-nope", "--json"]);
    assert!(!broken.status.success());
    assert!(
        String::from_utf8_lossy(&broken.stderr).contains("t-nope"),
        "{}",
        String::from_utf8_lossy(&broken.stderr)
    );
}

#[test]
fn a_sitrep_costs_one_command_and_retires_what_it_supersedes() {
    // A note needs a task. A checkpoint needs a task AND a lease. So an agent
    // working across several tasks, between them, or exploring before it has
    // claimed anything had nowhere to write down where things stand -- and it
    // went into a reply that scrolls away. This is the low-ceremony sibling of
    // a handoff: lane-keyed, no lease, no task required.
    let fixture = Fixture::new("sitrep");
    fixture.ok_json(&fixture.main, &["init", "--name", "SITREP", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["task", "add", "Some work", "--id", "t-1", "--json"],
    );

    // No lease anywhere in this call, and no task.
    let first = fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "Reading the queue code; nothing changed yet.",
            "--as",
            "claude@driver-2",
            "--lane",
            "driver-2",
            "--json",
        ],
    );
    assert_eq!(first["lane"], "driver-2");
    assert_eq!(first["author"], "claude@driver-2");
    assert_eq!(first["archived"], false);
    assert!(first["id"].as_str().unwrap().starts_with("sr-"));
    assert_eq!(first["taskID"], serde_json::Value::Null);

    // A task link is optional, and a task that does not exist is refused --
    // a sitrep pointing at nothing would read as context and carry none.
    let linked = fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "Retry path is the culprit.",
            "--as",
            "claude@driver-2",
            "--lane",
            "driver-2",
            "--task",
            "t-1",
            "--json",
        ],
    );
    assert_eq!(linked["taskID"], "t-1");
    let ghost = fixture.run(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "About nothing",
            "--as",
            "a",
            "--lane",
            "driver-2",
            "--task",
            "t-nope",
            "--json",
        ],
    );
    assert!(
        !ghost.status.success(),
        "a sitrep pointed at a missing task"
    );
    assert!(
        String::from_utf8_lossy(&ghost.stderr).contains("t-nope"),
        "{}",
        String::from_utf8_lossy(&ghost.stderr)
    );

    // Lanes do not bleed into each other: the question is "where does THIS
    // lane stand", and another driver's updates are not an answer to it.
    fixture.ok_json(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "Different lane entirely.",
            "--as",
            "claude@driver-1",
            "--lane",
            "driver-1",
            "--json",
        ],
    );
    let mine = fixture.ok_json(
        &fixture.main,
        &["sitrep", "list", "--lane", "driver-2", "--json"],
    );
    assert_eq!(mine.as_array().unwrap().len(), 2);
    // Newest first: the question this answers is "what is true now".
    assert_eq!(mine[0]["id"], linked["id"]);
    assert_eq!(mine[1]["id"], first["id"]);

    // Provenance is captured, not asked for -- an update saying "tests green"
    // without saying which checkout is a claim nobody can check. The fixture
    // cwd is a git repository, so the branch is captured rather than invented;
    // outside any checkout this write is refused, never stored blank.
    assert_eq!(mine[0]["branch"], "work");

    // Auto-archiving. Ten stay current per lane; the eleventh does not delete
    // the first, it retires it.
    for index in 0..12 {
        fixture.ok_json(
            &fixture.main,
            &[
                "sitrep",
                "post",
                &format!("Update number {index}"),
                "--as",
                "claude@driver-2",
                "--lane",
                "driver-2",
                "--json",
            ],
        );
    }
    let current = fixture.ok_json(
        &fixture.main,
        &[
            "sitrep", "list", "--lane", "driver-2", "--limit", "100", "--json",
        ],
    );
    assert_eq!(
        current.as_array().unwrap().len(),
        10,
        "the current view must stay bounded without anything running on a timer"
    );
    assert_eq!(current[0]["body"], "Update number 11");

    // Retired, not destroyed: everything is still there on request.
    let everything = fixture.ok_json(
        &fixture.main,
        &[
            "sitrep", "list", "--lane", "driver-2", "--all", "--limit", "100", "--json",
        ],
    );
    assert_eq!(everything.as_array().unwrap().len(), 14);
    assert!(
        everything
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == first["id"] && row["archived"] == true),
        "the oldest update must be archived and still readable"
    );

    // The other lane is untouched by all of that.
    assert_eq!(
        fixture
            .ok_json(
                &fixture.main,
                &["sitrep", "list", "--lane", "driver-1", "--all", "--json"]
            )
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Archiving lands in the trail, so a reader can see the current view was
    // bounded rather than wonder where the rest went.
    let events = fixture.ok_json(&fixture.main, &["events", "--limit", "200", "--json"]);
    let posted = events
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "sitrep_posted")
        .count();
    assert_eq!(posted, 15, "every post must be recorded");
    assert!(
        events
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["kind"] == "sitrep_posted"
                && event["payload"]["archived"].as_i64().unwrap_or(0) > 0),
        "the trail must record that an update was retired"
    );

    // A sitrep with no lane is refused rather than filed under a default: an
    // update nobody can address is one nobody will read.
    let laneless = fixture.run(
        &fixture.main,
        &[
            "sitrep",
            "post",
            "Where does this go?",
            "--as",
            "a",
            "--json",
        ],
    );
    assert!(!laneless.status.success(), "a laneless sitrep was accepted");
    assert!(
        String::from_utf8_lossy(&laneless.stderr).contains("lane"),
        "{}",
        String::from_utf8_lossy(&laneless.stderr)
    );

    // Empty prose is refused too. A sitrep that says nothing still
    // reads on the board as though the lane reported in.
    for empty in ["", "   "] {
        let blank = fixture.run(
            &fixture.main,
            &[
                "sitrep", "post", empty, "--as", "a", "--lane", "driver-2", "--json",
            ],
        );
        assert!(!blank.status.success(), "an empty sitrep was accepted");
    }

    // The short forms, because an alias nobody wrote down is one nobody can
    // use -- they resolve by exact match with no inference.
    fixture.ok_json(
        &fixture.main,
        &[
            "sr",
            "new",
            "Via the short forms.",
            "--as",
            "a",
            "--lane",
            "driver-3",
            "--json",
        ],
    );
    assert_eq!(
        fixture.ok_json(&fixture.main, &["sr", "ls", "--lane", "driver-3", "--json"])[0]["body"],
        "Via the short forms."
    );

    // The old concept fails closed. Silently accepting it would let an agent
    // believe it posted a sitrep when it did not.
    let old_name = fixture.run(&fixture.main, &["status", "list", "--json"]);
    assert!(
        !old_name.status.success(),
        "the old status command still resolves"
    );
    assert!(
        String::from_utf8_lossy(&old_name.stderr).contains("unknown command"),
        "{}",
        String::from_utf8_lossy(&old_name.stderr)
    );
}

#[test]
fn rules_have_an_ordered_audited_retire_only_lifecycle() {
    let fixture = Fixture::new("rules");
    fixture.ok_json(&fixture.main, &["init", "--name", "RULES", "--json"]);

    let first = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Never touch the PX database layer.",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(first["id"].as_str().unwrap().starts_with("r-"));
    assert_eq!(first["author"], "geoyws");
    assert_eq!(first["archived"], false);

    let body_file = fixture.root.join("rule.md");
    fs::write(
        &body_file,
        "crm-react only.\n\nPX repositories are read-only references.\n",
    )
    .unwrap();
    let second = fixture.ok_json(
        &fixture.main,
        &[
            "r",
            "new",
            "--body-file",
            body_file.to_str().unwrap(),
            "--as",
            "codex@driver",
            "--json",
        ],
    );
    assert_eq!(second["body"], fs::read_to_string(&body_file).unwrap());

    let listed = fixture.ok_json(&fixture.main, &["rule", "list", "--full", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 2);
    assert_eq!(listed[0]["id"], first["id"], "rules are not oldest first");
    assert_eq!(listed[1]["id"], second["id"]);

    let first_id = first["id"].as_str().unwrap();
    let revised = fixture.ok_json(
        &fixture.main,
        &[
            "r",
            "up",
            first_id,
            "--body",
            "Never alter the PX database layer.",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(revised["body"], "Never alter the PX database layer.");
    assert_eq!(
        fixture.ok_json(&fixture.main, &["r", "cat", first_id, "--json"])["body"],
        revised["body"]
    );
    let events = fixture.ok_json(
        &fixture.main,
        &["events", "--rule", first_id, "--limit", "100", "--json"],
    );
    let revision = events
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["kind"] == "rule_updated")
        .expect("rule revision left no trail");
    assert_eq!(
        revision["payload"]["previousBody"],
        "Never touch the PX database layer."
    );

    fixture.ok_json(
        &fixture.main,
        &["rule", "retire", first_id, "--as", "geoyws", "--json"],
    );
    let active = fixture.ok_json(&fixture.main, &["rule", "list", "--json"]);
    assert_eq!(active.as_array().unwrap().len(), 1);
    assert_eq!(active[0]["id"], second["id"]);
    let all = fixture.ok_json(
        &fixture.main,
        &["rule", "list", "--all", "--full", "--json"],
    );
    assert_eq!(all.as_array().unwrap().len(), 2);
    assert_eq!(all[0]["id"], first["id"]);
    assert_eq!(all[0]["archived"], true);

    let deletion_alias = fixture.run(&fixture.main, &["rule", "rm", first_id, "--json"]);
    assert!(
        !deletion_alias.status.success(),
        "an rm alias implied a destructive operation that does not exist"
    );

    for args in [
        vec!["rule", "add", "", "--as", "geoyws", "--json"],
        vec!["rule", "add", "valid", "--as", "", "--json"],
    ] {
        let refused = fixture.run(&fixture.main, &args);
        assert!(
            !refused.status.success(),
            "an empty rule field was accepted"
        );
    }
    let two_bodies = fixture.run(
        &fixture.main,
        &[
            "rule",
            "add",
            "positional",
            "--body",
            "flagged",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !two_bodies.status.success(),
        "two rule bodies were silently ranked"
    );
}

#[test]
fn active_rule_summaries_frame_context_and_new_claims_without_leaking_into_get_claim() {
    let fixture = Fixture::new("rule-context");
    fixture.ok_json(&fixture.main, &["init", "--name", "RULE-CONTEXT", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Rule-framed work",
            "--id",
            "t-rules",
            "--json",
        ],
    );
    let short = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Never touch the PX database layer.",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let long_body = format!(
        "crm-react only; PX repos are read-only references.\n\n{}",
        "supporting detail ".repeat(160)
    );
    let long = fixture.ok_json(
        &fixture.main,
        &[
            "rule", "add", "--body", &long_body, "--as", "geoyws", "--json",
        ],
    );

    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-rules", "--as", "worker", "--json"],
    );
    assert_eq!(claim["taskID"], "t-rules", "claim wire shape was nested");
    assert_eq!(claim["rules"].as_array().unwrap().len(), 2);
    assert_eq!(claim["rules"][0]["id"], short["id"]);
    assert_eq!(claim["rules"][1]["id"], long["id"]);
    assert_eq!(claim["rules"][0]["tags"], json!(["ALL"]));
    assert_eq!(claim["rules"][0]["hasMore"], false);
    assert_eq!(claim["rules"][1]["hasMore"], true);
    assert!(claim["rules"][1]["bytes"].as_u64().unwrap() > 2_000);
    assert!(
        claim.get("claim").is_none(),
        "claim receipt stopped being flat"
    );

    let shown = fixture.ok_json(&fixture.main, &["task", "show", "t-rules", "--json"]);
    assert!(
        shown["claim"].get("rules").is_none(),
        "get_claim serialized an empty rules field as if it had checked the board"
    );

    let packet = fixture.ok_json(&fixture.main, &["context", "t-rules", "--json"]);
    assert_eq!(packet["rules"], claim["rules"]);
    let rendered = fixture.run(
        &fixture.main,
        &["context", "t-rules", "--max-chars", "1000"],
    );
    assert!(rendered.status.success());
    let rendered = String::from_utf8(rendered.stdout).unwrap();
    assert!(
        rendered.contains("## Rules (2 applicable; bodies lazy)"),
        "{rendered}"
    );
    assert!(
        rendered.contains("Never touch the PX database layer."),
        "{rendered}"
    );
    assert!(
        rendered.contains("crm-react only; PX repos are read-only references."),
        "{rendered}"
    );
    assert!(rendered.contains("KB · kb r cat"), "{rendered}");
    assert!(
        rendered.contains(long["id"].as_str().unwrap()),
        "{rendered}"
    );
    assert!(
        !rendered.contains("supporting detail supporting detail"),
        "context carried a full long rule instead of its table of contents"
    );
}

#[test]
fn compiled_binary_matches_task_scoped_rules_across_boards() {
    let fixture = Fixture::new("rule-task-tags");
    let second = fixture.root.join("second");
    let third = fixture.root.join("third");
    fs::create_dir_all(&second).unwrap();
    fs::create_dir_all(&third).unwrap();
    fixture.ok_json(
        &fixture.main,
        &["init", "--name", "RULE-TAGS-ONE", "--json"],
    );
    fixture.ok_json(&second, &["init", "--name", "RULE-TAGS-TWO", "--json"]);
    fixture.ok_json(&third, &["init", "--name", "RULE-TAGS-THREE", "--json"]);
    for tag in ["geoyws/infra", "geoyws/queuer"] {
        fixture.ok_json(
            &fixture.main,
            &["tag", "add", tag, "--as", "geoyws", "--json"],
        );
    }
    for cwd in [&fixture.main, &second, &third] {
        fixture.ok_json(
            cwd,
            &["tag", "add", "geoyws/shared", "--as", "geoyws", "--json"],
        );
    }

    let scoped = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Only tagged task context.",
            "--as",
            "geoyws",
            "--tag",
            "geoyws/queuer",
            "--tag",
            "geoyws/infra",
            "--json",
        ],
    );
    assert_eq!(
        scoped["tags"],
        json!(["ALL", "geoyws/infra", "geoyws/queuer"])
    );

    let global = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Cross-board selector.",
            "--as",
            "geoyws",
            "--board",
            "RULE-TAGS-ONE",
            "--board",
            "RULE-TAGS-TWO",
            "--tag",
            "geoyws/shared",
            "--json",
        ],
    );
    assert_eq!(
        global["tags"],
        json!(["ONLY:RULE-TAGS-ONE", "ONLY:RULE-TAGS-TWO", "geoyws/shared"])
    );

    let args = vec![
        "rule",
        "add",
        "Unknown project tag.",
        "--as",
        "geoyws",
        "--tag",
        "missing",
        "--json",
    ];
    let refused = fixture.run(&fixture.main, &args);
    assert!(!refused.status.success(), "accepted {args:?}");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("missing"),
        "stderr: {}",
        String::from_utf8_lossy(&refused.stderr)
    );

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Tagged work",
            "--id",
            "t-tagged",
            "--tag",
            "geoyws/queuer",
            "--json",
        ],
    );
    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-tagged", "--as", "worker", "--json"],
    );
    assert_eq!(claim["rules"].as_array().unwrap().len(), 1);
    assert_eq!(claim["rules"][0]["id"], scoped["id"]);
    assert_eq!(
        fixture.ok_json(&fixture.main, &["context", "t-tagged", "--json"])["rules"],
        claim["rules"]
    );

    for (cwd, id, should_match_global) in [
        (&fixture.main, "t-shared-one", true),
        (&second, "t-shared-two", true),
        (&third, "t-shared-three", false),
    ] {
        fixture.ok_json(
            cwd,
            &[
                "task",
                "add",
                id,
                "--id",
                id,
                "--tag",
                "geoyws/shared",
                "--json",
            ],
        );
        let tagged_claim = fixture.ok_json(cwd, &["claim", id, "--as", "worker", "--json"]);
        let has_global = tagged_claim["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|rule| rule["id"] == global["id"]);
        assert_eq!(has_global, should_match_global, "claim: {tagged_claim}");
        assert_eq!(
            fixture.ok_json(cwd, &["context", id, "--json"])["rules"],
            tagged_claim["rules"]
        );
    }

    fixture.ok_json(
        &second,
        &["task", "add", "t-untagged", "--id", "t-untagged", "--json"],
    );
    let untagged = fixture.ok_json(
        &second,
        &["claim", "t-untagged", "--as", "worker", "--json"],
    );
    assert!(untagged["rules"].as_array().unwrap().is_empty());

    let session = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "--as",
            "outgoing",
            "--to",
            "incoming",
            "--reason",
            "session_end",
            "--summary",
            "Session boundary",
            "--intent",
            "Continue safely",
            "--next-action",
            "Read the board",
            "--json",
        ],
    );
    let accepted = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "accept",
            session["id"].as_str().unwrap(),
            "--as",
            "incoming",
            "--json",
        ],
    );
    assert!(accepted["rules"].as_array().unwrap().is_empty());

    fixture.ok_json(
        &fixture.main,
        &[
            "task",
            "add",
            "Handoff-tagged work",
            "--id",
            "t-handoff-tagged",
            "--tag",
            "geoyws/infra",
            "--json",
        ],
    );
    let outgoing = fixture.ok_json(
        &fixture.main,
        &["claim", "t-handoff-tagged", "--as", "outgoing", "--json"],
    );
    let task_handoff = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "t-handoff-tagged",
            "--lease",
            outgoing["leaseToken"].as_str().unwrap(),
            "--as",
            "outgoing",
            "--to",
            "incoming",
            "--summary",
            "Transfer tagged work",
            "--intent",
            "Continue tagged work",
            "--next-action",
            "Accept the handoff",
            "--json",
        ],
    );
    let task_accepted = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "accept",
            task_handoff["id"].as_str().unwrap(),
            "--as",
            "incoming",
            "--json",
        ],
    );
    assert_eq!(task_accepted["rules"].as_array().unwrap().len(), 1);
    assert_eq!(task_accepted["rules"][0]["id"], scoped["id"]);

    let remove_in_use = fixture.run(
        &fixture.main,
        &[
            "tag",
            "remove",
            "geoyws/infra",
            "--as",
            "geoyws",
            "--force",
            "--json",
        ],
    );
    assert!(
        !remove_in_use.status.success(),
        "force removed a tag that still scopes an active rule"
    );
    assert!(
        String::from_utf8_lossy(&remove_in_use.stderr).contains("silently widen"),
        "stderr: {}",
        String::from_utf8_lossy(&remove_in_use.stderr)
    );

    let updated = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "update",
            scoped["id"].as_str().unwrap(),
            "--as",
            "geoyws",
            "--clear-tags",
            "--json",
        ],
    );
    assert_eq!(updated["body"], "Only tagged task context.");
    assert_eq!(updated["tags"], json!(["ALL"]));
    let events = fixture.ok_json(
        &fixture.main,
        &["events", "--rule", scoped["id"].as_str().unwrap(), "--json"],
    );
    assert_eq!(
        events[0]["payload"]["previousTags"],
        json!(["ALL", "geoyws/infra", "geoyws/queuer"])
    );
}

#[test]
fn registry_rejects_duplicate_names_before_creating_a_new_board() {
    let fixture = Fixture::new("board-name-uniqueness");

    let omega_root = fixture.root.join("omega-rooted");
    fs::create_dir_all(&omega_root).unwrap();
    fixture.ok_json(&omega_root, &["init", "--name", "OMEGA", "--json"]);
    let omega_root_str = omega_root
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let omega_rootless = fixture.root.join("omega-rootless");
    fs::create_dir_all(&omega_rootless).unwrap();
    let rooted_then_rootless = fixture.run(
        &omega_rootless,
        &["init", "--name", "OMEGA", "--rootless", "--json"],
    );
    assert!(
        !rooted_then_rootless.status.success(),
        "a rootless OMEGA board was created beside the rooted one"
    );
    let rooted_then_rootless_message =
        String::from_utf8_lossy(&rooted_then_rootless.stderr).into_owned();
    assert!(
        rooted_then_rootless_message.contains("a Kanban board is already named OMEGA"),
        "{rooted_then_rootless_message}"
    );

    let detached = fixture.ok_json(
        &fixture.root,
        &[
            "workspace",
            "detach",
            "--root",
            omega_root_str.as_str(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(detached["rootPath"], omega_root_str);
    let omega_rows = fixture.ok_json(&fixture.root, &["workspace", "list", "--json"]);
    assert!(
        omega_rows
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "OMEGA" && row["rootless"] == true),
        "detaching the last root should keep the unique board reachable by name: {omega_rows}"
    );

    let sigma_rootless = fixture.root.join("sigma-rootless");
    fs::create_dir_all(&sigma_rootless).unwrap();
    fixture.ok_json(
        &sigma_rootless,
        &["init", "--name", "SIGMA", "--rootless", "--json"],
    );
    let sigma_rooted = fixture.root.join("sigma-rooted");
    fs::create_dir_all(&sigma_rooted).unwrap();
    let rootless_then_rooted = fixture.run(&sigma_rooted, &["init", "--name", "SIGMA", "--json"]);
    assert!(
        !rootless_then_rooted.status.success(),
        "a rooted SIGMA board was created beside the rootless one"
    );
    let rootless_then_rooted_message =
        String::from_utf8_lossy(&rootless_then_rooted.stderr).into_owned();
    assert!(
        rootless_then_rooted_message.contains("a Kanban board is already named SIGMA"),
        "{rootless_then_rooted_message}"
    );
}

#[test]
fn registry_v3_rules_migrate_to_the_unified_all_tag() {
    let fixture = Fixture::new("global-rule-target-migration");
    fs::create_dir_all(&fixture.data).unwrap();
    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    registry
        .execute_batch(
            r#"
            CREATE TABLE workspaces (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL UNIQUE,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE workspace_aliases (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX idx_workspace_aliases_board ON workspace_aliases(board_path);
            CREATE TABLE global_rules (
             id TEXT PRIMARY KEY NOT NULL,body TEXT NOT NULL,author TEXT NOT NULL,
             archived INTEGER NOT NULL DEFAULT 0,created_at INTEGER NOT NULL,updated_at INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX idx_global_rules_active ON global_rules(archived,created_at);
            CREATE TABLE global_rule_events (
             seq INTEGER PRIMARY KEY AUTOINCREMENT,rule_id TEXT NOT NULL,kind TEXT NOT NULL,
             actor TEXT NOT NULL,payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),
             created_at INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX idx_global_rule_events_rule_seq ON global_rule_events(rule_id,seq);
            INSERT INTO global_rules VALUES('g-old','Existing global rule.','geoyws',0,1,1);
            PRAGMA user_version=3;
            "#,
        )
        .unwrap();
    drop(registry);

    let rules = fixture.ok_json(&fixture.main, &["rule", "list", "--full", "--json"]);
    assert_eq!(rules[0]["tags"], json!(["ALL"]));
    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    assert_eq!(
        registry
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        15
    );
}

#[test]
fn registry_v10_migration_records_discarded_alias_names() {
    let fixture = Fixture::new("rootless-name-drift");
    fs::create_dir_all(&fixture.data).unwrap();
    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    registry
        .execute_batch(
            r#"
            CREATE TABLE registry_meta (key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL) STRICT;
            CREATE TABLE workspaces (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL UNIQUE,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE workspace_aliases (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX idx_workspace_aliases_board ON workspace_aliases(board_path);
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
            CREATE TABLE rule_events (
             seq INTEGER PRIMARY KEY AUTOINCREMENT,
             rule_id TEXT NOT NULL,
             kind TEXT NOT NULL,
             actor TEXT NOT NULL,
             payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),
             created_at INTEGER NOT NULL,
             prev_hash TEXT,
             event_hash TEXT
            ) STRICT;
            CREATE INDEX idx_registry_rule_events_rule_seq ON rule_events(rule_id,seq);
            INSERT INTO workspaces VALUES('/workspace/alpha','Alpha','/boards/alpha.db',10,20);
            INSERT INTO workspace_aliases VALUES('/workspace/alpha/alias','Beta','/boards/alpha.db',30,40);
            PRAGMA user_version=10;
            "#,
        )
        .unwrap();
    drop(registry);

    fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);

    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    assert_eq!(
        registry
            .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        15
    );
    let (kind, actor, payload): (String, String, String) = registry
        .query_row(
            "SELECT kind,actor,payload FROM rule_events WHERE kind='workspace_alias_name_discarded'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(kind, "workspace_alias_name_discarded");
    assert_eq!(actor, "system@migration");
    let payload: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["discardedName"], "Beta");
    assert_eq!(payload["boardName"], "Alpha");
    assert_eq!(payload["rootPath"], "/workspace/alpha/alias");
    assert_eq!(payload["boardPath"], "/boards/alpha.db");
}

#[test]
fn registry_v10_migration_records_discarded_alias_names_once_across_competing_processes() {
    let fixture = Fixture::new("rootless-name-drift-race");
    fs::create_dir_all(&fixture.data).unwrap();
    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    registry
        .execute_batch(
            r#"
            CREATE TABLE registry_meta (key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL) STRICT;
            CREATE TABLE workspaces (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL UNIQUE,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE workspace_aliases (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX idx_workspace_aliases_board ON workspace_aliases(board_path);
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
            CREATE TABLE rule_events (
             seq INTEGER PRIMARY KEY AUTOINCREMENT,
             rule_id TEXT NOT NULL,
             kind TEXT NOT NULL,
             actor TEXT NOT NULL,
             payload TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(payload)),
             created_at INTEGER NOT NULL,
             prev_hash TEXT,
             event_hash TEXT
            ) STRICT;
            CREATE INDEX idx_registry_rule_events_rule_seq ON rule_events(rule_id,seq);
            INSERT INTO workspaces VALUES('/workspace/alpha','Alpha','/boards/alpha.db',10,20);
            INSERT INTO workspace_aliases VALUES('/workspace/alpha/alias','Beta','/boards/alpha.db',30,40);
            PRAGMA user_version=10;
            "#,
        )
        .unwrap();
    drop(registry);

    fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    registry
        .execute_batch(
            r#"
            DELETE FROM registry_meta
            WHERE key='workspace_root_model_v11_name_drift_audited';
            DELETE FROM rule_events
            WHERE kind='workspace_alias_name_discarded';
            "#,
        )
        .unwrap();
    drop(registry);

    // The race has to be between two WRITABLE registry opens, and `workspace
    // list` is no longer one: it reads the registry read-only once the schema
    // is current, so that a resuming driver can list projects from a
    // `registry.db` it cannot write. `rule add` is a registry write by
    // definition, so it is the durable vehicle for this race.
    let outputs = std::thread::scope(|scope| {
        let start = Arc::new(Barrier::new(3));
        let fixture = &fixture;

        let first_start = Arc::clone(&start);
        let first = scope.spawn(move || {
            first_start.wait();
            fixture.run(
                &fixture.main,
                &[
                    "rule",
                    "add",
                    "First racer body.",
                    "--as",
                    "geoyws",
                    "--json",
                ],
            )
        });

        let second_start = Arc::clone(&start);
        let second = scope.spawn(move || {
            second_start.wait();
            fixture.run(
                &fixture.main,
                &[
                    "rule",
                    "add",
                    "Second racer body.",
                    "--as",
                    "geoyws",
                    "--json",
                ],
            )
        });

        start.wait();
        [first.join().unwrap(), second.join().unwrap()]
    });

    assert!(
        outputs.iter().all(|output| output.status.success()),
        "both concurrent opens must succeed: first={:?}\nsecond={:?}",
        outputs[0],
        outputs[1]
    );

    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    let marker_count = registry
        .query_row(
            "SELECT count(*) FROM registry_meta WHERE key='workspace_root_model_v11_name_drift_audited'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(marker_count, 1, "the drift marker must be written once");
    let event_count = registry
        .query_row(
            "SELECT count(*) FROM rule_events WHERE kind='workspace_alias_name_discarded'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(event_count, 1, "the drift event must be written once");
}

#[test]
fn registry_rejects_last_root_detach_for_legacy_duplicate_names() {
    let fixture = Fixture::new("rootless-duplicate-name-detach");
    fs::create_dir_all(&fixture.data).unwrap();

    let omega_left = fixture.root.join("omega-left");
    let omega_right = fixture.root.join("omega-right");
    let sigma_root = fixture.root.join("sigma-root");
    for dir in [&omega_left, &omega_right, &sigma_root] {
        fs::create_dir_all(dir).unwrap();
    }
    let omega_left_root = omega_left
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let omega_right_root = omega_right
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let sigma_root_path = sigma_root
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    registry
        .execute_batch(
            format!(
                r#"
            CREATE TABLE registry_meta (key TEXT PRIMARY KEY NOT NULL,value TEXT NOT NULL) STRICT;
            CREATE TABLE workspaces (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL UNIQUE,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE workspace_aliases (
             root_path TEXT PRIMARY KEY NOT NULL,name TEXT NOT NULL,board_path TEXT NOT NULL,
             created_at INTEGER NOT NULL,last_used_at INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX idx_workspace_aliases_board ON workspace_aliases(board_path);
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
            CREATE TABLE rule_events (
             seq INTEGER PRIMARY KEY AUTOINCREMENT,
             rule_id TEXT NOT NULL,
             kind TEXT NOT NULL,
             actor TEXT NOT NULL,
             payload TEXT NOT NULL DEFAULT '{{}}' CHECK(json_valid(payload)),
             created_at INTEGER NOT NULL,
             prev_hash TEXT,
             event_hash TEXT
            ) STRICT;
            CREATE INDEX idx_registry_rule_events_rule_seq ON rule_events(rule_id,seq);
            INSERT INTO workspaces VALUES('{omega_left_root}','OMEGA','/boards/omega-left.db',10,20);
            INSERT INTO workspaces VALUES('{omega_right_root}','OMEGA','/boards/omega-right.db',30,40);
            INSERT INTO workspaces VALUES('{sigma_root_path}','SIGMA','/boards/sigma.db',50,60);
            INSERT INTO boards VALUES('/boards/omega-left.db','OMEGA',10,20);
            INSERT INTO boards VALUES('/boards/omega-right.db','OMEGA',30,40);
            INSERT INTO boards VALUES('/boards/sigma.db','SIGMA',50,60);
            INSERT INTO workspace_roots VALUES('{omega_left_root}','/boards/omega-left.db',10,20);
            INSERT INTO workspace_roots VALUES('{omega_right_root}','/boards/omega-right.db',30,40);
            INSERT INTO workspace_roots VALUES('{sigma_root_path}','/boards/sigma.db',50,60);
            PRAGMA user_version=11;
            "#
            )
            .as_str(),
        )
        .unwrap();
    drop(registry);

    let before = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert_eq!(before.as_array().unwrap().len(), 3);

    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    assert_eq!(
        registry
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='workspace_alias_history'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1,
        "the archive table is missing from the hybrid fixture"
    );
    assert_eq!(
        registry
            .query_row(
                "SELECT count(*) FROM workspace_roots WHERE root_path=?",
                [sigma_root_path.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1,
        "the migrated unique root was not registered"
    );

    let refused = fixture.run(
        &fixture.main,
        &[
            "workspace",
            "detach",
            "--root",
            &omega_left_root,
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "detaching a legacy duplicate-name root was accepted"
    );
    let refused_message = String::from_utf8_lossy(&refused.stderr).into_owned();
    assert!(
        refused_message.contains("would create a second active board named OMEGA"),
        "{refused_message}"
    );

    let after = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert_eq!(after, before, "the rejected detach changed registry state");

    let detached = fixture.ok_json(
        &fixture.main,
        &[
            "workspace",
            "detach",
            "--root",
            &sigma_root_path,
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(detached["rootPath"], sigma_root_path);
    assert_eq!(detached["archived"], true);

    let sigma_rows = fixture.ok_json(&fixture.main, &["workspace", "list", "--json"]);
    assert!(
        sigma_rows
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "SIGMA" && row["rootless"] == true),
        "detaching the unique board's last root should leave it rootless: {sigma_rows}"
    );
}

#[test]
fn compiled_binary_consolidates_board_rules_once_and_retires_the_sources() {
    let fixture = Fixture::new("unified-rule-consolidation");
    let second = fixture.root.join("second");
    fs::create_dir_all(&second).unwrap();
    fixture.ok_json(&fixture.main, &["init", "--name", "ONE", "--json"]);
    fixture.ok_json(&second, &["init", "--name", "TWO", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &["tag", "add", "geoyws/infra", "--as", "geoyws", "--json"],
    );
    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    let board_path = |name: &str| {
        registry
            .query_row(
                "SELECT board_path FROM boards WHERE name=?",
                [name],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    };
    let one_path = board_path("ONE");
    let two_path = board_path("TWO");
    registry
        .execute(
            "INSERT INTO global_rules(id,body,author,archived,created_at,updated_at,board_tags,task_tags) \
             VALUES('g-late','Late rolling-upgrade rule.','geoyws',0,3,3,'[\"ALL\"]','[\"geoyws/infra\"]')",
            [],
        )
        .unwrap();
    registry
        .execute(
            "INSERT INTO global_rule_events(rule_id,kind,actor,payload,created_at) \
             VALUES('g-late','global_rule_added','geoyws','{\"ruleID\":\"g-late\"}',3)",
            [],
        )
        .unwrap();
    drop(registry);
    Connection::open(&one_path)
        .unwrap()
        .execute(
            "INSERT INTO rules(id,body,author,archived,created_at,updated_at,task_tags) VALUES('r-legacy-one','ONE infrastructure rule.','geoyws',0,1,1,'[\"geoyws/infra\"]')",
            [],
        )
        .unwrap();
    Connection::open(&two_path)
        .unwrap()
        .execute(
            "INSERT INTO rules(id,body,author,archived,created_at,updated_at,task_tags) VALUES('r-legacy-two','TWO board rule.','geoyws',0,2,2,'[]')",
            [],
        )
        .unwrap();
    let one = json!({"id":"r-legacy-one"});
    let two = json!({"id":"r-legacy-two"});

    let first = fixture.ok_json(
        &fixture.root,
        &["rule", "consolidate", "--as", "geoyws", "--json"],
    );
    assert_eq!(first["boardsMigrated"], 2);
    assert_eq!(first["rulesImported"], 2);
    assert_eq!(first["sourceRulesRetired"], 2);
    assert_eq!(first["legacyRegistryMigrated"], true);
    assert_eq!(first["legacyRulesImported"], 1);
    assert_eq!(first["legacyEventsImported"], 1);
    assert_eq!(first["legacyRulesRetired"], 1);

    let registry = Connection::open(fixture.data.join("registry.db")).unwrap();
    let imported_one: (String, String, String) = registry
        .query_row(
            "SELECT tags,source_board,source_rule_id FROM rules WHERE source_board='ONE'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&imported_one.0).unwrap(),
        ["ONLY:ONE", "geoyws/infra"]
    );
    assert_eq!(imported_one.1, "ONE");
    assert_eq!(imported_one.2, one["id"]);
    let imported_two: String = registry
        .query_row(
            "SELECT tags FROM rules WHERE source_board='TWO' AND source_rule_id=?",
            [two["id"].as_str().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&imported_two).unwrap(),
        ["ONLY:TWO"]
    );
    let late_tags: String = registry
        .query_row("SELECT tags FROM rules WHERE id='g-late'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<String>>(&late_tags).unwrap(),
        ["ALL", "geoyws/infra"]
    );
    drop(registry);

    for path in [&one_path, &two_path] {
        let board = Connection::open(path).unwrap();
        let active: i64 = board
            .query_row("SELECT count(*) FROM rules WHERE archived=0", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(active, 0, "legacy source remained active in {path}");
    }

    let second_run = fixture.ok_json(
        &fixture.root,
        &["rule", "consolidate", "--as", "geoyws", "--json"],
    );
    assert_eq!(second_run["boardsAlreadyMigrated"], 2);
    assert_eq!(second_run["legacyRegistryAlreadyMigrated"], true);
    assert_eq!(second_run["rulesImported"], 0);
    assert_eq!(second_run["sourceRulesRetired"], 0);
}

#[test]
fn unified_rules_are_stored_once_and_frame_every_board_claim_and_context() {
    let fixture = Fixture::new("global-rules");
    let second = fixture.root.join("second");
    fs::create_dir_all(&second).unwrap();
    fixture.ok_json(&fixture.main, &["init", "--name", "ONE", "--json"]);
    fixture.ok_json(&second, &["init", "--name", "TWO", "--json"]);

    let global = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Never store credentials in Kanban.\n\nKeep secrets in git-crypt.",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(global["id"].as_str().unwrap().starts_with("r-"));
    assert_eq!(global["tags"], json!(["ALL"]));
    let global_id = global["id"].as_str().unwrap();

    for (cwd, board, task, local) in [
        (&fixture.main, "ONE", "t-one", "ONE uses Rust."),
        (&second, "TWO", "t-two", "TWO uses SQLite."),
    ] {
        fixture.ok_json(cwd, &["task", "add", task, "--id", task, "--json"]);
        fixture.ok_json(
            cwd,
            &[
                "rule", "add", local, "--board", board, "--as", "geoyws", "--json",
            ],
        );
        let claim = fixture.ok_json(cwd, &["claim", task, "--as", "worker", "--json"]);
        let rules = claim["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["id"], global["id"]);
        assert_eq!(rules[0]["tags"], json!(["ALL"]));
        assert_eq!(rules[1]["tags"], json!([format!("ONLY:{board}")]));
        let context = fixture.ok_json(cwd, &["context", task, "--json"]);
        assert_eq!(context["rules"], claim["rules"]);
    }

    let registry_path = fixture.data.join("registry.db");
    let registry = Connection::open(&registry_path).unwrap();
    let board_paths = registry
        .prepare("SELECT board_path FROM boards ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    for board_path in board_paths {
        let board = Connection::open(board_path).unwrap();
        let copied: i64 = board
            .query_row(
                "SELECT count(*) FROM rules WHERE id=?",
                [global_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(copied, 0, "a global rule was copied into a project board");
    }

    fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "update",
            global_id,
            "--body",
            "Never store secrets in Kanban.",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let events = fixture.ok_json(&fixture.main, &["events", "--rule", global_id, "--json"]);
    assert!(
        events[0]["payload"]["previousBody"]
            .as_str()
            .unwrap()
            .starts_with("Never store credentials")
    );

    fixture.ok_json(
        &fixture.main,
        &["rule", "retire", global_id, "--as", "geoyws", "--json"],
    );
    let active = fixture.ok_json(&fixture.main, &["rule", "list", "--json"]);
    assert_eq!(active.as_array().unwrap().len(), 2);
    let retained = fixture.ok_json(
        &fixture.main,
        &["rule", "list", "--all", "--full", "--json"],
    );
    assert!(
        retained
            .as_array()
            .unwrap()
            .iter()
            .any(|rule| rule["id"] == global["id"] && rule["archived"] == true)
    );

    let conflicting = fixture.run(
        &fixture.main,
        &["rule", "list", "--global", "--project", "ONE", "--json"],
    );
    assert!(!conflicting.status.success());
    assert!(String::from_utf8_lossy(&conflicting.stderr).contains("superseded"));
}

#[test]
fn rule_selector_tags_target_named_boards_or_all_except_named_boards() {
    let fixture = Fixture::new("targeted-global-rules");
    let second = fixture.root.join("second");
    let third = fixture.root.join("third");
    fs::create_dir_all(&second).unwrap();
    fs::create_dir_all(&third).unwrap();
    fixture.ok_json(&fixture.main, &["init", "--name", "ONE", "--json"]);
    fixture.ok_json(&second, &["init", "--name", "TWO", "--json"]);
    fixture.ok_json(&third, &["init", "--name", "THREE", "--json"]);

    let all = fixture.ok_json(
        &fixture.main,
        &["rule", "add", "Every board.", "--as", "geoyws", "--json"],
    );
    let only = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Only one and two.",
            "--board",
            "ONE",
            "--board",
            "TWO",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let except = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Everything except one.",
            "--except-board",
            "ONE",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(only["tags"], json!(["ONLY:ONE", "ONLY:TWO"]));
    assert_eq!(except["tags"], json!(["ALL", "EXCEPT:ONE"]));

    for (cwd, id, expected) in [
        (
            &fixture.main,
            "t-one",
            vec![all["id"].clone(), only["id"].clone()],
        ),
        (
            &second,
            "t-two",
            vec![all["id"].clone(), only["id"].clone(), except["id"].clone()],
        ),
        (
            &third,
            "t-three",
            vec![all["id"].clone(), except["id"].clone()],
        ),
    ] {
        fixture.ok_json(cwd, &["task", "add", id, "--id", id, "--json"]);
        let claim = fixture.ok_json(cwd, &["claim", id, "--as", "worker", "--json"]);
        let actual = claim["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|rule| rule["id"].clone())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert_eq!(
            fixture.ok_json(cwd, &["context", id, "--json"])["rules"],
            claim["rules"]
        );
    }

    let only_id = only["id"].as_str().unwrap();
    let retargeted = fixture.ok_json(
        &fixture.main,
        &[
            "rule", "update", only_id, "--board", "THREE", "--as", "geoyws", "--json",
        ],
    );
    assert_eq!(retargeted["body"], "Only one and two.");
    assert_eq!(retargeted["tags"], json!(["ONLY:THREE"]));
    let events = fixture.ok_json(&fixture.main, &["events", "--rule", only_id, "--json"]);
    assert_eq!(
        events[0]["payload"]["previousTags"],
        json!(["ONLY:ONE", "ONLY:TWO"])
    );
    assert_eq!(events[0]["payload"]["changed"], json!(["selectorTags"]));

    for args in [
        vec![
            "rule", "add", "Bad mix.", "--board", "ALL", "--board", "ONE", "--as", "geoyws",
            "--json",
        ],
        vec![
            "rule",
            "add",
            "Bad subtraction.",
            "--board",
            "ONE",
            "--except-board",
            "TWO",
            "--as",
            "geoyws",
            "--json",
        ],
        vec![
            "rule",
            "add",
            "Unknown board.",
            "--board",
            "MISSING",
            "--as",
            "geoyws",
            "--json",
        ],
        vec![
            "rule",
            "add",
            "Legacy scope.",
            "--global",
            "--as",
            "geoyws",
            "--json",
        ],
    ] {
        assert!(
            !fixture.run(&fixture.main, &args).status.success(),
            "accepted {args:?}"
        );
    }
}

#[test]
fn sprint_scoped_rules_match_authoritative_task_sprint_and_update_atomically() {
    let fixture = Fixture::new("sprint-scoped-rules");
    let second = fixture.root.join("second");
    fs::create_dir_all(&second).unwrap();
    fixture.ok_json(&fixture.main, &["init", "--name", "ONE", "--json"]);
    fixture.ok_json(&second, &["init", "--name", "TWO", "--json"]);
    for cwd in [&fixture.main, &second] {
        fixture.ok_json(
            cwd,
            &[
                "sprint",
                "new",
                "Scoped sprint",
                "--id",
                "sp-shared",
                "--target-version",
                "1.0.0",
                "--start",
                "0",
                "--end",
                "4102444800000",
                "--as",
                "operator",
                "--json",
            ],
        );
    }
    for cwd in [&fixture.main, &second] {
        fixture.ok_json(cwd, &["tag", "add", "geoyws/infra", "--json"]);
    }
    let scoped = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Sprint rule.",
            "--board",
            "ONE",
            "--sprint",
            "sp-shared",
            "--tag",
            "geoyws/infra",
            "--as",
            "operator",
            "--json",
        ],
    );
    assert_eq!(
        scoped["tags"],
        json!(["ONLY:ONE", "SPRINT:sp-shared", "geoyws/infra"])
    );
    let rule_id = scoped["id"].as_str().unwrap();

    for (cwd, id, sprint, tag, expected) in [
        (&fixture.main, "t-match", true, true, true),
        (&fixture.main, "t-other", false, true, false),
        (&fixture.main, "t-no-tag", true, false, false),
        (&second, "t-other-board", true, true, false),
    ] {
        let mut args = vec!["task", "add", id, "--id", id];
        if sprint {
            args.extend(["--sprint", "sp-shared"]);
        }
        if tag {
            args.extend(["--tag", "geoyws/infra"]);
        }
        args.push("--json");
        fixture.ok_json(cwd, &args);
        let packet = fixture.ok_json(cwd, &["context", id, "--json"]);
        assert_eq!(
            packet["rules"]
                .as_array()
                .unwrap()
                .iter()
                .any(|rule| rule["id"] == rule_id),
            expected,
            "wrong applicability for {id}",
        );
    }
    let claim = fixture.ok_json(
        &fixture.main,
        &["claim", "t-match", "--as", "worker", "--json"],
    );
    assert!(
        claim["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|rule| rule["id"] == rule_id)
    );
    let handoff = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "create",
            "t-match",
            "--lease",
            claim["leaseToken"].as_str().unwrap(),
            "--as",
            "worker",
            "--summary",
            "continue",
            "--intent",
            "finish",
            "--next-action",
            "resume",
            "--reason",
            "manual",
            "--json",
        ],
    );
    let accepted = fixture.ok_json(
        &fixture.main,
        &[
            "handoff",
            "accept",
            handoff["id"].as_str().unwrap(),
            "--as",
            "next",
            "--json",
        ],
    );
    assert!(
        accepted["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|rule| rule["id"] == rule_id)
    );

    let retained = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "update",
            rule_id,
            "--body",
            "Retained scope.",
            "--as",
            "operator",
            "--json",
        ],
    );
    assert_eq!(retained["tags"], scoped["tags"]);
    let before_invalid = fixture.ok_json(&fixture.main, &["rule", "show", rule_id, "--json"]);
    let invalid = fixture.run(
        &fixture.main,
        &[
            "rule",
            "update",
            rule_id,
            "--sprint",
            "sp-missing",
            "--as",
            "operator",
            "--json",
        ],
    );
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("does not exist on this board"));
    assert_eq!(
        fixture.ok_json(&fixture.main, &["rule", "show", rule_id, "--json"]),
        before_invalid
    );

    let cleared = fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "update",
            rule_id,
            "--clear-sprint",
            "--as",
            "operator",
            "--json",
        ],
    );
    assert_eq!(cleared["tags"], json!(["ONLY:ONE", "geoyws/infra"]));
    let conflict = fixture.run(
        &fixture.main,
        &[
            "rule",
            "update",
            rule_id,
            "--sprint",
            "sp-shared",
            "--clear-sprint",
            "--as",
            "operator",
            "--json",
        ],
    );
    assert!(String::from_utf8_lossy(&conflict.stderr).contains("mutually exclusive"));
}

#[test]
fn sprint_scoped_rule_transfer_requires_destination_sprint_and_round_trips() {
    let source = Fixture::new("sprint-rule-transfer-source");
    source.ok_json(&source.main, &["init", "--name", "ONE", "--json"]);
    source.ok_json(
        &source.main,
        &[
            "sprint",
            "new",
            "Release",
            "--id",
            "sp-release",
            "--target-version",
            "1.0.0",
            "--start",
            "0",
            "--end",
            "4102444800000",
            "--as",
            "operator",
            "--json",
        ],
    );
    source.ok_json(
        &source.main,
        &[
            "rule",
            "add",
            "Release-only.",
            "--board",
            "ONE",
            "--sprint",
            "sp-release",
            "--as",
            "operator",
            "--json",
        ],
    );
    let bundle = source.root.join("sprint-rules.json");
    source.ok_json(
        &source.main,
        &[
            "rule",
            "export",
            "--board",
            "ONE",
            "--as",
            "operator",
            "--output",
            bundle.to_str().unwrap(),
            "--json",
        ],
    );

    let destination = Fixture::new("sprint-rule-transfer-destination");
    destination.ok_json(&destination.main, &["init", "--name", "ONE", "--json"]);
    let missing = destination.run(
        &destination.main,
        &[
            "rule",
            "import",
            bundle.to_str().unwrap(),
            "--as",
            "operator",
            "--json",
        ],
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("sprint sp-release does not exist"));
    assert!(
        destination
            .ok_json(&destination.main, &["rule", "list", "--json"])
            .as_array()
            .unwrap()
            .is_empty()
    );
    destination.ok_json(
        &destination.main,
        &[
            "sprint",
            "new",
            "Release",
            "--id",
            "sp-release",
            "--target-version",
            "1.0.0",
            "--start",
            "0",
            "--end",
            "4102444800000",
            "--as",
            "operator",
            "--json",
        ],
    );
    let report = destination.ok_json(
        &destination.main,
        &[
            "rule",
            "import",
            bundle.to_str().unwrap(),
            "--as",
            "operator",
            "--json",
        ],
    );
    assert_eq!(report["importedRules"], 1);
    let rules = destination.ok_json(&destination.main, &["rule", "list", "--full", "--json"]);
    assert_eq!(rules[0]["tags"], json!(["ONLY:ONE", "SPRINT:sp-release"]));
}

#[test]
fn compiled_binary_exports_and_imports_allowlisted_rules_without_mutating_source() {
    let source = Fixture::new("rule-transfer-source");
    let source_second = source.root.join("second");
    fs::create_dir_all(&source_second).unwrap();
    source.ok_json(&source.main, &["init", "--name", "ALPHA", "--json"]);
    source.ok_json(&source_second, &["init", "--name", "BETA", "--json"]);
    source.ok_json(
        &source.main,
        &["tag", "add", "geoyws/alpha", "--as", "geoyws", "--json"],
    );
    source.ok_json(
        &source.main,
        &["tag", "add", "geoyws/beta", "--as", "geoyws", "--json"],
    );
    source.ok_json(
        &source.main,
        &[
            "rule",
            "add",
            "Alpha source rule.",
            "--board",
            "ALPHA",
            "--tag",
            "geoyws/alpha",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    source.ok_json(
        &source_second,
        &[
            "rule",
            "add",
            "Beta source rule.",
            "--board",
            "BETA",
            "--tag",
            "geoyws/beta",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let source_before =
        source.ok_json(&source.main, &["rule", "list", "--all", "--full", "--json"]);
    let bundle_path = source.root.join("rule-transfer.json");
    let export = source.ok_json(
        &source.main,
        &[
            "rule",
            "export",
            "--board",
            "ALPHA",
            "--board",
            "BETA",
            "--as",
            "geoyws",
            "--output",
            bundle_path.to_str().unwrap(),
            "--json",
        ],
    );
    assert_eq!(export["written"], json!(bundle_path.to_str().unwrap()));
    assert_eq!(export["sourceBoards"], json!(["ALPHA", "BETA"]));
    assert_eq!(export["rulesExported"], 2);

    let bundle: Value = serde_json::from_slice(&fs::read(&bundle_path).unwrap()).unwrap();
    assert_eq!(bundle["formatVersion"], 1);
    assert_eq!(bundle["exportedBy"], "geoyws");
    assert_eq!(bundle["sourceBoards"], json!(["ALPHA", "BETA"]));
    assert_eq!(bundle["rules"].as_array().unwrap().len(), 2);
    assert_eq!(
        source.ok_json(&source.main, &["rule", "list", "--all", "--full", "--json"]),
        source_before,
        "export mutated the source registry"
    );

    let destination = Fixture::new("rule-transfer-destination");
    let destination_second = destination.root.join("second");
    fs::create_dir_all(&destination_second).unwrap();
    destination.ok_json(&destination.main, &["init", "--name", "ALPHA", "--json"]);
    destination.ok_json(&destination_second, &["init", "--name", "BETA", "--json"]);

    let imported = destination.ok_json(
        &destination.main,
        &[
            "rule",
            "import",
            bundle_path.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(imported["importedRules"], 2);
    assert_eq!(imported["alreadyImportedRules"], 0);
    assert_eq!(imported["destinationBoardsVerified"], 2);

    let imported_rules = destination.ok_json(
        &destination.main,
        &["rule", "list", "--all", "--full", "--json"],
    );
    let mut imported_by_source = BTreeMap::new();
    for rule in imported_rules.as_array().unwrap() {
        imported_by_source.insert(
            (
                rule["sourceBoard"].as_str().unwrap().to_owned(),
                rule["sourceRuleId"].as_str().unwrap().to_owned(),
            ),
            rule.clone(),
        );
    }
    for rule in bundle["rules"].as_array().unwrap() {
        let source_board = rule["sourceBoard"].as_str().unwrap().to_owned();
        let source_rule_id = rule["sourceRuleId"].as_str().unwrap().to_owned();
        let imported_rule = imported_by_source
            .get(&(source_board.clone(), source_rule_id.clone()))
            .unwrap_or_else(|| panic!("missing imported rule {source_board}/{source_rule_id}"));
        assert_ne!(imported_rule["id"], rule["sourceRuleId"]);
        assert_eq!(imported_rule["body"], rule["body"]);
        assert_eq!(imported_rule["author"], rule["author"]);
        assert_eq!(imported_rule["tags"], rule["tags"]);
        assert_eq!(imported_rule["sourceBoard"], rule["sourceBoard"]);
        assert_eq!(imported_rule["sourceRuleId"], rule["sourceRuleId"]);
    }

    let imported_again = destination.ok_json(
        &destination.main,
        &[
            "rule",
            "import",
            bundle_path.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert_eq!(imported_again["importedRules"], 0);
    assert_eq!(imported_again["alreadyImportedRules"], 2);
    assert_eq!(
        destination.ok_json(
            &destination.main,
            &["rule", "list", "--all", "--full", "--json"],
        ),
        imported_rules,
        "import was not idempotent"
    );
}

#[test]
fn compiled_binary_refuses_rule_import_when_a_bundle_item_source_registry_uuid_differs() {
    let source = Fixture::new("rule-transfer-source-tamper");
    let source_second = source.root.join("second");
    fs::create_dir_all(&source_second).unwrap();
    source.ok_json(&source.main, &["init", "--name", "ALPHA", "--json"]);
    source.ok_json(&source_second, &["init", "--name", "BETA", "--json"]);
    source.ok_json(
        &source.main,
        &["tag", "add", "geoyws/alpha", "--as", "geoyws", "--json"],
    );
    source.ok_json(
        &source.main,
        &["tag", "add", "geoyws/beta", "--as", "geoyws", "--json"],
    );
    source.ok_json(
        &source.main,
        &[
            "rule",
            "add",
            "Alpha source rule.",
            "--board",
            "ALPHA",
            "--tag",
            "geoyws/alpha",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    source.ok_json(
        &source_second,
        &[
            "rule",
            "add",
            "Beta source rule.",
            "--board",
            "BETA",
            "--tag",
            "geoyws/beta",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let bundle_path = source.root.join("rule-transfer.json");
    source.ok_json(
        &source.main,
        &[
            "rule",
            "export",
            "--board",
            "ALPHA",
            "--board",
            "BETA",
            "--as",
            "geoyws",
            "--output",
            bundle_path.to_str().unwrap(),
            "--json",
        ],
    );
    let mut bundle: Value = serde_json::from_slice(&fs::read(&bundle_path).unwrap()).unwrap();
    bundle["rules"][0]["sourceRegistryUuid"] = json!(Uuid::new_v4().to_string());
    fs::write(&bundle_path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();

    let destination = Fixture::new("rule-transfer-destination-tamper");
    let destination_second = destination.root.join("second");
    fs::create_dir_all(&destination_second).unwrap();
    destination.ok_json(&destination.main, &["init", "--name", "ALPHA", "--json"]);
    destination.ok_json(&destination_second, &["init", "--name", "BETA", "--json"]);

    let failed = destination.run(
        &destination.main,
        &[
            "rule",
            "import",
            bundle_path.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !failed.status.success(),
        "tampered bundle unexpectedly imported"
    );
    let stderr = String::from_utf8_lossy(&failed.stderr).into_owned();
    assert!(
        stderr.contains("claims source registry") || stderr.contains("sourceRegistryUuid"),
        "{stderr}"
    );
    assert!(
        destination
            .ok_json(
                &destination.main,
                &["rule", "list", "--all", "--full", "--json"]
            )
            .as_array()
            .unwrap()
            .is_empty(),
        "a refused import mutated the destination registry"
    );
    let registry = Connection::open(destination.data.join("registry.db")).unwrap();
    let rule_count: i64 = registry
        .query_row("SELECT count(*) FROM rules", [], |row| row.get(0))
        .unwrap();
    let ledger_count: i64 = registry
        .query_row("SELECT count(*) FROM rule_import_ledger", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rule_count, 0, "a refused import wrote destination rules");
    assert_eq!(ledger_count, 0, "a refused import left ledger residue");
}

#[test]
fn compiled_binary_refuses_rule_import_when_destination_lacks_an_exported_board() {
    let source = Fixture::new("rule-transfer-missing-destination");
    let source_second = source.root.join("second");
    fs::create_dir_all(&source_second).unwrap();
    source.ok_json(&source.main, &["init", "--name", "ALPHA", "--json"]);
    source.ok_json(&source_second, &["init", "--name", "BETA", "--json"]);
    source.ok_json(
        &source.main,
        &[
            "rule",
            "add",
            "Alpha source rule.",
            "--board",
            "ALPHA",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    source.ok_json(
        &source_second,
        &[
            "rule",
            "add",
            "Beta source rule.",
            "--board",
            "BETA",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let bundle_path = source.root.join("rule-transfer.json");
    source.ok_json(
        &source.main,
        &[
            "rule",
            "export",
            "--board",
            "ALPHA",
            "--board",
            "BETA",
            "--as",
            "geoyws",
            "--output",
            bundle_path.to_str().unwrap(),
            "--json",
        ],
    );

    let destination = Fixture::new("rule-transfer-missing-destination-target");
    destination.ok_json(&destination.main, &["init", "--name", "ALPHA", "--json"]);
    let failed = destination.run(
        &destination.main,
        &[
            "rule",
            "import",
            bundle_path.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !failed.status.success(),
        "import without BETA unexpectedly succeeded"
    );
    let stderr = String::from_utf8_lossy(&failed.stderr).into_owned();
    assert!(
        stderr.contains("not registered in this registry"),
        "{stderr}"
    );
    assert!(
        destination
            .ok_json(
                &destination.main,
                &["rule", "list", "--all", "--full", "--json"]
            )
            .as_array()
            .unwrap()
            .is_empty(),
        "a refused import mutated the destination registry"
    );
}

#[test]
fn hig_registry_refuses_absent_named_selectors_on_add_and_refingerprinted_import() {
    let source = Fixture::new("rule-selector-source-px-only");
    source.ok_json(&source.main, &["init", "--name", "px", "--json"]);
    source.ok_json(
        &source.main,
        &[
            "rule",
            "add",
            "PX-only source rule.",
            "--board",
            "px",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let bundle_path = source.root.join("px-rules.json");
    source.ok_json(
        &source.main,
        &[
            "rule",
            "export",
            "--board",
            "px",
            "--as",
            "geoyws",
            "--output",
            bundle_path.to_str().unwrap(),
            "--json",
        ],
    );

    let destination = Fixture::new("rule-selector-destination-px-only");
    destination.ok_json(&destination.main, &["init", "--name", "px", "--json"]);
    for board in ["kanban", "unum"] {
        let refused = destination.run(
            &destination.main,
            &[
                "rule",
                "add",
                "Wrong-host rule.",
                "--board",
                board,
                "--as",
                "geoyws",
                "--json",
            ],
        );
        assert!(
            !refused.status.success(),
            "a px-only registry accepted selector {board}"
        );
        assert!(
            String::from_utf8_lossy(&refused.stderr)
                .contains(&format!("no registered Kanban board named {board}")),
            "{}",
            String::from_utf8_lossy(&refused.stderr)
        );
    }

    let mut bundle: Value = serde_json::from_slice(&fs::read(&bundle_path).unwrap()).unwrap();
    bundle["rules"][0]["tags"]
        .as_array_mut()
        .unwrap()
        .push(json!("ONLY:unum"));
    let fingerprint = rule_transfer_item_fingerprint(&bundle["rules"][0]);
    bundle["rules"][0]["sourceContentSha256"] = json!(fingerprint);
    fs::write(&bundle_path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();

    let refused = destination.run(
        &destination.main,
        &[
            "rule",
            "import",
            bundle_path.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "a correctly re-fingerprinted absent selector was imported"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("selector ONLY:unum"), "{stderr}");
    assert!(
        stderr.contains("outside the bundle sourceBoards allowlist [px]"),
        "{stderr}"
    );

    let registry = Connection::open(destination.data.join("registry.db")).unwrap();
    let rule_count: i64 = registry
        .query_row("SELECT count(*) FROM rules", [], |row| row.get(0))
        .unwrap();
    let ledger_count: i64 = registry
        .query_row("SELECT count(*) FROM rule_import_ledger", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rule_count, 0, "a refused import wrote a destination rule");
    assert_eq!(
        ledger_count, 0,
        "a refused import wrote an import-ledger row"
    );
}

#[test]
fn compiled_binary_refuses_refingerprinted_import_whose_only_selector_targets_an_active_board_outside_source_boards()
 {
    let source = Fixture::new("rule-transfer-scope-source");
    let source_second = source.root.join("second");
    fs::create_dir_all(&source_second).unwrap();
    source.ok_json(&source.main, &["init", "--name", "ALPHA", "--json"]);
    source.ok_json(&source_second, &["init", "--name", "BETA", "--json"]);
    let added = source.ok_json(
        &source.main,
        &[
            "rule",
            "add",
            "Alpha-only source rule.",
            "--board",
            "ALPHA",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let source_rule_id = added["id"].as_str().unwrap().to_owned();
    let bundle_path = source.root.join("alpha-rules.json");
    source.ok_json(
        &source.main,
        &[
            "rule",
            "export",
            "--board",
            "ALPHA",
            "--as",
            "geoyws",
            "--output",
            bundle_path.to_str().unwrap(),
            "--json",
        ],
    );

    // BETA is a live, uniquely named board in the destination, so the
    // active-selector check alone would accept ONLY:BETA; only the
    // sourceBoards allowlist can refuse it.
    let destination = Fixture::new("rule-transfer-scope-destination");
    let destination_second = destination.root.join("second");
    fs::create_dir_all(&destination_second).unwrap();
    destination.ok_json(&destination.main, &["init", "--name", "ALPHA", "--json"]);
    destination.ok_json(&destination_second, &["init", "--name", "BETA", "--json"]);

    let mut bundle: Value = serde_json::from_slice(&fs::read(&bundle_path).unwrap()).unwrap();
    assert_eq!(bundle["sourceBoards"], json!(["ALPHA"]));
    bundle["rules"][0]["tags"]
        .as_array_mut()
        .unwrap()
        .push(json!("ONLY:BETA"));
    let fingerprint = rule_transfer_item_fingerprint(&bundle["rules"][0]);
    bundle["rules"][0]["sourceContentSha256"] = json!(fingerprint);
    fs::write(&bundle_path, serde_json::to_vec_pretty(&bundle).unwrap()).unwrap();

    let registry = Connection::open(destination.data.join("registry.db")).unwrap();
    let audit_head = |registry: &Connection| -> (i64, Option<String>) {
        registry
            .query_row(
                "SELECT count(*), (SELECT event_hash FROM rule_events ORDER BY seq DESC LIMIT 1) FROM rule_events",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    };
    let audit_before = audit_head(&registry);

    let refused = destination.run(
        &destination.main,
        &[
            "rule",
            "import",
            bundle_path.to_str().unwrap(),
            "--as",
            "geoyws",
            "--json",
        ],
    );
    assert!(
        !refused.status.success(),
        "a re-fingerprinted ONLY selector outside sourceBoards was imported"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains(&format!("item {source_rule_id} selector ONLY:BETA")),
        "{stderr}"
    );
    assert!(
        stderr.contains("outside the bundle sourceBoards allowlist [ALPHA]"),
        "{stderr}"
    );
    assert!(
        stderr.contains("`rule export --board ALPHA --board BETA --as ACTOR`"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("found 0"),
        "refusal came from the absent-selector check, not the allowlist: {stderr}"
    );

    let rule_count: i64 = registry
        .query_row("SELECT count(*) FROM rules", [], |row| row.get(0))
        .unwrap();
    let ledger_count: i64 = registry
        .query_row("SELECT count(*) FROM rule_import_ledger", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rule_count, 0, "a refused import wrote a destination rule");
    assert_eq!(
        ledger_count, 0,
        "a refused import wrote an import-ledger row"
    );
    assert_eq!(
        audit_head(&registry),
        audit_before,
        "a refused import appended to the destination audit chain"
    );
    assert!(
        destination
            .ok_json(
                &destination.main,
                &["rule", "list", "--all", "--full", "--json"]
            )
            .as_array()
            .unwrap()
            .is_empty(),
        "a refused import mutated the destination registry"
    );
}

#[test]
fn compiled_binary_refuses_duplicate_rule_export_selectors_and_missing_boards() {
    let fixture = Fixture::new("rule-export-refusals");
    fixture.ok_json(&fixture.main, &["init", "--name", "ALPHA", "--json"]);
    fixture.ok_json(
        &fixture.main,
        &[
            "rule",
            "add",
            "Alpha source rule.",
            "--board",
            "ALPHA",
            "--as",
            "geoyws",
            "--json",
        ],
    );
    let duplicate_bundle = fixture.root.join("duplicate-bundle.json");

    let duplicate = fixture.run(
        &fixture.main,
        &[
            "rule",
            "export",
            "--board",
            "ALPHA",
            "--board",
            "ALPHA",
            "--as",
            "geoyws",
            "--output",
            duplicate_bundle.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(!duplicate.status.success());
    assert!(
        String::from_utf8_lossy(&duplicate.stderr).contains("given more than once"),
        "{:?}",
        String::from_utf8_lossy(&duplicate.stderr)
    );

    let missing_bundle = fixture.root.join("missing-bundle.json");
    let missing = fixture.run(
        &fixture.main,
        &[
            "rule",
            "export",
            "--board",
            "MISSING",
            "--as",
            "geoyws",
            "--output",
            missing_bundle.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("not registered in this registry"),
        "{:?}",
        String::from_utf8_lossy(&missing.stderr)
    );
}
