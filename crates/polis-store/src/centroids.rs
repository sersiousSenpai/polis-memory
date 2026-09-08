// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Class centroids (Session C1, plan §7.1): one running mean per live class
//! node and embedding model, built from its members' chunk-0 vectors, so a
//! new lake item can be filed by cosine against every class under its
//! provenance root without a model call.
//!
//! The table is a CACHE over `class_links × embeddings`: `rebuild` recomputes
//! it from scratch (a few thousand vectors — milliseconds), `add` folds one
//! more member in as an organize run files it. Nothing downstream depends on
//! it being current: a stale centroid files slightly worse, never wrongly by
//! construction — every filing still passes the margin rule against fresh
//! item vectors, and the gardener rebuilds at the start of each run.
//!
//! The mean is stored as a sum of unit vectors (`sum_vec`, f32 little-endian)
//! plus `n`; the comparison vector is the L2-normalized mean. A node with
//! `n = 0` has no row and no centroid.

use rusqlite::{params, OptionalExtension};

use crate::PolisStore;

/// One class's running mean for one model.
#[derive(Debug, Clone, PartialEq)]
pub struct Centroid {
    pub node_id: String,
    pub model: String,
    pub dim: usize,
    pub n: i64,
    pub sum: Vec<f32>,
}

impl Centroid {
    /// The L2-normalized mean — the vector a cosine is taken against.
    pub fn mean_unit(&self) -> Vec<f32> {
        unit(&self.sum)
    }
}

/// L2-normalize (a zero vector stays zero).
pub fn unit(v: &[f32]) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm <= f32::EPSILON {
        return vec![0.0; v.len()];
    }
    v.iter().map(|x| x / norm).collect()
}

/// Cosine between two f32 vectors (0 when the lengths differ).
pub fn cosine_f32(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// A stored int8 vector back to f32 (`bytes × scale`; already unit-length
/// before quantization, so this is the unit vector up to rounding).
pub fn qvec_to_f32(q: &polis_core::vec::QVec) -> Vec<f32> {
    q.bytes.iter().map(|b| *b as f32 * q.scale).collect()
}

fn pack_f32(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

fn unpack_f32(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// A member's chunk-0 vector, resolved from a link's target: a `prompt`
/// link names a ledger seq whose event carries the prompt row id; a
/// `browse_event` link names a seq whose event's `ref_id` is the page row.
/// Other target kinds (decision, revision, session, note, …) have no text
/// of their own and yield `None`.
const ITEM_VECTOR_SQL: &str = "SELECT e.scale, e.vec
         FROM ledger_events le
         JOIN embeddings e
           ON e.model = ?3 AND e.chunk_ix = 0
          AND ((?2 = 'prompt' AND e.target_kind = 'prompt' AND e.target_id = le.prompt_id)
            OR (?2 = 'browse_event' AND e.target_kind = 'browse_event'
                AND e.target_id = CAST(le.ref_id AS INTEGER)))
         WHERE le.seq = ?1
         LIMIT 1";

impl PolisStore {
    /// The centroid of one node for one model, when it has members.
    pub fn centroid(&self, node_id: &str, model: &str) -> rusqlite::Result<Option<Centroid>> {
        let conn = self.conn();
        conn.query_row(
            "SELECT dim, n, sum_vec FROM class_centroids WHERE node_id = ?1 AND model = ?2",
            params![node_id, model],
            |r| {
                let dim: i64 = r.get(0)?;
                let n: i64 = r.get(1)?;
                let blob: Vec<u8> = r.get(2)?;
                Ok(Centroid { node_id: node_id.to_string(), model: model.to_string(), dim: dim as usize, n, sum: unpack_f32(&blob) })
            },
        )
        .optional()
    }

    /// Every centroid for a model (live nodes only), keyed by node id.
    pub fn centroids_for_model(&self, model: &str) -> rusqlite::Result<Vec<Centroid>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT c.node_id, c.dim, c.n, c.sum_vec
             FROM class_centroids c
             JOIN class_nodes n ON n.id = c.node_id AND n.retired_by_run IS NULL
             WHERE c.model = ?1",
        )?;
        let rows = stmt.query_map(params![model], |r| {
            let node_id: String = r.get(0)?;
            let dim: i64 = r.get(1)?;
            let n: i64 = r.get(2)?;
            let blob: Vec<u8> = r.get(3)?;
            Ok(Centroid { node_id, model: model.to_string(), dim: dim as usize, n, sum: unpack_f32(&blob) })
        })?;
        rows.collect()
    }

    /// Fold one more member vector into a node's centroid (creating the row
    /// on first use). `vec` is normalized here, so callers may pass raw
    /// embedder output.
    pub fn centroid_add(&self, node_id: &str, model: &str, vec: &[f32]) -> rusqlite::Result<()> {
        let u = unit(vec);
        let now = polis_core::ledger::now_millis();
        let conn = self.conn();
        let existing: Option<(i64, Vec<u8>)> = conn
            .query_row(
                "SELECT n, sum_vec FROM class_centroids WHERE node_id = ?1 AND model = ?2",
                params![node_id, model],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match existing {
            Some((n, blob)) => {
                let mut sum = unpack_f32(&blob);
                if sum.len() != u.len() {
                    // A different dimension under the same model id is a
                    // corrupt row; start over rather than add mismatched axes.
                    sum = vec![0.0; u.len()];
                    conn.execute(
                        "UPDATE class_centroids SET dim = ?3, n = 1, sum_vec = ?4, updated_at = ?5
                         WHERE node_id = ?1 AND model = ?2",
                        params![node_id, model, u.len() as i64, pack_f32(&u), now],
                    )?;
                    let _ = sum;
                    return Ok(());
                }
                for (s, x) in sum.iter_mut().zip(&u) {
                    *s += x;
                }
                conn.execute(
                    "UPDATE class_centroids SET n = ?3, sum_vec = ?4, updated_at = ?5
                     WHERE node_id = ?1 AND model = ?2",
                    params![node_id, model, n + 1, pack_f32(&sum), now],
                )?;
            }
            None => {
                conn.execute(
                    "INSERT INTO class_centroids (node_id, model, dim, n, sum_vec, updated_at)
                     VALUES (?1, ?2, ?3, 1, ?4, ?5)",
                    params![node_id, model, u.len() as i64, pack_f32(&u), now],
                )?;
            }
        }
        Ok(())
    }

    /// Recompute every centroid for a model from the live links whose target
    /// has a chunk-0 vector. Returns the number of nodes with a centroid.
    pub fn rebuild_centroids(&self, model: &str) -> rusqlite::Result<usize> {
        let members = self.link_vectors(model)?;
        let mut acc: std::collections::HashMap<String, (i64, Vec<f32>)> = std::collections::HashMap::new();
        for m in &members {
            let u = unit(&m.vec);
            let e = acc.entry(m.node_id.clone()).or_insert_with(|| (0, vec![0.0; u.len()]));
            if e.1.len() != u.len() {
                continue;
            }
            e.0 += 1;
            for (s, x) in e.1.iter_mut().zip(&u) {
                *s += x;
            }
        }
        let now = polis_core::ledger::now_millis();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM class_centroids WHERE model = ?1", params![model])?;
        for (node_id, (n, sum)) in &acc {
            tx.execute(
                "INSERT INTO class_centroids (node_id, model, dim, n, sum_vec, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![node_id, model, sum.len() as i64, n, pack_f32(sum), now],
            )?;
        }
        tx.commit()?;
        Ok(acc.len())
    }

    /// One item's chunk-0 vector for a model, by its ledger seq and link
    /// target kind. `None` when the item has no vector yet (or no text).
    pub fn item_vector(&self, target_kind: &str, seq: i64, model: &str) -> rusqlite::Result<Option<Vec<f32>>> {
        let conn = self.conn();
        conn.query_row(ITEM_VECTOR_SQL, params![seq, target_kind, model], |r| {
            let scale: f64 = r.get(0)?;
            let blob: Vec<u8> = r.get(1)?;
            Ok(qvec_to_f32(&polis_core::vec::unpack(&blob, scale as f32)))
        })
        .optional()
    }

    /// The text behind an item, for embedding it on the fly when it has no
    /// vector yet: the prompt's search text, or the page text.
    pub fn item_text(&self, target_kind: &str, seq: i64) -> rusqlite::Result<Option<(i64, String, String)>> {
        let conn = self.conn();
        match target_kind {
            "prompt" => conn
                .query_row(
                    "SELECT p.id, p.fts_text, p.body_hash FROM ledger_events le
                     JOIN prompts p ON p.id = le.prompt_id
                     WHERE le.seq = ?1 AND COALESCE(p.role, 'user') <> 'agent' AND LENGTH(p.fts_text) > 0",
                    params![seq],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional(),
            "browse_event" => conn
                .query_row(
                    "SELECT be.id, be.text, be.context_hash FROM ledger_events le
                     JOIN browse_events be ON be.id = CAST(le.ref_id AS INTEGER)
                     WHERE le.seq = ?1 AND LENGTH(be.text) > 0",
                    params![seq],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional(),
            _ => Ok(None),
        }
    }

    /// Every live link whose target has a chunk-0 vector for the model —
    /// the calibration's population and `rebuild_centroids`' input.
    pub fn link_vectors(&self, model: &str) -> rusqlite::Result<Vec<LinkVector>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT cl.id, cl.node_id, cl.target_kind, CAST(cl.target_id AS INTEGER), e.scale, e.vec
             FROM class_links cl
             JOIN class_nodes n ON n.id = cl.node_id AND n.retired_by_run IS NULL
             JOIN ledger_events le ON le.seq = CAST(cl.target_id AS INTEGER)
             JOIN embeddings e
               ON e.model = ?1 AND e.chunk_ix = 0
              AND ((cl.target_kind = 'prompt' AND e.target_kind = 'prompt' AND e.target_id = le.prompt_id)
                OR (cl.target_kind = 'browse_event' AND e.target_kind = 'browse_event'
                    AND e.target_id = CAST(le.ref_id AS INTEGER)))
             WHERE cl.retired_by_run IS NULL
               AND cl.target_kind IN ('prompt', 'browse_event')
             ORDER BY cl.id ASC",
        )?;
        let rows = stmt.query_map(params![model], |r| {
            let scale: f64 = r.get(4)?;
            let blob: Vec<u8> = r.get(5)?;
            Ok(LinkVector {
                link_id: r.get(0)?,
                node_id: r.get(1)?,
                target_kind: r.get(2)?,
                seq: r.get(3)?,
                vec: qvec_to_f32(&polis_core::vec::unpack(&blob, scale as f32)),
            })
        })?;
        rows.collect()
    }

    /// Retire one link under a run (a filing the model moved out of `~inbox`;
    /// `into` names where it went). A plain mark, not a delete — B2's
    /// `vacuum_retired` removes it past the horizon.
    pub fn retire_link(&self, link_id: i64, run_id: i64, into: Option<&str>) -> rusqlite::Result<bool> {
        let conn = self.conn();
        let n = conn.execute(
            "UPDATE class_links SET retired_by_run = ?2, note = COALESCE(?3, note)
             WHERE id = ?1 AND retired_by_run IS NULL",
            params![link_id, run_id, into.map(|s| format!("refiled → {s}"))],
        )?;
        Ok(n > 0)
    }

    /// Live links under every `~inbox` node, oldest first — the items a
    /// later run with a model re-tries. `(link_id, inbox_node_id, root_id,
    /// target_kind, seq)`.
    pub fn inbox_links(&self, inbox_title: &str, limit: i64) -> rusqlite::Result<Vec<(i64, String, String, String, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT cl.id, n.id, n.parent_id, cl.target_kind, CAST(cl.target_id AS INTEGER)
             FROM class_links cl
             JOIN class_nodes n ON n.id = cl.node_id AND n.retired_by_run IS NULL
             WHERE n.title = ?1 AND n.parent_id IS NOT NULL AND cl.retired_by_run IS NULL
             ORDER BY cl.id ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![inbox_title, limit], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?;
        rows.collect()
    }
}

/// One live link with its member vector.
#[derive(Debug, Clone)]
pub struct LinkVector {
    pub link_id: i64,
    pub node_id: String,
    pub target_kind: String,
    pub seq: i64,
    pub vec: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_centroid_is_the_unit_mean_of_its_members() {
        let store = PolisStore::open_in_memory().unwrap();
        store
            .seed_class_roots(&[("root-x".into(), "x".into(), Some("/x".into()))])
            .unwrap();
        assert!(store.centroid("root-x", "m").unwrap().is_none());
        store.centroid_add("root-x", "m", &[2.0, 0.0]).unwrap();
        store.centroid_add("root-x", "m", &[0.0, 2.0]).unwrap();
        let c = store.centroid("root-x", "m").unwrap().unwrap();
        assert_eq!(c.n, 2);
        assert_eq!(c.dim, 2);
        let m = c.mean_unit();
        assert!((m[0] - m[1]).abs() < 1e-6, "{m:?}");
        assert!((cosine_f32(&m, &[1.0, 1.0]) - 1.0).abs() < 1e-5);
        // The live-node join hides a retired node's centroid.
        assert_eq!(store.centroids_for_model("m").unwrap().len(), 1);
        assert_eq!(store.centroids_for_model("other").unwrap().len(), 0);
    }

    #[test]
    fn f32_pack_round_trips() {
        let v = vec![0.5, -1.25, 3.0e-3, 0.0];
        assert_eq!(unpack_f32(&pack_f32(&v)), v);
        assert_eq!(unit(&[0.0, 0.0]), vec![0.0, 0.0]);
    }
}
