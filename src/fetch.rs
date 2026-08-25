//! Talking to the GraphQL endpoint.

use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::errors::Error;
use crate::options::Options;

/// GraphQL error codes worth retrying. Everything else is deterministic: a validation
/// error will fail identically three times in a row.
const RETRYABLE_CODES: &[&str] = &[
    "RATE_LIMITED",
    "THROTTLED",
    "TIMEOUT",
    "SERVICE_UNAVAILABLE",
    "INTERNAL_SERVER_ERROR",
];

#[derive(Debug, Deserialize)]
pub struct GraphQlError {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub path: Option<Vec<Value>>,
    #[serde(default)]
    pub extensions: Map<String, Value>,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
}

impl GraphQlError {
    fn code(&self) -> Option<String> {
        self.kind.clone().or_else(|| {
            self.extensions
                .get("code")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
    }

    fn retryable(&self) -> bool {
        self.code()
            .map(|c| RETRYABLE_CODES.contains(&c.to_ascii_uppercase().as_str()))
            .unwrap_or(false)
    }
}

#[derive(Debug, Deserialize)]
struct GraphQlResponse {
    #[serde(default)]
    data: Option<Value>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

pub fn client(options: &Options) -> Result<reqwest::Client, Error> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &options.headers {
        let name: reqwest::header::HeaderName = name
            .parse()
            .map_err(|_| Error::Config(format!("invalid header name {name:?}")))?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| Error::Config(format!("invalid value for header {name}")))?;
        headers.insert(name, value);
    }
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static(
            "application/graphql-response+json, application/json;q=0.9",
        ),
    );

    reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(options.connect_timeout)
        .timeout(options.request_timeout)
        // A 301 turns a POST into a GET and yields the site's home page as HTML with a 200.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("gqlfreez/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| Error::Config(format!("could not build the HTTP client: {e}")))
}

/// One request, with retries on transient failures only.
pub async fn execute(
    client: &reqwest::Client,
    options: &Options,
    file: &Path,
    query: &str,
    operation: Option<&str>,
    variables: Map<String, Value>,
) -> Result<Value, Error> {
    let mut attempt = 0u32;
    loop {
        match once(client, options, file, query, operation, &variables).await {
            Ok(value) => return Ok(value),
            Err(Fault::Fatal(error)) => return Err(error),
            Err(Fault::Transient { error, wait }) => {
                attempt += 1;
                if attempt > options.retries {
                    return Err(error);
                }
                let delay = wait.unwrap_or(Duration::from_secs(2));
                options.logger.verbose(&format!(
                    "{}: {error} — retrying in {}s ({}/{})",
                    file.display(),
                    delay.as_secs(),
                    attempt,
                    options.retries
                ));
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// A failure, and whether waiting could fix it.
enum Fault {
    Fatal(Error),
    Transient {
        error: Error,
        /// What the server asked us to wait, when it said so.
        wait: Option<Duration>,
    },
}

impl Fault {
    fn transient(error: Error, wait: Option<Duration>) -> Self {
        Fault::Transient { error, wait }
    }
}

async fn once(
    client: &reqwest::Client,
    options: &Options,
    file: &Path,
    query: &str,
    operation: Option<&str>,
    variables: &Map<String, Value>,
) -> Result<Value, Fault> {
    let mut body = Map::new();
    body.insert("query".into(), Value::String(query.to_owned()));
    if !variables.is_empty() {
        body.insert("variables".into(), Value::Object(variables.clone()));
    }
    // Never send an empty operationName: servers treat it as "no operation with that name".
    if let Some(name) = operation.filter(|n| !n.is_empty()) {
        body.insert("operationName".into(), Value::String(name.to_owned()));
    }

    let response = client
        .post(&options.endpoint)
        .json(&Value::Object(body))
        .send()
        .await
        .map_err(|e| {
            Fault::transient(
                Error::Response {
                    path: file.to_owned(),
                    message: format!("{e}"),
                },
                None,
            )
        })?;

    let status = response.status();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs);

    if status.is_redirection() {
        let target = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<no Location header>")
            .to_owned();
        return Err(Fault::Fatal(Error::Response {
            path: file.to_owned(),
            message: format!(
                "the endpoint redirects to {target} (HTTP {status}). \
                 A redirected POST silently becomes a GET — fix the URL in your config."
            ),
        }));
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let graphql_media_type = content_type.starts_with("application/graphql-response+json");

    let bytes = read_capped(response, options, file).await?;

    // A WAF, a maintenance page or a proxy answers HTML with a 200.
    if !content_type.contains("json") {
        let head: String = String::from_utf8_lossy(&bytes).chars().take(200).collect();
        return Err(Fault::transient(
            Error::Response {
                path: file.to_owned(),
                message: format!(
                    "expected JSON, got HTTP {status} with Content-Type {:?}.\nFirst bytes: {head}",
                    if content_type.is_empty() {
                        "<none>"
                    } else {
                        &content_type
                    }
                ),
            },
            retry_after,
        ));
    }

    let parsed: GraphQlResponse = serde_json::from_slice(&bytes).map_err(|e| {
        Fault::Fatal(Error::Response {
            path: file.to_owned(),
            message: format!("HTTP {status}: malformed JSON: {e}"),
        })
    })?;

    // Per GraphQL-over-HTTP, a response in the graphql media type is read regardless of the
    // status code; a legacy `application/json` response carries its meaning in the body too.
    if !graphql_media_type && status.is_server_error() {
        return Err(Fault::transient(
            Error::Response {
                path: file.to_owned(),
                message: format!("HTTP {status} from the server"),
            },
            retry_after,
        ));
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return Err(Fault::transient(
            Error::Response {
                path: file.to_owned(),
                message: "HTTP 429 Too Many Requests".into(),
            },
            retry_after,
        ));
    }

    if !parsed.errors.is_empty() {
        let retryable = parsed.errors.iter().any(GraphQlError::retryable);
        let partial = parsed.data.as_ref().is_some_and(|d| !d.is_null());
        let summary = summarize(&parsed.errors);

        if retryable {
            return Err(Fault::transient(
                Error::GraphQl {
                    path: file.to_owned(),
                    message: summary,
                },
                retry_after,
            ));
        }
        // Field errors: `data` is there, some fields are null. Fatal unless allowed.
        if !(partial && options.allow_partial) {
            return Err(Fault::Fatal(Error::GraphQl {
                path: file.to_owned(),
                message: summary,
            }));
        }
        options
            .logger
            .warn(&format!("{}: {summary}", file.display()));
    }

    match parsed.data {
        Some(data) if !data.is_null() => Ok(data),
        _ => Err(Fault::Fatal(Error::GraphQl {
            path: file.to_owned(),
            message: if parsed.errors.is_empty() {
                format!("HTTP {status}: the response has no `data`")
            } else {
                summarize(&parsed.errors)
            },
        })),
    }
}

fn summarize(errors: &[GraphQlError]) -> String {
    let mut lines: Vec<String> = errors
        .iter()
        .take(5)
        .map(|e| {
            let at = e
                .path
                .as_ref()
                .map(|p| {
                    let parts: Vec<String> = p.iter().map(render_path_segment).collect();
                    format!(" at {}", parts.join("."))
                })
                .unwrap_or_default();
            let code = e.code().map(|c| format!(" [{c}]")).unwrap_or_default();
            format!("{}{at}{code}", e.message)
        })
        .collect();
    if errors.len() > 5 {
        lines.push(format!("… and {} more", errors.len() - 5));
    }
    lines.join("; ")
}

fn render_path_segment(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Read the body, stopping as soon as the cap is passed rather than after allocating it.
async fn read_capped(
    response: reqwest::Response,
    options: &Options,
    file: &Path,
) -> Result<Vec<u8>, Fault> {
    let cap = options.max_response_bytes;
    let mut out: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            Fault::Fatal(Error::Response {
                path: file.to_owned(),
                message: format!("while reading the response: {e}"),
            })
        })?;
        out.extend_from_slice(&chunk);
        // Counted after decompression: a 5 MB gzip response can expand to gigabytes.
        if out.len() as u64 > cap {
            return Err(Fault::Fatal(Error::Response {
                path: file.to_owned(),
                message: format!(
                    "the response went past --max-response-bytes ({cap} bytes). \
                     Narrow the query, or raise the limit."
                ),
            }));
        }
    }
    Ok(out)
}
