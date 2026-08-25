use std::path::PathBuf;
use std::time::Duration;

use crate::logger::Logger;

/// Engine configuration. The CLI builds it from clap + graphql-config + the environment.
#[derive(Debug, Clone)]
pub struct Options {
    pub root: PathBuf,
    pub glob: String,
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
    pub concurrency: usize,
    pub page_size: usize,
    pub retries: u32,
    pub delay: Duration,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub file_timeout: Duration,
    pub max_pages: usize,
    pub max_response_bytes: u64,
    pub allow_partial: bool,
    pub envelope: bool,
    pub force: bool,
    pub check: bool,
    pub dry_run: bool,
    pub logger: Logger,
}

pub mod defaults {
    use std::time::Duration;

    pub const GLOB: &str = "**/*.{graphql,gql}";
    pub const CONCURRENCY: usize = 1;
    pub const PAGE_SIZE: usize = 100;
    pub const RETRIES: u32 = 2;
    pub const DELAY: Duration = Duration::from_millis(0);
    pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
    pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
    pub const FILE_TIMEOUT: Duration = Duration::from_secs(300);
    pub const MAX_PAGES: usize = 20;
    pub const MAX_RESPONSE_BYTES: u64 = 50 * 1024 * 1024;
}

impl Options {
    /// Reject values that would hang or make no sense, rather than discovering it at runtime
    /// (a `concurrency` of 0 would block on the semaphore until the CI runner times out).
    pub fn validate(&self) -> Result<(), crate::errors::Error> {
        use crate::errors::Error;
        if self.concurrency == 0 {
            return Err(Error::Config("--concurrency must be at least 1".into()));
        }
        if self.page_size == 0 {
            return Err(Error::Config("--page-size must be at least 1".into()));
        }
        if self.max_pages == 0 {
            return Err(Error::Config("--max-pages must be at least 1".into()));
        }
        if self.endpoint.trim().is_empty() {
            return Err(Error::Config(
                "no GraphQL endpoint. Pass --endpoint, or set one in graphql-config \
                 (extensions.endpoints.default.url, or a schema URL)."
                    .into(),
            ));
        }
        if !self.endpoint.starts_with("http://") && !self.endpoint.starts_with("https://") {
            return Err(Error::Config(format!(
                "the endpoint must be an http(s) URL, got {:?}",
                self.endpoint
            )));
        }
        Ok(())
    }
}
