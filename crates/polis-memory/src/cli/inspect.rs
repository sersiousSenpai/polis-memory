// SPDX-License-Identifier: Apache-2.0
//! Portable local inspection; source bodies are resolved only on explicit lookup.
use polis_core::{
    diagnostics::{EvidenceRequest, TraceRequest},
    MemoryApi,
};
use std::path::Path;

pub fn export(
    api: &dyn MemoryApi,
    id: Option<String>,
    limit: usize,
    seq: Option<i64>,
) -> Result<serde_json::Value, String> {
    if let Some(seq) = seq {
        return serde_json::to_value(
            api.evidence(&EvidenceRequest {
                seq,
                ..Default::default()
            })
            .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string());
    }
    let traces = api
        .traces(&TraceRequest {
            id,
            limit: Some(limit),
            ..Default::default()
        })
        .map_err(|e| e.to_string())?;
    let runs = api
        .list_runs(limit as i64, &Default::default())
        .map_err(|e| e.to_string());
    Ok(
        serde_json::json!({"schemaVersion":1,"exportedAt":polis_core::ledger::now_millis(),"traces":traces,"gardenerRuns":runs,
        "privacy":"Source bodies are omitted. Use polis inspect --seq N to resolve evidence under the current scope."}),
    )
}

pub fn write_html(path: &Path, data: &serde_json::Value) -> Result<(), String> {
    let payload = serde_json::to_string(data)
        .map_err(|e| e.to_string())?
        .replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");
    let html = include_str!("inspector.html").replace("__POLIS_DIAGNOSTIC_DATA__", &payload);
    std::fs::write(path, html).map_err(|e| format!("write {}: {e}", path.display()))
}
