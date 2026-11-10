//! Which endpoint each query file talks to, and with which headers.
//!
//! Each file takes the graphql-config closest to it, so that one run can freeze several data
//! sources. `--endpoint` applies to every file, with the `--header` values only: the configs
//! are still read for their collections, but no URL or header of theirs is used, so a token
//! meant for another server never reaches the forced one.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

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

/// The config of each query, in the same order. Read before any request: a broken config
/// must not surface after half the files were fetched.
pub fn configs(queries: &[Query]) -> Result<Vec<Arc<Resolved>>, Error> {
    let mut by_dir: HashMap<&Path, Arc<Resolved>> = HashMap::new();
    let mut configs = vec![];
    for q in queries {
        let dir = q.source.parent().unwrap_or(Path::new("/"));
        let config = match by_dir.get(dir) {
            Some(c) => c.clone(),
            None => {
                let c = Arc::new(config::discover(dir)?);
                by_dir.insert(dir, c.clone());
                c
            }
        };
        configs.push(config);
    }
    Ok(configs)
}

/// One target per query, in the same order.
pub fn resolve(
    queries: &[Query],
    configs: &[Arc<Resolved>],
    options: &Options,
) -> Result<Vec<Target>, Error> {
    if let Some(url) = &options.endpoint {
        let target = target(url.clone(), &[], &options.headers)?;
        return Ok(vec![target; queries.len()]);
    }

    let mut targets = vec![];
    for (q, config) in queries.iter().zip(configs) {
        let Some(url) = config.endpoint.clone() else {
            return Err(Error::Config(format!(
                "{}: no GraphQL endpoint. Pass --endpoint, or put a graphql-config \
                 (.graphqlrc.yml…) in its folder or above, with extensions.endpoints.default.url \
                 or a schema URL.",
                q.source.display()
            )));
        };
        if !config::is_url(&url) {
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
        targets.push(target(url, &config.headers, &options.headers)?);
    }
    Ok(targets)
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
