// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Reading the record: the answer pack, the Timeline page, the stats, the map, the thread tree. Lifted from Redline's `context.rs` in Session A5.

#[allow(unused_imports)]
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
#[allow(unused_imports)]
use std::path::{Path, PathBuf};

#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
#[allow(unused_imports)]
use serde_json::Value;

#[allow(unused_imports)]
use polis_core::coldness::{auto_collapse_safe, subtree_stats, BranchStat, LakeEnvelope};
#[allow(unused_imports)]
use polis_core::ledger::{now_millis, EventKind, LedgerEventRow};
#[allow(unused_imports)]
use polis_core::api::ContextBlock;
use polis_core::pack::*;
use polis_core::query::plan_fts_query;
#[allow(unused_imports)]
use polis_core::proposal::{parse_proposals, parse_supersede_verdicts, Proposal, SupersedeVerdict, SUPERSEDE_CONFIDENCE_MIN};
#[allow(unused_imports)]
use polis_core::types::*;
#[allow(unused_imports)]
use polis_store::record::{record_curate, record_reorg, revert_link, DecisionInput};
#[allow(unused_imports)]
use polis_store::PolisStore;

#[allow(unused_imports)]
use crate::agent::{run_classifier, run_keeper_summarizer};
use crate::Polis;

/// Clamp + default for `GET /v1/context/prompts`'s `?limit=`.
// The limit and its clamp live in `polis_core::types` since A6 (the server
// clamps the same query the facade does); re-exported so the old path holds.
pub use polis_core::types::{clamp_prompt_limit, PROMPT_LIMIT_MAX};

/// Run a filtered prompt query and enforce the response byte budget. The DB
/// caps each body at 4000 chars already; this additionally drops trailing items
/// once the cumulative body size would exceed `MAX_CONTEXT_BYTES`, so a wide
/// `limit` on long prompts still can't blow up the response.
pub fn list_prompts(polis: &Polis<'_>, filters: &PromptFilters) -> Result<Vec<LakeItem>, String> {
    let db = polis.store;
    let mut items = db.list_context_prompts(filters).map_err(|e| e.to_string())?;
    let keep = budgeted_item_count(items.iter().map(|i| i.body.as_deref()));
    items.truncate(keep);
    Ok(items)
}

/// Build from this store. Aggregates deliberately have no process-global cache:
/// different stores can share a ledger sequence, and derived projections can
/// change without moving the ledger head.
pub fn build_stats_cached(polis: &Polis<'_>) -> ContextStats {
    build_stats(polis)
}

/// Build the stats digest. Best-effort per axis (an unmigrated table yields an
/// empty list rather than failing the whole response).
pub fn build_stats(polis: &Polis<'_>) -> ContextStats {
    let db = polis.store;
    let by_day = db.prompt_counts_by_day().unwrap_or_default();
    let by_surface = db.prompt_counts_by_surface().unwrap_or_default();
    let by_kind = db.event_counts_by_kind().unwrap_or_default();
    let by_class = db.class_link_counts_by_root().unwrap_or_default();
    let by_author = db.event_counts_by_author().unwrap_or_default();
    let total_prompts = by_surface.iter().map(|(_, c)| c).sum();
    let total_events = db.max_ledger_seq().unwrap_or(0);
    ContextStats {
        generated_ts: now_millis(),
        total_prompts,
        total_events,
        by_day,
        by_surface,
        by_kind,
        by_class,
        by_author,
        latency: Vec::new(),
    }
}

/// Assemble the Map: accepted classes + session-tree threads (plus sessions a
/// supersession resolves to), with the four declared edge kinds. Everything is
/// ordered (nodes by id, edges by kind/from/to) so the payload — and therefore
/// the seeded layout downstream — is deterministic. Best-effort per source: a
/// missing table yields empty buckets, never an error (some event kinds may
/// never have fired; the Map must render an honest empty state).
pub fn build_memory_map(polis: &Polis<'_>) -> MemoryMapView {
    let db = polis.store;
    use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

    let mut nodes: Vec<MapNode> = Vec::new();
    let mut edges: Vec<MapEdge> = Vec::new();

    // --- classes (accepted only — proposed nodes are not yet on the record) --
    let all_classes = db.list_class_nodes_with_counts().unwrap_or_default();
    let accepted: Vec<&(polis_core::types::ClassNode, i64)> = all_classes
        .iter()
        .filter(|(n, _)| n.status == "accepted")
        .collect();
    let accepted_ids: HashSet<&str> = accepted.iter().map(|(n, _)| n.id.as_str()).collect();
    let mut class_parent: HashMap<&str, &str> = HashMap::new();
    let mut class_project: HashMap<&str, Option<&str>> = HashMap::new();
    for (n, count) in &accepted {
        let parent = n
            .parent_id
            .as_deref()
            .filter(|p| accepted_ids.contains(p));
        if let Some(p) = parent {
            class_parent.insert(n.id.as_str(), p);
            edges.push(MapEdge {
                kind: "contains".into(),
                from: format!("class:{p}"),
                to: format!("class:{}", n.id),
                weight: 1,
                basis: None,
            });
        }
        class_project.insert(n.id.as_str(), n.project_path.as_deref());
        nodes.push(MapNode {
            id: format!("class:{}", n.id),
            kind: if n.kind == "digest" { "digest" } else { "class" }.into(),
            label: n.title.clone(),
            mass: *count,
            parent_id: parent.map(|p| format!("class:{p}")),
            pinned: n.pinned,
            project_path: n.project_path.clone(),
            class_node_id: Some(n.id.clone()),
            session_id: None,
            browse_id: None,
            thread_id: None,
        });
    }

    // --- session tree (lineage) ---------------------------------------------
    let tree_rows = db.list_session_tree_rows().unwrap_or_default();
    let mut threads: BTreeSet<(String, String)> = BTreeSet::new();
    let mut thread_parent: HashMap<(String, String), (String, String)> = HashMap::new();
    for (ck, cid, pk, pid) in &tree_rows {
        threads.insert((ck.clone(), cid.clone()));
        threads.insert((pk.clone(), pid.clone()));
        thread_parent
            .entry((ck.clone(), cid.clone()))
            .or_insert_with(|| (pk.clone(), pid.clone()));
        edges.push(MapEdge {
            kind: "lineage".into(),
            from: format!("thread:{pk}:{pid}"),
            to: format!("thread:{ck}:{cid}"),
            weight: 1,
            basis: None,
        });
    }

    // --- supersedes (decision chain, endpoints mapped per rule 1) -----------
    let pairs = db.list_supersession_pairs().unwrap_or_default();
    let seqs: Vec<i64> = pairs.iter().flat_map(|&(o, n)| [o, n]).collect();
    let endpoints = db.resolve_map_endpoints(&seqs).unwrap_or_default();
    // A decision lands on its class when filed, its session otherwise. A
    // session seen only here still becomes a node — it hosts a decision.
    let resolve = |seq: i64, threads: &mut BTreeSet<(String, String)>| -> Option<String> {
        let (session, class) = endpoints.get(&seq)?;
        if let Some(c) = class.as_deref().filter(|c| accepted_ids.contains(c)) {
            return Some(format!("class:{c}"));
        }
        let sid = session.as_deref()?;
        threads.insert(("session".into(), sid.to_string()));
        Some(format!("thread:session:{sid}"))
    };
    let mut chain: BTreeMap<(String, String), (i64, String)> = BTreeMap::new();
    for (old, new) in &pairs {
        let (Some(from), Some(to)) = (
            resolve(*old, &mut threads),
            resolve(*new, &mut threads),
        ) else {
            continue;
        };
        if from == to {
            continue;
        }
        let entry = chain
            .entry((from, to))
            .or_insert_with(|| (0, format!("#{old} → #{new}")));
        entry.0 += 1;
    }
    for ((from, to), (weight, basis)) in chain {
        edges.push(MapEdge {
            kind: "supersedes".into(),
            from,
            to,
            weight,
            basis: Some(basis),
        });
    }

    // --- thread nodes (labels + message-count mass, resolved per node) ------
    for (kind, id) in &threads {
        let (count, _) = polis.thread_stats(kind, id).unwrap_or((0, None));
        let label = polis.thread_label(kind, id)
            .unwrap_or_else(|| format!("{kind} {}", id.chars().take(8).collect::<String>()));
        let is_session = kind == "session";
        let is_browse = kind == "browse";
        nodes.push(MapNode {
            id: format!("thread:{kind}:{id}"),
            kind: if is_session { "session" } else { "thread" }.into(),
            label,
            mass: count,
            parent_id: thread_parent
                .get(&(kind.clone(), id.clone()))
                .map(|(pk, pid)| format!("thread:{pk}:{pid}")),
            pinned: false,
            project_path: None,
            class_node_id: None,
            session_id: is_session.then(|| id.clone()),
            browse_id: is_browse.then(|| id.clone()),
            thread_id: (!is_session && !is_browse).then(|| id.clone()),
        });
    }

    // --- co-occurs (the one derived edge, opt-in downstream) ----------------
    // Classes sharing sessions (through their accepted links) or a project
    // while filed apart. Direct parent↔child pairs are skipped — `contains`
    // already states that relation; the signal here is UNEXPECTED adjacency.
    let mut sessions_by_class: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let pairs = db.class_session_pairs().unwrap_or_default();
    for (node, session) in &pairs {
        if accepted_ids.contains(node.as_str()) {
            sessions_by_class
                .entry(node.as_str())
                .or_default()
                .insert(session.as_str());
        }
    }
    let mut class_list: Vec<&str> = accepted_ids.iter().copied().collect();
    class_list.sort_unstable();
    for (i, a) in class_list.iter().enumerate() {
        for b in &class_list[i + 1..] {
            if class_parent.get(a) == Some(b) || class_parent.get(b) == Some(a) {
                continue;
            }
            let shared = match (sessions_by_class.get(a), sessions_by_class.get(b)) {
                (Some(sa), Some(sb)) => sa.intersection(sb).count() as i64,
                _ => 0,
            };
            let same_project = matches!(
                (class_project.get(a), class_project.get(b)),
                (Some(Some(pa)), Some(Some(pb))) if pa == pb
            );
            if shared == 0 && !same_project {
                continue;
            }
            let basis = match (shared, same_project) {
                (0, _) => "shared project".to_string(),
                (n, false) => format!("{n} shared session{}", if n == 1 { "" } else { "s" }),
                (n, true) => format!(
                    "{n} shared session{} · project",
                    if n == 1 { "" } else { "s" }
                ),
            };
            edges.push(MapEdge {
                kind: "co_occurs".into(),
                from: format!("class:{a}"),
                to: format!("class:{b}"),
                weight: shared + i64::from(same_project),
                basis: Some(basis),
            });
        }
    }

    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    edges.sort_by(|a, b| {
        (a.kind.as_str(), a.from.as_str(), a.to.as_str())
            .cmp(&(b.kind.as_str(), b.from.as_str(), b.to.as_str()))
    });
    MemoryMapView {
        generated_ts: now_millis(),
        nodes,
        edges,
    }
}

/// Assemble the pack. Every list is bounded by `limit`, and the whole response
/// is bounded by `MAX_CONTEXT_BYTES` — trimming links, then prompt hits, then
/// browse hits, and the user's notes only if nothing else is left to give.
pub fn build_answer_pack(
    polis: &Polis<'_>,
    q: Option<&str>,
    node_id: Option<&str>,
    limit: i64,
) -> AnswerPack {
    build_answer_pack_scoped(polis, q, node_id, limit, &polis_store::principals::ScopeFilter::default())
}

/// Compatibility entry point. Production requests use `search_request` so
/// database errors propagate and a single SQLite snapshot describes the pack.
pub fn build_answer_pack_scoped(
    polis: &Polis<'_>, q: Option<&str>, node_id: Option<&str>, limit: i64,
    scope: &polis_store::principals::ScopeFilter,
) -> AnswerPack {
    match assemble_pack(polis, q, node_id, limit, 200, scope, &polis_core::api::EvidenceFilter { roles:scope.roles.clone(), after:scope.after, before:scope.before, ..Default::default() }) {
        Ok(pack) => { crate::warmth::record_pack(polis.store, &pack, now_millis()); pack },
        Err(e) => empty_pack(q, e.to_string()),
    }
}

fn empty_pack(q: Option<&str>, error: String) -> AnswerPack {
    AnswerPack { head_seq: 0, query: q.map(str::to_string), node: None,
        matched_nodes: Vec::new(), notes: Vec::new(), prompt_hits: Vec::new(),
        browse_hits: Vec::new(), grep_hits: Vec::new(), arm_coverage: Vec::new(),
        truncated: Vec::new(), shared_hits: Vec::new(), claims:Vec::new(),
        retrieval: RetrievalReport { errors: vec![error], ..Default::default() } }
}

fn assemble_pack(
    polis: &Polis<'_>, q: Option<&str>, node_id: Option<&str>, limit: i64,
    candidate_limit: usize, scope: &polis_store::principals::ScopeFilter,
    filter: &polis_core::api::EvidenceFilter,
) -> rusqlite::Result<AnswerPack> {
    let db = polis.store;
    let _timer = crate::latency::Timer::start("pack");
    let (head_seq, snapshot_hash) = db.snapshot_head()?;
    let query = q.map(str::trim).filter(|s| !s.is_empty());
    let limit = clamp_answer_pack_limit(Some(limit)) as usize;
    let pool = candidate_limit.clamp(limit, 200);
    let plan = query.and_then(plan_fts_query);
    let terms = plan.as_ref().map(|p| p.terms.clone()).unwrap_or_default();
    let mut cuts = Vec::new();
    let mut considered = 0usize;
    let mut timings=std::collections::BTreeMap::new();
    let mut arm_start=std::time::Instant::now();
    macro_rules! mark { ($name:literal) => {{ let duration=arm_start.elapsed();crate::latency::record($name,duration.as_millis().min(u32::MAX as u128) as u32);timings.insert($name.to_string(),duration.as_micros().min(u64::MAX as u128) as u64);arm_start=std::time::Instant::now(); }}; }

    let mut matched = match query { Some(q) => db.match_class_nodes_scoped(q, pool as i64, scope)?, None => Vec::new() };
    let explicit = match node_id.filter(|id| !id.trim().is_empty()) { Some(id) => db.get_class_node_scoped(id, scope)?, None => None };
    let resolved = match explicit { Some(n) => Some(n), None => match query { Some(q) => db.resolve_class_node_scoped(q, pool as i64, scope)?, None => None } };
    if let Some(n) = &resolved { matched.retain(|m| m.id != n.id); }
    considered += matched.len();
    if matched.len() > limit { cuts.push("matchedNodes".into()); matched.truncate(limit); }
    mark!("pack.resolve");
    let node = if let Some(node) = resolved {
        let mut children = db.list_class_children_scoped(&node.id, scope)?;
        let ids = children.iter().map(|n| n.id.clone()).collect::<Vec<_>>();
        let mut grandchildren = db.list_class_children_for_parents_scoped(&ids, scope)?;
        let mut raw_links = db.list_class_links_for_node_scoped(&node.id, scope)?;
        considered += raw_links.len() + children.len() + grandchildren.len();
        let seqs = raw_links.iter().filter_map(link_seq).collect::<Vec<_>>();
        let scores = db.score_link_seqs(&seqs, &terms)?;
        raw_links.sort_by(|a,b| {
            let score = |l: &ClassLink| scores.get(&link_seq(l).unwrap_or(0)).copied().unwrap_or(0)
                + terms.iter().filter(|t| l.note.as_deref().unwrap_or("").to_lowercase().contains(t.as_str())).count() as i64;
            score(b).cmp(&score(a)).then_with(|| a.id.cmp(&b.id))
        });
        // Relevance is established over all eligible links before taking a
        // bounded preview. A later link has the same opportunity as the first.
        if raw_links.len() > limit { cuts.push("links".into()); raw_links.truncate(limit); }
        let seqs = raw_links.iter().filter_map(link_seq).collect::<Vec<_>>();
        let mut labels = db.link_previews_for_seqs(&seqs)?;
        let mut source_items=db.lake_items_for_seqs_scoped(&seqs,scope)?;
        source_items.extend(db.decision_items_scoped(None,Some(&seqs),pool,scope)?.into_iter().map(|(it,_)|it));
        for item in source_items {if let Some(body)=item.body {labels.insert(item.seq,format!("[{}] {}",item.role.as_deref().unwrap_or("unknown"),polis_core::dedup::excerpt_around(&body,&terms,400)));}}

        let mut superseded = db.supersessions_for_seqs(&seqs)?;
        let eligible=db.eligible_seqs(&superseded.values().copied().collect::<Vec<_>>(),scope)?;
        superseded.retain(|_,replacement|eligible.contains(replacement));
        let links = raw_links.into_iter().map(|link| {
            let seq = link_seq(&link);
            PackLink { label: seq.and_then(|s| labels.get(&s).cloned()), superseded_by: seq.and_then(|s| superseded.get(&s).copied()), link }
        }).collect();
        let mut observations = db.list_class_observations_scoped(&node.id, false, scope)?;
        considered += observations.len();
        for (name, list) in [("children", &mut children), ("grandchildren", &mut grandchildren)] {
            if list.len() > limit { cuts.push(name.into()); list.truncate(limit); }
        }
        if observations.len() > limit { cuts.push("observations".into()); observations.truncate(limit); }
        Some(PackNode { node, children, grandchildren, links, observations })
    } else { None };
    mark!("pack.node");
    let mut notes = match query { Some(q) => db.search_user_notes_scoped(q, pool as i64, scope)?, None => Vec::new() };
    considered += notes.len();
    if notes.len() > limit { notes.truncate(limit); cuts.push("notes".into()); }
    mark!("pack.notes");
    let mut ranked = match query { Some(q) => db.search_prompts_ranked_scoped(q, pool as i64, scope)?, None => Vec::new() };
    if let Some(q)=query { ranked.extend(db.decision_items_scoped(Some(q),None,pool,scope)?); }
    let lexical_count = ranked.len();
    mark!("pack.lexical");
    let mut errors=Vec::new();
    let semantic = match (query, polis.embedder.as_deref()) {
        (Some(q), Some(embedder)) => match polis_embed::semantic_search_scoped_checked(db, embedder, q, pool, scope) {
            Ok(hits)=>hits,Err(e)=>{ errors.push(format!("semantic: {e}"));None }
        },
        _ => None,
    };
    let index=if let Some(embedder)=polis.embedder.as_deref() {
        let model=embedder.model_id();let (total,indexed)=db.scoped_index_readiness(&model,scope)?;
        IndexReadiness {model:Some(model),eligible_sources:total as usize,indexed_sources:indexed as usize,pending_sources:total.saturating_sub(indexed) as usize}
    } else {let (total,_)=db.scoped_index_readiness("",scope)?;IndexReadiness {eligible_sources:total as usize,pending_sources:total as usize,..Default::default()}};
    mark!("pack.semantic");
    let sem_prompts = semantic.as_ref().into_iter().flatten().filter(|h| h.target_kind == "prompt").collect::<Vec<_>>();
    let ids = sem_prompts.iter().map(|h| h.target_id).collect::<Vec<_>>();
    let seq_of = db.seqs_for_prompt_ids(&ids)?;
    let lexical = ranked.iter().enumerate().map(|(i,(it,_))| (it.seq.to_string(), -(i as f64))).collect();
    let sem = sem_prompts.iter().filter_map(|h| seq_of.get(&h.target_id).map(|seq| (seq.to_string(), f64::from(h.score)))).collect();
    let fused = rrf_fuse(&[(Arm::Lexical, lexical), (Arm::Semantic, sem)]);
    let known = ranked.iter().map(|(it,_)| it.seq).collect::<HashSet<_>>();
    let extra = fused.iter().filter_map(|(key,_,_)| key.parse::<i64>().ok()).filter(|s| !known.contains(s)).collect::<Vec<_>>();
    ranked.extend(db.lake_items_for_seqs_scoped(&extra, scope)?.into_iter().map(|it| (it, polis_core::query::MatchStage::Or)));
    let order = fused.iter().enumerate().filter_map(|(i,(key,_,_))| key.parse::<i64>().ok().map(|s| (s,i))).collect::<HashMap<_,_>>();
    let arms = fused.into_iter().filter_map(|(key,arms,_)| key.parse::<i64>().ok().map(|s| (s,arms))).collect::<HashMap<_,_>>();
    ranked.sort_by_key(|(it,_)| order.get(&it.seq).copied().unwrap_or(usize::MAX));
    considered += ranked.len();
    // Deduplicate within speaker roles. Identical user and assistant text must
    // remain independently attributable evidence.
    let mut by_role: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i,(item,_)) in ranked.iter().enumerate() { by_role.entry(format!("{}:{}",item.kind,item.role.clone().unwrap_or_default())).or_default().push(i); }
    let mut absorbed = HashMap::new();
    for indices in by_role.values() {
        let candidates = indices.iter().map(|&i| polis_core::dedup::Candidate {
            key: ranked[i].0.seq, exact_hash: None, text: ranked[i].0.body.as_deref().unwrap_or("")
        }).collect::<Vec<_>>();
        for (&i, verdict) in indices.iter().zip(polis_core::dedup::dedup(&candidates)) {
            if let polis_core::dedup::Verdict::Keep { absorbed: duplicates } = verdict { absorbed.insert(i, duplicates); }
        }
    }
    let mut superseded = db.supersessions_for_seqs(&ranked.iter().map(|(it,_)| it.seq).collect::<Vec<_>>())?;
    let eligible=db.eligible_seqs(&superseded.values().copied().collect::<Vec<_>>(),scope)?;
    superseded.retain(|_,replacement|eligible.contains(replacement));
    let mut prompt_hits = ranked.into_iter().enumerate().filter_map(|(i,(mut item,stage))| {
        let duplicate_of = absorbed.remove(&i)?;
        item.body = item.body.map(|b| polis_core::dedup::excerpt_around(&b, &terms, 1600));
        Some(PackPromptHit { superseded_by: superseded.get(&item.seq).copied(), duplicate_of,
            stage: if arms.get(&item.seq).is_some_and(|a| a.iter().all(|h| h.arm==Arm::Semantic)) { "semantic".into() } else { stage.as_str().into() },
            arms: arms.get(&item.seq).cloned().unwrap_or_default(), item })
    }).collect::<Vec<_>>();
    // One-hop conversational expansion. A question often contains the search
    // terms while its immediately following answer contains only the value.
    let mut anchors=prompt_hits.iter().take(25).filter(|hit| {
        let text=hit.item.body.as_deref().unwrap_or("").trim().to_ascii_lowercase();
        (hit.item.role.as_deref()==Some("user") && text.contains('?')) ||
            ["yes", "no,", "that", "it ", "use that", "the latter", "the former"].iter().any(|p|text.starts_with(p))
    }).map(|h|h.item.clone()).collect::<Vec<_>>();
    // Role filters select returned evidence. A permitted user question can
    // locate an assistant answer without returning the excluded question.
    if scope.roles.iter().any(|r|r=="assistant") && !scope.roles.iter().any(|r|r=="user") {
        if let Some(q)=query {
            let mut questions=scope.clone();questions.roles=vec!["user".into()];
            anchors.extend(db.search_prompts_ranked_scoped(q,25,&questions)?.into_iter().map(|(item,_)|item).filter(|item|item.body.as_deref().is_some_and(|b|b.contains('?'))));
        }
    }
    let known=prompt_hits.iter().map(|h|h.item.seq).collect::<HashSet<_>>();
    let mut discovered=HashSet::new();let mut neighbor_seqs=Vec::new();let mut neighbors_by_anchor=HashMap::new();
    for anchor in anchors.iter().take(25) {
        if let Some(session)=anchor.session_id.as_deref() {
            let seqs=db.neighboring_prompt_seqs(anchor.seq,session,scope)?.into_iter().filter(|seq|!known.contains(seq)&&discovered.insert(*seq)).collect::<Vec<_>>();
            neighbor_seqs.extend(seqs.iter().copied());neighbors_by_anchor.insert(anchor.seq,seqs);
        }
    }
    let neighbor_candidates=neighbor_seqs.len();
    let mut neighbor_superseded=db.supersessions_for_seqs(&neighbor_seqs)?;
    let eligible=db.eligible_seqs(&neighbor_superseded.values().copied().collect::<Vec<_>>(),scope)?;
    neighbor_superseded.retain(|_,replacement|eligible.contains(replacement));
    let mut neighbors=db.lake_items_for_seqs_scoped(&neighbor_seqs,scope)?.into_iter().map(|mut item| {
        item.body=item.body.map(|body|polis_core::dedup::excerpt_around(&body,&terms,800));
        (item.seq,PackPromptHit {superseded_by:neighbor_superseded.get(&item.seq).copied(),item,duplicate_of:Vec::new(),stage:"neighbor".into(),arms:vec![ArmHit {arm:Arm::Neighbor,rank:1,score:0.0}]})
    }).collect::<HashMap<_,_>>();
    let mut expanded=Vec::new();
    for hit in prompt_hits {
        let seq=hit.item.seq;expanded.push(hit);
        for neighbor in neighbors_by_anchor.get(&seq).into_iter().flatten() {
            if let Some(hit)=neighbors.remove(neighbor) {expanded.push(hit);}
        }
    }
    for seq in neighbor_seqs {if let Some(hit)=neighbors.remove(&seq) {expanded.push(hit);}}
    if expanded.len()>limit {expanded.truncate(limit);cuts.push("promptHits".into());if neighbor_candidates>0 {cuts.push("neighbors".into());}}
    prompt_hits=expanded;
    considered+=neighbor_candidates;
    mark!("pack.rank");
    let mut browse = match query { Some(q) => db.search_browse_events_scoped(q, pool as i64, scope)?, None => Vec::new() };
    let browse_lexical_count = browse.len();
    let lexical = browse.iter().map(|h| (h.id.to_string(), -h.score)).collect();
    let sem_pages = semantic.as_ref().into_iter().flatten().filter(|h| h.target_kind=="browse_event").collect::<Vec<_>>();
    let sem = sem_pages.iter().map(|h| (h.target_id.to_string(), f64::from(h.score))).collect();
    let page_fused = rrf_fuse(&[(Arm::Lexical,lexical),(Arm::Semantic,sem)]);
    let existing = browse.iter().map(|h| h.id).collect::<HashSet<_>>();
    let extra = sem_pages.iter().map(|h| h.target_id).filter(|id| !existing.contains(id)).collect::<Vec<_>>();
    browse.extend(db.browse_hits_for_ids(&extra, &terms)?);
    let order = page_fused.iter().enumerate().filter_map(|(i,(id,_,_))| id.parse::<i64>().ok().map(|id|(id,i))).collect::<HashMap<_,_>>();
    browse.sort_by_key(|h| order.get(&h.id).copied().unwrap_or(usize::MAX));
    considered += browse.len();
    let hashes = db.context_hashes_for_browse_ids(&browse.iter().map(|h| h.id).collect::<Vec<_>>())?;
    let mut seen = HashSet::new();
    browse.retain(|h| seen.insert(hashes.get(&h.id).filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| h.id.to_string())));
    if browse.len()>limit { browse.truncate(limit); cuts.push("browseHits".into()); }
    mark!("pack.browse");
    let literal = matches!((&plan,query), (Some(p),Some(q)) if polis_core::query::looks_literal(p,q));
    let grep_hits = if literal {
        let needle = plan.as_ref().and_then(|p| p.phrases.iter().chain(p.terms.iter()).max_by_key(|s| (s.starts_with("--") || s.contains(['/','_','.']),s.len())));
        match needle {
            Some(needle) if needle.chars().count()>=polis_store::GREP_MIN_LITERAL => db.grep_memory_scoped(needle,None,false,GrepScope::All,limit as i64,scope).map_err(|e| rusqlite::Error::InvalidParameterName(e.to_string()))?,
            _ => Vec::new(),
        }
    } else { Vec::new() };
    considered += grep_hits.len();
    mark!("pack.grep");
    let mut shared_error=None;
    let shared_hits = if scope.include_shared {
        match crate::union::shared_hits_scoped_checked(polis,query,limit as i64,scope) {
            Ok(hits)=>hits,Err(error)=>{errors.push(format!("shared: {error}"));shared_error=Some(error);Vec::new()}
        }
    } else { Vec::new() };
    considered += shared_hits.len();
    mark!("pack.shared");
    let coverage = vec![
        ArmCoverage {arm:Arm::Neighbor,ran:neighbor_candidates>0,hits:neighbor_candidates,absent_because:None},
        ArmCoverage { arm: Arm::Node, ran: query.is_some()||node_id.is_some(),hits:usize::from(node.is_some())+matched.len(),absent_because:None },
        ArmCoverage { arm: Arm::Note, ran:query.is_some(),hits:notes.len(),absent_because:None },
        ArmCoverage { arm: Arm::Lexical, ran:query.is_some(),hits:lexical_count+browse_lexical_count,absent_because:None },
        ArmCoverage { arm: Arm::Grep, ran:literal,hits:grep_hits.len(),absent_because:(!literal).then(||"query does not name a literal".into()) },
        ArmCoverage { arm: Arm::Semantic, ran:semantic.is_some(),hits:semantic.as_ref().map_or(0,Vec::len),absent_because:semantic.is_none().then(|| if polis.embedder.is_none() { "no embedding provider configured" } else { "semantic index unavailable, incomplete, or provider failed" }.into()) },
    ];
    let mut coverage = crate::union::with_shared_coverage(coverage,scope.include_shared,shared_hits.len());
    if let Some(error)=shared_error {if let Some(arm)=coverage.iter_mut().find(|c|c.arm==Arm::Shared) {arm.ran=false;arm.absent_because=Some(format!("retrieval failed: {error}"));}}

    let claim_scope=polis_core::Scope {principal:scope.principal.clone(),project:scope.project.clone(),agent:scope.agent.clone(),run:scope.run.clone(),org:scope.org.clone(),include_shared:false};
    let mut claims=if query.is_some() {
        db.query_claims(&polis_core::claims::ClaimQuery {q:query.map(str::to_string),scope:claim_scope,limit:Some(pool),valid_at:filter.valid_at,known_at:filter.known_at,evidence_filter:filter.clone(),..Default::default()})?
    } else {Vec::new()};
    considered+=claims.len();
    coverage.push(ArmCoverage {arm:Arm::Claim,ran:query.is_some(),hits:claims.len(),absent_because:None});
    if claims.len()>limit {claims.truncate(limit);cuts.push("claims".into());}
    mark!("pack.claims");
    let _ = arm_start;
    let mut pack = AnswerPack { head_seq, query:query.map(str::to_string),node,matched_nodes:matched,notes,prompt_hits,browse_hits:browse,grep_hits,arm_coverage:coverage,truncated:cuts,shared_hits,claims,
        retrieval: RetrievalReport { version:"rrf-v2".into(),candidate_limit:pool,candidates_considered:considered,snapshot_hash,errors,timings_us:timings,index,..Default::default() } };
    enforce_pack_budget(&mut pack);
    pack.retrieval.candidates_returned = evidence_count(&pack);
    Ok(pack)
}

fn link_seq(link: &ClassLink) -> Option<i64> {
    matches!(link.target_kind.as_str(),"prompt"|"decision"|"ledger"|"resolution"|"approval"|"review_verdict").then(||link.target_id.parse().ok()).flatten()
}
fn evidence_count(pack: &AnswerPack) -> usize {
    pack.claims.len()+pack.prompt_hits.len()+pack.browse_hits.len()+pack.notes.len()+pack.grep_hits.len()+pack.shared_hits.len()+pack.node.as_ref().map_or(0,|n| n.links.len()+n.observations.len())
}

/// Execute a production retrieval against one pinned SQLite read snapshot.
pub fn search_request(polis: &Polis<'_>, req: &polis_core::api::SearchRequest, scope: &polis_store::principals::ScopeFilter) -> Result<AnswerPack, polis_core::MemoryError> {
    use polis_core::MemoryError;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let mut resolved = scope.clone();
    resolved.roles = req.filter.roles.iter().map(|r| polis_core::ledger::CorpusRole::parse(r).map(|r|r.as_str().to_string()).ok_or_else(||MemoryError::Rejected(format!("unknown role `{r}`")))).collect::<Result<Vec<_>,_>>()?;
    resolved.after=req.filter.after; resolved.before=req.filter.before;
    let scope=&resolved;
    let start = std::time::Instant::now();
    let started_at = now_millis();
    if req.q.as_ref().is_some_and(|q| q.len()>16_384) { return Err(MemoryError::Rejected("query exceeds 16 KiB".into())); }
    if req.cursor.as_ref().is_some_and(|cursor|cursor.len()>2048) {return Err(MemoryError::Rejected("cursor exceeds 2048 bytes".into()));}
    if req.trace_id.as_ref().is_some_and(|id| id.len()>128) { return Err(MemoryError::Rejected("traceId exceeds 128 bytes".into())); }
    if let (Some(after),Some(before))=(scope.after,scope.before) { if after>=before { return Err(MemoryError::Rejected("after must precede before".into())); } }
    let max_bytes = req.max_tokens.unwrap_or(MAX_CONTEXT_BYTES).min(MAX_CONTEXT_BYTES);
    if max_bytes<1024 { return Err(MemoryError::Rejected("search maxTokens must be at least 1024; use context for smaller budgets".into())); }
    let snapshot = polis.store.read_snapshot().map_err(|e| MemoryError::Store(e.to_string()))?;
    let view = Polis::new(&snapshot, polis.agent.clone(), polis.host, polis.sink).with_embedder(polis.embedder.clone());
    let limit = clamp_answer_pack_limit(req.limit);
    let pool = req.candidate_limit.unwrap_or(200).clamp(limit as usize,200);
    let mut pack = if let Some(cursor) = &req.cursor {
        inspect_page(&view, req, scope, cursor, limit)?
    } else {
        assemble_pack(&view, req.q.as_deref(), req.node.as_deref(), limit, pool, scope, &req.filter).map_err(|e| MemoryError::Store(e.to_string()))?
    };
    let id = format!("{}-{}-{}", req.trace_id.as_deref().unwrap_or("retrieval"),started_at,NEXT.fetch_add(1,Ordering::Relaxed));
    pack.retrieval.trace_id = Some(id.clone());
    if req.cursor.is_none() && (!pack.truncated.is_empty() || pack.retrieval.candidates_considered>=pool) {
        pack.retrieval.continuation = Some(inspection_cursor(&pack,req,scope,0));
    }
    enforce_pack_budget_bytes(&mut pack,max_bytes);
    pack.retrieval.candidates_returned = evidence_count(&pack);
    if req.cursor.is_some() && pack.retrieval.continuation.is_some() {
        if let Some(last)=pack.prompt_hits.last() { pack.retrieval.continuation=Some(inspection_cursor(&pack,req,scope,last.item.seq)); }
    }
    if serde_json::to_vec(&pack).map_err(|e|MemoryError::Store(e.to_string()))?.len()>max_bytes {
        return Err(MemoryError::Rejected("response metadata exceeds requested budget".into()));
    }
    crate::warmth::record_pack(polis.store, &pack, now_millis());
    let trace = polis_core::diagnostics::RetrievalTrace {
        id,started_at,elapsed_ms:start.elapsed().as_secs_f64()*1000.0,scope: polis_core::Scope { principal:scope.principal.clone(),project:scope.project.clone(),agent:scope.agent.clone(),run:scope.run.clone(),org:scope.org.clone(),include_shared:scope.include_shared },head_seq:pack.head_seq,snapshot_hash:pack.retrieval.snapshot_hash.clone(),
        config:serde_json::json!({"version":"rrf-v2","limit":limit,"candidateLimit":pool,"maxBytes":max_bytes,"filter":req.filter,"correlationId":req.trace_id,"timingsUs":pack.retrieval.timings_us,"index":pack.retrieval.index,"selectedForeignCitations":pack.shared_hits.iter().map(|h|h.cite()).collect::<Vec<_>>(),"ranking":pack.prompt_hits.iter().map(|h|serde_json::json!({"seq":h.item.seq,"stage":h.stage,"arms":h.arms})).collect::<Vec<_>>()}),
        coverage:serde_json::to_value(&pack.arm_coverage).unwrap_or_default(),
        selected_seqs:pack.prompt_hits.iter().map(|h|h.item.seq).chain(pack.browse_hits.iter().filter_map(|h|h.seq)).chain(pack.notes.iter().filter_map(|n|n.seq)).chain(pack.grep_hits.iter().filter_map(|h|h.seq)).chain(pack.claims.iter().flat_map(|c|c.assertion.sources.iter().map(|s|s.seq))).chain(pack.node.iter().flat_map(|n|n.links.iter().filter_map(|l|link_seq(&l.link)))).collect(),
        cuts:pack.truncated.clone(),errors:pack.retrieval.errors.clone(),
        replay:"requires original query and preserved evidence/index snapshot; raw queries are not stored".into(),
    };
    drop(view); drop(snapshot);
    if let Err(e)=polis.store.save_retrieval_trace(&trace) { tracing::warn!(error=%e,"retrieval trace persistence failed"); }
    Ok(pack)
}

fn inspection_key(req: &polis_core::api::SearchRequest, scope: &polis_store::principals::ScopeFilter) -> String {
    polis_core::ledger::sha256_hex(format!("{:?}|{:?}|{:?}|{:?}",req.q,req.node,req.filter,scope).as_bytes())
}
fn inspection_cursor(pack: &AnswerPack, req: &polis_core::api::SearchRequest, scope: &polis_store::principals::ScopeFilter, after: i64) -> String {
    serde_json::json!({"mode":"inspection","head":pack.retrieval.snapshot_hash,"query":inspection_key(req,scope),"after":after}).to_string()
}
/// Continuations deliberately expose chronological source inspection, without
/// pretending the bounded relevance pool contains every possible match.
fn inspect_page(polis: &Polis<'_>, req: &polis_core::api::SearchRequest, scope: &polis_store::principals::ScopeFilter, cursor: &str, limit:i64) -> Result<AnswerPack,polis_core::MemoryError> {
    use polis_core::MemoryError;
    let c:serde_json::Value=serde_json::from_str(cursor).map_err(|_|MemoryError::Rejected("invalid inspection cursor".into()))?;
    let (head,hash)=polis.store.snapshot_head().map_err(|e|MemoryError::Store(e.to_string()))?;
    if c["mode"]!="inspection" || c["head"].as_str()!=Some(&hash) || c["query"].as_str()!=Some(&inspection_key(req,scope)) { return Err(MemoryError::Rejected("cursor does not match this query, scope, or evidence snapshot".into())); }
    let after=c["after"].as_i64().filter(|n|*n>=0).ok_or_else(||MemoryError::Rejected("invalid inspection position".into()))?;
    let rows=polis.store.list_lake_items_since_scoped(after,limit+1,scope).map_err(|e|MemoryError::Store(e.to_string()))?;
    let has_more=rows.len()>limit as usize;
    let mut pack=empty_pack(req.q.as_deref(),String::new()); pack.retrieval.errors.clear();
    pack.head_seq=head; pack.retrieval.snapshot_hash=hash; pack.retrieval.version="inspection-v1".into(); pack.retrieval.candidate_limit=limit as usize;
    pack.retrieval.candidates_considered=rows.len();
    let seqs=rows.iter().map(|r|r.seq).collect::<Vec<_>>();
    let superseded=polis.store.supersessions_for_seqs(&seqs).map_err(|e|MemoryError::Store(e.to_string()))?;
    pack.prompt_hits=rows.into_iter().take(limit as usize).map(|item|PackPromptHit { superseded_by:superseded.get(&item.seq).copied(),item,duplicate_of:Vec::new(),stage:"inspection".into(),arms:Vec::new() }).collect();
    if has_more { let last=pack.prompt_hits.last().map_or(after,|h|h.item.seq); pack.retrieval.continuation=Some(inspection_cursor(&pack,req,scope,last)); }
    Ok(pack)
}

pub fn context_request(polis: &Polis<'_>, req: &polis_core::api::ContextRequest, scope: &polis_store::principals::ScopeFilter) -> Result<ContextBlock, polis_core::MemoryError> {
    let _timer=crate::latency::Timer::start("context");
    let search = polis_core::api::SearchRequest { q:Some(req.q.clone()),node:req.node.clone(),limit:Some(INLINE_PACK_LIMIT),scope:req.scope.clone(),filter:req.filter.clone(),trace_id:req.trace_id.clone(),..Default::default() };
    let mut pack=search_request(polis,&search,scope)?;
    let plan=plan_fts_query(&req.q);
    // Bytes are a conservative tokenizer-independent ceiling: any supported
    // byte-level model tokenizer needs no more tokens than UTF-8 bytes.
    let budget=req.max_tokens.unwrap_or(2000).min(req.max_bytes.unwrap_or(INLINE_PACK_MAX_BYTES)).min(INLINE_PACK_MAX_BYTES);
    let text=render_answer_pack_block(&pack,plan.as_ref(),budget);
    let full_len=render_answer_pack_block(&pack,plan.as_ref(),usize::MAX).map_or(0,|s|s.len());
    let rendered_len=text.as_ref().map_or(0,String::len);
    if full_len>rendered_len {pack.truncated.push("renderedContext".into());}
    if let Some(id)=pack.retrieval.trace_id.as_deref() {
        match polis.store.retrieval_traces(Some(id),1) {
            Ok(mut traces)=>if let Some(trace)=traces.first_mut() {
                trace.config["contextMaxBytes"]=budget.into();
                trace.config["contextRenderedBytes"]=rendered_len.into();
                trace.config["selectionStage"]=serde_json::json!("selectedSeqs describe the structured pack before context rendering");
                trace.cuts=pack.truncated.clone();
                if let Err(error)=polis.store.save_retrieval_trace(trace) {tracing::warn!(%error,"context trace update failed");}
            },
            Err(error)=>tracing::warn!(%error,"context trace read failed"),
        }
    }
    Ok(ContextBlock { text,terms:plan.map(|p|p.terms).unwrap_or_default(),coverage:pack.arm_coverage,truncated:pack.truncated,retrieval:pack.retrieval })
}

/// Scoped threads are reconstructed from ledger-backed turns. Host-only text
/// has no scope proof and is exposed only through the unscoped host contract.
pub fn thread_view_scoped(polis: &Polis<'_>, kind: &str, id: &str, limit: i64, scope: &polis_store::principals::ScopeFilter) -> rusqlite::Result<Option<serde_json::Value>> {
    if scope.is_empty() { if let Some(view) = thread_view(polis, kind, id, limit) { return Ok(Some(view)); } }
    let items = polis.store.thread_items_scoped(kind, id, limit, scope)?;
    if items.is_empty() { return Ok(None); }
    let mut messages = items.into_iter().map(|it| serde_json::json!({"seq":it.seq,"role":it.role.unwrap_or_else(|| "user".into()),"body":it.body.unwrap_or_default(),"createdAt":it.ts})).collect::<Vec<_>>();
    let mut size = 0; let mut start = messages.len();
    for (index, message) in messages.iter().enumerate().rev() { size += serde_json::to_vec(message).map(|v|v.len()).unwrap_or(0); if size > MAX_CONTEXT_BYTES { break; } start=index; }
    messages.drain(..start);
    Ok(Some(serde_json::json!({"kind":kind,"id":id,"label":format!("{kind} {id}"),"messages":messages,"source":"ledger"})))
}

pub fn build_thread_tree_scoped(polis: &Polis<'_>, kind: &str, id: &str, scope: &polis_store::principals::ScopeFilter) -> rusqlite::Result<serde_json::Value> {
    if scope.is_empty() { return Ok(build_thread_tree(polis,kind,id)); }
    let digests = polis.store.thread_digests_scoped(scope)?;
    let find = |kind:&str,id:&str| digests.iter().find(|d|d.kind==kind && d.id==id);
    let Some(node) = find(kind,id) else { return Ok(serde_json::json!({"node":null,"parent":null,"children":[]})); };
    let render = |d:&polis_store::scoped_views::ThreadDigest| serde_json::json!({"kind":d.kind,"id":d.id,"label":format!("{} {}",d.kind,d.id),"messageCount":d.count,"lastTs":d.last_ts});
    let parent = polis.store.session_tree_parent(kind,id)?.and_then(|(k,i)|find(&k,&i).map(render));
    let children = polis.store.session_tree_children(kind,id)?.into_iter().filter_map(|(k,i,created)|find(&k,&i).map(|d| {let mut value=render(d);value["createdAt"]=created.into();value})).collect::<Vec<_>>();
    Ok(serde_json::json!({"node":render(node),"parent":parent,"children":children}))
}

pub fn build_memory_map_scoped(polis: &Polis<'_>, scope: &polis_store::principals::ScopeFilter) -> rusqlite::Result<MemoryMapView> {
    if scope.is_empty() { return Ok(build_memory_map(polis)); }
    let classes = polis.store.list_class_nodes_scoped(scope)?.into_iter().filter(|n| n.status=="accepted").collect::<Vec<_>>();
    let class_ids = classes.iter().map(|n| n.id.clone()).collect::<std::collections::HashSet<_>>();
    let mut nodes=Vec::new(); let mut edges=Vec::new();
    for node in classes {
        let parent=node.parent_id.as_ref().filter(|id|class_ids.contains(*id)).map(|id|format!("class:{id}"));
        let id=format!("class:{}",node.id);
        if let Some(parent)=&parent { edges.push(MapEdge{kind:"contains".into(),from:parent.clone(),to:id.clone(),weight:1,basis:None}); }
        let mass=polis.store.list_class_links_for_node_scoped(&node.id,scope)?.len() as i64;
        nodes.push(MapNode{id,kind:if node.kind=="digest"{"digest"}else{"class"}.into(),label:node.title,mass,parent_id:parent,pinned:node.pinned,project_path:node.project_path,class_node_id:Some(node.id),session_id:None,browse_id:None,thread_id:None});
    }
    for thread in polis.store.thread_digests_scoped(scope)? {
        let session=thread.kind=="session";let browse=thread.kind=="browse";
        nodes.push(MapNode{id:format!("thread:{}:{}",thread.kind,thread.id),kind:if session{"session"}else{"thread"}.into(),label:format!("{} {}",thread.kind,thread.id),mass:thread.count,parent_id:None,pinned:false,project_path:scope.project.clone(),class_node_id:None,session_id:session.then(||thread.id.clone()),browse_id:browse.then(||thread.id.clone()),thread_id:(!session&&!browse).then_some(thread.id)});
    }
    let ids=nodes.iter().map(|n|n.id.clone()).collect::<std::collections::HashSet<_>>();
    for (ck,ci,pk,pi) in polis.store.list_session_tree_rows()? {
        let from=format!("thread:{pk}:{pi}");let to=format!("thread:{ck}:{ci}");
        if ids.contains(&from)&&ids.contains(&to){if let Some(node)=nodes.iter_mut().find(|n|n.id==to){node.parent_id=Some(from.clone());}edges.push(MapEdge{kind:"lineage".into(),from,to,weight:1,basis:None});}
    }
    let pairs=polis.store.list_supersession_pairs()?;let seqs=pairs.iter().flat_map(|(a,b)|[*a,*b]).collect::<Vec<_>>();let eligible=polis.store.eligible_seqs(&seqs,scope)?;let endpoints=polis.store.resolve_map_endpoints(&seqs)?;
    let endpoint=|seq:i64|endpoints.get(&seq).and_then(|(session,class)| class.as_ref().map(|id|format!("class:{id}")).filter(|id|ids.contains(id)).or_else(||session.as_ref().map(|id|format!("thread:session:{id}")).filter(|id|ids.contains(id))));
    for (old,new) in pairs { if eligible.contains(&old)&&eligible.contains(&new){if let(Some(from),Some(to))=(endpoint(old),endpoint(new)){if from!=to{edges.push(MapEdge{kind:"supersedes".into(),from,to,weight:1,basis:Some(format!("#{old} → #{new}"))});}}} }
    nodes.sort_by(|a,b|a.id.cmp(&b.id));edges.sort_by(|a,b|(&a.kind,&a.from,&a.to).cmp(&(&b.kind,&b.from,&b.to)));
    Ok(MemoryMapView{generated_ts:now_millis(),nodes,edges})
}

/// One session-tree node with its parent and child digests — shared by
/// `GET /v1/context/tree/:kind/:id` AND the `context_thread_tree` command
/// (same assembly, two thin callers, so route and GUI can't drift).
pub fn build_thread_tree(polis: &Polis<'_>, kind: &str, id: &str) -> serde_json::Value {
    let db = polis.store;
    let parent = db.session_tree_parent(kind, id).ok().flatten();
    let children = db.session_tree_children(kind, id).unwrap_or_default();
    let child_digests: Vec<serde_json::Value> = children
        .into_iter()
        .map(|(ck, cid, created_at)| {
            let (count, last_ts) = polis.thread_stats(&ck, &cid).unwrap_or((0, None));
            serde_json::json!({
                "kind": ck,
                "id": cid,
                "label": polis.thread_label(&ck, &cid),
                "createdAt": created_at,
                "messageCount": count,
                "lastTs": last_ts,
            })
        })
        .collect();
    let (count, last_ts) = polis.thread_stats(kind, id).unwrap_or((0, None));
    serde_json::json!({
        "node": {
            "kind": kind,
            "id": id,
            "label": polis.thread_label(kind, id),
            "messageCount": count,
            "lastTs": last_ts,
        },
        "parent": parent.map(|(pk, pid)| serde_json::json!({
            "kind": pk,
            "id": pid,
            "label": polis.thread_label(&pk, &pid),
        })),
        "children": child_digests,
    })
}

/// A host thread's tail as `GET /v1/context/threads/:kind/:id` serves it
/// (Session A6): the host's rows (`HostResolver::thread_messages`), each body
/// capped, then the leading turns dropped once the byte budget is spent —
/// the tail wins. `None` for a kind the host has no table for (the route
/// 404s). The body is the route's assembly, moved verbatim.
pub fn thread_view(polis: &Polis<'_>, kind: &str, id: &str, limit: i64) -> Option<serde_json::Value> {
    let msgs = polis.host.thread_messages(kind, id, limit)?;
    // Byte-bound the response like /v1/context/prompts: cap each body,
    // then drop leading turns once the budget is spent (tail wins).
    let mut msgs = msgs;
    for m in &mut msgs {
        if m.body.chars().count() > 4000 {
            m.body = m.body.chars().take(4000).collect::<String>() + "…";
        }
    }
    let mut total = 0usize;
    let mut start = msgs.len();
    for (i, m) in msgs.iter().enumerate().rev() {
        total += 120 + m.body.len();
        if total > MAX_CONTEXT_BYTES {
            break;
        }
        start = i;
    }
    let tail = &msgs[start..];
    Some(serde_json::json!({
        "kind": kind,
        "id": id,
        "label": polis.thread_label(kind, id),
        "messages": tail,
    }))
}

/// The answer pack rendered as ONE grounding block (`GET /v1/memory/context`,
/// the MCP `memory_context` tool): the discussion prefetch's exact shape —
/// plan the question, build the pack, render it honest about what it searched
/// and trimmed. `text: None` when the record has nothing on it.
pub fn context_block(polis: &Polis<'_>, q: &str, node: Option<&str>, max_bytes: usize) -> ContextBlock {
    context_block_scoped(polis, q, node, max_bytes, &polis_store::principals::ScopeFilter::default())
}

/// The grounding block under an identity scope (E2).
pub fn context_block_scoped(polis: &Polis<'_>, q: &str, node: Option<&str>, max_bytes: usize, scope: &polis_store::principals::ScopeFilter) -> ContextBlock {
    crate::latency::timed("context", || {
        let plan = plan_fts_query(q);
        let terms = plan.as_ref().map(|p| p.terms.clone()).unwrap_or_default();
        let mut pack = build_answer_pack_scoped(polis, Some(q), node, INLINE_PACK_LIMIT, scope);
        enforce_pack_budget(&mut pack);
        let text = render_answer_pack_block(&pack, plan.as_ref(), max_bytes);
        ContextBlock { text, terms,coverage:pack.arm_coverage,truncated:pack.truncated,retrieval:pack.retrieval }
    })
}

// ---------------------------------------------------------------------------
// The route views (Session A5): what `/v1/memory/tree`, `/v1/memory/node/:id`
// and the Timeline serve — assembled here so the HTTP router, the MCP tools
// and a host's handlers all build one shape.
// ---------------------------------------------------------------------------

use polis_core::api::{LinkView, NodeView, TreeNodeView};

/// The Timeline page — the store's rows with the host's own pictures joined
/// (`HostResolver::surface_shot_keys`).
pub fn query_ledger(polis: &Polis<'_>, f: &LedgerFilters) -> Result<Vec<TimelineItem>, String> {
    polis.query_ledger_events(f).map_err(|e| e.to_string())
}

/// The catalog as the tree route serves it: every node with its link count,
/// optionally one root (by id, or by the project path bound to it).
pub fn tree_view(
    polis: &Polis<'_>,
    root: Option<&str>,
    project: Option<&str>,
) -> rusqlite::Result<Vec<TreeNodeView>> {
    tree_view_scoped(polis, root, project, &Default::default())
}

pub fn tree_view_scoped(polis: &Polis<'_>, root: Option<&str>, project: Option<&str>, scope: &polis_store::principals::ScopeFilter) -> rusqlite::Result<Vec<TreeNodeView>> {
    let mut filter=scope.clone();
    if let Some(project)=project { if filter.project.as_deref().is_some_and(|p|p!=project) { return Ok(Vec::new()); } filter.project=Some(project.into()); }
    let all = polis.store.list_class_nodes_scoped(&filter)?.into_iter().map(|node| {
        let count=polis.store.list_class_links_for_node_scoped(&node.id,&filter)?.len() as i64;
        Ok((node,count))
    }).collect::<rusqlite::Result<Vec<_>>>()?;
    let root_id: Option<String> = if let Some(r) = root.filter(|s| !s.trim().is_empty()) {
        Some(r.trim().to_string())
    } else if let Some(p) = project.filter(|s| !s.trim().is_empty()) {
        all.iter()
            .find(|(n, _)| n.parent_id.is_none() && n.project_path.as_deref() == Some(p.trim()))
            .map(|(n, _)| n.id.clone())
    } else {
        None
    };
    Ok(match &root_id {
        Some(rid) => {
            let keep = subtree_ids(&all, rid);
            all.into_iter()
                .filter(|(n, _)| keep.contains(&n.id))
                .map(|(node, link_count)| TreeNodeView { node, link_count })
                .collect()
        }
        None => all.into_iter().map(|(node, link_count)| TreeNodeView { node, link_count }).collect(),
    })
}

fn subtree_ids(all: &[(ClassNode, i64)], root: &str) -> std::collections::HashSet<String> {
    let mut keep = std::collections::HashSet::new();
    keep.insert(root.to_string());
    // Iterate to a fixpoint (tree is small).
    loop {
        let before = keep.len();
        for (n, _) in all {
            if let Some(p) = &n.parent_id {
                if keep.contains(p) {
                    keep.insert(n.id.clone());
                }
            }
        }
        if keep.len() == before {
            break;
        }
    }
    keep
}

/// One node with its children, decorated links (lake label + supersession
/// status) and observations — the node route.
pub fn node_view(polis: &Polis<'_>, id: &str) -> rusqlite::Result<Option<NodeView>> {
    node_view_scoped(polis,id,&Default::default())
}

pub fn node_view_scoped(polis: &Polis<'_>, id: &str, scope: &polis_store::principals::ScopeFilter) -> rusqlite::Result<Option<NodeView>> {
    let db = polis.store;
    let Some(node) = db.get_class_node_scoped(id,scope)? else {
        return Ok(None);
    };
    let children = db.list_class_children_scoped(id,scope)?;
    let raw_links = db.list_class_links_for_node_scoped(id,scope)?;
    let ledger_seq = |l: &ClassLink| -> Option<i64> {
        matches!(l.target_kind.as_str(), "prompt" | "decision" | "ledger")
            .then(|| l.target_id.trim().parse().ok())
            .flatten()
    };
    let seqs: Vec<i64> = raw_links.iter().filter_map(&ledger_seq).collect();
    let labels = db.link_previews_for_seqs(&seqs).unwrap_or_default();
    let mut superseded = db.supersessions_for_seqs(&seqs)?;
    let eligible=db.eligible_seqs(&superseded.values().copied().collect::<Vec<_>>(),scope)?;
    superseded.retain(|_,replacement|eligible.contains(replacement));
    let links: Vec<LinkView> = raw_links
        .into_iter()
        .map(|link| {
            let seq = ledger_seq(&link);
            LinkView {
                label: seq.and_then(|s| labels.get(&s).cloned()),
                superseded_by: seq.and_then(|s| superseded.get(&s).copied()),
                link,
            }
        })
        .collect();
    let observations = db.list_class_observations_scoped(id,false,scope)?;
    Ok(Some(NodeView { node, children, links, observations }))
}
