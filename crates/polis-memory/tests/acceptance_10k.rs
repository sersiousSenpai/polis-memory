// SPDX-License-Identifier: Apache-2.0
//! Explicit, keyless reference workload. Run with:
//! POLIS_ACCEPTANCE_OUTPUT=bench/results/acceptance-10k.json cargo test --release
//! -p polis-memory --test acceptance_10k measure_capture_and_pack_10k -- --ignored --nocapture
use polis_core::{
    api::{IngestItem, IngestRequest, Scope, SearchRequest},
    host::NoHost,
    MemoryApi,
};
use polis_llm::NoopSink;
use polis_memory::PolisHandle;
use polis_store::PolisStore;
use serde_json::{json, Value};
use std::{path::Path, process::Command, sync::Arc, time::Instant};

fn api(path: &Path) -> PolisHandle {
    PolisHandle::new(
        Arc::new(PolisStore::open(path).unwrap()),
        None,
        Arc::new(NoHost),
        Arc::new(NoopSink),
    )
}
fn scope(index: usize) -> Scope {
    Scope {
        principal: Some("bench-human".into()),
        agent: Some("bench-agent".into()),
        org: Some("bench-org".into()),
        project: Some(format!("/bench/project-{}", index % 10)),
        ..Default::default()
    }
}
fn request(index: usize) -> SearchRequest {
    SearchRequest {
        q: Some(format!("artifact{index:05}")),
        node: Some(format!("bench-{}", index % 10)),
        limit: Some(10),
        candidate_limit: Some(100),
        max_tokens: Some(4096),
        scope: scope(index),
        ..Default::default()
    }
}
fn measure_search(handle: &PolisHandle, index: usize) -> (f64, usize) {
    let start = Instant::now();
    let pack = handle.search(&request(index)).unwrap();
    let elapsed = start.elapsed().as_secs_f64() * 1000.;
    assert!(
        pack.retrieval.errors.is_empty(),
        "{:?}",
        pack.retrieval.errors
    );
    assert!(
        pack.prompt_hits
            .iter()
            .any(|hit| hit.item.seq == index as i64 + 1),
        "lost artifact{index:05}"
    );
    let bytes = serde_json::to_vec(&pack).unwrap().len();
    assert!(bytes <= 4096);
    (elapsed, bytes)
}
fn summary(samples: &[f64]) -> Value {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let percentile = |p: f64| sorted[((sorted.len() as f64 * p).ceil() as usize).saturating_sub(1)];
    json!({"n":sorted.len(),"p50Ms":percentile(0.50),"p95Ms":percentile(0.95),"maxMs":sorted.last(),"meanMs":sorted.iter().sum::<f64>()/sorted.len() as f64,"samplesMs":samples})
}
fn command(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[test]
#[ignore = "measurement worker; launched in fresh processes by the main fixture"]
fn measure_10k_cold_worker() {
    let Ok(path) = std::env::var("POLIS_ACCEPTANCE_WORKER_DB") else {
        return;
    };
    let index = std::env::var("POLIS_ACCEPTANCE_WORKER_INDEX")
        .unwrap()
        .parse()
        .unwrap();
    let handle = api(Path::new(&path));
    let (elapsed, bytes) = measure_search(&handle, index);
    println!("POLIS_COLD_SAMPLE={}", json!({"ms":elapsed,"bytes":bytes}));
}

#[test]
#[ignore = "explicit 10k acceptance measurement; no model or network calls"]
fn measure_capture_and_pack_10k() {
    const PROMPTS: usize = 10_000;
    let root = std::env::temp_dir().join(format!(
        "polis-acceptance-10k-{}-{}",
        std::process::id(),
        polis_core::ledger::now_millis()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("polis.db");
    let handle = api(&path);
    let mut capture = Vec::with_capacity(PROMPTS);
    let mut source_bytes = 0;
    let capture_started = Instant::now();
    for index in 0..PROMPTS {
        let body=format!("artifact{index:05} records the selected configuration for service {}. Keep the primary region local, retain the source citation, and apply version {} to the {} deployment. The deterministic recovery procedure checks the checksum before accepting the replacement. This evidence belongs to project {} and interaction {}.",index%97,index%13,if index%2==0 {"staging"}else{"production"},index%10,index/100);
        source_bytes += body.len();
        let req = IngestRequest {
            scope: Scope {
                run: Some(format!("run-{}", index / 20)),
                ..scope(index)
            },
            items: vec![IngestItem {
                body,
                role: Some(if index % 2 == 0 { "user" } else { "assistant" }.into()),
                ts: Some(1_700_000_000_000 + index as i64 * 1000),
                session: Some(format!("session-{}", index / 100)),
                ..Default::default()
            }],
        };
        let start = Instant::now();
        let receipt = handle.ingest(&req).unwrap();
        capture.push(start.elapsed().as_secs_f64() * 1000.);
        assert_eq!(receipt.recorded, vec![index as i64 + 1]);
        // Outside the capture timer: prove lexical availability immediately
        // after every acknowledgement, with no deferred indexing pass.
        let count: i64 = handle
            .store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM prompts_fts WHERE prompts_fts MATCH ?1",
                [format!("artifact{index:05}")],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }
    let capture_wall_ms = capture_started.elapsed().as_secs_f64() * 1000.;
    assert!(handle.verify().unwrap().ok);
    // Deterministically pre-file 1,000 sources into each of 10 accepted classes.
    // This is fixture setup, excluded from both capture and retrieval timings.
    {
        let mut conn = handle.store.conn();
        let tx = conn.transaction().unwrap();
        for project in 0..10 {
            tx.execute("INSERT INTO class_nodes(id,kind,title,project_path,principal_id,agent_id,org_id,status,created_at,updated_at) VALUES(?1,'class',?2,?3,'bench-human','bench-agent','bench-org','accepted',1,1)",rusqlite::params![format!("bench-{project}"),format!("Project {project} configurations"),format!("/bench/project-{project}")]).unwrap();
        }
        for index in 0..PROMPTS {
            tx.execute("INSERT INTO class_links(node_id,target_kind,target_id,status,created_at) VALUES(?1,'prompt',?2,'accepted',1)",rusqlite::params![format!("bench-{}",index%10),(index+1).to_string()]).unwrap();
        }
        tx.commit().unwrap();
    }
    let query_index = |index: usize| (index * 7919 + 137) % PROMPTS;
    for sample in 0..10 {
        measure_search(&handle, query_index(sample));
    }
    let mut warm = Vec::new();
    let mut output_bytes = Vec::new();
    for sample in 0..120 {
        let (ms, bytes) = measure_search(&handle, query_index(sample % 40));
        warm.push(ms);
        output_bytes.push(bytes);
    }
    let mut cold = Vec::new();
    for sample in 0..40 {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "measure_10k_cold_worker",
                "--nocapture",
            ])
            .env("POLIS_ACCEPTANCE_WORKER_DB", &path)
            .env(
                "POLIS_ACCEPTANCE_WORKER_INDEX",
                query_index(sample).to_string(),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "cold worker failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let sample: Value = serde_json::from_str(
            stdout
                .lines()
                .find_map(|line| line.strip_prefix("POLIS_COLD_SAMPLE="))
                .expect("worker sample"),
        )
        .unwrap();
        cold.push(sample["ms"].as_f64().unwrap());
    }
    let capture_summary = summary(&capture);
    let warm_summary = summary(&warm);
    let cold_summary = summary(&cold);
    let hardware=std::env::var("POLIS_ACCEPTANCE_MACHINE_FILE").ok().map(|path|serde_json::from_slice::<Value>(&std::fs::read(path).unwrap()).unwrap()).unwrap_or_else(||json!({"cpu":command("sysctl",&["-n","machdep.cpu.brand_string"]),"memoryBytes":command("sysctl",&["-n","hw.memsize"]),"logicalCpus":std::thread::available_parallelism().ok().map(|n|n.get()),"model":command("sysctl",&["-n","hw.model"])}));
    let models_root = std::env::var("POLIS_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|p| Path::new(&p).join(".polis"))
        })
        .map(|p| p.join("models"));
    let manifest = json!({
        "schema":1,"kind":"deterministic-capture-and-pack-10k","measuredAt":command("date",&["-u","+%Y-%m-%dT%H:%M:%SZ"]),
        "git":{"head":command("git",&["rev-parse","HEAD"]),"workingTree":"uncommitted build under evaluation"},
        "machine":{"hardware":hardware,"os":std::env::consts::OS,"architecture":std::env::consts::ARCH,"osVersion":command("sw_vers",&["-productVersion"]),"rustc":command("rustc",&["-Vv"]),"concurrentActivity":"other integration agents may compile or run checks; system not isolated"},
        "configuration":{"profile":"release","store":"disk SQLite WAL, synchronous NORMAL","schemaVersion":polis_store::meta::STORE_SCHEMA_VERSION,"llm":"none","embedder":"none","modelCalls":0,"embeddingCalls":0,"remoteCalls":0,"captureConcurrency":1,"ingestBatchSize":1,"retrievalLimit":10,"candidateLimit":100,"maxTokens":4096,"effectiveSerializedByteBudget":4096,"rolePolicy":"user and assistant","explicitScopeAxes":["principal","agent","org","project"],"queryPermutation":"(i*7919+137)%10000"},
        "workload":{"prompts":PROMPTS,"sourceBytes":source_bytes,"userSources":5000,"assistantSources":5000,"projects":10,"sessions":100,"runs":500,"acceptedClasses":10,"linksPerClass":1000,"queryCount":40,"lexicalAcknowledgementsChecked":PROMPTS,"goldReachedInEveryMeasuredQuery":true},
        "method":{"capture":"one synchronous facade ingest; source and citation acknowledgement timed; immediate lexical check excluded from timer","warm":"10 discarded warmups, then 120 reads on one handle over 40 fixed queries","cold":"40 fresh OS processes; first pack timed after opening the store; process startup/store-open excluded; filesystem page cache was not evicted","percentile":"nearest-rank over unrounded milliseconds","freshness":"not measured: this zero-model workload does not run the daemon or an embedder","checkedModelsDirectory":models_root.as_ref().map(|p|p.display().to_string()),"modelsDirectoryExists":models_root.as_ref().is_some_and(|p|p.is_dir())},
        "capture":capture_summary,"captureWallMsIncludingAckChecks":capture_wall_ms,"warmPack":warm_summary,"coldPack":cold_summary,
        "packBytes":{"min":output_bytes.iter().min(),"max":output_bytes.iter().max()},
        "gates":{"captureP50Below5Ms":capture_summary["p50Ms"].as_f64().unwrap()<5.,"captureP95Below20Ms":capture_summary["p95Ms"].as_f64().unwrap()<20.,"warmP50Below50Ms":warm_summary["p50Ms"].as_f64().unwrap()<50.,"warmP95Below200Ms":warm_summary["p95Ms"].as_f64().unwrap()<200.,"coldP50Below300Ms":cold_summary["p50Ms"].as_f64().unwrap()<300.,"coldP95Below800Ms":cold_summary["p95Ms"].as_f64().unwrap()<800.}
    });
    let output = std::env::var("POLIS_ACCEPTANCE_OUTPUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/results/acceptance-10k.json")
        });
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&output, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
    println!(
        "ACCEPTANCE_10K={}",
        json!({"artifact":output,"captureP50Ms":capture_summary["p50Ms"],"captureP95Ms":capture_summary["p95Ms"],"warmP50Ms":warm_summary["p50Ms"],"warmP95Ms":warm_summary["p95Ms"],"coldP50Ms":cold_summary["p50Ms"],"coldP95Ms":cold_summary["p95Ms"],"gates":manifest["gates"]})
    );
    drop(handle);
    std::fs::remove_dir_all(root).unwrap();
}
