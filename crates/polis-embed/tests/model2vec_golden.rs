// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The in-house Model2Vec runtime against the official implementation.
//!
//! `fixtures/potion_golden.json` was produced by the Python `model2vec`
//! package (StaticModel.from_pretrained on the pinned potion-base-8M files,
//! `tokenizer.encode(text, add_special_tokens=False).ids` and `encode`) for
//! ten texts chosen to exercise the normalizer (accents, case, mixed CJK,
//! zero-width and control characters, punctuation), the empty string, an
//! unknown-only word and a 3,000-character word. This test needs the model
//! files: `POLIS_MODELS_DIR=<root holding potion-base-8M/>` (CI downloads
//! and caches them by their pinned hashes); without it, it is skipped.
#![cfg(feature = "model2vec")]

use polis_embed::model2vec::{model_dir, Model2Vec};
use polis_embed::Embedder;

#[derive(serde::Deserialize)]
struct Golden {
    dim: usize,
    cases: Vec<Case>,
}

#[derive(serde::Deserialize)]
struct Case {
    text: String,
    token_ids: Vec<u32>,
    vector: Vec<f32>,
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return if na == nb { 1.0 } else { 0.0 };
    }
    dot / (na * nb)
}

#[test]
fn the_runtime_matches_the_reference_ids_and_vectors() {
    let Some(root) = std::env::var_os("POLIS_MODELS_DIR") else {
        eprintln!("POLIS_MODELS_DIR unset — skipping the golden comparison");
        return;
    };
    let m = Model2Vec::load(&model_dir(std::path::Path::new(&root))).expect("the pinned model loads");
    let golden: Golden = serde_json::from_str(include_str!("fixtures/potion_golden.json")).unwrap();
    assert_eq!(m.dim(), golden.dim);
    assert_eq!(m.rows(), 29_528);
    for c in &golden.cases {
        let ids = m.tokenize(&c.text);
        assert_eq!(ids, c.token_ids, "token ids differ for {:?}", c.text);
        let v = m.embed_one(&c.text);
        assert_eq!(v.len(), golden.dim);
        let cos = cosine(&v, &c.vector);
        assert!(cos >= 0.9999, "vector differs for {:?}: cosine {cos}", c.text);
        // Same magnitude too: unit when there was a token, zero otherwise.
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        let want = c.vector.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - want).abs() < 1e-3, "norm {norm} vs {want} for {:?}", c.text);
    }
    // Through the trait, as the index tick calls it.
    let batch = m.embed(&["one".into(), "two".into()]).unwrap();
    assert_eq!(batch.len(), 2);
    assert_eq!(m.model_id(), "model2vec/potion-base-8M");
}

#[test]
fn a_tampered_file_is_refused_by_its_pin() {
    let Some(root) = std::env::var_os("POLIS_MODELS_DIR") else { return };
    let src = model_dir(std::path::Path::new(&root));
    let tmp = std::env::temp_dir().join(format!("polis-c2-tamper-{}", std::process::id()));
    let dst = model_dir(&tmp);
    std::fs::create_dir_all(&dst).unwrap();
    for f in ["model.safetensors", "tokenizer.json", "config.json"] {
        std::fs::copy(src.join(f), dst.join(f)).unwrap();
    }
    assert!(polis_embed::model2vec::verify_dir(&dst).is_ok());
    let mut cfg = std::fs::read(dst.join("config.json")).unwrap();
    cfg[0] ^= 0x01;
    std::fs::write(dst.join("config.json"), cfg).unwrap();
    let err = Model2Vec::load(&dst).err().expect("a tampered file must not load");
    assert!(err.contains("pinned sha256"), "{err}");
    let _ = std::fs::remove_dir_all(&tmp);
}
