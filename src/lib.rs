//! Freeze GraphQL query results into JSON files, next to the queries.
//!
//! This library is the engine. It never decides to stop the program: it returns one result
//! per file, and the caller (the CLI, or a future service mode) decides what to do with them.
//! Failing fast is a CLI policy, not an engine behaviour.

pub mod config;
pub mod errors;
pub mod logger;
pub mod options;

mod discover;
mod fetch;
mod output;
mod paginate;
mod query;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub use errors::Error;
pub use logger::{Level, Logger};
pub use options::Options;

/// Outcome for one `.graphql` file, successful or not.
#[derive(Debug)]
pub struct FileResult {
    /// Position in the discovery order, so failures can be reported deterministically.
    pub index: usize,
    /// The source file.
    pub source: PathBuf,
    /// The output file it maps to.
    pub output: PathBuf,
    pub outcome: Outcome,
}

#[derive(Debug)]
pub enum Outcome {
    /// Written: the result differs from what was on disk.
    Frozen {
        pages: usize,
    },
    /// Not written: identical to what was already there (incremental build).
    Unchanged,
    /// Would have been written — `--check` / `--dry-run`.
    WouldChange,
    /// Skipped: the file holds no operation (fragments only).
    Skipped {
        reason: String,
    },
    Failed(Error),
}

impl FileResult {
    pub fn is_failure(&self) -> bool {
        matches!(self.outcome, Outcome::Failed(_))
    }
}

/// Run every query found under `options.root`.
///
/// Results come back in discovery order. The engine never aborts on its own — inspect
/// `Outcome::Failed` on each result.
pub async fn run(options: &Options) -> Result<Vec<FileResult>, Error> {
    options.validate()?;

    let queries = discover::find(&options.root, &options.glob)?;
    if queries.is_empty() {
        return Err(Error::Config(format!(
            "no query file under {} matching {:?}",
            options.root.display(),
            options.glob
        )));
    }
    output::sweep_temporaries(&queries.iter().map(|q| q.output.clone()).collect::<Vec<_>>());

    options.logger.status(&format!(
        "Freezing {} quer{}…",
        queries.len(),
        if queries.len() == 1 { "y" } else { "ies" }
    ));

    let client = fetch::client(options)?;
    let semaphore = Arc::new(tokio::sync::Semaphore::new(options.concurrency));
    let stop = Arc::new(AtomicBool::new(false));

    let mut tasks = tokio::task::JoinSet::new();
    for (index, q) in queries.iter().cloned().enumerate() {
        let permit = semaphore.clone();
        let client = client.clone();
        let options = options.clone();
        let stop = stop.clone();
        tasks.spawn(async move {
            let _guard = permit.acquire().await.expect("semaphore is never closed");
            // Fail fast: once something has gone wrong, do not keep hammering the API.
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            let outcome = one(&client, &options, &q).await;
            if matches!(outcome, Outcome::Failed(_)) {
                stop.store(true, Ordering::Relaxed);
            }
            Some(FileResult {
                index,
                source: q.source,
                output: q.output,
                outcome,
            })
        });
    }

    let mut results = vec![];
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Some(r)) => results.push(r),
            Ok(None) => {}
            Err(e) => {
                return Err(Error::Config(format!("internal task failure: {e}")));
            }
        }
    }
    // Discovery order, so the reported failure is the same on every run.
    results.sort_by_key(|r| r.index);

    if !options.dry_run {
        commit(&mut results, options)?;
    }
    Ok(results)
}

/// Fetch one file. The rendered JSON is staged in a temporary file, so memory stays bounded
/// to a single response rather than the whole tree.
async fn one(client: &reqwest::Client, options: &Options, q: &discover::Query) -> Outcome {
    let source = match std::fs::read_to_string(&q.source) {
        Ok(s) => s,
        Err(source_err) => {
            return Outcome::Failed(Error::Io {
                path: q.source.clone(),
                source: source_err,
            })
        }
    };

    let analysis = match query::analyze(&source, &q.source) {
        Ok(a) => a,
        Err(e) => return Outcome::Failed(e),
    };
    match analysis.kind {
        query::Kind::FragmentsOnly => {
            return Outcome::Skipped {
                reason: "fragments only, no operation".into(),
            }
        }
        query::Kind::Empty => {
            return Outcome::Skipped {
                reason: "no definition".into(),
            }
        }
        query::Kind::Operation => {}
    }

    let deadline = tokio::time::timeout(
        options.file_timeout,
        paginate::run(client, options, &q.source, &analysis),
    );
    let (data, pages) = match deadline.await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Outcome::Failed(e),
        Err(_) => {
            return Outcome::Failed(Error::Response {
                path: q.source.clone(),
                message: format!(
                    "gave up after --file-timeout ({}s)",
                    options.file_timeout.as_secs()
                ),
            })
        }
    };

    // `--envelope` restores the GraphQL shape. `extensions` is dropped either way: Shopify
    // puts request costs in it and Apollo puts tracing, which would make every diff noisy.
    let data = if options.envelope {
        serde_json::json!({ "data": data })
    } else {
        data
    };
    let rendered = output::render(&data);
    match output::stage(&q.output, &rendered) {
        Ok(output::Staged::Unchanged) => Outcome::Unchanged,
        Ok(output::Staged::Ready) if options.dry_run || options.check => Outcome::WouldChange,
        Ok(output::Staged::Ready) => Outcome::Frozen { pages },
        Err(e) => Outcome::Failed(e),
    }
}

/// Move every staged file into place. Nothing is written before this point, so a failure
/// in the fetch phase leaves the tree untouched.
fn commit(results: &mut [FileResult], options: &Options) -> Result<(), Error> {
    let failed = results.iter().any(FileResult::is_failure);
    if failed || options.check {
        for r in results.iter() {
            output::discard(&r.output);
        }
        return Ok(());
    }
    for r in results.iter_mut() {
        if matches!(r.outcome, Outcome::Frozen { .. }) {
            if let Err(e) = output::commit(&r.output) {
                r.outcome = Outcome::Failed(e);
            }
        }
    }
    Ok(())
}
