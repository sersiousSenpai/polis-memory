// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! `polis-embed` — the semantic arm's providers and index.
//!
//! The [`Embedder`] trait, the on-device Apple backends (feature `apple`,
//! macOS only), the process-wide vector cache, brute-force cosine search over
//! the store's int8 rows, and the bounded index tick that embeds the backlog.
//! The pure vector arithmetic and chunker live in `polis_core::vec` and are
//! re-exported here.
//!
//! Provider SELECTION is the host's: `semantic_search` and `index_tick` take
//! the embedder to use, so a host that reads a setting (Redline: a cloud
//! opt-in in `app_settings`) or a CLI that reads `POLIS_EMBED` decide, and
//! this crate never reads configuration. `provider()` is the on-device
//! default a host may fall back to.
//!
//! Lifted from Redline's `embed.rs` in Session A3 of the Polis extraction.

use std::sync::{Arc, OnceLock, RwLock};

use polis_store::PolisStore;

pub use polis_core::vec::{
    chunk_text, cosine, pack, quantize, unpack, Chunk, QVec, CHUNK_MAX, CHUNK_OVERLAP, CHUNK_TARGET,
    OPENING_CHARS,
};

#[cfg(feature = "fastembed")]
pub mod fastembed;
#[cfg(feature = "model2vec")]
pub mod model2vec;
#[cfg(feature = "remote")]
pub mod remote;

/// The chunk count at which brute-force scan stops fitting a 50 ms budget.
///
/// **Write the number down so nobody re-litigates it from intuition.** Today:
/// 3,073 chunks × 512 dims = 1.57 M multiply-accumulates per query, well under
/// 1 ms in scalar Rust on `i8 → i32`. Linear in chunk count, so the crossover
/// is around **150,000 chunks**; at the observed ingest rate (~27k chunks/year)
/// that is about five years away. Until then an ANN index would be a structure
/// to maintain, invalidate and debug in exchange for nothing.
///
/// This is where `retrace`'s judgment explicitly does NOT transfer. It
/// concluded "loads ALL vectors into memory, O(N) per query" and excluded its
/// own vector search from the build — correctly, for a corpus of MILLIONS of
/// screen frames. Same algorithm, three orders of magnitude smaller regime,
/// opposite answer.
pub const BRUTE_FORCE_CEILING_CHUNKS: usize = 150_000;

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

/// A text-embedding backend. Behind a trait so the default is a SETTING rather
/// than a rewrite — a user who wants cloud quality flips a switch, and a user
/// who wants nothing to leave the machine keeps the default.
pub trait Embedder: Send + Sync {
    /// Stable identifier stored on every row, so a model change is a clean
    /// re-index rather than a migration.
    fn model_id(&self) -> String;
    fn dim(&self) -> usize;
    /// Embed a batch. Returns one vector per input, in order.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
}

/// Which provider is configured, including the honest third state.
///
/// Every provider reports as ITSELF (C2): a cloud model used to read as
/// `Absent` because the kind was derived from two Apple prefixes and
/// nothing else — the bug plan §7.3 names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// Apple's contextual embeddings (macOS 14+).
    AppleContextual,
    /// Apple's sentence embeddings (macOS 11+) — the 11–13 fallback.
    AppleSentence,
    /// The portable Model2Vec runtime (`model2vec/…`).
    Model2Vec,
    /// bge-small through ONNX Runtime (`fastembed/…`).
    FastEmbed,
    /// An OpenAI-compatible endpoint (`remote/…`, or a host's own `openai/…`).
    Remote,
    /// A model id this crate does not know — a host's own provider. Present,
    /// just not one of ours.
    Other,
    /// No provider available. The arm is ABSENT, not empty: the answer pack
    /// says so via `armCoverage` rather than returning nothing and letting the
    /// reader conclude the record is empty.
    Absent,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProviderKind::AppleContextual => "apple-contextual",
            ProviderKind::AppleSentence => "apple-sentence",
            ProviderKind::Model2Vec => "model2vec",
            ProviderKind::FastEmbed => "fastembed",
            ProviderKind::Remote => "remote",
            ProviderKind::Other => "other",
            ProviderKind::Absent => "absent",
        }
    }

    /// The kind a row's model id belongs to.
    pub fn of_model_id(model_id: &str) -> ProviderKind {
        match model_id {
            m if m.starts_with("apple-contextual") => ProviderKind::AppleContextual,
            m if m.starts_with("apple-sentence") => ProviderKind::AppleSentence,
            m if m.starts_with("model2vec/") => ProviderKind::Model2Vec,
            m if m.starts_with("fastembed/") => ProviderKind::FastEmbed,
            m if m.starts_with("remote/") || m.starts_with("openai/") => ProviderKind::Remote,
            "" => ProviderKind::Absent,
            _ => ProviderKind::Other,
        }
    }
}

/// Resolve the best available on-device provider, once per process.
///
/// `tauri.conf.json` pins `minimumSystemVersion: "11.0"` while
/// `NLContextualEmbedding` is macOS 14+, so the availability check is not
/// optional — it is the difference between working on the app's stated floor
/// and crashing on it. The three states are tried in quality order.
pub fn provider() -> Option<Arc<dyn Embedder>> {
    static P: OnceLock<Option<Arc<dyn Embedder>>> = OnceLock::new();
    P.get_or_init(build_provider).clone()
}

pub fn provider_kind() -> ProviderKind {
    provider().map(|p| p.kind()).unwrap_or(ProviderKind::Absent)
}

impl dyn Embedder {
    pub fn kind(&self) -> ProviderKind {
        ProviderKind::of_model_id(&self.model_id())
    }
}

/// Which provider to build (C2). Read from `POLIS_EMBED` by the CLI; a host
/// passes its own choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderChoice {
    /// The platform default, decided by measurement (docs/bench.md
    /// "Embedding providers", 2026-09-08): Model2Vec when its files are
    /// present — on the real corpus it beat the Apple sentence model on
    /// semantic Recall@10 (0.96 vs 0.60), tied the fused pack, embedded a
    /// thousand times faster and was the first provider to clear the
    /// filing precision floor — else Apple's on-device model on macOS, else
    /// none. Never a download on this path (`polis init` / `serve` fetch
    /// the files; a read never does).
    Auto,
    Apple,
    Model2Vec,
    FastEmbed,
    Remote,
    /// No semantic arm.
    None,
}

impl ProviderChoice {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => ProviderChoice::Auto,
            "apple" => ProviderChoice::Apple,
            "model2vec" | "potion" => ProviderChoice::Model2Vec,
            "fastembed" | "bge" => ProviderChoice::FastEmbed,
            "remote" | "openai" => ProviderChoice::Remote,
            "none" | "off" | "absent" => ProviderChoice::None,
            _ => return None,
        })
    }

    /// `POLIS_EMBED`, or `Auto`.
    pub fn from_env() -> Self {
        std::env::var("POLIS_EMBED").ok().and_then(|v| Self::parse(&v)).unwrap_or(ProviderChoice::Auto)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ProviderChoice::Auto => "auto",
            ProviderChoice::Apple => "apple",
            ProviderChoice::Model2Vec => "model2vec",
            ProviderChoice::FastEmbed => "fastembed",
            ProviderChoice::Remote => "remote",
            ProviderChoice::None => "none",
        }
    }
}

/// Build the chosen provider. `models_root` is where downloadable models
/// live (`$POLIS_HOME/models`); `allow_download` permits a first-use fetch
/// for an EXPLICIT choice only — `Auto` never downloads (a read path must
/// not start one). `Ok(None)` is the honest absent state; `Err` names why
/// an explicit choice could not be met.
pub fn select(
    choice: ProviderChoice,
    models_root: Option<&std::path::Path>,
    allow_download: bool,
) -> Result<Option<Arc<dyn Embedder>>, String> {
    match choice {
        ProviderChoice::None => Ok(None),
        ProviderChoice::Apple => Ok(provider()),
        ProviderChoice::Model2Vec => {
            #[cfg(feature = "model2vec")]
            {
                let root = models_root.ok_or("model2vec needs a models directory")?;
                let m = model2vec::Model2Vec::ensure(root, allow_download)?;
                Ok(Some(Arc::new(m)))
            }
            #[cfg(not(feature = "model2vec"))]
            {
                let _ = (models_root, allow_download);
                Err("this build has no `model2vec` feature".into())
            }
        }
        ProviderChoice::FastEmbed => {
            #[cfg(feature = "fastembed")]
            {
                let root = models_root.ok_or("fastembed needs a models directory")?;
                let m = fastembed::FastEmbedder::load(&root.join("fastembed"))?;
                Ok(Some(Arc::new(m)))
            }
            #[cfg(not(feature = "fastembed"))]
            {
                Err("this build has no `fastembed` feature".into())
            }
        }
        ProviderChoice::Remote => {
            #[cfg(feature = "remote")]
            {
                remote::RemoteEmbedder::from_env()
                    .map(|r| Some(Arc::new(r) as Arc<dyn Embedder>))
                    .ok_or_else(|| format!("remote needs {} and {} (and no POLIS_NO_NETWORK)", remote::ENV_URL, remote::ENV_MODEL))
            }
            #[cfg(not(feature = "remote"))]
            {
                Err("this build has no `remote` feature".into())
            }
        }
        ProviderChoice::Auto => {
            #[cfg(feature = "model2vec")]
            {
                if let Some(root) = models_root {
                    if let Ok(m) = model2vec::Model2Vec::load(&model2vec::model_dir(root)) {
                        return Ok(Some(Arc::new(m)));
                    }
                }
                #[cfg(feature = "bundled-model")]
                {
                    if let Ok(m) = model2vec::Model2Vec::bundled() {
                        return Ok(Some(Arc::new(m)));
                    }
                }
            }
            Ok(provider())
        }
    }
}

#[cfg(all(target_os = "macos", feature = "apple"))]
fn build_provider() -> Option<Arc<dyn Embedder>> {
    if let Some(e) = apple::ContextualEmbedder::available() {
        return Some(Arc::new(e));
    }
    if let Some(e) = apple::SentenceEmbedder::available() {
        return Some(Arc::new(e));
    }
    tracing::info!("no on-device embedding provider — the semantic arm is absent");
    None
}

#[cfg(not(all(target_os = "macos", feature = "apple")))]
fn build_provider() -> Option<Arc<dyn Embedder>> {
    None
}

/// One specific Apple provider, for a measurement that wants to name it
/// (`polis_embed::select(Apple)` takes the best available). `None` off
/// macOS, without the feature, or when that model is not loadable.
pub fn apple_named(kind: ProviderKind) -> Option<Arc<dyn Embedder>> {
    #[cfg(all(target_os = "macos", feature = "apple"))]
    {
        match kind {
            ProviderKind::AppleContextual => apple::ContextualEmbedder::available().map(|e| Arc::new(e) as Arc<dyn Embedder>),
            ProviderKind::AppleSentence => apple::SentenceEmbedder::available().map(|e| Arc::new(e) as Arc<dyn Embedder>),
            _ => None,
        }
    }
    #[cfg(not(all(target_os = "macos", feature = "apple")))]
    {
        let _ = kind;
        None
    }
}

/// Ask the OS for the Apple contextual model's assets (macOS + `apple`);
/// `Ok(false)` everywhere else. See `apple::request_contextual_assets`.
pub fn request_apple_assets() -> Result<bool, String> {
    #[cfg(all(target_os = "macos", feature = "apple"))]
    {
        apple::request_contextual_assets()
    }
    #[cfg(not(all(target_os = "macos", feature = "apple")))]
    {
        Ok(false)
    }
}

#[cfg(all(target_os = "macos", feature = "apple"))]
mod apple {
    use super::Embedder;
    use std::sync::Mutex;
    use objc2_foundation::{NSRange, NSString};
    use objc2_natural_language::{NLContextualEmbedding, NLEmbedding, NLLanguage};

    /// The language every embedding is requested for. Apple's models are
    /// per-language and the corpus is a developer's prompts and English-language
    /// pages; a language-detection pass would be a second model to be wrong.
    fn english() -> Option<&'static NLLanguage> {
        unsafe { objc2_natural_language::NLLanguageEnglish }
    }

    /// `NLContextualEmbedding` — a transformer, 512-dim, 256-token cap,
    /// per-token vectors we mean-pool. macOS 14+.
    ///
    /// The handle lives inside a `Mutex` and the `Send`/`Sync` claim rests on
    /// that, not on a comment. An earlier version asserted "only ever used one
    /// call at a time" as a convention and shared the raw `Retained` across
    /// threads; concurrent inference on one instance aborted the process
    /// (SIGABRT), which is what an unenforced safety comment buys you. The
    /// mutex makes the serialization real, and inference is a `spawn_blocking`
    /// job anyway — there is no throughput being given up.
    pub struct ContextualEmbedder {
        inner: Mutex<objc2::rc::Retained<NLContextualEmbedding>>,
        revision: usize,
        dim: usize,
    }

    // SAFETY: every use goes through `inner.lock()`, so at most one thread
    // touches the ObjC object at a time. `NLContextualEmbedding` is a
    // compute object with no main-thread affinity (not a UI class).
    unsafe impl Send for ContextualEmbedder {}
    unsafe impl Sync for ContextualEmbedder {}

    impl ContextualEmbedder {
        pub fn available() -> Option<Self> {
            let lang = english()?;
            let inner = unsafe { NLContextualEmbedding::contextualEmbeddingWithLanguage(lang) }?;
            // Assets are OS-managed but not necessarily PRESENT. Loading a model
            // whose assets have not been downloaded fails; we do not trigger a
            // download here — a background asset fetch is not something a
            // retrieval path should start on the user's behalf.
            if !unsafe { inner.hasAvailableAssets() } {
                tracing::info!("contextual embedding assets are not downloaded yet");
                return None;
            }
            if let Err(e) = unsafe { inner.loadWithError() } {
                tracing::info!(error = ?e, "contextual embedding model failed to load");
                return None;
            }
            let revision = unsafe { inner.revision() };
            let dim = unsafe { inner.dimension() };
            Some(Self { inner: Mutex::new(inner), revision, dim })
        }
    }

    /// Ask the OS to fetch the contextual model's assets (C2): the one-time
    /// download that lets an install leave the weak sentence fallback.
    /// Returns `Ok(true)` when a request was issued (the OS downloads in
    /// the background and `available()` succeeds on a later start),
    /// `Ok(false)` when the assets are already present or the API is not
    /// offered on this OS. Never called from a read path — `polis init`,
    /// `polis serve` and a host's boot call it.
    pub fn request_contextual_assets() -> Result<bool, String> {
        let lang = english().ok_or("NLLanguageEnglish unavailable")?;
        let Some(inner) = (unsafe { NLContextualEmbedding::contextualEmbeddingWithLanguage(lang) }) else {
            return Ok(false);
        };
        if unsafe { inner.hasAvailableAssets() } {
            return Ok(false);
        }
        let block = block2::RcBlock::new(
            |result: objc2_natural_language::NLContextualEmbeddingAssetsResult, _err: *mut objc2_foundation::NSError| {
                tracing::info!(result = result.0, "contextual embedding assets request completed");
            },
        );
        unsafe { inner.requestEmbeddingAssetsWithCompletionHandler(&block) };
        Ok(true)
    }

    impl Embedder for ContextualEmbedder {
        fn model_id(&self) -> String {
            format!("apple-contextual-en-r{}", self.revision)
        }
        fn dim(&self) -> usize {
            self.dim
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            let lang = english().ok_or("NLLanguageEnglish unavailable")?;
            let inner = self.inner.lock().map_err(|_| "embedder mutex poisoned")?;
            let mut out = Vec::with_capacity(texts.len());
            for t in texts {
                let ns = NSString::from_str(t);
                let result = unsafe {
                    inner.embeddingResultForString_language_error(&ns, Some(lang))
                }
                .map_err(|e| format!("embedding failed: {e:?}"))?;
                let len = unsafe { result.sequenceLength() };
                let dim = self.dim();
                // Mean-pool the per-token vectors. The block is called once per
                // token; `sum`/`count` accumulate across the enumeration.
                let acc = std::cell::RefCell::new(vec![0f64; dim]);
                let count = std::cell::Cell::new(0usize);
                let block = block2::RcBlock::new(
                    |vec: std::ptr::NonNull<objc2_foundation::NSArray<objc2_foundation::NSNumber>>,
                     _range: NSRange,
                     _stop: std::ptr::NonNull<objc2::runtime::Bool>| {
                        let vec = unsafe { vec.as_ref() };
                        let mut acc = acc.borrow_mut();
                        let n_vals = vec.len();
                        for (i, slot) in acc.iter_mut().enumerate().take(dim.min(n_vals)) {
                            *slot += vec.objectAtIndex(i).as_f64();
                        }
                        count.set(count.get() + 1);
                    },
                );
                unsafe {
                    result.enumerateTokenVectorsInRange_usingBlock(
                        NSRange::new(0, len),
                        &block,
                    );
                }
                let n = count.get().max(1) as f64;
                out.push(acc.borrow().iter().map(|x| (*x / n) as f32).collect());
            }
            Ok(out)
        }
    }

    /// `NLEmbedding.sentenceEmbedding` — macOS 11+, also 512-dim, one vector per
    /// string. Weaker than the contextual model (it is closer to a pooled word
    /// embedding), which is why it is the fallback rather than the default; it
    /// exists because the app's pinned `minimumSystemVersion` is 11.0 and an
    /// arm that only works on 14+ would be an arm most installs never see.
    pub struct SentenceEmbedder {
        inner: Mutex<objc2::rc::Retained<NLEmbedding>>,
        dim: usize,
    }

    // SAFETY: as `ContextualEmbedder` — serialized by the mutex it holds.
    unsafe impl Send for SentenceEmbedder {}
    unsafe impl Sync for SentenceEmbedder {}

    impl SentenceEmbedder {
        pub fn available() -> Option<Self> {
            let lang = english()?;
            let inner = unsafe { NLEmbedding::sentenceEmbeddingForLanguage(lang) }?;
            let dim = unsafe { inner.dimension() };
            Some(Self {
                inner: Mutex::new(inner),
                // The sentence model is 512-wide; a zero from the API means
                // "not reported", never a zero-dimensional vector.
                dim: if dim == 0 { 512 } else { dim },
            })
        }
    }

    impl Embedder for SentenceEmbedder {
        fn model_id(&self) -> String {
            "apple-sentence-en".to_string()
        }
        fn dim(&self) -> usize {
            self.dim
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            let inner = self.inner.lock().map_err(|_| "embedder mutex poisoned")?;
            let mut out = Vec::with_capacity(texts.len());
            for t in texts {
                let ns = NSString::from_str(t);
                let v = unsafe { inner.vectorForString(&ns) };
                out.push(match v {
                    Some(arr) => (0..arr.len()).map(|i| arr.objectAtIndex(i).as_f64() as f32).collect(),
                    // A string the model has no vector for is a zero vector,
                    // which scores 0 against everything — honest, and never a
                    // spurious match.
                    None => vec![0.0; self.dim()],
                });
            }
            Ok(out)
        }
    }
}

// ---------------------------------------------------------------------------
// Hot vector cache
// ---------------------------------------------------------------------------

/// Every stored vector, kept resident and keyed on the index's high-water mark.
/// Mirrors `context::build_stats_cached`: cheap to rebuild, invalidated by a
/// single monotonic number rather than by anyone remembering to clear it.
pub struct VectorCache {
    pub head_id: i64,
    /// The model whose rows these are (C2): a switch of provider is a
    /// different cache, never a stale one served under a new name.
    pub model: String,
    pub rows: Vec<(i64, String, i64, QVec)>,
}

pub fn cache() -> &'static RwLock<Option<Arc<VectorCache>>> {
    static C: OnceLock<RwLock<Option<Arc<VectorCache>>>> = OnceLock::new();
    C.get_or_init(|| RwLock::new(None))
}

/// One semantic hit: what it points at, and how similar.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticHit {
    pub target_kind: String,
    pub target_id: i64,
    pub score: f32,
}

/// Nearest targets to `query` by cosine, best first.
///
/// Brute force over every vector, deliberately — see
/// [`BRUTE_FORCE_CEILING_CHUNKS`] for the number that makes that defensible and
/// the year it stops being. Chunks are collapsed to their target by MAX, so a
/// long document does not out-rank a short one by having more chances.
///
/// Returns `None` when no provider is configured. That is a THIRD state,
/// distinct from "no matches": the caller reports the arm as absent rather than
/// letting an empty list read as "you never thought about this".
pub fn semantic_search(
    store: &PolisStore,
    embedder: &dyn Embedder,
    query: &str,
    limit: usize,
) -> Option<Vec<SemanticHit>> {
    let provider = embedder;
    let model = provider.model_id();
    let qvec = provider.embed(&[query.to_string()]).ok()?.into_iter().next()?;
    let q = quantize(&qvec);

    let head = store.max_embedding_id().unwrap_or(0);
    if head == 0 {
        // The index exists but is empty — still "absent", not "no matches".
        return None;
    }
    // Hot cache keyed on the index head, mirroring `build_stats_cached`.
    let cached = {
        let guard = cache().read().ok()?;
        guard.as_ref().filter(|c| c.head_id == head && c.model == model).cloned()
    };
    let vectors = match cached {
        Some(c) => c,
        None => {
            let rows = store.all_embeddings(&model).ok()?;
            if rows.is_empty() {
                // The index has rows, none under THIS model: absent for this
                // provider until a reindex (the pack says which).
                return None;
            }
            let fresh = Arc::new(VectorCache { head_id: head, model: model.clone(), rows });
            if let Ok(mut guard) = cache().write() {
                *guard = Some(fresh.clone());
            }
            fresh
        }
    };

    // The crossover, checked rather than assumed. Brute force is correct here
    // and stops being correct at a specific size; saying so once, when it
    // happens, beats a comment nobody re-reads.
    if vectors.rows.len() > BRUTE_FORCE_CEILING_CHUNKS {
        tracing::warn!(
            chunks = vectors.rows.len(),
            ceiling = BRUTE_FORCE_CEILING_CHUNKS,
            "the vector index has outgrown a brute-force scan — an ANN index is now worth its \
             maintenance cost (see BRUTE_FORCE_CEILING_CHUNKS)"
        );
    }
    // Collapse chunks to their target by MAX. The target kinds are a
    // handful of short strings, so they are interned to a small index once
    // per row rather than cloned into every map key (C2: at 100k rows that
    // clone was most of the scan).
    let mut kinds: Vec<&str> = Vec::with_capacity(4);
    let mut best: std::collections::HashMap<(u8, i64), f32> = std::collections::HashMap::with_capacity(vectors.rows.len());
    for (_, kind, id, v) in &vectors.rows {
        let s = cosine(&q, v);
        let k = match kinds.iter().position(|k| k == kind) {
            Some(i) => i as u8,
            None => {
                kinds.push(kind.as_str());
                (kinds.len() - 1) as u8
            }
        };
        let e = best.entry((k, *id)).or_insert(f32::MIN);
        if s > *e {
            *e = s;
        }
    }
    let mut hits: Vec<SemanticHit> = best
        .into_iter()
        .map(|((k, target_id), score)| SemanticHit { target_kind: kinds[k as usize].to_string(), target_id, score })
        .collect();
    // Deterministic order: score desc, then target for ties.
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.target_kind.cmp(&b.target_kind))
            .then_with(|| a.target_id.cmp(&b.target_id))
    });
    hits.truncate(limit);
    Some(hits)
}

/// Embed one tick's worth of backlog. Best-effort and bounded: a retrieval
/// index is never allowed to become a reason the app is busy.
pub fn index_tick(store: &PolisStore, embedder: &dyn Embedder, max_targets: usize) -> usize {
    let provider = embedder;
    let model = provider.model_id();
    let Ok(mut backlog) = store.embedding_backlog(&model, max_targets as i64) else {
        return 0;
    };
    // E3: foreign bodies are re-embedded LOCALLY under this model (a peer's
    // vectors were kept only when its model id matched); they take the
    // tick's leftover budget after the lake's own rows.
    let left = max_targets.saturating_sub(backlog.len()) as i64;
    if left > 0 {
        backlog.extend(store.foreign_embedding_backlog(&model, left).unwrap_or_default());
    }
    let mut done = 0usize;
    for (kind, id, text, hash) in backlog {
        let chunks = chunk_text(&text);
        if chunks.is_empty() {
            continue;
        }
        let texts: Vec<String> = chunks.iter().map(|c| c.text.clone()).collect();
        let Ok(vectors) = provider.embed(&texts) else { continue };
        let rows: Vec<(Chunk, QVec)> = chunks
            .into_iter()
            .zip(vectors.into_iter().map(|v| quantize(&v)))
            .collect();
        if let Err(e) = store.store_embeddings(&kind, id, &model, &hash, &rows) {
            tracing::warn!(error = %e, kind, id, "failed to store embeddings");
            continue;
        }
        done += 1;
    }
    if done > 0 {
        // The head moved; the next search rebuilds the cache from its key.
        if let Ok(mut guard) = cache().write() {
            *guard = None;
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crossover number, asserted so a future reader finds it as a fact
    /// rather than re-deriving it from intuition.
    #[test]
    fn brute_force_stays_viable_for_years_at_the_observed_rate() {
        const CHUNKS_TODAY: usize = 3_073;
        const CHUNKS_PER_YEAR: usize = 27_000;
        const { assert!(CHUNKS_TODAY < BRUTE_FORCE_CEILING_CHUNKS / 40) };
        let years = (BRUTE_FORCE_CEILING_CHUNKS - CHUNKS_TODAY) / CHUNKS_PER_YEAR;
        assert!(years >= 5, "only {years} years of headroom — time to reconsider ANN");
    }
}
