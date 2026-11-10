//! Which endpoint each query file talks to, and with which headers.
//!
//! `--endpoint` applies to every file, with the `--header` values only: no graphql-config is
//! read, so a token meant for another server never reaches the forced one. Otherwise each file takes the graphql-config closest
//! to it, walking up no further than the root, so that one run can freeze several data
//! sources. A file with no config of its own falls back on the one found from the root up.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

use crate::config::{self, Resolved};
use crate::discover::Query;
use crate::errors::Error;
use crate::options::Options;

#[derive(Debug, Clone)]
pub struct Target {
    pub url: String,
    pub headers: HeaderMap,
}

/// One target per query, in the same order. Resolved before any request: a missing endpoint
/// must not surface after half the files were fetched.
pub fn resolve(queries: &[Query], options: &Options, root: &Path) -> Result<Vec<Target>, Error> {
    if let Some(url) = &options.endpoint {
        let target = target(url.clone(), &[], &options.headers)?;
        return Ok(vec![target; queries.len()]);
    }

    // Looked up only when needed: a broken config above the root must not block a run
    // where every file has its own.
    let mut root_config: Option<Resolved> = None;
    let mut from_root = || -> Result<Resolved, Error> {
        if let Some(c) = &root_config {
            return Ok(c.clone());
        }
        let c = config::discover(root)?;
        root_config = Some(c.clone());
        Ok(c)
    };

    let mut by_dir: HashMap<PathBuf, Target> = HashMap::new();
    let mut targets = vec![];
    for q in queries {
        let dir = q
            .source
            .parent()
            .and_then(|d| d.canonicalize().ok())
            .unwrap_or_else(|| root.to_owned());
        if let Some(t) = by_dir.get(&dir) {
            targets.push(t.clone());
            continue;
        }
        let config = match config::nearest(&dir, root)? {
            Some(c) => c,
            None => from_root()?,
        };
        let Some(url) = config.endpoint.clone() else {
            return Err(Error::Config(format!(
                "{}: no GraphQL endpoint. Pass --endpoint, or put a graphql-config \
                 (.graphqlrc.yml…) in its folder or above, with extensions.endpoints.default.url \
                 or a schema URL.",
                q.source.display()
            )));
        };
        if !is_http(&url) {
            return Err(Error::Config(format!(
                "{}: the endpoint must be an http(s) URL, got {url:?}",
                config.origin.as_deref().unwrap_or(&q.source).display()
            )));
        }
        options.logger.verbose(&format!(
            "{}: {url}{}",
            q.source.display(),
            config
                .origin
                .as_ref()
                .map(|o| format!(" (from {})", o.display()))
                .unwrap_or_default()
        ));
        let t = target(url, &config.headers, &options.headers)?;
        by_dir.insert(dir, t.clone());
        targets.push(t);
    }
    Ok(targets)
}

pub fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// The config's headers first, then `--header`, which wins on a duplicate name.
fn target(
    url: String,
    config: &[(String, String)],
    cli: &[(String, String)],
) -> Result<Target, Error> {
    let mut headers = HeaderMap::new();
    for (name, value) in config.iter().chain(cli) {
        let name: HeaderName = name
            .parse()
            .map_err(|_| Error::Config(format!("invalid header name {name:?}")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| Error::Config(format!("invalid value for header {name}")))?;
        headers.insert(name, value);
    }
    Ok(Target { url, headers })
}
