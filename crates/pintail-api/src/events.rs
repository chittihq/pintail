use std::convert::Infallible;

use axum::{
    Extension,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::{
        IntoResponse as _, Response,
        sse::{Event, KeepAlive, Sse},
    },
};
use chrono::Utc;
use futures_util::{Stream, stream};
use serde::Serialize;

use crate::{ApiState, auth::AuthPrincipal, error::ApiError};

/// One live control-plane event.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ApiEvent {
    pub(crate) kind: String,
    pub(crate) database_id: Option<String>,
    pub(crate) table: Option<String>,
    pub(crate) message: String,
    pub(crate) rows: Option<u64>,
    pub(crate) bytes: Option<u64>,
    pub(crate) eta_seconds: Option<u64>,
    pub(crate) at: String,
}

impl ApiEvent {
    pub(crate) fn database(kind: &str, database_id: &str, message: impl Into<String>) -> Self {
        Self {
            kind: kind.to_owned(),
            database_id: Some(database_id.to_owned()),
            table: None,
            message: message.into(),
            rows: None,
            bytes: None,
            eta_seconds: None,
            at: Utc::now().to_rfc3339(),
        }
    }
}

pub(crate) async fn sse(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    principal.require_scope("read")?;
    let receiver = state.subscribe()?;
    let database_scope = principal.database_scope().map(str::to_owned);
    let stream = stream::unfold(
        (receiver, database_scope),
        |(mut receiver, database_scope)| async move {
            loop {
                match receiver.recv().await {
                    Ok(event)
                        if visible_to_database(
                            event.database_id.as_deref(),
                            database_scope.as_deref(),
                        ) =>
                    {
                        let event = Event::default()
                            .event(event.kind.clone())
                            .json_data(event)
                            .unwrap_or_else(|error| {
                                Event::default()
                                    .event("error")
                                    .data(format!("event encoding failed: {error}"))
                            });
                        return Some((Ok(event), (receiver, database_scope)));
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

pub(crate) async fn websocket(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    principal.require_scope("read")?;
    let receiver = state.subscribe()?;
    let database_scope = principal.database_scope().map(str::to_owned);
    Ok(upgrade
        .on_upgrade(move |socket| send_events(socket, receiver, database_scope))
        .into_response())
}

async fn send_events(
    mut socket: WebSocket,
    mut receiver: tokio::sync::broadcast::Receiver<ApiEvent>,
    database_scope: Option<String>,
) {
    loop {
        let event = match receiver.recv().await {
            Ok(event) => event,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };
        if !visible_to_database(event.database_id.as_deref(), database_scope.as_deref()) {
            continue;
        }
        let Ok(encoded) = serde_json::to_string(&event) else {
            continue;
        };
        if socket.send(Message::Text(encoded.into())).await.is_err() {
            break;
        }
    }
}

fn visible_to_database(event_database: Option<&str>, database_scope: Option<&str>) -> bool {
    database_scope.is_none_or(|allowed| event_database == Some(allowed))
}

#[cfg(test)]
mod tests {
    use super::visible_to_database;

    #[test]
    fn database_scoped_streams_hide_global_and_cross_database_events() {
        assert!(visible_to_database(Some("db-a"), None));
        assert!(visible_to_database(None, None));
        assert!(visible_to_database(Some("db-a"), Some("db-a")));
        assert!(!visible_to_database(Some("db-b"), Some("db-a")));
        assert!(!visible_to_database(None, Some("db-a")));
    }
}
