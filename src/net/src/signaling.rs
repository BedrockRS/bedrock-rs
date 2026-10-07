//! NetherNet HTTP signaling (`docs/ARCHITECTURE.md` §3.1).
//!
//! - `GET /v1/join` returns the server status as JSON. Clients use it as a
//!   capability probe and abort the join unless it succeeds.
//! - `POST /v1/join/{networkId}` takes the client's SDP offer and returns our SDP
//!   answer in the same response. There is exactly one request per join attempt.
//!
//! Error status codes mirror go-nethernet's handler.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio::sync::watch;

/// Largest SDP offer accepted, matching go-nethernet.
pub const MAX_OFFER_SIZE: usize = 1 << 20;

/// Server status returned by `GET /v1/join` and shown in the client's server list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub name: String,
    pub protocol: i32,
    pub version: String,
    pub level: String,
    pub players: u32,
    pub max_players: u32,
    /// 0 Survival, 1 Creative, 2 Adventure.
    pub game_type: i32,
}

/// Answers SDP offers; the listener implements this to start WebRTC sessions.
pub trait OfferHandler: Send + Sync + 'static {
    fn handle_offer(
        &self,
        network_id: u64,
        offer: String,
    ) -> impl Future<Output = Result<String, OfferError>> + Send;
}

/// Why an offer was not answered.
#[derive(Debug, thiserror::Error)]
pub enum OfferError {
    #[error("malformed offer: {0}")]
    BadOffer(String),
    #[error("identity rejected: {0}")]
    Forbidden(String),
    #[error("not accepting connections: {0}")]
    Unavailable(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl OfferError {
    fn status(&self) -> StatusCode {
        match self {
            Self::BadOffer(_) => StatusCode::BAD_REQUEST,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

struct Signaling<H> {
    handler: Arc<H>,
    status: watch::Receiver<ServerStatus>,
    negotiation_timeout: Duration,
}

/// Builds the signaling routes. Offers that take longer than
/// `negotiation_timeout` to answer fail with 502, as in go-nethernet.
pub fn router<H: OfferHandler>(
    handler: Arc<H>,
    status: watch::Receiver<ServerStatus>,
    negotiation_timeout: Duration,
) -> Router {
    let signaling = Arc::new(Signaling {
        handler,
        status,
        negotiation_timeout,
    });
    Router::new()
        .route("/v1/join", get(join_status::<H>))
        .route("/v1/join/{network_id}", post(join::<H>))
        .layer(DefaultBodyLimit::max(MAX_OFFER_SIZE))
        .with_state(signaling)
}

async fn join_status<H: OfferHandler>(
    State(signaling): State<Arc<Signaling<H>>>,
) -> Json<ServerStatus> {
    Json(signaling.status.borrow().clone())
}

async fn join<H: OfferHandler>(
    State(signaling): State<Arc<Signaling<H>>>,
    Path(network_id): Path<String>,
    offer: String,
) -> Response {
    let Ok(network_id) = network_id.parse::<u64>() else {
        return error(StatusCode::BAD_REQUEST, "invalid network id");
    };
    if offer.trim().is_empty() {
        return error(StatusCode::BAD_REQUEST, "missing SDP offer");
    }

    let negotiation = signaling.handler.handle_offer(network_id, offer);
    match tokio::time::timeout(signaling.negotiation_timeout, negotiation).await {
        Ok(Ok(answer)) => ([(header::CONTENT_TYPE, "application/sdp")], answer).into_response(),
        Ok(Err(err)) => {
            tracing::debug!(network_id, %err, "rejected NetherNet offer");
            error(err.status(), &err.to_string())
        }
        Err(_) => {
            tracing::debug!(network_id, "NetherNet negotiation timed out");
            error(StatusCode::BAD_GATEWAY, "negotiation timed out")
        }
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain")],
        message.to_owned(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request};
    use tower::ServiceExt as _;

    use super::*;

    enum Behavior {
        Answer,
        Forbid,
        Busy,
        Hang,
    }

    struct MockHandler(Behavior);

    impl OfferHandler for MockHandler {
        async fn handle_offer(&self, network_id: u64, offer: String) -> Result<String, OfferError> {
            match self.0 {
                Behavior::Answer => Ok(format!("answer to {network_id}: {} bytes", offer.len())),
                Behavior::Forbid => Err(OfferError::Forbidden("no identity".into())),
                Behavior::Busy => Err(OfferError::Unavailable("full".into())),
                Behavior::Hang => std::future::pending().await,
            }
        }
    }

    fn status() -> ServerStatus {
        ServerStatus {
            name: "BedrockRS".into(),
            protocol: 2193,
            version: "1.26.51".into(),
            level: "world".into(),
            players: 3,
            max_players: 20,
            game_type: 1,
        }
    }

    async fn call(behavior: Behavior, request: Request<Body>) -> (StatusCode, String, String) {
        let (_sender, receiver) = watch::channel(status());
        let app = router(
            Arc::new(MockHandler(behavior)),
            receiver,
            Duration::from_millis(100),
        );
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|value| value.to_str().unwrap().to_owned())
            .unwrap_or_default();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (
            status,
            content_type,
            String::from_utf8(body.to_vec()).unwrap(),
        )
    }

    fn offer(path: &str, body: impl Into<Body>) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/sdp")
            .body(body.into())
            .unwrap()
    }

    #[tokio::test]
    async fn status_is_camel_case_json() {
        let request = Request::get("/v1/join").body(Body::empty()).unwrap();
        let (status, content_type, body) = call(Behavior::Answer, request).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "name": "BedrockRS",
                "protocol": 2193,
                "version": "1.26.51",
                "level": "world",
                "players": 3,
                "maxPlayers": 20,
                "gameType": 1
            })
        );
    }

    #[tokio::test]
    async fn offer_is_answered_with_sdp() {
        let (status, content_type, body) =
            call(Behavior::Answer, offer("/v1/join/42", "v=0\r\n")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/sdp");
        assert_eq!(body, "answer to 42: 5 bytes");
    }

    #[tokio::test]
    async fn invalid_requests_are_rejected() {
        let not_numeric = call(Behavior::Answer, offer("/v1/join/steve", "v=0")).await;
        assert_eq!(not_numeric.0, StatusCode::BAD_REQUEST);

        let empty = call(Behavior::Answer, offer("/v1/join/1", "")).await;
        assert_eq!(empty.0, StatusCode::BAD_REQUEST);

        let too_large = call(
            Behavior::Answer,
            offer("/v1/join/1", vec![b'a'; MAX_OFFER_SIZE + 1]),
        )
        .await;
        assert_eq!(too_large.0, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn handler_errors_map_to_status_codes() {
        let forbidden = call(Behavior::Forbid, offer("/v1/join/1", "v=0")).await;
        assert_eq!(forbidden.0, StatusCode::FORBIDDEN);
        assert_eq!(forbidden.1, "text/plain");

        let busy = call(Behavior::Busy, offer("/v1/join/1", "v=0")).await;
        assert_eq!(busy.0, StatusCode::SERVICE_UNAVAILABLE);

        let slow = call(Behavior::Hang, offer("/v1/join/1", "v=0")).await;
        assert_eq!(slow.0, StatusCode::BAD_GATEWAY);
    }
}
