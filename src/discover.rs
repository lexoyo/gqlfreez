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

#[derive(Debug, Clone)]
pub struct Query {
    pub source: PathBuf,
    pub output: PathBuf,
}

/// Walk `root` for query files, sorted, with output collisions rejected up front.
pub fn find(root: &Path, glob: &str) -> Result<Vec<Query>, Error> {
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

    let queries: Vec<Query> = found
        .into_iter()
        .map(|source| {
            let output = source.with_extension("json");
            Query { source, output }
        })
        .collect();

    check_collisions(&queries)?;
    Ok(queries)
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
        let dir = std::env::temp_dir().join(format!("gqlfreez-test-{name}"));
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
}
