//! The steps of a long admin action, shown in the app while it runs.
//!
//! The app passes a [`Sink`] with each admin call ([`crate::Portal::admin_with_progress`]);
//! code anywhere under that call reports steps with [`step`]. Outside an admin
//! call (background jobs, the web portal) reporting does nothing.
//!
//! The sink lives in a task-local, so it does not cross `tokio::spawn` or
//! `spawn_blocking` by itself: take it with [`current`] before handing work to
//! another task or thread and report there with [`report`].

use std::sync::Arc;

/// Receives step texts, e.g. "Closing Miracle… (4 s)".
pub type Sink = Arc<dyn Fn(String) + Send + Sync>;

tokio::task_local! {
    static SINK: Sink;
}

/// Run `f` with `sink` receiving the steps it reports.
pub async fn scope<F: std::future::Future>(sink: Sink, f: F) -> F::Output {
    SINK.scope(sink, f).await
}

/// Report a step of the current admin action (no-op outside one).
pub fn step(text: impl Into<String>) {
    let _ = SINK.try_with(|sink| sink(text.into()));
}

/// The current admin action's sink, to report from another task or thread.
pub fn current() -> Option<Sink> {
    SINK.try_with(Sink::clone).ok()
}

/// Report a step through a sink taken with [`current`].
pub fn report(sink: &Option<Sink>, text: impl Into<String>) {
    if let Some(sink) = sink {
        sink(text.into());
    }
}
