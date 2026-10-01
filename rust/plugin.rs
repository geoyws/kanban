//! Synchronous plugin calls (docs/specs/plugin.md, ADR-058).
//!
//! A plugin is a `"kind": "plugin"` action in `dispatchers.json` version 2.
//! `plugin call` rechecks it (PLUGIN-07), runs it once under the adapter
//! process rules (PLUGIN-10), checks its exact v1 response and its reported
//! revision (PLUGIN-08, PLUGIN-09), and prints one canonical envelope
//! (PLUGIN-11). Nothing here opens a board or writes anything.

use crate::adapter_process::{ProcessSpec, run_process};
use crate::dispatch::{DispatcherConfigLoader, ResolvedPlugin};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::ffi::OsString;
use std::sync::atomic::AtomicBool;

/// The bound on a request and on a response (PLUGIN-08), equal to the adapter
/// envelope's.
pub(crate) const MAX_MESSAGE_BYTES: usize = 1 << 20;
/// The protocol version of the request and of the response (PLUGIN-08).
const PROTOCOL_VERSION: i64 = 1;
/// The version of the envelope `plugin call` prints (PLUGIN-11).
const SCHEMA_VERSION: i64 = 1;

/// The response a plugin writes: exactly these fields, named in camelCase.
/// `output` is opaque here; only its being an object is checked.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PluginResponse {
    protocol_version: i64,
    revision: String,
    output: Value,
}

/// The call's input object: `--input-json`, `--input-file`, or `{}`. Refused
/// before any plugin check runs (PLUGIN-06).
pub(crate) fn parse_input(
    input_json: Option<&str>,
    input_file: Option<&str>,
) -> Result<Map<String, Value>> {
    let text = match (input_json, input_file) {
        (Some(_), Some(_)) => bail!("--input-json and --input-file cannot be combined"),
        (Some(text), None) => text.to_owned(),
        (None, Some(path)) => std::fs::read_to_string(path)
            .with_context(|| format!("read plugin input file {path}"))?,
        (None, None) => return Ok(Map::new()),
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => bail!("plugin input must be a JSON object"),
        Err(error) => bail!("plugin input is not valid JSON: {error}"),
    }
}

/// The one request written to the plugin's stdin (PLUGIN-08).
fn encode_request(plugin: &ResolvedPlugin, input: Map<String, Value>) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(&json!({
        "protocolVersion": PROTOCOL_VERSION,
        "target": {
            "consumerID": plugin.consumer_id,
            "actionID": plugin.action_id,
        },
        "revision": plugin.revision,
        "input": Value::Object(input),
    }))?;
    if bytes.len() > MAX_MESSAGE_BYTES {
        bail!(
            "plugin {}/{} request exceeds {MAX_MESSAGE_BYTES} bytes",
            plugin.consumer_id,
            plugin.action_id
        );
    }
    Ok(bytes)
}

/// Decode the plugin's stdout under PLUGIN-08, returning the reported
/// revision and the output object. A broken rule is refused naming it.
fn decode_response(
    bytes: &[u8],
    consumer_id: &str,
    action_id: &str,
) -> Result<(String, Map<String, Value>)> {
    let invalid = |rule: String| {
        anyhow!("plugin {consumer_id}/{action_id} returned an invalid response: {rule}")
    };
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(invalid(format!(
            "response exceeds {MAX_MESSAGE_BYTES} bytes"
        )));
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let response = PluginResponse::deserialize(&mut deserializer)
        .map_err(|error| invalid(error.to_string()))?;
    deserializer
        .end()
        .map_err(|error| invalid(error.to_string()))?;
    if response.protocol_version != PROTOCOL_VERSION {
        return Err(invalid(format!(
            "protocolVersion must be {PROTOCOL_VERSION}, got {}",
            response.protocol_version
        )));
    }
    let Value::Object(output) = response.output else {
        return Err(invalid("output must be a JSON object".to_owned()));
    };
    Ok((response.revision, output))
}

/// The canonical envelope line (PLUGIN-11): keys sorted by their UTF-8 bytes
/// at every depth — `serde_json`'s map is a `BTreeMap` in this build — no
/// insignificant whitespace, numbers re-serialized from their parsed value.
fn envelope(plugin: &ResolvedPlugin, output: Map<String, Value>) -> Result<String> {
    Ok(serde_json::to_string(&json!({
        "action": plugin.action_id,
        "consumer": plugin.consumer_id,
        "output": Value::Object(output),
        "revision": plugin.revision,
        "schemaVersion": SCHEMA_VERSION,
        "sha256": plugin.sha256,
    }))?)
}

/// Run one plugin action synchronously and return its canonical line.
pub(crate) fn call(
    consumer_id: &str,
    action_id: &str,
    input: Map<String, Value>,
) -> Result<String> {
    let Some(config) = DispatcherConfigLoader::load_optional()? else {
        bail!("no plugin {consumer_id}/{action_id} is configured");
    };
    let plugin = config.resolve_plugin(consumer_id, action_id)?;
    let request = encode_request(&plugin, input)?;
    let spec = ProcessSpec {
        executable: plugin.executable.clone(),
        args: plugin.args.iter().map(OsString::from).collect(),
        secret: plugin.secret.as_ref().map(|secret| {
            (
                OsString::from(&secret.target_env),
                secret.secret_value().to_owned(),
            )
        }),
    };
    let stdout = run_process(&spec, &request, plugin.timeout_ms, &AtomicBool::new(false))
        .map_err(|failure| anyhow!("plugin {consumer_id}/{action_id} failed: {}", failure.code))?;
    let (revision, output) = decode_response(&stdout.bytes, consumer_id, action_id)?;
    if revision != plugin.revision {
        bail!(
            "plugin {consumer_id}/{action_id} reported revision {revision}, pinned {}",
            plugin.revision
        );
    }
    envelope(&plugin, output)
}

/// One row per plugin action, with whether it is callable now (PLUGIN-12).
/// Runs the PLUGIN-07 checks for each row and spawns nothing.
pub(crate) fn list() -> Result<Value> {
    let Some(config) = DispatcherConfigLoader::load_optional()? else {
        return Ok(Value::Array(Vec::new()));
    };
    let rows = config
        .plugin_entries()
        .into_iter()
        .map(|entry| {
            let reason = config
                .resolve_plugin(&entry.consumer_id, &entry.action_id)
                .err()
                .map(|error| format!("{error:#}"));
            json!({
                "consumer": entry.consumer_id,
                "action": entry.action_id,
                "capability": entry.capability,
                "revision": entry.revision,
                "sha256": entry.sha256,
                "executable": entry.executable,
                "available": reason.is_none(),
                "reason": reason,
            })
        })
        .collect();
    Ok(Value::Array(rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn plugin() -> ResolvedPlugin {
        ResolvedPlugin {
            consumer_id: "acme".to_owned(),
            action_id: "lookup".to_owned(),
            executable: PathBuf::from("/bin/true"),
            args: Vec::new(),
            secret: None,
            revision: "r1".to_owned(),
            sha256: "a".repeat(64),
            timeout_ms: 1000,
        }
    }

    #[test]
    fn the_request_carries_exactly_the_v1_envelope() {
        let mut input = Map::new();
        input.insert("q".to_owned(), json!(1));
        let bytes = encode_request(&plugin(), input).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value,
            json!({
                "protocolVersion": 1,
                "target": {"consumerID": "acme", "actionID": "lookup"},
                "revision": "r1",
                "input": {"q": 1},
            })
        );
    }

    #[test]
    fn an_oversized_request_is_refused_before_anything_runs() {
        let mut input = Map::new();
        input.insert("big".to_owned(), json!("x".repeat(MAX_MESSAGE_BYTES)));
        let error = encode_request(&plugin(), input).unwrap_err().to_string();
        assert_eq!(error, "plugin acme/lookup request exceeds 1048576 bytes");
    }

    #[test]
    fn a_valid_response_yields_revision_and_output() {
        let (revision, output) = decode_response(
            br#"{"protocolVersion":1,"revision":"r1","output":{"Mixed_Case":{"any-key":1}}}  
"#,
            "acme",
            "lookup",
        )
        .unwrap();
        assert_eq!(revision, "r1");
        assert_eq!(Value::Object(output), json!({"Mixed_Case": {"any-key": 1}}));
    }

    #[test]
    fn every_broken_rule_is_refused_by_name() {
        let cases: [(&[u8], &str); 7] = [
            (
                br#"{"protocolVersion":1,"revision":"r1","output":{}} x"#,
                "trailing characters",
            ),
            (
                br#"{"protocolVersion":1,"revision":"r1"}"#,
                "missing field `output`",
            ),
            (
                br#"{"protocolVersion":1,"revision":"r1","output":{},"extra":1}"#,
                "unknown field `extra`",
            ),
            (
                br#"{"protocol_version":1,"revision":"r1","output":{}}"#,
                "unknown field `protocol_version`",
            ),
            (
                br#"{"protocolVersion":2,"revision":"r1","output":{}}"#,
                "protocolVersion must be 1, got 2",
            ),
            (
                br#"{"protocolVersion":1,"revision":"r1","output":[1]}"#,
                "output must be a JSON object",
            ),
            (b"not json", "expected"),
        ];
        for (bytes, rule) in cases {
            let error = decode_response(bytes, "acme", "lookup")
                .unwrap_err()
                .to_string();
            assert!(
                error.starts_with("plugin acme/lookup returned an invalid response: "),
                "{error}"
            );
            assert!(error.contains(rule), "{rule}: {error}");
        }
        let oversized = vec![b' '; MAX_MESSAGE_BYTES + 1];
        let error = decode_response(&oversized, "acme", "lookup")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "plugin acme/lookup returned an invalid response: response exceeds 1048576 bytes"
        );
    }

    #[test]
    fn the_envelope_is_canonical_at_every_depth() {
        let (_, output) = decode_response(
            br#"{"protocolVersion":1,"revision":"r1","output":{"b":2,"a":[1,{"d":4,"c":3}],"n":1E0,"m":1.0,"i":10}}"#,
            "acme",
            "lookup",
        )
        .unwrap();
        let line = envelope(&plugin(), output).unwrap();
        assert_eq!(
            line,
            format!(
                r#"{{"action":"lookup","consumer":"acme","output":{{"a":[1,{{"c":3,"d":4}}],"b":2,"i":10,"m":1.0,"n":1.0}},"revision":"r1","schemaVersion":1,"sha256":"{}"}}"#,
                "a".repeat(64)
            )
        );
    }

    #[test]
    fn input_must_be_one_json_object() {
        assert_eq!(parse_input(None, None).unwrap(), Map::new());
        assert_eq!(
            Value::Object(parse_input(Some(r#"{"q":1}"#), None).unwrap()),
            json!({"q": 1})
        );
        for (json_text, expected) in [
            ("[1]", "plugin input must be a JSON object"),
            ("1", "plugin input must be a JSON object"),
            ("{", "plugin input is not valid JSON"),
        ] {
            let error = parse_input(Some(json_text), None).unwrap_err().to_string();
            assert!(error.starts_with(expected), "{json_text}: {error}");
        }
        let error = parse_input(Some("{}"), Some("/nonexistent"))
            .unwrap_err()
            .to_string();
        assert_eq!(error, "--input-json and --input-file cannot be combined");
    }

    /// PLUGIN-14: every locked package is a crates.io package or this one, and
    /// the tree ships no plugin configuration and no plugin submodule.
    #[test]
    fn the_public_tree_carries_no_plugin_and_no_private_dependency() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
        let mut packages = 0;
        for block in lock.split("[[package]]").skip(1) {
            packages += 1;
            let name = block
                .lines()
                .find_map(|line| line.strip_prefix("name = "))
                .unwrap_or_default();
            match block
                .lines()
                .find_map(|line| line.strip_prefix("source = "))
            {
                Some(source) => assert_eq!(
                    source, "\"registry+https://github.com/rust-lang/crates.io-index\"",
                    "package {name} comes from somewhere other than crates.io"
                ),
                None => assert_eq!(name, "\"kanban\"", "package {name} has no registry source"),
            }
        }
        assert!(packages > 1, "Cargo.lock listed no packages");
        assert!(!root.join("dispatchers.json").exists());
        let gitmodules = std::fs::read_to_string(root.join(".gitmodules")).unwrap_or_default();
        assert!(
            !gitmodules.to_ascii_lowercase().contains("plugin"),
            ".gitmodules names a plugin repository"
        );
    }
}
