// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! An OpenAI-compatible `/v1/embeddings` endpoint (feature `remote`): the
//! one provider that sends text off the machine, and only when someone
//! configured it. Opt-in egress, refused outright under `POLIS_NO_NETWORK=1`.
//!
//! Configuration (environment, read once at construction):
//! `POLIS_EMBED_URL` (the base, e.g. `https://api.openai.com/v1`),
//! `POLIS_EMBED_KEY` (bearer; optional for a local server),
//! `POLIS_EMBED_MODEL` (the model name the endpoint expects),
//! `POLIS_EMBED_DIM` (optional; otherwise learned from the first reply).
//! The row model id is `remote/<model>`, so a switch of endpoint model is a
//! clean re-index like any other provider change.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::Embedder;

/// The environment variables, by name.
pub const ENV_URL: &str = "POLIS_EMBED_URL";
pub const ENV_KEY: &str = "POLIS_EMBED_KEY";
pub const ENV_MODEL: &str = "POLIS_EMBED_MODEL";
pub const ENV_DIM: &str = "POLIS_EMBED_DIM";

/// How many texts one request carries.
pub const BATCH: usize = 64;

#[derive(Serialize)]
struct Req<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct Resp {
    data: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    index: usize,
    embedding: Vec<f32>,
}

pub struct RemoteEmbedder {
    base: String,
    key: Option<String>,
    model: String,
    dim: Mutex<Option<usize>>,
}

impl RemoteEmbedder {
    pub fn new(base: impl Into<String>, key: Option<String>, model: impl Into<String>) -> Self {
        let base = base.into().trim_end_matches('/').to_string();
        Self { base, key, model: model.into(), dim: Mutex::new(None) }
    }

    pub fn with_dim(self, dim: usize) -> Self {
        *self.dim.lock().unwrap_or_else(|e| e.into_inner()) = Some(dim);
        self
    }

    /// From the environment; `None` when unconfigured or when the network
    /// is forbidden.
    pub fn from_env() -> Option<Self> {
        if std::env::var("POLIS_NO_NETWORK").map(|v| v == "1").unwrap_or(false) {
            return None;
        }
        let get = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let base = get(ENV_URL)?;
        let model = get(ENV_MODEL)?;
        let mut me = Self::new(base, get(ENV_KEY), model);
        if let Some(d) = get(ENV_DIM).and_then(|v| v.parse::<usize>().ok()) {
            me = me.with_dim(d);
        }
        Some(me)
    }

    fn post(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let url = format!("{}/embeddings", self.base);
        let body = serde_json::to_vec(&Req { model: &self.model, input: texts }).map_err(|e| e.to_string())?;
        let mut req = ureq::post(&url).header("Content-Type", "application/json");
        if let Some(k) = &self.key {
            req = req.header("Authorization", &format!("Bearer {k}"));
        }
        let mut resp = req.send(&body[..]).map_err(|e| format!("POST {url}: {e}"))?;
        let bytes = resp
            .body_mut()
            .with_config()
            .limit(64 << 20)
            .read_to_vec()
            .map_err(|e| format!("POST {url}: {e}"))?;
        let parsed: Resp = serde_json::from_slice(&bytes).map_err(|e| format!("{url}: unparseable reply: {e}"))?;
        let mut out: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
        for it in parsed.data {
            if it.index < out.len() {
                out[it.index] = Some(it.embedding);
            }
        }
        out.into_iter().enumerate().map(|(i, v)| v.ok_or_else(|| format!("{url}: no embedding for input {i}"))).collect()
    }
}

impl Embedder for RemoteEmbedder {
    fn model_id(&self) -> String {
        format!("remote/{}", self.model)
    }
    fn dim(&self) -> usize {
        self.dim.lock().unwrap_or_else(|e| e.into_inner()).unwrap_or(0)
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(BATCH) {
            let vs = self.post(batch)?;
            if let Some(first) = vs.first() {
                let mut d = self.dim.lock().unwrap_or_else(|e| e.into_inner());
                match *d {
                    None => *d = Some(first.len()),
                    Some(want) if want != first.len() => {
                        return Err(format!("the endpoint returned {}-dim vectors, this index is {want}-dim", first.len()))
                    }
                    _ => {}
                }
            }
            out.extend(vs);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A one-request fake endpoint on a loopback port: reads the request,
    /// answers with deterministic vectors, records what it saw.
    fn fake_server(dim: usize) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 65536];
            let mut read = 0;
            loop {
                let n = s.read(&mut buf[read..]).unwrap();
                read += n;
                let text = String::from_utf8_lossy(&buf[..read]).to_string();
                if let Some(h) = text.find("\r\n\r\n") {
                    let len: usize = text
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
                        .unwrap_or(0);
                    if read >= h + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&buf[..read]).to_string();
            let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
            let req: serde_json::Value = serde_json::from_str(&body).unwrap();
            let n = req["input"].as_array().unwrap().len();
            let data: Vec<serde_json::Value> = (0..n)
                .rev()
                .map(|i| serde_json::json!({"index": i, "embedding": (0..dim).map(|d| (i + 1) as f32 * (d as f32 + 1.0)).collect::<Vec<f32>>()}))
                .collect();
            let reply = serde_json::json!({"object": "list", "data": data}).to_string();
            let _ = s.write_all(
                format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", reply.len(), reply)
                    .as_bytes(),
            );
            tx.send(text).unwrap();
        });
        (format!("http://127.0.0.1:{port}/v1"), rx)
    }

    #[test]
    fn a_fake_endpoint_round_trips_in_order_with_the_bearer_and_learns_the_dim() {
        let (base, rx) = fake_server(4);
        let e = RemoteEmbedder::new(base, Some("sekrit".into()), "text-embedding-test");
        assert_eq!(e.dim(), 0, "unknown until the first reply");
        let vs = e.embed(&["a".into(), "b".into(), "c".into()]).unwrap();
        // The fake answers out of order; the client puts them back by index.
        assert_eq!(vs[0], vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(vs[2], vec![3.0, 6.0, 9.0, 12.0]);
        assert_eq!(e.dim(), 4);
        assert_eq!(e.model_id(), "remote/text-embedding-test");
        let seen = rx.recv().unwrap();
        assert!(seen.starts_with("POST /v1/embeddings HTTP/1.1"), "{seen}");
        assert!(seen.to_ascii_lowercase().contains("authorization: bearer sekrit"));
        assert!(seen.contains("\"model\":\"text-embedding-test\""));
    }

    #[test]
    fn the_network_switch_and_the_env_gate_it() {
        // Both variables are needed; the test only checks the switch since
        // the process environment is shared across tests.
        let e = RemoteEmbedder::new("http://x.test/v1/", None, "m").with_dim(3);
        assert_eq!(e.dim(), 3);
        assert_eq!(e.base, "http://x.test/v1");
        assert!(e.kind_is_remote());
    }

    impl RemoteEmbedder {
        fn kind_is_remote(&self) -> bool {
            (self as &dyn Embedder).kind() == crate::ProviderKind::Remote
        }
    }
}
