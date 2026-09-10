// SPDX-License-Identifier: Apache-2.0
//! Generated JSON contracts and cited temporal claims over the same MemoryApi.
use crate::{
    routes::{error_response, memory_error},
    PolisState,
};
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use polis_core::api::Scope;
use polis_core::claims::{ClaimPredicate, ClaimQuery, ClaimWrite};
use polis_core::host::Change;
use serde::Deserialize;

pub async fn handle_schema() -> Response {
    // Generated from core serde types; a local artifact ships in the crate.
    (
        [(axum::http::header::CONTENT_TYPE, "application/schema+json")],
        include_str!("api-v1.schema.json"),
    )
        .into_response()
}

#[derive(Default, Deserialize)]
pub struct ClaimsQ {
    q: Option<String>,
    subject: Option<String>,
    predicate: Option<ClaimPredicate>,
    valid_at: Option<i64>,
    known_at: Option<i64>,
    limit: Option<usize>,
    principal: Option<String>,
    project: Option<String>,
    agent: Option<String>,
    run: Option<String>,
    org: Option<String>,
    include_shared: Option<bool>,
    roles: Option<String>,
    after: Option<i64>,
    before: Option<i64>,
}

pub async fn handle_claims(State(state): State<PolisState>, Query(q): Query<ClaimsQ>) -> Response {
    let req = ClaimQuery {
        q: q.q,
        subject: q.subject,
        predicate: q.predicate,
        valid_at: q.valid_at,
        known_at: q.known_at,
        limit: q.limit,
        scope: Scope {
            principal: q.principal,
            project: q.project,
            agent: q.agent,
            run: q.run,
            org: q.org,
            include_shared: q.include_shared.unwrap_or(false),
        },
        evidence_filter: polis_core::api::EvidenceFilter {
            roles: q
                .roles
                .map(|r| r.split(',').map(str::to_string).collect())
                .unwrap_or_default(),
            after: q.after,
            before: q.before,
            ..Default::default()
        },
    };
    match tokio::task::spawn_blocking(move || state.api.claims(&req)).await {
        Ok(Ok(claims)) => Json(serde_json::json!({"claims": claims})).into_response(),
        Ok(Err(error)) => memory_error(error),
        Err(error) => error_response(format!("claim query failed: {error}")),
    }
}

pub async fn handle_decide(
    State(state): State<PolisState>,
    Json(req): Json<polis_core::diagnostics::DecisionRequest>,
) -> Response {
    let api = state.api.clone();
    match tokio::task::spawn_blocking(move || api.decide(&req)).await {
        Ok(Ok(receipt)) => {
            state.events.changed(&[Change::Ledger, Change::Memory]);
            Json(receipt).into_response()
        }
        Ok(Err(error)) => memory_error(error),
        Err(error) => error_response(format!("decision write failed: {error}")),
    }
}

pub async fn handle_write_claim(
    State(state): State<PolisState>,
    Json(req): Json<ClaimWrite>,
) -> Response {
    let api = state.api.clone();
    match tokio::task::spawn_blocking(move || api.write_claim(&req)).await {
        Ok(Ok(claim)) => {
            state.events.changed(&[Change::Ledger, Change::Memory]);
            Json(claim).into_response()
        }
        Ok(Err(error)) => memory_error(error),
        Err(error) => error_response(format!("claim write failed: {error}")),
    }
}
