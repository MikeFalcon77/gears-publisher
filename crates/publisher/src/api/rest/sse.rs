//! `GET /publisher/v1/runs/{id}/stream?after=<seq>`: Server-Sent Events.
//!
//! Sends stored lines after `after` first, then live ones, so a client that reconnects
//! with its last `seq` misses nothing. `run` events carry the run's progress whenever
//! it changes (steps report through the journal; engine transitions are polled).
//!
//! The stream ends with an `end` event once the run is settled (finished, failed or
//! waiting for a person), and in any case after [`MAX_LIFETIME`]: the client then
//! reconnects with a fresh ID token, so access is re-checked regularly.
use super::AppState;
use super::dto::LogLineDto;
use super::error::ApiError;
use crate::domain::models::{LiveEvent, LogLine};
use axum::{
    Extension,
    extract::{Path, Query},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use serde::Deserialize;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast::error::RecvError, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use toolkit_security::SecurityContext;
use uuid::Uuid;

const PAGE: u64 = 1000;
const POLL: Duration = Duration::from_secs(2);
const MAX_LIFETIME: Duration = Duration::from_mins(10);
/// Lines a step writes just before it settles may still be in the log writer's queue.
const SETTLE_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct StreamQuery {
    after: i64,
}

pub async fn stream_run(
    Extension(app): Extension<Arc<AppState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<Uuid>,
    Query(q): Query<StreamQuery>,
) -> Result<Response, ApiError> {
    let person = app.access.person(&ctx).await?;
    // Fails with 404 before the stream starts, also for a run the caller may not see.
    let first = app.service.progress_for(&person, id).await?;
    let (tx, rx) = mpsc::channel::<Event>(256);
    tokio::spawn(pump(app, id, q.after, first, tx));
    let stream = futures_util::StreamExt::map(ReceiverStream::new(rx), Ok::<_, Infallible>);
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response())
}

fn run_event(progress: &impl serde::Serialize) -> Option<Event> {
    Event::default().event("run").json_data(progress).ok()
}

fn log_event(line: &LogLine) -> Option<Event> {
    Event::default()
        .event("log")
        .id(line.seq.to_string())
        .json_data(LogLineDto::from(line.clone()))
        .ok()
}

fn end_event() -> Event {
    Event::default().event("end").data("settled")
}

/// Ends when the run settles, after [`MAX_LIFETIME`], or when the client goes away.
async fn pump(
    app: Arc<AppState>,
    id: Uuid,
    mut after: i64,
    first: toolkit_durable_execution::observation::RunProgress,
    tx: mpsc::Sender<Event>,
) {
    let service = &app.service;
    // Subscribe before reading the backlog so that nothing falls in between.
    let mut live = service.subscribe();
    let mut cursor = (first.cursor, first.updated_at);
    let Some(event) = run_event(&first) else {
        return;
    };
    if tx.send(event).await.is_err() {
        return;
    }

    let backlog = async |after: &mut i64| -> Result<(), ()> {
        loop {
            let lines = service.lines(id, *after, PAGE).await.map_err(|_| ())?;
            let n = lines.len();
            for line in lines {
                *after = line.seq;
                if let Some(event) = log_event(&line) {
                    tx.send(event).await.map_err(|_| ())?;
                }
            }
            if (n as u64) < PAGE {
                return Ok(());
            }
        }
    };
    if backlog(&mut after).await.is_err() {
        return;
    }
    if first.state.is_settled() {
        let _ = tx.send(end_event()).await;
        return;
    }

    let deadline = tokio::time::sleep(MAX_LIFETIME);
    tokio::pin!(deadline);
    let mut poll = tokio::time::interval(POLL);
    loop {
        let refresh = tokio::select! {
            () = &mut deadline => return,
            _ = poll.tick() => true,
            received = live.recv() => match received {
                Ok(LiveEvent::Log(entry)) if entry.run_id == id && entry.seq > after => {
                    after = entry.seq;
                    if let Some(event) = log_event(&entry)
                        && tx.send(event).await.is_err()
                    {
                        return;
                    }
                    false
                }
                Ok(LiveEvent::Changed(run)) => run == id,
                Ok(LiveEvent::Log(_)) => false,
                Err(RecvError::Lagged(_)) => {
                    if backlog(&mut after).await.is_err() {
                        return;
                    }
                    true
                }
                Err(RecvError::Closed) => return,
            },
        };
        if refresh && let Ok(progress) = service.progress(id).await {
            let now = (progress.cursor, progress.updated_at);
            if now != cursor {
                cursor = now;
                if let Some(event) = run_event(&progress)
                    && tx.send(event).await.is_err()
                {
                    return;
                }
            }
            if progress.state.is_settled() {
                tokio::time::sleep(SETTLE_GRACE).await;
                if backlog(&mut after).await.is_ok() {
                    let _ = tx.send(end_event()).await;
                }
                return;
            }
        }
        if tx.is_closed() {
            return;
        }
    }
}
