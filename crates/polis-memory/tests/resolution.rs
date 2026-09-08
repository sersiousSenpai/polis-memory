// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The answer pack claims a class only when that class covers the question.
//! On a real lake, "what repos did I look at for Redline's memory?" resolved
//! first to an astronomy class (both titles tokenized "…'s" to a lone `s`)
//! and then to "Payload CMS lookup" (`look*` → `lookup`): one loose term of
//! four. Neither is the node such a question is about.

use polis_core::host::NoHost;
use polis_core::proposal::Proposal;
use polis_llm::NoopSink;
use polis_memory::corpus::{seed_corpus, CorpusSpec};
use polis_memory::retrieval::build_answer_pack;
use polis_memory::Polis;
use polis_store::PolisStore;

fn store_with_classes(titles: &[&str]) -> PolisStore {
    let store = PolisStore::open_in_memory().unwrap();
    seed_corpus(&store, &CorpusSpec::new(40).with_seed(7)).unwrap();
    let root = store
        .list_class_nodes()
        .unwrap()
        .into_iter()
        .find(|n| n.parent_id.is_none())
        .expect("the corpus seeds a root")
        .id;
    for title in titles {
        store
            .stage_proposal(None, &Proposal::Create { parent_id: root.clone(), title: (*title).to_string(), rationale: None })
            .unwrap();
    }
    store.accept_all_pending("test").unwrap();
    store
}

const Q: &str = "what repos did i look at for Redline's memory?";

#[test]
fn one_loose_term_does_not_resolve_a_class() {
    let store = store_with_classes(&["Payload CMS lookup", "AION-1 — Polymathic's astronomy foundation model"]);
    let polis = Polis::new(&store, None, &NoHost, &NoopSink);
    let pack = build_answer_pack(&polis, Some(Q), None, 8);
    assert!(pack.node.is_none(), "resolved a class on a single loose term: {:?}", pack.node.map(|n| n.node.title));
    // The miss path still answers from the arms (the corpus has prompts).
    assert!(pack.arm_coverage.iter().any(|c| c.ran), "no arm ran");
}

#[test]
fn a_covering_class_resolves() {
    let store = store_with_classes(&["Payload CMS lookup", "Redline memory research — repos compared"]);
    let polis = Polis::new(&store, None, &NoHost, &NoopSink);
    let pack = build_answer_pack(&polis, Some(Q), None, 8);
    let title = pack.node.map(|n| n.node.title).expect("the covering class resolves");
    assert!(title.starts_with("Redline memory research"), "{title}");
}

#[test]
fn a_one_term_question_still_resolves_on_its_term() {
    let store = store_with_classes(&["SQLite FTS5 and the trigram tokenizer"]);
    let polis = Polis::new(&store, None, &NoHost, &NoopSink);
    let pack = build_answer_pack(&polis, Some("sqlite"), None, 8);
    assert!(pack.node.map(|n| n.node.title).unwrap_or_default().starts_with("SQLite"));
}

#[test]
fn stemming_counts_as_a_whole_token_match() {
    // The index stems (porter): "compacting" is "compaction" there, and a
    // plain-token check would have refused this resolution.
    let store = store_with_classes(&["Memory keeper compaction", "Embedded browser"]);
    let polis = Polis::new(&store, None, &NoHost, &NoopSink);
    let pack = build_answer_pack(&polis, Some("compacting cold bodies"), None, 8);
    assert_eq!(pack.node.map(|n| n.node.title).as_deref(), Some("Memory keeper compaction"));
    let pack = build_answer_pack(&polis, Some("what did I decide about the browser tab suspension"), None, 8);
    assert_eq!(pack.node.map(|n| n.node.title).as_deref(), Some("Embedded browser"));
}

#[test]
fn several_single_term_matches_are_ambiguous_and_resolve_nothing() {
    // On the live lake the question matched "repos" (stemmed: repo) in a
    // repo-comparison class and "memory" in the memory class — one term
    // each. Neither is the answer; the candidates are listed instead.
    let store = store_with_classes(&[
        "Payload CMS lookup",
        "SecuritiesList MVP — repo comparison & where to build",
        "Polis Memory extraction — standalone cross-model memory server & MCP",
    ]);
    let polis = Polis::new(&store, None, &NoHost, &NoopSink);
    let pack = build_answer_pack(&polis, Some(Q), None, 8);
    assert!(pack.node.is_none(), "ambiguous single-term matches resolved {:?}", pack.node.map(|n| n.node.title));
    assert!(pack.matched_nodes.len() >= 2, "the candidates are still listed: {:?}", pack.matched_nodes.len());
}

#[test]
fn the_widest_cover_beats_single_term_matches() {
    let store = store_with_classes(&[
        "SecuritiesList MVP — repo comparison & where to build",
        "Polis Memory extraction — standalone cross-model memory server & MCP",
        "Redline memory research — repos compared",
    ]);
    let polis = Polis::new(&store, None, &NoHost, &NoopSink);
    let pack = build_answer_pack(&polis, Some(Q), None, 8);
    assert_eq!(pack.node.map(|n| n.node.title).as_deref(), Some("Redline memory research — repos compared"));
}
