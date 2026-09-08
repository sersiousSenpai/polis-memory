// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The portable provider: Model2Vec's `potion-base-8M` as a self-contained
//! runtime (plan §7.3, Session C2).
//!
//! A Model2Vec model is a static table — one vector per WordPiece token —
//! and an embedding is the mean of a text's token rows, L2-normalized. That
//! is a tokenizer and a lookup, so this file carries both: a BERT-style
//! WordPiece tokenizer read from the model's `tokenizer.json` (normalizer,
//! pre-tokenizer and vocabulary), a `safetensors` reader for the table, and
//! the pooling — about four hundred lines, against the reference crate's
//! tree of `tokenizers` (+ the oniguruma C library), `ndarray`, `clap` and
//! `anyhow`. The plan's §11 flag asked for exactly this measurement.
//!
//! Fidelity is pinned, not assumed: `tests/model2vec_golden.rs` runs the
//! same texts through the official Python implementation (token ids and
//! vectors recorded in `tests/fixtures/potion_golden.json`) and asserts the
//! ids are identical and the vectors agree to cosine ≥ 0.9999.
//!
//! The model files are fetched once into a models directory (feature
//! `download`; refused under `POLIS_NO_NETWORK=1`) or compiled in (feature
//! `bundled-model`), and every byte is verified against [`PINNED`] before
//! use — a download that does not hash is deleted, never loaded.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::Embedder;

/// The model, by its Hugging Face name.
pub const MODEL_NAME: &str = "potion-base-8M";
/// The id stored on every row this provider writes.
pub const MODEL_ID: &str = "model2vec/potion-base-8M";
/// Where the pinned files come from.
pub const REPO_URL: &str = "https://huggingface.co/minishlab/potion-base-8M/resolve/main/";
/// The reference implementation's token cap per text.
pub const MAX_TOKENS: usize = 512;

/// One of the three files, with the digest the provider refuses to run
/// without. Recorded 2026-09-07 from the repository's `main`.
#[derive(Debug, Clone, Copy)]
pub struct PinnedFile {
    pub name: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
}

pub const PINNED: [PinnedFile; 3] = [
    PinnedFile {
        name: "model.safetensors",
        sha256: "f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2",
        bytes: 30_236_760,
    },
    PinnedFile {
        name: "tokenizer.json",
        sha256: "e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50",
        bytes: 683_666,
    },
    PinnedFile {
        name: "config.json",
        sha256: "2a6ac0e9aaa356a68a5688070db78fc3a464fefe85d2f06a1905ce3718687553",
        bytes: 202,
    },
];

/// The directory a models root keeps this model in.
pub fn model_dir(models_root: &Path) -> PathBuf {
    models_root.join(MODEL_NAME)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Read one pinned file from `dir`, refusing a byte that does not hash.
fn read_pinned(dir: &Path, file: &PinnedFile) -> Result<Vec<u8>, String> {
    let path = dir.join(file.name);
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let got = sha256_hex(&bytes);
    if got != file.sha256 {
        return Err(format!(
            "{} does not match its pinned sha256 (got {got}, want {}) — refusing to load it",
            path.display(),
            file.sha256
        ));
    }
    Ok(bytes)
}

/// Are all three files present and intact under `dir`?
pub fn verify_dir(dir: &Path) -> Result<(), String> {
    for f in &PINNED {
        read_pinned(dir, f)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The tokenizer
// ---------------------------------------------------------------------------

/// What `tokenizer.json` says about normalization and the WordPiece model.
#[derive(Debug, Clone)]
struct TokenizerSpec {
    vocab: HashMap<String, u32>,
    unk_id: Option<u32>,
    unk_token: String,
    prefix: String,
    max_word_chars: usize,
    clean_text: bool,
    handle_chinese: bool,
    strip_accents: bool,
    lowercase: bool,
    /// The reference truncates a text to `MAX_TOKENS × median token length`
    /// characters before tokenizing; the median is over the vocabulary's
    /// byte lengths.
    median_token_len: usize,
}

impl TokenizerSpec {
    fn parse(json: &[u8]) -> Result<Self, String> {
        let v: serde_json::Value = serde_json::from_slice(json).map_err(|e| format!("tokenizer.json: {e}"))?;
        let model = v.get("model").ok_or("tokenizer.json: no `model`")?;
        if model.get("type").and_then(|t| t.as_str()) != Some("WordPiece") {
            return Err("tokenizer.json: only a WordPiece model is supported".into());
        }
        let vocab_obj = model.get("vocab").and_then(|x| x.as_object()).ok_or("tokenizer.json: no vocab")?;
        let mut vocab = HashMap::with_capacity(vocab_obj.len());
        let mut lens: Vec<usize> = Vec::with_capacity(vocab_obj.len());
        for (tok, id) in vocab_obj {
            let id = id.as_u64().ok_or("tokenizer.json: a non-integer id")? as u32;
            lens.push(tok.len());
            vocab.insert(tok.clone(), id);
        }
        lens.sort_unstable();
        let median_token_len = lens.get(lens.len() / 2).copied().unwrap_or(1);
        let unk_token = model.get("unk_token").and_then(|x| x.as_str()).unwrap_or("[UNK]").to_string();
        let unk_id = vocab.get(&unk_token).copied();
        let prefix = model.get("continuing_subword_prefix").and_then(|x| x.as_str()).unwrap_or("##").to_string();
        let max_word_chars = model.get("max_input_chars_per_word").and_then(|x| x.as_u64()).unwrap_or(100) as usize;
        let norm = v.get("normalizer").cloned().unwrap_or(serde_json::Value::Null);
        let flag = |k: &str, d: bool| norm.get(k).and_then(|x| x.as_bool()).unwrap_or(d);
        let lowercase = flag("lowercase", true);
        // HF's BertNormalizer: `strip_accents: null` follows `lowercase`.
        let strip_accents = match norm.get("strip_accents") {
            Some(serde_json::Value::Bool(b)) => *b,
            _ => lowercase,
        };
        Ok(Self {
            vocab,
            unk_id,
            unk_token,
            prefix,
            max_word_chars,
            clean_text: flag("clean_text", true),
            handle_chinese: flag("handle_chinese_chars", true),
            strip_accents,
            lowercase,
            median_token_len,
        })
    }
}

/// The reference's `_is_control`: tab, newline and return are whitespace;
/// every other general-category-C code point is control.
fn is_control(c: char) -> bool {
    if matches!(c, '\t' | '\n' | '\r') {
        return false;
    }
    use unicode_general_category::{get_general_category, GeneralCategory as G};
    matches!(
        get_general_category(c),
        G::Control | G::Format | G::Surrogate | G::PrivateUse | G::Unassigned
    )
}

/// The reference's `_is_whitespace`: the four ASCII ones or a space
/// separator (Zs).
fn is_whitespace(c: char) -> bool {
    if matches!(c, ' ' | '\t' | '\n' | '\r') {
        return true;
    }
    use unicode_general_category::{get_general_category, GeneralCategory as G};
    get_general_category(c) == G::SpaceSeparator
}

/// The reference's `_is_punctuation`: the ASCII punctuation ranges (treated
/// as punctuation even where Unicode disagrees, e.g. `^` and `$`) or any
/// general-category-P code point.
fn is_punctuation(c: char) -> bool {
    let cp = c as u32;
    if (33..=47).contains(&cp) || (58..=64).contains(&cp) || (91..=96).contains(&cp) || (123..=126).contains(&cp) {
        return true;
    }
    use unicode_general_category::{get_general_category, GeneralCategory as G};
    matches!(
        get_general_category(c),
        G::ConnectorPunctuation
            | G::DashPunctuation
            | G::OpenPunctuation
            | G::ClosePunctuation
            | G::InitialPunctuation
            | G::FinalPunctuation
            | G::OtherPunctuation
    )
}

/// BERT's CJK ranges (`_is_chinese_char`).
fn is_cjk(c: char) -> bool {
    let cp = c as u32;
    (0x4E00..=0x9FFF).contains(&cp)
        || (0x3400..=0x4DBF).contains(&cp)
        || (0x20000..=0x2A6DF).contains(&cp)
        || (0x2A700..=0x2B73F).contains(&cp)
        || (0x2B740..=0x2B81F).contains(&cp)
        || (0x2B820..=0x2CEAF).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0x2F800..=0x2FA1F).contains(&cp)
}

fn is_nonspacing_mark(c: char) -> bool {
    use unicode_general_category::{get_general_category, GeneralCategory as G};
    get_general_category(c) == G::NonspacingMark
}

impl TokenizerSpec {
    /// BertNormalizer, in the reference's order: clean, CJK padding,
    /// lowercase + accent stripping.
    fn normalize(&self, text: &str) -> String {
        let mut s = String::with_capacity(text.len());
        for c in text.chars() {
            if self.clean_text {
                if c == '\0' || c == '\u{FFFD}' || is_control(c) {
                    continue;
                }
                if is_whitespace(c) {
                    s.push(' ');
                    continue;
                }
            }
            if self.handle_chinese && is_cjk(c) {
                s.push(' ');
                s.push(c);
                s.push(' ');
                continue;
            }
            s.push(c);
        }
        if self.strip_accents {
            s = s.nfd().filter(|c| !is_nonspacing_mark(*c)).collect();
        }
        if self.lowercase {
            s = s.to_lowercase();
        }
        s
    }

    /// BertPreTokenizer: whitespace splits, every punctuation character its
    /// own word.
    fn pre_tokenize(text: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut cur = String::new();
        for c in text.chars() {
            if is_whitespace(c) {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            } else if is_punctuation(c) {
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
                words.push(c.to_string());
            } else {
                cur.push(c);
            }
        }
        if !cur.is_empty() {
            words.push(cur);
        }
        words
    }

    /// Greedy longest-match-first WordPiece over one word.
    fn wordpiece(&self, word: &str, out: &mut Vec<u32>) {
        let chars: Vec<char> = word.chars().collect();
        if chars.len() > self.max_word_chars {
            if let Some(u) = self.unk_id {
                out.push(u);
            }
            return;
        }
        let mut pieces: Vec<u32> = Vec::new();
        let mut start = 0usize;
        while start < chars.len() {
            let mut end = chars.len();
            let mut found = None;
            while start < end {
                let mut sub: String = if start > 0 { self.prefix.clone() } else { String::new() };
                sub.extend(&chars[start..end]);
                if let Some(id) = self.vocab.get(&sub) {
                    found = Some(*id);
                    break;
                }
                end -= 1;
            }
            match found {
                Some(id) => {
                    pieces.push(id);
                    start = end;
                }
                None => {
                    // Any piece the vocabulary lacks makes the whole word unknown.
                    if let Some(u) = self.unk_id {
                        out.push(u);
                    }
                    return;
                }
            }
        }
        out.extend(pieces);
    }

    /// The reference's char-level truncation before tokenizing.
    fn truncate<'a>(&self, text: &'a str) -> &'a str {
        text.char_indices()
            .nth(MAX_TOKENS.saturating_mul(self.median_token_len))
            .map_or(text, |(i, _)| &text[..i])
    }

    /// Token ids for a text, exactly as `tokenizer.encode(text, add_special_tokens=False)`.
    fn encode(&self, text: &str) -> Vec<u32> {
        let normalized = self.normalize(self.truncate(text));
        let mut ids = Vec::new();
        for w in Self::pre_tokenize(&normalized) {
            self.wordpiece(&w, &mut ids);
        }
        ids
    }
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// `safetensors`: an 8-byte little-endian header length, a JSON header
/// (`{ name: { dtype, shape, data_offsets } }`), then the raw tensors.
fn read_safetensors(bytes: &[u8]) -> Result<(Vec<f32>, usize, usize), String> {
    if bytes.len() < 8 {
        return Err("safetensors: truncated header".into());
    }
    let n = u64::from_le_bytes(bytes[..8].try_into().unwrap()) as usize;
    let header = bytes.get(8..8 + n).ok_or("safetensors: header past the end")?;
    let h: serde_json::Value = serde_json::from_slice(header).map_err(|e| format!("safetensors header: {e}"))?;
    let obj = h.as_object().ok_or("safetensors: header is not an object")?;
    // The one tensor that is not metadata (the reference takes the first).
    let (name, t) = obj
        .iter()
        .find(|(k, v)| k.as_str() != "__metadata__" && v.get("shape").is_some())
        .ok_or("safetensors: no tensor")?;
    let shape: Vec<usize> = t["shape"].as_array().ok_or("safetensors: no shape")?.iter().map(|x| x.as_u64().unwrap_or(0) as usize).collect();
    if shape.len() != 2 {
        return Err(format!("safetensors: tensor `{name}` is not 2-D"));
    }
    let (rows, cols) = (shape[0], shape[1]);
    let offs = t["data_offsets"].as_array().ok_or("safetensors: no data_offsets")?;
    let (a, b) = (offs[0].as_u64().unwrap_or(0) as usize, offs[1].as_u64().unwrap_or(0) as usize);
    let data = bytes.get(8 + n + a..8 + n + b).ok_or("safetensors: data past the end")?;
    let dtype = t["dtype"].as_str().unwrap_or("F32");
    let floats: Vec<f32> = match dtype {
        "F32" => data.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect(),
        "F16" => data.as_chunks::<2>().0.iter().map(|c| f16_to_f32(u16::from_le_bytes(*c))).collect(),
        other => return Err(format!("safetensors: dtype {other} is not supported")),
    };
    if floats.len() != rows * cols {
        return Err(format!("safetensors: {} values for a {rows}×{cols} table", floats.len()));
    }
    Ok((floats, rows, cols))
}

/// IEEE half → single, bit by bit (no `half` crate for one conversion).
fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 1) as u32;
    let exp = ((h >> 10) & 0x1f) as u32;
    let frac = (h & 0x3ff) as u32;
    let bits = match exp {
        0 => {
            if frac == 0 {
                sign << 31
            } else {
                // Subnormal: renormalize.
                let mut e = 127 - 15 + 1;
                let mut f = frac;
                while f & 0x400 == 0 {
                    f <<= 1;
                    e -= 1;
                }
                (sign << 31) | ((e as u32) << 23) | ((f & 0x3ff) << 13)
            }
        }
        0x1f => (sign << 31) | (0xff << 23) | (frac << 13),
        _ => (sign << 31) | ((exp + 127 - 15) << 23) | (frac << 13),
    };
    f32::from_bits(bits)
}

/// The loaded model.
pub struct Model2Vec {
    spec: TokenizerSpec,
    table: Vec<f32>,
    rows: usize,
    dim: usize,
    normalize: bool,
}

impl Model2Vec {
    /// Build from the three files' bytes (already verified by the caller
    /// or compiled in).
    pub fn from_bytes(tokenizer_json: &[u8], safetensors: &[u8], config_json: &[u8]) -> Result<Self, String> {
        let spec = TokenizerSpec::parse(tokenizer_json)?;
        let (table, rows, dim) = read_safetensors(safetensors)?;
        let cfg: serde_json::Value = serde_json::from_slice(config_json).map_err(|e| format!("config.json: {e}"))?;
        let normalize = cfg.get("normalize").and_then(|x| x.as_bool()).unwrap_or(true);
        if let Some(hd) = cfg.get("hidden_dim").and_then(|x| x.as_u64()) {
            if hd as usize != dim {
                return Err(format!("config.json says hidden_dim {hd}, the table is {dim}-wide"));
            }
        }
        Ok(Self { spec, table, rows, dim, normalize })
    }

    /// Load and verify the three files under `dir`.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let model = read_pinned(dir, &PINNED[0])?;
        let tok = read_pinned(dir, &PINNED[1])?;
        let cfg = read_pinned(dir, &PINNED[2])?;
        Self::from_bytes(&tok, &model, &cfg)
    }

    /// The compiled-in copy (feature `bundled-model`): the build reads the
    /// three files from the directory `POLIS_BUNDLED_MODEL_DIR` names, so a
    /// Docker stage downloads and verifies them once and the binary never
    /// touches the network.
    #[cfg(feature = "bundled-model")]
    pub fn bundled() -> Result<Self, String> {
        // `build.rs` put the verified files (or empty placeholders, when
        // `POLIS_BUNDLED_MODEL_DIR` was unset at build time) in OUT_DIR.
        const MODEL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/model.safetensors"));
        const TOK: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tokenizer.json"));
        const CFG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/config.json"));
        if MODEL.is_empty() {
            return Err("no model was bundled at build time (POLIS_BUNDLED_MODEL_DIR was unset)".into());
        }
        for (bytes, f) in [(MODEL, &PINNED[0]), (TOK, &PINNED[1]), (CFG, &PINNED[2])] {
            let got = sha256_hex(bytes);
            if got != f.sha256 {
                return Err(format!("bundled {} does not match its pinned sha256", f.name));
            }
        }
        Self::from_bytes(TOK, MODEL, CFG)
    }

    /// Fetch the three files into `dir` (feature `download`), verifying each
    /// before it is kept. Refused under `POLIS_NO_NETWORK=1`. Never called
    /// from a read path: `ensure` runs it at init / serve / reindex time.
    #[cfg(feature = "download")]
    pub fn download(dir: &Path) -> Result<(), String> {
        if std::env::var("POLIS_NO_NETWORK").map(|v| v == "1").unwrap_or(false) {
            return Err("POLIS_NO_NETWORK=1: the model download is refused; place the pinned files by hand or build with `bundled-model`".into());
        }
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for f in &PINNED {
            let path = dir.join(f.name);
            if let Ok(bytes) = std::fs::read(&path) {
                if sha256_hex(&bytes) == f.sha256 {
                    continue;
                }
            }
            let url = format!("{REPO_URL}{}", f.name);
            tracing::info!(url, bytes = f.bytes, "downloading the embedding model");
            let mut resp = ureq::get(&url).call().map_err(|e| format!("GET {url}: {e}"))?;
            let bytes = resp
                .body_mut()
                .with_config()
                .limit(f.bytes.saturating_mul(2).max(1 << 20))
                .read_to_vec()
                .map_err(|e| format!("GET {url}: {e}"))?;
            let got = sha256_hex(&bytes);
            if got != f.sha256 {
                return Err(format!("{} from {url} does not hash to the pinned sha256 (got {got}); not kept", f.name));
            }
            let tmp = path.with_extension("part");
            std::fs::write(&tmp, &bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
            std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        Ok(())
    }

    /// The provider, from wherever the model can be had: the models
    /// directory if the files verify; else the compiled-in copy; else — when
    /// `allow_download` — a download into that directory.
    pub fn ensure(models_root: &Path, allow_download: bool) -> Result<Self, String> {
        let dir = model_dir(models_root);
        match Self::load(&dir) {
            Ok(m) => return Ok(m),
            Err(e) => tracing::debug!(error = %e, "model2vec not loadable from the models directory"),
        }
        #[cfg(feature = "bundled-model")]
        {
            if let Ok(m) = Self::bundled() {
                return Ok(m);
            }
        }
        #[cfg(feature = "download")]
        {
            if allow_download {
                Self::download(&dir)?;
                return Self::load(&dir);
            }
        }
        let _ = allow_download;
        Err(format!(
            "no {MODEL_NAME} under {} (pinned files: {}); run `polis reindex --model {MODEL_ID}` to fetch it, or build with `bundled-model`",
            dir.display(),
            PINNED.iter().map(|f| f.name).collect::<Vec<_>>().join(", ")
        ))
    }

    /// Token ids, as the reference tokenizer produces them (unknowns
    /// included — the pooling drops them).
    pub fn tokenize(&self, text: &str) -> Vec<u32> {
        self.spec.encode(text)
    }

    /// One text's vector: the mean of its known tokens' rows (the first
    /// `MAX_TOKENS`), L2-normalized when the model says so; a text with no
    /// known token is the zero vector, which scores 0 against everything.
    pub fn embed_one(&self, text: &str) -> Vec<f32> {
        let mut ids = self.tokenize(text);
        if let Some(u) = self.spec.unk_id {
            ids.retain(|id| *id != u);
        }
        ids.truncate(MAX_TOKENS);
        let mut sum = vec![0f32; self.dim];
        let mut n = 0usize;
        for id in ids {
            let row = id as usize;
            if row >= self.rows {
                continue;
            }
            let base = row * self.dim;
            for (s, v) in sum.iter_mut().zip(&self.table[base..base + self.dim]) {
                *s += v;
            }
            n += 1;
        }
        if n == 0 {
            return sum;
        }
        let inv = 1.0 / n as f32;
        for s in sum.iter_mut() {
            *s *= inv;
        }
        if self.normalize {
            let norm = sum.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > f32::EPSILON {
                for s in sum.iter_mut() {
                    *s /= norm;
                }
            }
        }
        sum
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn unk_token(&self) -> &str {
        &self.spec.unk_token
    }
}

impl Embedder for Model2Vec {
    fn model_id(&self) -> String {
        MODEL_ID.to_string()
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts.iter().map(|t| self.embed_one(t)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_precision_converts_the_reference_points() {
        assert_eq!(f16_to_f32(0x3C00), 1.0);
        assert_eq!(f16_to_f32(0xC000), -2.0);
        assert_eq!(f16_to_f32(0x0000), 0.0);
        assert!((f16_to_f32(0x3555) - 0.333_251_95).abs() < 1e-6);
        assert!(f16_to_f32(0x7C00).is_infinite());
        // The smallest subnormal.
        assert!((f16_to_f32(0x0001) - 5.960_464_5e-8).abs() < 1e-12);
    }

    #[test]
    fn a_tiny_hand_built_model_tokenizes_and_pools() {
        // Vocabulary: "hello" → 0, "wor" → 1, "##ld" → 2, "[UNK]" → 3.
        let tok = serde_json::json!({
            "normalizer": {"type": "BertNormalizer", "clean_text": true, "handle_chinese_chars": true, "strip_accents": null, "lowercase": true},
            "pre_tokenizer": {"type": "BertPreTokenizer"},
            "model": {"type": "WordPiece", "unk_token": "[UNK]", "continuing_subword_prefix": "##", "max_input_chars_per_word": 100,
                      "vocab": {"hello": 0, "wor": 1, "##ld": 2, "[UNK]": 3, ",": 4}}
        });
        // A 5×2 F32 table: row i = [i, -i].
        let mut data = Vec::new();
        for i in 0..5 {
            data.extend_from_slice(&(i as f32).to_le_bytes());
            data.extend_from_slice(&(-(i as f32)).to_le_bytes());
        }
        let header = serde_json::json!({"embeddings": {"dtype": "F32", "shape": [5, 2], "data_offsets": [0, data.len()]}}).to_string();
        let mut st = Vec::new();
        st.extend_from_slice(&(header.len() as u64).to_le_bytes());
        st.extend_from_slice(header.as_bytes());
        st.extend_from_slice(&data);
        let m = Model2Vec::from_bytes(tok.to_string().as_bytes(), &st, br#"{"normalize": false, "hidden_dim": 2}"#).unwrap();
        assert_eq!(m.tokenize("Hello, WORLD"), vec![0, 4, 1, 2]);
        assert_eq!(m.tokenize("héllo"), vec![0], "accents stripped before lookup");
        assert_eq!(m.tokenize("zzz"), vec![3], "an unknown word is one [UNK]");
        // Mean of rows 0,4,1,2 = [(0+4+1+2)/4, -(7/4)] = [1.75, -1.75].
        let v = m.embed_one("Hello, WORLD");
        assert!((v[0] - 1.75).abs() < 1e-6 && (v[1] + 1.75).abs() < 1e-6, "{v:?}");
        // Unknowns are dropped from the pool; all-unknown is the zero vector.
        assert_eq!(m.embed_one("zzz"), vec![0.0, 0.0]);
        assert_eq!(m.embed_one(""), vec![0.0, 0.0]);
        assert_eq!(m.dim(), 2);
        assert_eq!(m.model_id(), MODEL_ID);
    }

    #[test]
    fn the_pins_are_well_formed() {
        for f in &PINNED {
            assert_eq!(f.sha256.len(), 64, "{}", f.name);
            assert!(f.sha256.chars().all(|c| c.is_ascii_hexdigit()));
            assert!(f.bytes > 0);
        }
        assert!(REPO_URL.starts_with("https://"));
    }
}
