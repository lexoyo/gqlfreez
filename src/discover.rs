//! Finding query files, and mapping each to its output path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

use crate::errors::Error;

/// Directories never worth walking, whether or not a `.gitignore` says so.
const ALWAYS_EXCLUDE: &[&str] = &[
    "!**/node_modules/**",
    "!**/.git/**",
    "!**/target/**",
    "!**/dist/**",
    "!**/.next/**",
    "!**/.nuxt/**",
    "!**/.astro/**",
    "!**/.svelte-kit/**",
    "!**/vendor/**",
];

/// Extensions a companion file may have, in the order they are looked up.
pub const COMPANION_EXTENSIONS: &[&str] = &["json", "yaml", "yml"];
const COMPANION_GLOB: &str = "**/*.gqlfreez.{json,yaml,yml}";
const QUERY_EXTENSIONS: &[&str] = &["graphql", "gql"];

#[derive(Debug, Clone)]
pub struct Query {
    pub source: PathBuf,
    pub output: PathBuf,
    /// `<name>.gqlfreez.{json,yaml,yml}` next to the query, if there is one.
    pub companion: Option<PathBuf>,
}

/// Walk `root` for query files, sorted, with output collisions rejected up front.
pub fn find(root: &Path, glob: &str) -> Result<Vec<Query>, Error> {
    let found = walk(root, glob)?;

    let mut queries = vec![];
    for source in found {
        if stem(&source).ends_with(".gqlfreez") {
            return Err(Error::Config(format!(
                "{}: a query named *.gqlfreez.* would write over the companion file of another \
                 query — rename it",
                source.display()
            )));
        }
        let output = source.with_extension("json");
        let companion = companion_of(&source)?;
        queries.push(Query {
            source,
            output,
            companion,
        });
    }

    check_collisions(&queries)?;
    check_orphan_companions(root)?;
    Ok(queries)
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn companion_of(source: &Path) -> Result<Option<PathBuf>, Error> {
    let stem = stem(source);
    let existing: Vec<PathBuf> = COMPANION_EXTENSIONS
        .iter()
        .map(|ext| source.with_file_name(format!("{stem}.gqlfreez.{ext}")))
        .filter(|p| p.is_file())
        .collect();
    match existing.len() {
        0 => Ok(None),
        1 => Ok(existing.into_iter().next()),
        _ => Err(Error::Config(format!(
            "{} has several companion files, keep only one: {}",
            source.display(),
            existing
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// A companion whose query was renamed or deleted would otherwise be ignored without a word.
/// Checked against the disk rather than the run, so a narrower `--glob` does not trip it.
fn check_orphan_companions(root: &Path) -> Result<(), Error> {
    for companion in walk(root, COMPANION_GLOB)? {
        let name = companion
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some((query_stem, _)) = name.rsplit_once(".gqlfreez.") else {
            continue;
        };
        let has_query = QUERY_EXTENSIONS.iter().any(|ext| {
            companion
                .with_file_name(format!("{query_stem}.{ext}"))
                .is_file()
        });
        if !has_query {
            return Err(Error::Config(format!(
                "{}: no query next to it, expected {query_stem}.graphql or {query_stem}.gql",
                companion.display()
            )));
        }
    }
    Ok(())
}

fn walk(root: &Path, glob: &str) -> Result<Vec<PathBuf>, Error> {
    let mut overrides = OverrideBuilder::new(root);
    overrides
        .add(glob)
        .map_err(|e| Error::Config(format!("invalid glob {glob:?}: {e}")))?;
    for pattern in ALWAYS_EXCLUDE {
        overrides
            .add(pattern)
            .map_err(|e| Error::Config(format!("internal glob {pattern:?}: {e}")))?;
    }
    let overrides = overrides
        .build()
        .map_err(|e| Error::Config(format!("invalid glob {glob:?}: {e}")))?;

    let mut found: Vec<PathBuf> = vec![];
    for entry in WalkBuilder::new(root)
        .overrides(overrides)
        .follow_links(false)
        .hidden(false)
        .git_ignore(true)
        // .gitignore is only honoured inside a repository; the explicit excludes above are
        // what actually protects a tarball or a Docker context with no .git.
        .require_git(false)
        .build()
    {
        let entry = entry.map_err(|e| Error::Config(format!("walking {}: {e}", root.display())))?;
        if entry.file_type().is_some_and(|t| t.is_file()) {
            found.push(entry.into_path());
        }
    }

    // Deterministic order: with fail-fast, an unsorted walk reports a different error on
    // every run when several files are broken.
    found.sort();
    Ok(found)
}

/// Two sources mapping to the same `.json` — including through case-insensitive filesystems.
fn check_collisions(queries: &[Query]) -> Result<(), Error> {
    let mut seen: HashMap<String, &Path> = HashMap::new();
    for q in queries {
        let key = q.output.to_string_lossy().to_lowercase();
        if let Some(first) = seen.insert(key, &q.source) {
            return Err(Error::Config(format!(
                "{} and {} both write to {} — rename one of them",
                first.display(),
                q.source.display(),
                q.output.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/tmp")
            .join(format!("gqlfreez-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn finds_graphql_and_gql_sorted() {
        let d = scratch("find");
        write(&d, "b.graphql", "{ a }");
        write(&d, "a.gql", "{ a }");
        write(&d, "sub/c.graphql", "{ a }");
        let found = find(&d, "**/*.{graphql,gql}").unwrap();
        let names: Vec<String> = found
            .iter()
            .map(|q| {
                q.source
                    .strip_prefix(&d)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(names, vec!["a.gql", "b.graphql", "sub/c.graphql"]);
        assert!(found[0].output.ends_with("a.json"));
    }

    #[test]
    fn node_modules_is_never_walked() {
        let d = scratch("nodemodules");
        write(&d, "ok.graphql", "{ a }");
        write(&d, "node_modules/pkg/bad.graphql", "{ a }");
        let found = find(&d, "**/*.{graphql,gql}").unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn output_collision_is_refused() {
        let d = scratch("collision");
        write(&d, "x.graphql", "{ a }");
        write(&d, "x.gql", "{ a }");
        let e = find(&d, "**/*.{graphql,gql}").unwrap_err();
        assert!(format!("{e}").contains("both write to"), "{e}");
    }

    #[test]
    fn the_companion_is_found_next_to_its_query() {
        let d = scratch("companion");
        write(&d, "a.graphql", "{ a }");
        write(&d, "a.gqlfreez.yaml", "{}");
        write(&d, "b.graphql", "{ a }");
        let found = find(&d, "**/*.{graphql,gql}").unwrap();
        assert!(found[0]
            .companion
            .as_ref()
            .unwrap()
            .ends_with("a.gqlfreez.yaml"));
        assert!(found[1].companion.is_none());
    }

    #[test]
    fn two_companions_for_one_query_are_refused() {
        let d = scratch("twocompanions");
        write(&d, "a.graphql", "{ a }");
        write(&d, "a.gqlfreez.yaml", "{}");
        write(&d, "a.gqlfreez.json", "{}");
        let e = find(&d, "**/*.{graphql,gql}").unwrap_err();
        assert!(format!("{e}").contains("several companion files"), "{e}");
    }

    #[test]
    fn a_companion_without_its_query_is_refused() {
        let d = scratch("orphancompanion");
        write(&d, "a.graphql", "{ a }");
        write(&d, "gone.gqlfreez.json", "{}");
        let e = find(&d, "**/*.{graphql,gql}").unwrap_err();
        assert!(format!("{e}").contains("expected gone.graphql"), "{e}");
    }

    #[test]
    fn a_query_named_like_a_companion_is_refused() {
        let d = scratch("querycompanion");
        write(&d, "a.gqlfreez.graphql", "{ a }");
        let e = find(&d, "**/*.{graphql,gql}").unwrap_err();
        assert!(format!("{e}").contains("companion file"), "{e}");
    }
}
