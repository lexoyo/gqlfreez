//! Reading `graphql-config` (declarative variants only).
//!
//! A foreign, nested, polymorphic format: dedicated `serde` types rather than `twelf`.
//! `${VAR}` / `${VAR:default}` / `\$` interpolation happens on the raw text BEFORE
//! deserialization.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::errors::Error;

/// Search places, in order. Finding several is an error — never a silent pick.
pub const SEARCH_PLACES: &[&str] = &[
    "graphql.config.toml",
    "graphql.config.yaml",
    "graphql.config.yml",
    "graphql.config.json",
    ".graphqlrc.toml",
    ".graphqlrc.yaml",
    ".graphqlrc.yml",
    ".graphqlrc.json",
    ".graphqlrc",
];

/// Variants a binary cannot evaluate: detected so we can say so plainly.
pub const UNSUPPORTED_PLACES: &[&str] = &[
    "graphql.config.ts",
    "graphql.config.mts",
    "graphql.config.cts",
    "graphql.config.js",
    "graphql.config.mjs",
    "graphql.config.cjs",
    ".graphqlrc.ts",
    ".graphqlrc.js",
    ".graphqlrc.mjs",
    ".graphqlrc.cjs",
];

#[derive(Debug, Default, Deserialize)]
pub struct GraphqlConfig {
    pub schema: Option<Schema>,
    pub documents: Option<Documents>,
    #[serde(default)]
    pub extensions: Extensions,
    pub projects: Option<BTreeMap<String, Box<GraphqlConfig>>>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Schema {
    Url(String),
    List(Vec<Schema>),
    WithHeaders(BTreeMap<String, SchemaOptions>),
}

#[derive(Debug, Deserialize)]
pub struct SchemaOptions {
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Documents {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Default, Deserialize)]
pub struct Extensions {
    #[serde(default)]
    pub endpoints: BTreeMap<String, Endpoint>,
    /// One entry per query, keyed by its path relative to the config file's folder.
    #[serde(default)]
    pub collections: BTreeMap<String, CollectionConfig>,
}

/// One file per item of a list in the result of a query.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CollectionConfig {
    pub list: String,
    pub dir: String,
    pub file: String,
    #[serde(default)]
    pub format: Format,
    pub front_matter: serde_json::Value,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Toml,
    Yaml,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Endpoint {
    Url(String),
    Full {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

/// What the CLI actually needs out of the config.
#[derive(Debug, Default, Clone)]
pub struct Resolved {
    pub endpoint: Option<String>,
    pub headers: Vec<(String, String)>,
    pub documents: Option<String>,
    pub origin: Option<PathBuf>,
    pub collections: BTreeMap<String, CollectionConfig>,
}

/// Find and read a config file, starting at `from` and walking up to the filesystem root.
pub fn discover(from: &Path) -> Result<Resolved, Error> {
    for current in from.ancestors() {
        if let Some(found) = in_dir(current)? {
            return Ok(found);
        }
    }
    Ok(Resolved::default())
}

/// The config closest to `from`, walking up no further than `root`.
///
/// `None` when there is none between the two: the caller decides what applies then.
pub fn nearest(from: &Path, root: &Path) -> Result<Option<Resolved>, Error> {
    for current in from.ancestors().take_while(|d| d.starts_with(root)) {
        if let Some(found) = in_dir(current)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// The config file that applies to a file in `from`: the first one found walking up.
pub fn closest_file(from: &Path) -> Result<Option<PathBuf>, Error> {
    for current in from.ancestors() {
        if let Some(found) = file_in(current)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

fn in_dir(current: &Path) -> Result<Option<Resolved>, Error> {
    file_in(current)?.map(|path| read(&path)).transpose()
}

pub fn file_in(current: &Path) -> Result<Option<PathBuf>, Error> {
    let found: Vec<PathBuf> = SEARCH_PLACES
        .iter()
        .map(|n| current.join(n))
        .filter(|p| p.is_file())
        .collect();

    if found.len() > 1 {
        let names: Vec<String> = found
            .iter()
            .map(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        return Err(Error::Config(format!(
            "found several config files in {}: {} — keep only one",
            current.display(),
            names.join(", ")
        )));
    }
    if let Some(path) = found.into_iter().next() {
        return Ok(Some(path));
    }

    let unsupported: Vec<String> = UNSUPPORTED_PLACES
        .iter()
        .filter(|n| current.join(n).is_file())
        .map(|n| (*n).to_string())
        .collect();
    if !unsupported.is_empty() {
        return Err(Error::Config(format!(
            "found {} in {}, which gqlfreez cannot evaluate (it is a binary, not a JS runtime).\n\
             Use a declarative config instead: {}.",
            unsupported.join(", "),
            current.display(),
            SEARCH_PLACES.join(", ")
        )));
    }
    Ok(None)
}

pub fn read(path: &Path) -> Result<Resolved, Error> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("{}: cannot be read: {e}", path.display())))?;
    let text = interpolate(&raw, path)?;

    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let name = path.file_name().and_then(|e| e.to_str()).unwrap_or("");
    let cfg: GraphqlConfig = if ext == "toml" {
        let fail = |e: toml::de::Error| Error::Config(format!("{}: {e}", path.display()));
        let table: toml::Table = toml::from_str(&text).map_err(fail)?;
        datetimes_as_strings(toml::Value::Table(table))
            .try_into()
            .map_err(fail)?
    } else if ext == "json" || (name == ".graphqlrc" && text.trim_start().starts_with('{')) {
        serde_json::from_str(&text)
            .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?
    } else {
        serde_yaml_ng::from_str(&text)
            .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?
    };

    let cfg = flatten_projects(cfg, path)?;
    Ok(resolve(cfg, path))
}

/// A TOML date reaches serde as a private table, which would end up as is in a front matter.
fn datetimes_as_strings(value: toml::Value) -> toml::Value {
    match value {
        toml::Value::Datetime(d) => toml::Value::String(d.to_string()),
        toml::Value::Array(items) => {
            toml::Value::Array(items.into_iter().map(datetimes_as_strings).collect())
        }
        toml::Value::Table(table) => toml::Value::Table(
            table
                .into_iter()
                .map(|(k, v)| (k, datetimes_as_strings(v)))
                .collect(),
        ),
        other => other,
    }
}

/// A single project (or one named `default`) is accepted; several are not. The extensions
/// written next to `projects` apply to it, the project's own winning.
fn flatten_projects(cfg: GraphqlConfig, path: &Path) -> Result<GraphqlConfig, Error> {
    let Some(mut projects) = cfg.projects else {
        return Ok(cfg);
    };
    let mut project = if projects.len() == 1 {
        projects.into_values().next().unwrap()
    } else if let Some(default) = projects.remove("default") {
        default
    } else {
        return Err(Error::Config(format!(
            "{}: multi-project configs are not supported yet ({} projects). \
             Keep a single project.",
            path.display(),
            projects.len()
        )));
    };
    let extensions = &mut project.extensions;
    // Endpoints are not merged one by one: a root `default` would win over the project's.
    if extensions.endpoints.is_empty() {
        extensions.endpoints = cfg.extensions.endpoints;
    }
    for (key, collection) in cfg.extensions.collections {
        extensions.collections.entry(key).or_insert(collection);
    }
    Ok(*project)
}

fn resolve(cfg: GraphqlConfig, path: &Path) -> Resolved {
    let mut out = Resolved {
        origin: Some(path.to_owned()),
        collections: cfg.extensions.collections.clone(),
        ..Default::default()
    };

    // `extensions.endpoints.default` wins over `schema`, matching vscode-graphql-execution.
    if let Some(ep) = cfg
        .extensions
        .endpoints
        .get("default")
        .or_else(|| cfg.extensions.endpoints.values().next())
    {
        match ep {
            Endpoint::Url(u) => out.endpoint = Some(u.clone()),
            Endpoint::Full { url, headers } => {
                out.endpoint = Some(url.clone());
                out.headers = headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
            }
        }
    }

    if out.endpoint.is_none() {
        if let Some(schema) = &cfg.schema {
            if let Some((url, headers)) = schema_url(schema) {
                out.endpoint = Some(url);
                out.headers = headers;
            }
        }
    }

    out.documents = match cfg.documents {
        Some(Documents::One(s)) => Some(s),
        Some(Documents::Many(v)) => v.into_iter().next(),
        None => None,
    };

    out
}

fn schema_url(schema: &Schema) -> Option<(String, Vec<(String, String)>)> {
    match schema {
        Schema::Url(u) if is_url(u) => Some((u.clone(), vec![])),
        Schema::Url(_) => None,
        Schema::List(l) => l.iter().find_map(schema_url),
        Schema::WithHeaders(m) => m.iter().find(|(k, _)| is_url(k)).map(|(k, v)| {
            (
                k.clone(),
                v.headers
                    .iter()
                    .map(|(a, b)| (a.clone(), b.clone()))
                    .collect(),
            )
        }),
    }
}

fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// `${VAR}`, `${VAR:default}`, `\$` — the graphql-config syntax, applied to raw text.
///
/// A missing variable is an error naming it: substituting an empty string would produce
/// `Authorization: Bearer ` and an unreadable 401.
pub fn interpolate(text: &str, origin: &Path) -> Result<String, Error> {
    let mut out = String::with_capacity(text.len());
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '\\' && i + 1 < bytes.len() && bytes[i + 1] == '$' {
            out.push('$');
            i += 2;
            continue;
        }
        if bytes[i] == '$' && i + 1 < bytes.len() && bytes[i + 1] == '{' {
            let Some(close) = bytes[i + 2..].iter().position(|c| *c == '}') else {
                out.push('$');
                i += 1;
                continue;
            };
            let inner: String = bytes[i + 2..i + 2 + close].iter().collect();
            let (name, default) = match inner.split_once(':') {
                Some((n, d)) => (n.trim().to_string(), Some(d.to_string())),
                None => (inner.trim().to_string(), None),
            };
            match std::env::var(&name) {
                Ok(v) => out.push_str(&v),
                Err(_) => match default {
                    Some(d) => out.push_str(&d),
                    None => {
                        let line = text[..text.char_indices().nth(i).map(|(b, _)| b).unwrap_or(0)]
                            .lines()
                            .count()
                            .max(1);
                        return Err(Error::Config(format!(
                            "{}:{line}: ${{{name}}} is not set in the environment.\n\
                             Set it, add it to .env / .env.local, or give it a default with \
                             ${{{name}:value}}.",
                            origin.display()
                        )));
                    }
                },
            }
            i += 2 + close + 1;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation_handles_defaults_and_escapes() {
        std::env::set_var("GQLFREEZ_TEST_TOKEN", "secret");
        let p = Path::new("c.yml");
        assert_eq!(
            interpolate("a ${GQLFREEZ_TEST_TOKEN} b", p).unwrap(),
            "a secret b"
        );
        assert_eq!(
            interpolate("${GQLFREEZ_NOPE:fallback}", p).unwrap(),
            "fallback"
        );
        assert_eq!(interpolate(r"\${LITERAL}", p).unwrap(), "${LITERAL}");
    }

    #[test]
    fn missing_variable_names_itself() {
        let e = interpolate("${GQLFREEZ_ABSENT_VAR}", Path::new("c.yml")).unwrap_err();
        assert!(format!("{e}").contains("GQLFREEZ_ABSENT_VAR"), "{e}");
    }

    #[test]
    fn schema_with_headers_yields_endpoint() {
        let yaml = "schema:\n  - https://example.com/graphql:\n      headers:\n        Authorization: Bearer x\n";
        let cfg: GraphqlConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let r = resolve(cfg, Path::new("c.yml"));
        assert_eq!(r.endpoint.as_deref(), Some("https://example.com/graphql"));
        assert_eq!(r.headers, vec![("Authorization".into(), "Bearer x".into())]);
    }

    #[test]
    fn endpoints_extension_wins_over_schema() {
        let yaml = "schema: https://from-schema/graphql\nextensions:\n  endpoints:\n    default:\n      url: https://from-endpoints/graphql\n";
        let cfg: GraphqlConfig = serde_yaml_ng::from_str(yaml).unwrap();
        let r = resolve(cfg, Path::new("c.yml"));
        assert_eq!(
            r.endpoint.as_deref(),
            Some("https://from-endpoints/graphql")
        );
    }

    #[test]
    fn root_extensions_apply_to_the_single_project_which_wins() {
        let yaml = "projects:\n\
                    \x20 site:\n\
                    \x20   extensions:\n\
                    \x20     collections:\n\
                    \x20       a.graphql: {list: a, dir: c, file: x.md, frontMatter: {}}\n\
                    extensions:\n\
                    \x20 endpoints:\n\
                    \x20   default: https://root/graphql\n\
                    \x20 collections:\n\
                    \x20   a.graphql: {list: root, dir: c, file: x.md, frontMatter: {}}\n\
                    \x20   b.graphql: {list: b, dir: c, file: x.md, frontMatter: {}}\n";
        let p = Path::new("c.yml");
        let cfg = flatten_projects(serde_yaml_ng::from_str(yaml).unwrap(), p).unwrap();
        let r = resolve(cfg, p);
        assert_eq!(r.endpoint.as_deref(), Some("https://root/graphql"));
        assert_eq!(r.collections["a.graphql"].list, "a");
        assert_eq!(r.collections["b.graphql"].list, "b");
    }

    #[test]
    fn local_schema_file_is_not_an_endpoint() {
        let cfg: GraphqlConfig = serde_yaml_ng::from_str("schema: ./schema.graphql\n").unwrap();
        assert!(resolve(cfg, Path::new("c.yml")).endpoint.is_none());
    }
}
