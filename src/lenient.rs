/* -------------------------------------------------------------------------- */
/*                                 lenient.rs                                 */
/* -------------------------------------------------------------------------- */

//! Arguments of the built-in tools as other agents spell them. The tools
//! refuse any argument they do not know, and models trained on Claude Code's
//! or opencode's tools often send `file_path`, `old_string` or a timeout in
//! milliseconds. Those are mapped to hey's own before the call, so the model
//! is not made to retry over a spelling. The tools' schemas stay as they are.

use serde_json::{Map, Value, json};

/// `args` for tool `name`, with other agents' spellings turned into hey's.
pub fn normalize(name: &str, mut args: Value) -> Value {
    let Some(object) = args.as_object_mut() else {
        return args;
    };
    match name {
        "read" | "write" | "list" => path(object),
        "edit" => edit(object),
        "shell" => shell(object),
        "search" => search(object),
        _ => {}
    }
    args
}

/// Renames `from` to `to` unless `to` is already given.
fn rename(object: &mut Map<String, Value>, from: &[&str], to: &str) {
    for from in from {
        if let Some(value) = object.remove(*from) {
            object.entry(to).or_insert(value);
        }
    }
}

fn path(object: &mut Map<String, Value>) {
    rename(object, &["file_path", "filePath", "filepath", "file", "filename", "file_name"], "path");
    rename(object, &["directory", "dir"], "path");
}

fn edit(object: &mut Map<String, Value>) {
    path(object);
    replacement(object);
    // One flat replacement, as other agents' edit tools take it.
    if !object.contains_key("edits") && object.contains_key("old_text") {
        let mut flat = Map::new();
        for key in ["old_text", "new_text", "replace_all"] {
            if let Some(value) = object.remove(key) {
                flat.insert(key.into(), value);
            }
        }
        object.insert("edits".into(), json!([flat]));
    }
    if let Some(Value::Array(edits)) = object.get_mut("edits") {
        for edit in edits.iter_mut().filter_map(Value::as_object_mut) {
            replacement(edit);
        }
    }
}

fn replacement(object: &mut Map<String, Value>) {
    rename(object, &["old_string", "oldString", "old_str", "oldText", "old"], "old_text");
    rename(object, &["new_string", "newString", "new_str", "newText", "new"], "new_text");
    rename(object, &["replaceAll", "replace_all_occurrences"], "replace_all");
}

fn shell(object: &mut Map<String, Value>) {
    rename(object, &["workdir", "working_dir", "working_directory", "directory", "dir"], "cwd");
    // Other agents give the timeout in milliseconds.
    for key in ["timeout", "timeout_ms", "timeoutMs"] {
        if let Some(value) = object.remove(key) {
            let secs = match value.as_f64() {
                Some(millis) if millis > 0.0 => Some((millis / 1000.0).ceil() as u64),
                _ => None,
            };
            if let Some(secs) = secs {
                object.entry("timeout_secs").or_insert(json!(secs));
            }
        }
    }
    // Only there to tell the user what the command is for.
    object.remove("description");
    object.remove("run_in_background");
}

fn search(object: &mut Map<String, Value>) {
    path(object);
    rename(object, &["include"], "glob");
    rename(object, &["query", "regex"], "pattern");
    let literal = object.remove("literal").and_then(|value| value.as_bool()).unwrap_or(false);
    let sensitive = object.remove("caseSensitive").and_then(|value| value.as_bool());
    let insensitive = ["-i", "case_insensitive", "ignore_case"]
        .into_iter()
        .any(|key| object.remove(key).and_then(|value| value.as_bool()).unwrap_or(false))
        || sensitive == Some(false);
    if let Some(Value::String(pattern)) = object.get_mut("pattern") {
        if literal {
            *pattern = regex::escape(pattern);
        }
        if insensitive && !pattern.starts_with("(?i)") {
            pattern.insert_str(0, "(?i)");
        }
    }
    if object.remove("output_mode").is_some_and(|mode| mode == "files_with_matches") {
        object.entry("files").or_insert(json!(true));
    }
    // Limits and context the tool has no counterpart for.
    for key in ["limit", "head_limit", "offset", "context", "-A", "-B", "-C", "-n", "multiline", "type"] {
        object.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_code_edit_becomes_a_one_replacement_edit() {
        let args = json!({"file_path": "a.rs", "old_string": "x", "new_string": "y", "replace_all": true});
        assert_eq!(
            normalize("edit", args),
            json!({"path": "a.rs", "edits": [{"old_text": "x", "new_text": "y", "replace_all": true}]})
        );
    }

    #[test]
    fn opencode_edit_and_edits_in_a_list_are_renamed() {
        let flat = json!({"filePath": "a", "oldString": "x", "newString": "y", "replaceAll": true});
        assert_eq!(
            normalize("edit", flat),
            json!({"path": "a", "edits": [{"old_text": "x", "new_text": "y", "replace_all": true}]})
        );
        let listed = json!({"path": "a", "edits": [{"old_string": "x", "new_string": "y"}]});
        assert_eq!(
            normalize("edit", listed),
            json!({"path": "a", "edits": [{"old_text": "x", "new_text": "y"}]})
        );
    }

    #[test]
    fn hey_own_arguments_are_left_alone() {
        let args = json!({"path": "a", "edits": [{"old_text": "x", "new_text": "y"}]});
        assert_eq!(normalize("edit", args.clone()), args);
        let read = json!({"path": "a", "offset": 3, "limit": 5});
        assert_eq!(normalize("read", read.clone()), read);
    }

    #[test]
    fn a_path_given_twice_keeps_heys() {
        let args = json!({"path": "a", "file_path": "b"});
        assert_eq!(normalize("read", args), json!({"path": "a"}));
    }

    #[test]
    fn shell_timeout_in_milliseconds_becomes_seconds() {
        let args = json!({"command": "ls", "timeout": 120000, "workdir": "/tmp", "description": "list"});
        assert_eq!(
            normalize("shell", args),
            json!({"command": "ls", "timeout_secs": 120, "cwd": "/tmp"})
        );
        let short = json!({"command": "ls", "timeout": 1500});
        assert_eq!(normalize("shell", short), json!({"command": "ls", "timeout_secs": 2}));
    }

    #[test]
    fn search_options_of_other_agents_become_patterns_and_globs() {
        let args = json!({"pattern": "a.b", "include": "*.rs", "literal": true, "caseSensitive": false, "limit": 10});
        assert_eq!(normalize("search", args), json!({"pattern": "(?i)a\\.b", "glob": "*.rs"}));
        let claude = json!({"pattern": "foo", "-i": true, "output_mode": "files_with_matches", "head_limit": 5});
        assert_eq!(normalize("search", claude), json!({"pattern": "(?i)foo", "files": true}));
    }

    #[test]
    fn other_tools_and_non_objects_pass_through() {
        let args = json!({"file_path": "a"});
        assert_eq!(normalize("fetch", args.clone()), args);
        assert_eq!(normalize("read", json!("x")), json!("x"));
    }
}
