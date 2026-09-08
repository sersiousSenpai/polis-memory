// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! `/v1/sync/*` — the org node's relay routes (plan §4.6, Session E4), over
//! [`polis_core::sync::SyncRelay`]. Every row is token-gated in BOTH
//! directions: a segment carries bodies, so a read is as sensitive as a
//! write, and an org node listens on a network. A daemon that is not an
//! org node serves these too — answering 503 "not an org node" with the
//! reason, never a silent hole.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use polis_core::sync::SyncApiError;

use crate::PolisState;

fn sync_error(e: SyncApiError) -> Response {
    let status = match &e {
        SyncApiError::NotAnOrgNode => StatusCode::SERVICE_UNAVAILABLE,
        SyncApiError::Refused(_) => StatusCode::BAD_REQUEST,
        SyncApiError::Forked(_) => StatusCode::CONFLICT,
        SyncApiError::NotFound => StatusCode::NOT_FOUND,
        SyncApiError::Store(_) => StatusCode::BAD_GATEWAY,
    };
    (status, Json(json!({ "error": e.to_string(), "reason": e }))).into_response()
}

/// `GET /v1/sync/chains` — the node's card and every chain it holds.
pub async fn handle_sync_chains(State(state): State<PolisState>) -> Response {
    let relay = state.sync.clone();
    let out = tokio::task::spawn_blocking(move || relay.node().and_then(|node| relay.chains().map(|chains| (node, chains)))).await;
    match out {
        Ok(Ok((node, chains))) => Json(json!({ "node": node, "chains": chains })).into_response(),
        Ok(Err(e)) => sync_error(e),
        Err(e) => sync_error(SyncApiError::Store(e.to_string())),
    }
}

#[derive(Deserialize)]
pub struct AfterQ {
    after: Option<i64>,
}

/// `GET /v1/sync/segments/:chain?after=` — a chain's segments past a seq.
pub async fn handle_sync_segments(State(state): State<PolisState>, Path(chain): Path<String>, Query(q): Query<AfterQ>) -> Response {
    let relay = state.sync.clone();
    let after = q.after.unwrap_or(0);
    match tokio::task::spawn_blocking(move || relay.segments(&chain, after)).await {
        Ok(Ok(segments)) => Json(json!({ "segments": segments })).into_response(),
        Ok(Err(e)) => sync_error(e),
        Err(e) => sync_error(SyncApiError::Store(e.to_string())),
    }
}

/// `GET /v1/sync/segments/:chain/:from/:to` — one envelope, verbatim.
pub async fn handle_sync_segment(State(state): State<PolisState>, Path((chain, from, to)): Path<(String, i64, i64)>) -> Response {
    let relay = state.sync.clone();
    match tokio::task::spawn_blocking(move || relay.segment(&chain, from, to)).await {
        Ok(Ok(Some(env))) => Json(env).into_response(),
        Ok(Ok(None)) => sync_error(SyncApiError::NotFound),
        Ok(Err(e)) => sync_error(e),
        Err(e) => sync_error(SyncApiError::Store(e.to_string())),
    }
}

/// `POST /v1/sync/segments` — publish an envelope; verified on receipt.
pub async fn handle_sync_publish(State(state): State<PolisState>, Json(envelope): Json<Value>) -> Response {
    let relay = state.sync.clone();
    match tokio::task::spawn_blocking(move || relay.publish(envelope)).await {
        Ok(Ok(receipt)) => (StatusCode::CREATED, Json(receipt)).into_response(),
        Ok(Err(e)) => sync_error(e),
        Err(e) => sync_error(SyncApiError::Store(e.to_string())),
    }
}

/// `GET /v1/sync/redactions` — every redaction the node has seen, with the
/// subscribers that have and have not moved past it.
pub async fn handle_sync_redactions(State(state): State<PolisState>) -> Response {
    let relay = state.sync.clone();
    match tokio::task::spawn_blocking(move || relay.redactions()).await {
        Ok(Ok(rows)) => Json(json!({ "redactions": rows })).into_response(),
        Ok(Err(e)) => sync_error(e),
        Err(e) => sync_error(SyncApiError::Store(e.to_string())),
    }
}

#[derive(Deserialize)]
pub struct ChainQ {
    chain: Option<String>,
}

/// `GET /v1/sync/acks?chain=` — what every subscriber has reported holding.
pub async fn handle_sync_acks(State(state): State<PolisState>, Query(q): Query<ChainQ>) -> Response {
    let relay = state.sync.clone();
    match tokio::task::spawn_blocking(move || relay.acks(q.chain.as_deref())).await {
        Ok(Ok(rows)) => Json(json!({ "acks": rows })).into_response(),
        Ok(Err(e)) => sync_error(e),
        Err(e) => sync_error(SyncApiError::Store(e.to_string())),
    }
}

/// `POST /v1/sync/acks` — a subscriber's signed report of what it holds.
pub async fn handle_sync_report_acks(State(state): State<PolisState>, Json(report): Json<Value>) -> Response {
    let relay = state.sync.clone();
    match tokio::task::spawn_blocking(move || relay.record_acks(report)).await {
        Ok(Ok(n)) => Json(json!({ "recorded": n })).into_response(),
        Ok(Err(e)) => sync_error(e),
        Err(e) => sync_error(SyncApiError::Store(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use polis_core::sync::*;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    /// A relay that remembers what it was asked to publish.
    struct Fake {
        published: Mutex<Vec<Value>>,
        forked: bool,
    }

    impl SyncRelay for Fake {
        fn node(&self) -> Result<NodeCard, SyncApiError> {
            Ok(NodeCard { principal_id: "p".into(), chain_id: "node-chain".into(), display_name: Some("acme".into()), fingerprint: "p".into() })
        }
        fn chains(&self) -> Result<Vec<ChainSummary>, SyncApiError> {
            Ok(vec![ChainSummary { chain_id: "c1".into(), human_id: "h1".into(), display_name: None, device_name: Some("laptop".into()), head_seq: 4, forked: false }])
        }
        fn segments(&self, chain_id: &str, after_seq: i64) -> Result<Vec<SegmentSummary>, SyncApiError> {
            Ok(vec![SegmentSummary { chain_id: chain_id.into(), from_seq: after_seq + 1, to_seq: after_seq + 2 }])
        }
        fn segment(&self, chain_id: &str, from_seq: i64, _to_seq: i64) -> Result<Option<Value>, SyncApiError> {
            Ok((from_seq == 1).then(|| json!({ "header": { "chainId": chain_id } })))
        }
        fn publish(&self, envelope: Value) -> Result<PublishReceipt, SyncApiError> {
            if self.forked {
                return Err(SyncApiError::Forked("seq 2 rewritten".into()));
            }
            self.published.lock().unwrap().push(envelope);
            Ok(PublishReceipt { chain_id: "c1".into(), from_seq: 1, to_seq: 2, outcome: "appended".into(), appended: 2, trusted_now: None })
        }
        fn redactions(&self) -> Result<Vec<RedactionSummary>, SyncApiError> {
            Ok(vec![RedactionSummary { chain_id: "c1".into(), event_seq: 3, target_seq: 1, acked_by: vec!["c2".into()], pending: vec!["c3".into()] }])
        }
        fn acks(&self, chain_id: Option<&str>) -> Result<Vec<AckSummary>, SyncApiError> {
            Ok(vec![AckSummary { acker_chain: "c2".into(), chain_id: chain_id.unwrap_or("c1").into(), acked_seq: 3 }])
        }
        fn record_acks(&self, report: Value) -> Result<usize, SyncApiError> {
            Ok(report["acks"].as_array().map(|a| a.len()).unwrap_or(0))
        }
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    fn app_over(relay: Arc<dyn SyncRelay>) -> axum::Router {
        testing::app_with(testing::state().with_sync(relay))
    }

    #[tokio::test]
    async fn the_relay_routes_serve_the_relay_and_a_plain_daemon_says_it_is_not_one() {
        let fake = Arc::new(Fake { published: Mutex::new(Vec::new()), forked: false });
        let app = app_over(fake.clone());
        let get = |uri: &str| axum::http::Request::builder().uri(uri).body(Body::empty()).unwrap();

        let r = app.clone().oneshot(get("/v1/sync/chains")).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let v = body_json(r).await;
        assert_eq!(v["node"]["chainId"], "node-chain");
        assert_eq!(v["chains"][0]["headSeq"], 4);

        let v = body_json(app.clone().oneshot(get("/v1/sync/segments/c1?after=2")).await.unwrap()).await;
        assert_eq!(v["segments"][0]["fromSeq"], 3);

        let r = app.clone().oneshot(get("/v1/sync/segments/c1/1/2")).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(body_json(r).await["header"]["chainId"], "c1");
        assert_eq!(app.clone().oneshot(get("/v1/sync/segments/c1/9/9")).await.unwrap().status(), StatusCode::NOT_FOUND);

        let post = axum::http::Request::builder().method("POST").uri("/v1/sync/segments").header("content-type", "application/json").body(Body::from(r#"{"header":{"chainId":"c1"}}"#)).unwrap();
        let r = app.clone().oneshot(post).await.unwrap();
        assert_eq!(r.status(), StatusCode::CREATED);
        assert_eq!(body_json(r).await["outcome"], "appended");
        assert_eq!(fake.published.lock().unwrap().len(), 1);

        let v = body_json(app.clone().oneshot(get("/v1/sync/redactions")).await.unwrap()).await;
        assert_eq!(v["redactions"][0]["pending"][0], "c3");
        let v = body_json(app.clone().oneshot(get("/v1/sync/acks?chain=c1")).await.unwrap()).await;
        assert_eq!(v["acks"][0]["ackerChain"], "c2");
        let post = axum::http::Request::builder().method("POST").uri("/v1/sync/acks").header("content-type", "application/json").body(Body::from(r#"{"ackerChain":"c2","acks":[["c1",4],["c3",2]],"at":1,"signature":""}"#)).unwrap();
        let v = body_json(app.clone().oneshot(post).await.unwrap()).await;
        assert_eq!(v["recorded"], 2);

        // A rewritten segment is a 409 with the reason as data.
        let forked = app_over(Arc::new(Fake { published: Mutex::new(Vec::new()), forked: true }));
        let post = axum::http::Request::builder().method("POST").uri("/v1/sync/segments").header("content-type", "application/json").body(Body::from("{}")).unwrap();
        let r = forked.oneshot(post).await.unwrap();
        assert_eq!(r.status(), StatusCode::CONFLICT);
        assert_eq!(body_json(r).await["reason"]["kind"], "forked");

        // Not an org node: the routes exist and say so.
        let plain = testing::app();
        let r = plain.oneshot(get("/v1/sync/chains")).await.unwrap();
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(body_json(r).await["error"].as_str().unwrap().contains("not an org node"));
    }
}
