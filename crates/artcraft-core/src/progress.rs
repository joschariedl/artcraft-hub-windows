//! Progress reporting without pulling a UI framework into the core.

/// A coarse-grained progress event. UIs render it, the CLI prints it.
#[derive(Clone, Debug)]
pub enum Progress {
    /// A human readable step, already in German.
    Message(String),
    /// Bytes received while downloading.
    Download { received: u64, total: Option<u64> },
}

/// Callback used by the long-running operations.
pub type ProgressFn<'a> = &'a (dyn Fn(Progress) + Send + Sync);

/// A callback that ignores everything, for tests and one-shot calls.
pub fn silent(_: Progress) {}

pub fn message(progress: ProgressFn<'_>, text: impl Into<String>) {
    progress(Progress::Message(text.into()));
}
