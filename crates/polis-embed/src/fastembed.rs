// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! `bge-small-en-v1.5` through ONNX Runtime (feature `fastembed`, opt-in).
//!
//! A 384-dim transformer: the quality ceiling of the portable providers and
//! the heaviest — the model is ~130 MB fetched from Hugging Face on first
//! use and the runtime is a ~30 MB shared library the BUILD downloads
//! (`ort/download-binaries`). `docs/bench.md` "Embedding providers" measures
//! what that buys before anyone makes it a default; here it is a provider
//! like any other. `POLIS_NO_NETWORK=1` refuses the first-use download (a
//! cached model still loads).

use std::path::Path;
use std::sync::Mutex;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::Embedder;

/// The row model id.
pub const MODEL_ID: &str = "fastembed/bge-small-en-v1.5";
pub const DIM: usize = 384;

pub struct FastEmbedder {
    inner: Mutex<TextEmbedding>,
}

impl FastEmbedder {
    /// Load (or fetch, when the network is allowed) into `cache_dir`.
    pub fn load(cache_dir: &Path) -> Result<Self, String> {
        let no_network = std::env::var("POLIS_NO_NETWORK").map(|v| v == "1").unwrap_or(false);
        let opts = InitOptions::new(EmbeddingModel::BGESmallENV15)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(false);
        if no_network && !cache_dir.exists() {
            return Err("POLIS_NO_NETWORK=1 and no cached bge-small-en-v1.5 model".into());
        }
        let inner = TextEmbedding::try_new(opts).map_err(|e| format!("fastembed: {e}"))?;
        Ok(Self { inner: Mutex::new(inner) })
    }
}

impl Embedder for FastEmbedder {
    fn model_id(&self) -> String {
        MODEL_ID.to_string()
    }
    fn dim(&self) -> usize {
        DIM
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let mut inner = self.inner.lock().map_err(|_| "fastembed mutex poisoned")?;
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        inner.embed(refs, None).map_err(|e| format!("fastembed: {e}"))
    }
}
