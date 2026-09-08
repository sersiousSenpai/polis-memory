// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Query planning for the lexical layer — pure, no database.
//!
//! What this replaces was one line: split the raw query on whitespace, quote
//! every token, `OR` them together. It kept stopwords (so "what did I decide
//! about the browser tab suspension" spent most of its recall budget on "what",
//! "did", "I", "the"), it had no `AND` stage (so the highest-signal reading of a
//! multi-word query was never even attempted), and it had no prefix stage (so a
//! stem the porter tokenizer didn't unify simply missed).
//!
//! The replacement is a **cascade**, and the cascade is the point:
//!
//! 1. `AND` over the content terms — the precise reading. If several documents
//!    contain all of them, those are the answer and nothing else needs trying.
//! 2. `OR` with prefixes — the recall net, used only when the precise reading
//!    found nothing.
//! 3. `LIKE` — the literal fallback for a query that produced no usable terms
//!    at all (all stopwords, all punctuation, one CJK character).
//!
//! Every hit is labelled with the stage that found it, so a caller can tell "we
//! found exactly what you asked for" from "we widened until something matched" —
//! a distinction the old single-stage OR could not express.
//!
//! The injection defense is preserved verbatim from `sanitize_fts_query`: every
//! term is wrapped as a quoted FTS5 phrase with embedded `"` doubled, which
//! neutralizes `*`, `-`, `:`, `NEAR` and parens. A hostile query can only ever
//! match literally.

/// Words that carry no retrieval signal in a question. Deliberately short: this
/// is not a linguistics exercise, it is the list of words that appear in the
/// shape "what did I decide about X" and would otherwise dominate an OR.
///
/// Dropped only when something survives — a query that is *entirely* stopwords
/// ("what did I do?") still gets to match them, because at that point they are
/// all the user gave us and an empty result would be a lie about the corpus.
const STOPWORDS: &[&str] = &[
    "a", "about", "after", "all", "also", "am", "an", "and", "any", "are", "as", "at", "be",
    "been", "before", "being", "but", "by", "can", "did", "do", "does", "doing", "for", "from",
    "had", "has", "have", "how", "i", "if", "in", "into", "is", "it", "its", "just", "me", "my",
    "of", "on", "or", "our", "out", "over", "same", "she", "so", "some", "than", "that", "the",
    "their", "them", "then", "there", "these", "they", "this", "to", "too", "up", "was", "we",
    "were", "what", "when", "where", "which", "while", "who", "why", "will", "with", "would",
    "you", "your",
];

/// Characters that stay INSIDE a token. Must mirror the `tokenchars` set the
/// indexes were built with (`db::TOKENIZER`), or the planner would emit terms
/// the index cannot contain: split `src/db.rs` here and it can never match the
/// single token the tokenizer stored.
const TOKEN_CHARS: [char; 5] = ['_', '-', '.', '/', '@'];

/// Terms beyond this are dropped. A 12-term `AND` already has essentially zero
/// chance of matching, and FTS5's query cost is linear in term count.
const MAX_TERMS: usize = 12;

/// Minimum length for a term to get a `*` in the OR stage. Shorter prefixes
/// match too much to be worth the cost.
const MIN_PREFIX_LEN: usize = 4;

/// Which stage of the cascade produced a hit. Carried through to the answer
/// pack so the model — and the user — can see how hard the retrieval had to
/// work to find something.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MatchStage {
    /// Every content term is present. The precise reading.
    And,
    /// Any term, with prefix expansion. The recall net.
    Or,
    /// Substring scan — no usable terms survived planning.
    Like,
}

impl MatchStage {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchStage::And => "and",
            MatchStage::Or => "or",
            MatchStage::Like => "like",
        }
    }
}

/// A planned query: the MATCH strings for each stage plus the terms themselves,
/// which callers need for excerpting (centering a window on the match) and for
/// deciding whether a literal/grep arm is worth running.
#[derive(Debug, Clone, PartialEq)]
pub struct FtsPlan {
    /// `"a" AND "b"` — the precise stage.
    pub and_match: String,
    /// `"a"* OR "b"*` — the recall stage.
    pub or_match: String,
    /// Quoted phrases the user asked for explicitly, in order.
    pub phrases: Vec<String>,
    /// The bare terms (unquoted, lowercased), for excerpting and heuristics.
    pub terms: Vec<String>,
    /// True when every token was a stopword and they were therefore kept. The
    /// caller can use this to lower its confidence rather than to hide the
    /// result: the user asked something, and the honest answer is a weak match,
    /// not silence.
    pub all_stopwords: bool,
}

impl FtsPlan {
    /// The MATCH string for a given stage, or `None` for `Like` (which is not
    /// an FTS query at all).
    pub fn match_for(&self, stage: MatchStage) -> Option<&str> {
        match stage {
            MatchStage::And => Some(&self.and_match),
            MatchStage::Or => Some(&self.or_match),
            MatchStage::Like => None,
        }
    }
}

/// Wrap a term as an FTS5 phrase literal, doubling embedded quotes. This is the
/// whole injection defense, unchanged from `sanitize_fts_query`: inside a quoted
/// phrase, FTS5's operators are just characters.
fn quote(term: &str) -> String {
    format!("\"{}\"", term.replace('"', "\"\""))
}

/// Split raw text into candidate terms: lowercase, break on anything that is
/// neither alphanumeric nor a `tokenchars` member, and drop tokens with no
/// alphanumeric content at all (a bare `--`, a lone `/`).
fn tokenize(raw: &str) -> Vec<String> {
    let all: Vec<String> = raw
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && !TOKEN_CHARS.contains(&c))
        .map(|t| t.trim_matches(|c: char| TOKEN_CHARS.contains(&c)))
        .filter(|t| t.chars().any(char::is_alphanumeric))
        .map(str::to_string)
        .collect();
    // A lone ASCII letter is the tail of a possessive or a contraction
    // ("Redline's" → "s", "don't" → "t"), never a term worth matching: it is
    // how "what repos did I look at for Redline's memory?" resolved to an
    // astronomy node whose title carried "Polymathic's". Digits and
    // non-ASCII single characters (a CJK word) stay. Dropped only when
    // something else survives, like the stopword rule.
    // (A one-letter stopword — "i", "a" — is left to the stopword rule, which
    // keeps it when the whole query is stopwords.)
    let kept: Vec<String> = all
        .iter()
        .filter(|t| {
            !(t.len() == 1 && t.chars().all(|c| c.is_ascii_alphabetic()) && !STOPWORDS.contains(&t.as_str()))
        })
        .cloned()
        .collect();
    if kept.is_empty() {
        all
    } else {
        kept
    }
}

/// Pull `"quoted phrases"` out of the raw query, returning them plus the
/// remaining text. A phrase is an explicit instruction to match those words
/// adjacently, and it survives every stage of the cascade intact.
fn extract_phrases(raw: &str) -> (Vec<String>, String) {
    let mut phrases = Vec::new();
    let mut rest = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '"' {
            rest.push(c);
            continue;
        }
        let mut phrase = String::new();
        let mut closed = false;
        for c in chars.by_ref() {
            if c == '"' {
                closed = true;
                break;
            }
            phrase.push(c);
        }
        let phrase = phrase.trim();
        if closed && !phrase.is_empty() {
            phrases.push(phrase.to_lowercase());
        } else {
            // An unbalanced quote is a typo, not a phrase: fold the text back
            // in rather than swallowing the rest of the query.
            rest.push_str(phrase);
        }
    }
    (phrases, rest)
}

/// Plan a raw user query. Returns `None` when nothing searchable survives, so a
/// caller returns no hits rather than an FTS5 syntax error.
pub fn plan_fts_query(raw: &str) -> Option<FtsPlan> {
    let (phrases, rest) = extract_phrases(raw);
    let tokens = tokenize(&rest);

    // Drop stopwords UNLESS that empties the list. "what did I decide about the
    // browser tab suspension" keeps only {decide, browser, tab, suspension};
    // "what did I do" keeps all four, because they are the whole question.
    let content: Vec<String> = tokens
        .iter()
        .filter(|t| !STOPWORDS.contains(&t.as_str()))
        .cloned()
        .collect();
    let all_stopwords = content.is_empty() && !tokens.is_empty();
    let mut terms = if all_stopwords { tokens } else { content };
    terms.dedup();
    terms.truncate(MAX_TERMS);

    if terms.is_empty() && phrases.is_empty() {
        return None;
    }

    // Phrases are non-negotiable in both stages — the user quoted them.
    let quoted_phrases: Vec<String> = phrases.iter().map(|p| quote(p)).collect();

    let mut and_parts = quoted_phrases.clone();
    and_parts.extend(terms.iter().map(|t| quote(t)));

    let mut or_parts = quoted_phrases;
    or_parts.extend(terms.iter().map(|t| {
        // A prefix wildcard goes OUTSIDE the closing quote: `"foo"*` is a
        // prefix query, `"foo*"` matches a literal asterisk.
        if t.chars().count() >= MIN_PREFIX_LEN {
            format!("{}*", quote(t))
        } else {
            quote(t)
        }
    }));

    Some(FtsPlan {
        and_match: and_parts.join(" AND "),
        or_match: or_parts.join(" OR "),
        phrases,
        terms,
        all_stopwords,
    })
}

/// How many of a plan's `terms` a piece of curated text (a class title and
/// summary) actually carries, as `(exact, prefix)`: whole-token matches, and
/// — for terms the planner would prefix-match (≥ `MIN_PREFIX_LEN` chars) — a
/// token merely starting with the term. Tokenized the way queries are, so
/// "Redline's" and "redline" agree.
pub fn term_coverage(terms: &[String], text: &str) -> (usize, usize) {
    let tokens = tokenize(text);
    let mut exact = 0;
    let mut prefix = 0;
    for term in terms {
        if tokens.iter().any(|tok| tok == term) {
            exact += 1;
        } else if term.chars().count() >= MIN_PREFIX_LEN && tokens.iter().any(|tok| tok.starts_with(term.as_str())) {
            prefix += 1;
        }
    }
    (exact, prefix)
}

/// Whether a title match is strong enough to be THE class a question is
/// about. The OR stage of the cascade finds a node on any single term, and a
/// single loose term is how "what repos did I look at for Redline's memory?"
/// resolved to "Payload CMS lookup" (`look*` → `lookup`): one prefix of one
/// term. A class is claimed when the text carries one of the query's terms
/// as a whole token (bm25 already ranks the specific word over the common
/// one — "what did I decide about the browser tab suspension" resolves
/// "Embedded browser" on `browser`), or covers two terms counting prefixes.
/// A prefix alone never resolves. Anything weaker is a candidate the pack
/// may list, never the node it answers from; the lexical and semantic arms
/// still answer (the miss path).
pub fn resolves_class(terms: &[String], text: &str) -> bool {
    if terms.is_empty() {
        return false;
    }
    let (exact, prefix) = term_coverage(terms, text);
    exact >= 1 || exact + prefix >= 2
}

/// Plan a follow-up question that is only a fragment ("and the beta?"), by
/// borrowing terms from the previous turn. A fragment carries its subject
/// implicitly; without this, turn two of a conversation retrieves against two
/// stopwords and one noun and reads as though the record were empty.
///
/// Only fires when the fragment is genuinely thin (fewer than two content
/// terms) — a full question is never diluted with stale context.
pub fn plan_query_with_context(raw: &str, prior_user_text: Option<&str>) -> Option<FtsPlan> {
    let plan = plan_fts_query(raw);
    let thin = plan.as_ref().is_none_or(|p| p.terms.len() < 2 || p.all_stopwords);
    if !thin {
        return plan;
    }
    let Some(prior) = prior_user_text.filter(|p| !p.trim().is_empty()) else {
        return plan;
    };
    let merged = format!("{raw} {prior}");
    plan_fts_query(&merged).or(plan)
}

/// Does this query look like it is reaching for a LITERAL — a flag, a path, an
/// identifier, an error string — rather than asking a question?
///
/// This gates the grep arm. Running a trigram probe on every natural-language
/// question costs an index scan to return noise; running it on `--allowedTools`
/// or `src-tauri/src/db.rs` is the only way to find them at all.
pub fn looks_literal(plan: &FtsPlan, raw: &str) -> bool {
    // An explicitly quoted phrase of any substance is a literal request.
    if plan.phrases.iter().any(|p| p.chars().count() >= 3) {
        return true;
    }
    let trimmed = raw.trim();
    if trimmed.starts_with("--") || trimmed.contains("::") {
        return true;
    }
    // An interior punctuation mark inside a token — `db.rs`, `src/db`,
    // `snake_case`, `#[serde(...)]` — rather than at a word boundary.
    plan.terms.iter().any(|t| {
        let chars: Vec<char> = t.chars().collect();
        chars.len() >= 3
            && chars[1..chars.len() - 1]
                .iter()
                .any(|c| TOKEN_CHARS.contains(c) || *c == '#' || *c == '(')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The query that proved the old planner broken on the live daemon:
    /// "what did I decide about the browser tab suspension" resolved nothing,
    /// because six of its nine words were stopwords ORed against the corpus.
    #[test]
    fn stopwords_drop_out_of_a_question() {
        let p = plan_fts_query("what did I decide about the browser tab suspension").unwrap();
        assert_eq!(p.terms, vec!["decide", "browser", "tab", "suspension"]);
        assert!(!p.all_stopwords);
        assert_eq!(
            p.and_match,
            "\"decide\" AND \"browser\" AND \"tab\" AND \"suspension\""
        );
        assert!(p.or_match.contains("\"browser\"*"), "long terms get a prefix");
        assert!(p.or_match.contains(" OR "));
    }

    /// …but a question made ENTIRELY of stopwords still gets to match them.
    /// Returning nothing would be a claim about the corpus rather than about
    /// the query.
    #[test]
    fn an_all_stopword_query_keeps_its_words() {
        let p = plan_fts_query("what did I do").unwrap();
        assert!(p.all_stopwords);
        assert_eq!(p.terms, vec!["what", "did", "i", "do"]);
    }

    /// A class is the answer only when it covers the question, not when one
    /// loose term happens to prefix a word in its title.
    #[test]
    fn a_class_resolves_on_coverage_not_on_one_loose_term() {
        let p = plan_fts_query("what repos did i look at for Redline's memory?").unwrap();
        assert!(!resolves_class(&p.terms, "Payload CMS lookup"), "one prefix hit (look → lookup) is not a resolution");
        assert!(!resolves_class(&p.terms, "AION-1 — Polymathic's astronomy foundation model"));
        assert!(resolves_class(&p.terms, "Redline memory research — repos compared"));
        assert_eq!(term_coverage(&p.terms, "Redline memory research — repos compared"), (3, 0));
        // One exact, specific term resolves a short broad title — bm25 ranks
        // it; the rule only refuses prefix-only matches.
        let b = plan_fts_query("what did I decide about the browser tab suspension").unwrap();
        assert!(resolves_class(&b.terms, "Embedded browser"));
        assert!(!resolves_class(&b.terms, "Browsing history lookup"), "browser* → browsing is a prefix alone");
        // One-term queries resolve on their one term.
        let one = plan_fts_query("sqlite").unwrap();
        assert!(resolves_class(&one.terms, "SQLite FTS5 and the trigram tokenizer"));
        assert!(!resolves_class(&one.terms, "Payload CMS lookup"));
        // Prefixes count only at the planner's threshold.
        let short = plan_fts_query("tab suspension").unwrap();
        assert!(resolves_class(&short.terms, "browser tab suspension"));
        assert!(!resolves_class(&short.terms, "tables and suspense"), "`tab` is too short to prefix `tables`");
    }

    /// The possessive and contraction tails. "Redline's memory" must plan to
    /// `redline` and `memory`, never to a lone `s` that an unrelated title's
    /// own possessive also tokenizes to — the answer pack resolved a question
    /// about Redline's memory to "Polymathic's astronomy foundation model"
    /// on exactly that match.
    #[test]
    fn possessive_and_contraction_tails_are_not_terms() {
        let p = plan_fts_query("what repos did i look at for Redline's memory?").unwrap();
        let terms: Vec<&str> = p.terms.iter().map(String::as_str).collect();
        assert!(terms.contains(&"redline"), "{terms:?}");
        assert!(terms.contains(&"memory"), "{terms:?}");
        assert!(!terms.contains(&"s"), "the possessive tail became a term: {terms:?}");
        let p = plan_fts_query("don't merge the classes").unwrap();
        assert!(!p.terms.iter().any(|t| t == "t"), "{:?}", p.terms);
        // A one-letter query still matches its letter rather than nothing.
        assert!(plan_fts_query("s").is_some());
    }

    /// `tokenchars` must mean the same thing in the planner as in the index, or
    /// the planner emits terms the index cannot hold.
    #[test]
    fn tokenchars_keep_identifiers_whole() {
        let p = plan_fts_query("why does src-tauri/src/db.rs use --allowedTools").unwrap();
        assert!(p.terms.contains(&"src-tauri/src/db.rs".to_string()), "{:?}", p.terms);
        assert!(p.terms.contains(&"allowedtools".to_string()), "{:?}", p.terms);
    }

    #[test]
    fn quoted_phrases_survive_both_stages() {
        let p = plan_fts_query("\"tab suspension\" browser").unwrap();
        assert_eq!(p.phrases, vec!["tab suspension"]);
        assert!(p.and_match.starts_with("\"tab suspension\" AND "));
        assert!(p.or_match.starts_with("\"tab suspension\" OR "));
    }

    /// The injection defense is the reason every term is a quoted phrase.
    #[test]
    fn fts_operators_are_neutralized_into_literals() {
        let p = plan_fts_query("foo* OR bar NEAR(baz) -qux \"a\"\"b\"").unwrap();
        // Every emitted term is inside quotes; no bare operator escapes.
        for part in p.and_match.split(" AND ") {
            assert!(part.starts_with('"'), "unquoted fragment: {part}");
        }
        // An embedded quote is doubled, not terminated.
        let p2 = plan_fts_query("say \\\"hi").unwrap();
        assert!(!p2.and_match.contains("\"\"\""), "{}", p2.and_match);
    }

    #[test]
    fn nothing_searchable_plans_to_none() {
        assert!(plan_fts_query("").is_none());
        assert!(plan_fts_query("   ").is_none());
        assert!(plan_fts_query("--- ///").is_none());
    }

    #[test]
    fn terms_are_capped() {
        let raw = (0..40).map(|i| format!("term{i}")).collect::<Vec<_>>().join(" ");
        let p = plan_fts_query(&raw).unwrap();
        assert_eq!(p.terms.len(), MAX_TERMS);
    }

    /// A follow-up fragment inherits the previous turn's subject; a full
    /// question is left alone.
    #[test]
    fn a_thin_followup_borrows_the_prior_turn() {
        let p = plan_query_with_context("and the beta?", Some("how did the alpha launch go"))
            .unwrap();
        assert!(p.terms.contains(&"beta".to_string()));
        assert!(p.terms.contains(&"alpha".to_string()), "{:?}", p.terms);

        let full = plan_query_with_context(
            "what did I decide about the browser tab suspension",
            Some("something else entirely"),
        )
        .unwrap();
        assert!(!full.terms.contains(&"entirely".to_string()), "{:?}", full.terms);
    }

    /// The gate on the grep arm: literals in, questions out.
    #[test]
    fn looks_literal_table() {
        let cases: &[(&str, bool)] = &[
            ("--allowedTools", true),
            ("src-tauri/src/db.rs", true),
            ("ledger::body_hash", true),
            ("\"exact phrase here\"", true),
            ("rl_del", true),
            ("what did I decide about the browser tab suspension", false),
            ("keeper compaction", false),
            ("browser", false),
        ];
        for (raw, want) in cases {
            let plan = plan_fts_query(raw).expect(raw);
            assert_eq!(looks_literal(&plan, raw), *want, "for {raw:?}");
        }
    }
}
