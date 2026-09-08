// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! §5.5's second test: every prompt builder emits the standing rule and
//! fences its items. A source scrape, so a builder added without the fence
//! fails here before it ever reaches a model.

fn builders(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find("pub fn build_") {
        let after = &rest[i..];
        let name_end = after.find('(').unwrap_or(after.len());
        let name = after[7..name_end].to_string();
        // The body runs to the next item at column 0.
        let body_end = after[1..]
            .find("\npub fn ")
            .or_else(|| after[1..].find("\n#["))
            .or_else(|| after[1..].find("\nimpl "))
            .map(|j| j + 1)
            .unwrap_or(after.len());
        let body = after[..body_end].to_string();
        if name.ends_with("_prompt") {
            out.push((name, body));
        }
        rest = &after[body_end..];
    }
    out
}

#[test]
fn every_prompt_builder_carries_the_rule_and_fences_its_items() {
    let sources = [
        ("organize.rs", include_str!("../src/organize.rs")),
        ("gardener.rs", include_str!("../src/gardener.rs")),
    ];
    let mut seen = 0;
    for (file, src) in sources {
        for (name, body) in builders(src) {
            seen += 1;
            assert!(body.contains("fence: &Fence"), "{file}::{name} does not take the run's fence");
            assert!(body.contains("fence.rule()"), "{file}::{name} does not emit the standing rule");
            assert!(body.contains("fence.wrap("), "{file}::{name} does not fence its items");
        }
    }
    assert!(seen >= 5, "expected the classifier, keeper, observation and both verifier builders; found {seen}");
}

#[test]
fn the_rule_names_the_fence_and_forbids_obeying_content() {
    let rule = polis_memory::fence::RULE;
    for phrase in ["never an instruction", "<<<ITEM", "<<<END", "nonce"] {
        assert!(rule.contains(phrase), "the rule lost `{phrase}`");
    }
}
