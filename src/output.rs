//! Writing results: stable serialization, conditional and atomic.

use std::path::Path;

use serde_json::Value;

use crate::errors::Error;

/// Serialize with sorted keys and a trailing newline, so Git diffs stay stable whatever
/// order the server (or an intermediate proxy) chose.
pub fn render(value: &Value) -> String {
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(
        &mut buf,
        serde_json::ser::PrettyFormatter::with_indent(b"  "),
    );
    sorted(value)
        .serialize(&mut ser)
        .expect("in-memory serialization");
    let mut out = String::from_utf8(buf).expect("serde_json emits utf-8");
    out.push('\n');
    out
}

use serde::Serialize;

pub(crate) fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for k in keys {
                out.insert(k.clone(), sorted(&map[k]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

#[derive(Debug, PartialEq)]
pub enum Staged {
    /// A temporary file is waiting to be moved into place.
    Ready,
    Unchanged,
}

/// How a file on disk is compared with what would replace it.
pub(crate) enum Compare {
    Json,
    /// Line endings aside.
    Text,
}

fn temp_for(path: &Path) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".gqlfreez-tmp");
    path.with_file_name(name)
}

/// Move a staged file into place. Atomic on the same filesystem.
pub fn commit(path: &Path) -> Result<(), Error> {
    let tmp = temp_for(path);
    if !tmp.exists() {
        return Ok(());
    }
    std::fs::rename(&tmp, path).map_err(|source| {
        let _ = std::fs::remove_file(&tmp);
        Error::Io {
            path: path.to_owned(),
            source,
        }
    })
}

/// Drop a staged file without touching the destination.
pub fn discard(path: &Path) {
    let _ = std::fs::remove_file(temp_for(path));
}

/// Write only when the content actually differs, and never leave a truncated file behind.
pub fn stage(path: &Path, rendered: &str) -> Result<Staged, Error> {
    if same(path, rendered, Compare::Json) {
        return Ok(Staged::Unchanged);
    }
    write_temp(path, rendered)?;
    Ok(Staged::Ready)
}

/// The comparison ignores CRLF: on Windows with `core.autocrlf`, a committed file comes back
/// with CRLF and a byte comparison would rewrite it on every single run.
pub(crate) fn same(path: &Path, rendered: &str, compare: Compare) -> bool {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return false;
    };
    match compare {
        Compare::Json => {
            let parse = |text: &str| serde_json::from_str::<Value>(text).ok();
            parse(&existing).is_some_and(|parsed| parse(rendered) == Some(parsed))
        }
        Compare::Text => existing.replace("\r\n", "\n") == rendered,
    }
}

/// The temporary file `commit` moves into place, written whatever is on disk.
pub(crate) fn write_temp(path: &Path, rendered: &str) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            path: parent.to_owned(),
            source,
        })?;
    }

    let tmp = temp_for(path);
    std::fs::write(&tmp, rendered).map_err(|source| Error::Io {
        path: tmp.clone(),
        source,
    })
}

/// Remove stale temporary files left by an interrupted run.
pub fn sweep_temporaries(paths: &[std::path::PathBuf]) {
    for p in paths {
        discard(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_are_sorted_and_file_ends_with_newline() {
        let out = render(&json!({"b": 1, "a": {"d": 2, "c": 3}}));
        assert_eq!(
            out,
            "{\n  \"a\": {\n    \"c\": 3,\n    \"d\": 2\n  },\n  \"b\": 1\n}\n"
        );
    }

    #[test]
    fn identical_content_is_not_rewritten() {
        let dir = std::env::temp_dir().join("gqlfreez-test-write");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.json");
        let body = render(&json!({"a": 1}));
        assert_eq!(stage(&p, &body).unwrap(), Staged::Ready);
        commit(&p).unwrap();
        assert_eq!(stage(&p, &body).unwrap(), Staged::Unchanged);
        assert_eq!(stage(&p, &render(&json!({"a": 2}))).unwrap(), Staged::Ready);
        commit(&p).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            render(&json!({"a": 2}))
        );
    }

    #[test]
    fn crlf_on_disk_does_not_force_a_rewrite() {
        let dir = std::env::temp_dir().join("gqlfreez-test-crlf");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("y.json");
        let body = render(&serde_json::json!({"a": 1}));
        std::fs::write(&p, body.replace('\n', "\r\n")).unwrap();
        assert_eq!(stage(&p, &body).unwrap(), Staged::Unchanged);
    }
}
