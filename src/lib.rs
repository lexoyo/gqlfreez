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
mod endpoint;
mod fetch;
mod output;
mod paginate;
mod query;
mod split;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;

pub use errors::Error;
pub use logger::{Level, Logger};
pub use options::Options;
pub use split::Report as SplitReport;

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
    /// Pages written from the companion file's `split` section, when there is one.
    pub split: Option<SplitReport>,
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
///
/// Pages split from a result are only ever written, or removed, under `options.root`.
pub async fn run(options: &Options) -> Result<Vec<FileResult>, Error> {
    options.validate()?;

    let root = options.root.canonicalize().map_err(|source| Error::Io {
        path: options.root.clone(),
        source,
    })?;
    let queries = discover::find(&options.root, &options.glob)?;
    // Companions are read before any request: a typo in one must not cost a full fetch.
    let mut splits = vec![];
    for q in &queries {
        splits.push(match &q.companion {
            Some(c) => split::load(c, &q.source, &root)?.map(Arc::new),
            None => None,
        });
    }
    if queries.is_empty() {
        return Err(Error::Config(format!(
            "no query file under {} matching {:?}",
            options.root.display(),
            options.glob
        )));
    }
    let targets = endpoint::resolve(&queries, options, &root)?;
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
    let jobs = queries.iter().cloned().zip(targets).zip(splits);
    for (index, ((q, target), split)) in jobs.enumerate() {
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
            let (outcome, plan) = one(&client, &options, &target, &q, split.as_deref()).await;
            if matches!(outcome, Outcome::Failed(_)) {
                stop.store(true, Ordering::Relaxed);
            }
            Some((
                FileResult {
                    index,
                    source: q.source,
                    output: q.output,
                    split: plan.as_ref().map(split::Plan::report),
                    outcome,
                },
                plan,
            ))
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
    results.sort_by_key(|(r, _)| r.index);
    let (mut results, plans): (Vec<FileResult>, Vec<Option<split::Plan>>) =
        results.into_iter().unzip();

    commit(&mut results, &plans, options);
    Ok(results)
}

/// Fetch one file, and plan its pages if it has a companion. The pages are kept in memory
/// until the commit, so that they are written all together or not at all.
async fn one(
    client: &reqwest::Client,
    options: &Options,
    target: &endpoint::Target,
    q: &discover::Query,
    split: Option<&split::Split>,
) -> (Outcome, Option<split::Plan>) {
    let (data, outcome) = match fetch(client, options, target, q).await {
        Fetched::Data(data, outcome) => (data, outcome),
        Fetched::Done(Outcome::Skipped { reason }) if split.is_some() => {
            let error = Error::Split {
                path: q.companion.clone().unwrap_or_default(),
                message: format!("nothing to split, the query is skipped ({reason})"),
            };
            return (Outcome::Failed(error), None);
        }
        Fetched::Done(outcome) => return (outcome, None),
    };
    let Some(split) = split else {
        return (outcome, None);
    };
    match split.plan(&data, &options.logger) {
        Ok(plan) => (outcome, Some(plan)),
        Err(e) => (Outcome::Failed(e), None),
    }
}

enum Fetched {
    /// The query ran. The result is the one before `--envelope`, which is what a split
    /// reads, so that its `list` keeps one meaning whatever the flags.
    Data(Value, Outcome),
    /// Skipped or failed: nothing to split.
    Done(Outcome),
}

/// The rendered JSON is staged in a temporary file, so memory stays bounded to a single
/// response rather than the whole tree.
async fn fetch(
    client: &reqwest::Client,
    options: &Options,
    target: &endpoint::Target,
    q: &discover::Query,
) -> Fetched {
    use Fetched::Done;
    let source = match std::fs::read_to_string(&q.source) {
        Ok(s) => s,
        Err(source_err) => {
            return Done(Outcome::Failed(Error::Io {
                path: q.source.clone(),
                source: source_err,
            }))
        }
    };

    let analysis = match query::analyze(&source, &q.source) {
        Ok(a) => a,
        Err(e) => return Done(Outcome::Failed(e)),
    };
    match analysis.kind {
        query::Kind::FragmentsOnly => {
            return Done(Outcome::Skipped {
                reason: "fragments only, no operation".into(),
            })
        }
        query::Kind::Empty => {
            return Done(Outcome::Skipped {
                reason: "no definition".into(),
            })
        }
        query::Kind::Operation => {}
    }

    let deadline = tokio::time::timeout(
        options.file_timeout,
        paginate::run(
            fetch::Remote { client, target },
            options,
            &q.source,
            &analysis,
        ),
    );
    let (data, pages) = match deadline.await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Done(Outcome::Failed(e)),
        Err(_) => {
            return Done(Outcome::Failed(Error::Response {
                path: q.source.clone(),
                message: format!(
                    "gave up after --file-timeout ({}s)",
                    options.file_timeout.as_secs()
                ),
            }))
        }
    };

    // `--envelope` restores the GraphQL shape. `extensions` is dropped either way: Shopify
    // puts request costs in it and Apollo puts tracing, which would make every diff noisy.
    let rendered = if options.envelope {
        output::render(&serde_json::json!({ "data": data }))
    } else {
        output::render(&data)
    };
    let outcome = match output::stage(&q.output, &rendered) {
        Ok(output::Staged::Unchanged) => Outcome::Unchanged,
        Ok(output::Staged::Ready) if options.dry_run || options.check => Outcome::WouldChange,
        Ok(output::Staged::Ready) => Outcome::Frozen { pages },
        Err(e) => return Done(Outcome::Failed(e)),
    };
    Fetched::Data(data, outcome)
}

/// Move every staged file into place, then write the split pages. Nothing is written before
/// this point, so a failure in the fetch phase leaves the tree untouched.
fn commit(results: &mut [FileResult], plans: &[Option<split::Plan>], options: &Options) {
    let with_plans: Vec<(usize, &split::Plan)> = plans
        .iter()
        .enumerate()
        .filter_map(|(i, p)| p.as_ref().map(|p| (i, p)))
        .collect();
    if let Some((i, message)) = split::check_overlap(&with_plans) {
        let path = results[i].source.clone();
        results[i].outcome = Outcome::Failed(Error::Split { path, message });
    }

    let failed = results.iter().any(FileResult::is_failure);
    if failed || options.check || options.dry_run {
        for r in results.iter() {
            output::discard(&r.output);
        }
        return;
    }
    if let Err((i, e)) = split::stage(&with_plans) {
        results[i].outcome = Outcome::Failed(e);
        for r in results.iter() {
            output::discard(&r.output);
        }
        return;
    }
    for r in results.iter_mut() {
        if matches!(r.outcome, Outcome::Frozen { .. }) {
            if let Err(e) = output::commit(&r.output) {
                r.outcome = Outcome::Failed(e);
            }
        }
    }
    if let Err((i, e)) = split::finish(&with_plans) {
        results[i].outcome = Outcome::Failed(e);
    }
}
