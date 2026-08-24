use crate::model::{Checkpoint, ContextPacket, Handoff, RuleSummary, Sitrep, Task, TaskNote};
use crate::store::Store;
use anyhow::{Result, bail};

fn task_line(task: &Task) -> String {
    format!(
        "- {} [{}] P{} {}",
        task.id, task.status, task.priority, task.title
    )
}

fn render_checkpoint(checkpoint: &Checkpoint) -> String {
    let mut lines = vec![
        format!(
            "### Checkpoint {} · {} · {} · {}",
            checkpoint.seq, checkpoint.state, checkpoint.author, checkpoint.created_at
        ),
        format!("Summary: {}", checkpoint.summary),
        format!("Intent: {}", checkpoint.intent),
        format!("Next action: {}", checkpoint.next_action),
    ];
    if !checkpoint.blockers.is_empty() {
        lines.push(format!("Blockers: {}", checkpoint.blockers.join("; ")));
    }
    if !checkpoint.validations.is_empty() {
        lines.push(format!(
            "Validations: {}",
            checkpoint.validations.join("; ")
        ));
    }
    let repository = [
        checkpoint.repo_path.as_deref(),
        checkpoint.branch.as_deref(),
        checkpoint.head_sha.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    if !repository.is_empty() {
        lines.push(format!("Repository: {repository}"));
    }
    if let Some(dirty) = &checkpoint.dirty_summary {
        lines.push(format!("Working tree: {dirty}"));
    }
    lines.join("\n")
}

/// The most recent sitrep, which for a lane that has been posting is
/// fresher than any checkpoint and cheaper than any handoff.
fn render_sitrep(sitrep: &Sitrep) -> String {
    let mut lines = vec![format!(
        "[{}] {} · {}{}",
        sitrep.lane,
        sitrep.author,
        sitrep.body,
        if sitrep.archived { " (archived)" } else { "" }
    )];
    if let Some(branch) = &sitrep.branch {
        lines.push(format!(
            "branch {branch}{}",
            sitrep
                .head_sha
                .as_deref()
                .map(|sha| format!(" @ {}", &sha[..sha.len().min(8)]))
                .unwrap_or_default()
        ));
    }
    lines.join("\n")
}

fn render_rules(rules: &[RuleSummary]) -> String {
    let globals = rules.iter().filter(|rule| rule.scope == "global").count();
    let projects = rules.len().saturating_sub(globals);
    let mut lines = vec![format!(
        "## Rules ({globals} global · {projects} project; bodies lazy)"
    )];
    if rules.is_empty() {
        lines.push("(none)".to_owned());
        return lines.join("\n");
    }
    for rule in rules {
        let scope = if rule.scope == "global" { "g" } else { "p" };
        let detail = if rule.has_more {
            let global = if rule.scope == "global" {
                " --global"
            } else {
                ""
            };
            format!(
                "  (+{:.1} KB · kb r cat {}{global})",
                rule.bytes as f64 / 1024.0,
                rule.id
            )
        } else {
            String::new()
        };
        lines.push(format!(
            "- [{scope}] {}  {}{detail}",
            rule.id, rule.headline
        ));
    }
    lines.join("\n")
}

fn render_handoff(handoff: &Handoff) -> String {
    let target = handoff
        .to_agent
        .as_deref()
        .unwrap_or("next compatible agent");
    let acceptance = handoff.accepted_by.as_ref().map_or_else(
        || "Accepted by: (pending)".to_owned(),
        |agent| {
            format!(
                "Accepted by: {} · {}",
                agent,
                handoff
                    .accepted_at
                    .map_or_else(|| "-".to_owned(), |at| at.to_string())
            )
        },
    );
    [
        format!(
            "### Handoff {} · {} · {}",
            handoff.id, handoff.status, handoff.reason
        ),
        format!("From: {} · To: {target}", handoff.from_agent),
        format!("Summary: {}", handoff.summary),
        format!("Intent: {}", handoff.intent),
        format!("Next action: {}", handoff.next_action),
        acceptance,
    ]
    .join("\n")
}

fn render_note(note: &TaskNote) -> String {
    format!(
        "- {} · {} · {}: {}",
        note.created_at, note.kind, note.author, note.body
    )
}

fn clip(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_owned();
    }
    let keep = max.saturating_sub(13);
    format!(
        "{}…[truncated]",
        value.chars().take(keep).collect::<String>()
    )
}

fn take_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

pub fn render_context(packet: &ContextPacket, max_chars: usize) -> Result<String> {
    if max_chars < 1_000 {
        bail!("max chars must be at least 1000");
    }
    let task = &packet.task;
    let fixed = [
        "# Kanban cold-start context".to_owned(),
        String::new(),
        format!("Generated: {}", packet.generated_at),
        String::new(),
        "## Current task".to_owned(),
        format!("ID: {}", task.id),
        format!("Type: {}", task.task_type),
        format!("Status: {}", task.status),
        format!("Priority: {}", task.priority),
        format!("Title: {}", task.title),
        format!("Body: {}", task.body.as_deref().unwrap_or("(none)")),
        String::new(),
        "## Claim".to_owned(),
        packet.claim.as_ref().map_or_else(
            || "unclaimed".to_owned(),
            |claim| {
                format!(
                    "{} · session {} · expires {}",
                    claim.agent_id,
                    claim.session_id.as_deref().unwrap_or("-"),
                    claim.expires_at
                )
            },
        ),
        String::new(),
        "## Ancestry".to_owned(),
        if packet.ancestors.is_empty() {
            "(none)".to_owned()
        } else {
            packet
                .ancestors
                .iter()
                .map(task_line)
                .collect::<Vec<_>>()
                .join("\n")
        },
        String::new(),
        "## Dependencies".to_owned(),
        if packet.dependencies.is_empty() {
            "(none)".to_owned()
        } else {
            packet
                .dependencies
                .iter()
                .map(task_line)
                .collect::<Vec<_>>()
                .join("\n")
        },
    ]
    .join("\n");
    let newest_checkpoint = packet.checkpoints.last();
    let newest_handoff = packet.handoffs.last();
    let newest_sitrep = packet.sitreps.last();
    let rules = render_rules(&packet.rules);
    let required = format!(
        "{fixed}\n\n{rules}\n\n## Latest sitrep\n{}\n\n## Latest checkpoint\n{}\n\n## Latest handoff\n{}",
        newest_sitrep.map_or_else(|| "(none)".to_owned(), render_sitrep),
        newest_checkpoint.map_or_else(|| "(none)".to_owned(), render_checkpoint),
        newest_handoff.map_or_else(|| "(none)".to_owned(), render_handoff)
    );
    if required.chars().count() > max_chars {
        let checkpoint = newest_checkpoint;
        let compact = [
            "# Kanban cold-start context (compact)".to_owned(),
            format!(
                "Task: {} [{}] {}",
                task.id,
                task.status,
                clip(&task.title, 160)
            ),
            format!(
                "Claim: {}",
                packet
                    .claim
                    .as_ref()
                    .map_or("unclaimed", |claim| claim.agent_id.as_str())
            ),
            rules,
            format!(
                "Sitrep: {}",
                clip(
                    newest_sitrep.map_or("(none)", |value| value.body.as_str()),
                    240
                )
            ),
            String::new(),
            "## Latest checkpoint".to_owned(),
            format!(
                "Next action: {}",
                clip(checkpoint.map_or("(none)", |value| &value.next_action), 240)
            ),
            format!(
                "Intent: {}",
                clip(checkpoint.map_or("(none)", |value| &value.intent), 180)
            ),
            format!(
                "Summary: {}",
                clip(checkpoint.map_or("(none)", |value| &value.summary), 180)
            ),
            format!(
                "Handoff: {}",
                clip(
                    &newest_handoff.map_or_else(
                        || "(none)".to_owned(),
                        |handoff| {
                            format!(
                                "{} {} from {}; next {}",
                                handoff.id, handoff.status, handoff.from_agent, handoff.next_action
                            )
                        }
                    ),
                    240,
                )
            ),
        ]
        .join("\n");
        // Reserved, not appended and hoped for. At the smallest budgets the
        // compact body already overruns, and `take_chars` cut from the end —
        // so the marker was the first thing lost, exactly when the reader most
        // needed telling that the ancestry, dependencies, every earlier
        // checkpoint and every note had gone. Same reservation the full path
        // makes for `[older history omitted]`.
        const MARKER: &str = "\n\n[context compacted: full durable history remains in SQLite]";
        let budget = max_chars.saturating_sub(MARKER.chars().count());
        return Ok(format!(
            "{}{MARKER}",
            take_chars(compact.trim_end(), budget)
        ));
    }

    let mut text = format!("{required}\n\n## Earlier checkpoints");
    let mut truncated = packet.truncated;
    let mut older = Vec::new();
    for checkpoint in packet.checkpoints.iter().rev().skip(1) {
        let section = format!("\n\n{}", render_checkpoint(checkpoint));
        if text.chars().count() + older.join("").chars().count() + section.chars().count()
            > max_chars
        {
            truncated = true;
            break;
        }
        older.insert(0, section);
    }
    if older.is_empty() {
        text.push_str("\n(none)");
    } else {
        text.push_str(&older.join(""));
    }
    text.push_str("\n\n## Recent notes");
    let mut notes = Vec::new();
    for note in packet.notes.iter().rev() {
        let line = format!("\n{}", render_note(note));
        let marker_len = if truncated { 25 } else { 0 };
        if text.chars().count() + notes.join("").chars().count() + line.chars().count() + marker_len
            > max_chars
        {
            truncated = true;
            break;
        }
        notes.insert(0, line);
    }
    if notes.is_empty() {
        text.push_str("\n(none)");
    } else {
        text.push_str(&notes.join(""));
    }
    if truncated {
        let marker = "\n\n[older history omitted]";
        let keep = max_chars.saturating_sub(marker.chars().count());
        if text.chars().count() > keep {
            text = take_chars(text.trim_end(), keep);
        }
        text.push_str(marker);
    }
    Ok(text)
}

pub fn render_todo(store: &Store) -> Result<String> {
    let name = store.board_name()?.unwrap_or_else(|| "Kanban".to_owned());
    let tasks = store.list_tasks(None, None)?;
    let active = tasks
        .iter()
        .filter(|task| task.status != "done" && task.status != "cancelled")
        .collect::<Vec<_>>();
    let done = tasks
        .iter()
        .filter(|task| task.status == "done")
        .collect::<Vec<_>>();
    let mut lines = vec![
        format!("# {name} — generated TODO"),
        String::new(),
        "> Projection only. SQLite is authoritative; do not edit this file as state.".into(),
        String::new(),
        "## Restart here".into(),
    ];
    let in_progress = active
        .iter()
        .filter(|task| task.status == "in_progress")
        .collect::<Vec<_>>();
    if in_progress.is_empty() {
        lines.extend([String::new(), "No task is currently in progress.".into()]);
    }
    for task in in_progress {
        let checkpoint = store.checkpoints(&task.id, 1)?.pop();
        let claim = store.get_claim(&task.id)?;
        lines.extend([
            String::new(),
            format!("### {} — {}", task.id, task.title),
            format!(
                "- Owner: {}",
                claim.as_ref().map_or("unclaimed", |value| &value.agent_id)
            ),
            format!(
                "- Next: {}",
                checkpoint
                    .as_ref()
                    .map_or("No checkpoint yet", |value| &value.next_action)
            ),
        ]);
        if let Some(checkpoint) = checkpoint
            && !checkpoint.blockers.is_empty()
        {
            lines.push(format!("- Blockers: {}", checkpoint.blockers.join("; ")));
        }
    }
    for status in ["blocked", "review", "todo", "backlog"] {
        let group = active
            .iter()
            .filter(|task| task.status == status)
            .collect::<Vec<_>>();
        lines.extend([String::new(), format!("## {}", status.replace('_', " "))]);
        if group.is_empty() {
            lines.extend([String::new(), "(none)".into()]);
        } else {
            lines.push(String::new());
            lines.extend(group.into_iter().map(|task| task_line(task)));
        }
    }
    lines.extend([String::new(), "## Recently done".into(), String::new()]);
    if done.is_empty() {
        lines.push("(none)".into());
    } else {
        lines.extend(done.into_iter().rev().take(20).map(task_line));
    }
    Ok(format!("{}\n", lines.join("\n")))
}
