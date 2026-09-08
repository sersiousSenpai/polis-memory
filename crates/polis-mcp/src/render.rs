// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Text summaries beside the structured content. A model reads the text; a
//! client that understands `structuredContent` reads the JSON. Both carry
//! every hit's `seq` (cite as `#seq`), because a hit without its seq cannot
//! be traced back to the record.

use polis_core::api::{ContextBlock, HealthReport, NodeView, TreeNodeView};
use polis_core::ledger::ChainVerdict;
use polis_core::pack::AnswerPack;
use polis_core::types::{BrowseHit, ContextStats, GrepHit, LakeItem, TimelineItem};

/// `YYYY-MM-DD HH:MM` (UTC) from unix milliseconds — a day-precision civil
/// date is all a summary needs, and it saves a calendar dependency.
pub fn ymd_hm(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m) = (rem / 3600, (rem % 3600) / 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}

fn head(s: &str, n: usize) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= n {
        one_line
    } else {
        let cut: String = one_line.chars().take(n).collect();
        format!("{cut}…")
    }
}

fn item_line(item: &LakeItem, superseded_by: Option<i64>) -> String {
    let mut line = format!(
        "#{} {} {}{}",
        item.seq,
        ymd_hm(item.ts),
        item.kind,
        item.surface.as_deref().map(|s| format!("/{s}")).unwrap_or_default()
    );
    if let Some(p) = &item.project_path {
        line.push_str(&format!(" [{p}]"));
    }
    if let Some(b) = item.body.as_deref() {
        line.push_str(": ");
        line.push_str(&head(b, 160));
    }
    if let Some(s) = superseded_by {
        line.push_str(&format!(" (superseded by #{s})"));
    }
    line
}

pub fn pack(p: &AnswerPack) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "answer pack · head #{}{}\n",
        p.head_seq,
        p.query.as_deref().map(|q| format!(" · q: {q}")).unwrap_or_default()
    ));
    match &p.node {
        Some(n) => {
            out.push_str(&format!(
                "class: {} ({}) · {} children · {} links · {} observations\n",
                n.node.title,
                n.node.id,
                n.children.len(),
                n.links.len(),
                n.observations.len()
            ));
            if let Some(s) = &n.node.summary {
                out.push_str(&format!("  {}\n", head(s, 240)));
            }
            for c in n.children.iter().take(12) {
                out.push_str(&format!("  ├ {} ({})\n", c.title, c.id));
            }
            for l in n.links.iter().take(20) {
                out.push_str(&format!(
                    "  → {}:{}{}{}\n",
                    l.link.target_kind,
                    l.link.target_id,
                    l.label.as_deref().map(|s| format!(" {}", head(s, 100))).unwrap_or_default(),
                    l.superseded_by.map(|s| format!(" (superseded by #{s})")).unwrap_or_default()
                ));
            }
            for o in n.observations.iter().take(6) {
                out.push_str(&format!("  ◇ {}\n", head(&o.summary, 200)));
            }
        }
        None => {
            if p.matched_nodes.is_empty() {
                out.push_str("class: none resolved\n");
            } else {
                out.push_str("classes matched: ");
                out.push_str(&p.matched_nodes.iter().take(6).map(|n| format!("{} ({})", n.title, n.id)).collect::<Vec<_>>().join(", "));
                out.push('\n');
            }
        }
    }
    if !p.notes.is_empty() {
        out.push_str(&format!("notes ({}):\n", p.notes.len()));
        for n in p.notes.iter().take(10) {
            out.push_str(&format!("  ✎ {}{}\n", n.seq.map(|s| format!("#{s} ")).unwrap_or_default(), head(&n.text, 160)));
        }
    }
    if !p.prompt_hits.is_empty() {
        out.push_str(&format!("prompt hits ({}):\n", p.prompt_hits.len()));
        for h in p.prompt_hits.iter().take(30) {
            out.push_str("  ");
            out.push_str(&item_line(&h.item, h.superseded_by));
            out.push('\n');
        }
    }
    if !p.browse_hits.is_empty() {
        out.push_str(&format!("browse hits ({}):\n", p.browse_hits.len()));
        for b in p.browse_hits.iter().take(15) {
            out.push_str(&format!("  {} {}{}\n", b.seq.map(|s| format!("#{s}")).unwrap_or_else(|| format!("browse:{}", b.id)), b.url, b.title.as_deref().map(|t| format!(" — {}", head(t, 80))).unwrap_or_default()));
        }
    }
    if !p.grep_hits.is_empty() {
        out.push_str(&format!("grep hits ({}):\n", p.grep_hits.len()));
        for g in p.grep_hits.iter().take(15) {
            out.push_str("  ");
            out.push_str(&grep_line(g));
            out.push('\n');
        }
    }
    let arms: Vec<String> = p
        .arm_coverage
        .iter()
        .map(|a| {
            if a.ran {
                format!("{:?}={}", a.arm, a.hits)
            } else {
                format!("{:?}=absent({})", a.arm, a.absent_because.as_deref().unwrap_or("?"))
            }
        })
        .collect();
    if !arms.is_empty() {
        out.push_str(&format!("arms: {}\n", arms.join(" ")));
    }
    if !p.truncated.is_empty() {
        out.push_str(&format!("truncated: {}\n", p.truncated.join(", ")));
    }
    out
}

pub fn grep_line(g: &GrepHit) -> String {
    format!(
        "{} {} {} {}: {}",
        g.seq.map(|s| format!("#{s}")).unwrap_or_else(|| "#?".into()),
        ymd_hm(g.ts),
        g.kind,
        head(&g.label, 60),
        head(&g.excerpt, 200)
    )
}

pub fn grep(hits: &[GrepHit]) -> String {
    if hits.is_empty() {
        return "no grep hits".into();
    }
    hits.iter().map(grep_line).collect::<Vec<_>>().join("\n")
}

pub fn context(b: &ContextBlock) -> String {
    match &b.text {
        Some(t) => t.clone(),
        None => format!("nothing on record for {}", if b.terms.is_empty() { "this question".to_string() } else { b.terms.join(" ") }),
    }
}

pub fn tree(nodes: &[TreeNodeView]) -> String {
    if nodes.is_empty() {
        return "the catalog is empty".into();
    }
    let mut out = String::new();
    fn walk(nodes: &[TreeNodeView], parent: Option<&str>, depth: usize, out: &mut String) {
        let mut kids: Vec<&TreeNodeView> = nodes.iter().filter(|n| n.node.parent_id.as_deref() == parent).collect();
        kids.sort_by(|a, b| b.link_count.cmp(&a.link_count).then(a.node.title.cmp(&b.node.title)));
        for k in kids {
            out.push_str(&format!(
                "{}{} ({}) · {} links{}{}\n",
                "  ".repeat(depth),
                k.node.title,
                k.node.id,
                k.link_count,
                if k.node.kind == "digest" { " · digest" } else { "" },
                if k.node.status != "accepted" { " · proposed" } else { "" }
            ));
            if depth < 8 {
                walk(nodes, Some(&k.node.id), depth + 1, out);
            }
        }
    }
    walk(nodes, None, 0, &mut out);
    out
}

pub fn node(v: &NodeView) -> String {
    let mut out = format!("{} ({}) · {}\n", v.node.title, v.node.id, v.node.kind);
    if let Some(s) = &v.node.summary {
        out.push_str(&format!("{}\n", head(s, 400)));
    }
    if !v.children.is_empty() {
        out.push_str(&format!("children ({}):\n", v.children.len()));
        for c in v.children.iter().take(30) {
            out.push_str(&format!("  ├ {} ({})\n", c.title, c.id));
        }
    }
    if !v.links.is_empty() {
        out.push_str(&format!("links ({}):\n", v.links.len()));
        for l in v.links.iter().take(60) {
            out.push_str(&format!(
                "  → {}:{}{}{}\n",
                l.link.target_kind,
                l.link.target_id,
                l.label.as_deref().map(|s| format!(" {}", head(s, 120))).unwrap_or_default(),
                l.superseded_by.map(|s| format!(" (superseded by #{s})")).unwrap_or_default()
            ));
        }
    }
    if !v.observations.is_empty() {
        out.push_str(&format!("observations ({}):\n", v.observations.len()));
        for o in v.observations.iter().take(10) {
            out.push_str(&format!("  ◇ {}\n", head(&o.summary, 240)));
        }
    }
    out
}

pub fn timeline(items: &[TimelineItem]) -> String {
    if items.is_empty() {
        return "no events match".into();
    }
    items
        .iter()
        .map(|t| {
            format!(
                "#{} {} {} by {}{}{}",
                t.event.seq,
                ymd_hm(t.event.ts),
                t.event.kind,
                t.event.author,
                t.surface.as_deref().map(|s| format!(" [{s}]")).unwrap_or_default(),
                t.preview.as_deref().map(|p| format!(": {}", head(p, 160))).unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn prompts(items: &[LakeItem]) -> String {
    if items.is_empty() {
        return "no prompts match".into();
    }
    items.iter().map(|i| item_line(i, None)).collect::<Vec<_>>().join("\n")
}

pub fn browse(hits: &[BrowseHit]) -> String {
    if hits.is_empty() {
        return "no pages match".into();
    }
    hits.iter()
        .map(|b| format!("{} {} {}{}: {}", b.seq.map(|s| format!("#{s}")).unwrap_or_else(|| format!("browse:{}", b.id)), ymd_hm(b.ts), b.url, b.title.as_deref().map(|t| format!(" — {}", head(t, 80))).unwrap_or_default(), head(&b.snippet, 160)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn top(pairs: &[(String, i64)], n: usize) -> String {
    let mut v: Vec<&(String, i64)> = pairs.iter().collect();
    v.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    v.iter().take(n).map(|(k, c)| format!("{k}={c}")).collect::<Vec<_>>().join(" ")
}

pub fn stats(s: &ContextStats) -> String {
    let mut out = format!("prompts {} · events {} · generated {}\n", s.total_prompts, s.total_events, ymd_hm(s.generated_ts));
    if !s.by_surface.is_empty() {
        out.push_str(&format!("by surface: {}\n", top(&s.by_surface, 8)));
    }
    if !s.by_kind.is_empty() {
        out.push_str(&format!("by kind: {}\n", top(&s.by_kind, 8)));
    }
    if !s.by_class.is_empty() {
        out.push_str(&format!("by class: {}\n", top(&s.by_class, 8)));
    }
    if !s.by_day.is_empty() {
        let recent: Vec<String> = s.by_day.iter().rev().take(7).map(|(d, c)| format!("{d}={c}")).collect();
        out.push_str(&format!("recent days: {}\n", recent.join(" ")));
    }
    out
}

pub fn verdict(v: &ChainVerdict) -> String {
    if v.ok {
        format!("chain ok · {} events checked · head {}", v.checked, v.head_hash.as_deref().map(|h| &h[..h.len().min(16)]).unwrap_or("-"))
    } else {
        format!("CHAIN BROKEN · first bad seq {:?} · {} checked", v.first_bad_seq, v.checked)
    }
}

pub fn health(h: &HealthReport) -> String {
    format!(
        "{} · head #{} · {} prompts · {} class nodes · model {} · embedder {} · schema {} · lexical {}",
        if h.ok { "healthy" } else { "CHAIN BROKEN" },
        h.head_seq,
        h.total_prompts,
        h.class_nodes,
        h.model.as_deref().unwrap_or("none"),
        h.embedder,
        h.schema_version.as_deref().unwrap_or("?"),
        h.lexical_version.as_deref().unwrap_or("?")
    )
}

/// One line for a write: what happened and the seq to cite.
pub fn write_receipt(verb: &str, r: &polis_core::api::WriteReceipt) -> String {
    match (r.seq, r.id) {
        (Some(seq), _) => format!("{verb} (event #{seq})"),
        (None, Some(id)) => format!("{verb} (row {id}, no new event)"),
        (None, None) => format!("{verb} (nothing new)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_are_right_at_the_edges() {
        assert_eq!(ymd_hm(0), "1970-01-01 00:00");
        assert_eq!(ymd_hm(951_782_400_000), "2000-02-29 00:00");
        assert_eq!(ymd_hm(1_788_774_679_521), "2026-09-07 09:51");
        assert_eq!(ymd_hm(-1000), "1969-12-31 23:59");
    }

    #[test]
    fn heads_clip_on_char_boundaries() {
        assert_eq!(head("héllo wörld", 5), "héllo…");
        assert_eq!(head("a  b\nc", 10), "a b c");
    }
}
