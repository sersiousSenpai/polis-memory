// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! The `classmemory` skill — the classifier's ops contract and the retrieval
//! contract an external model follows — shipped inside the crate so a host
//! installs it from here rather than carrying its own copy. The file lives
//! inside this crate (`skills/classmemory/SKILL.md`, so `cargo package`
//! ships it); a host embeds `CLASSMEMORY_SKILL` rather than carrying a copy.
//!
//! Owed: the text still names the host's bridge (`127.0.0.1:7676`); E1 makes
//! the daemon address a parameter and this becomes a template.

pub const CLASSMEMORY_SKILL: &str = include_str!("../skills/classmemory/SKILL.md");

/// The address the skill text is written for — Redline's daemon, the host the
/// text was carved from. [`CLASSMEMORY_SKILL`] is the rendering for exactly
/// this address, byte for byte.
pub const CLASSMEMORY_SKILL_ADDR: &str = "127.0.0.1:7676";

/// The same skill, rendered for another daemon address (the standalone
/// `polis serve` default is `127.0.0.1:7677`; a host passes its own). A
/// template by substitution, so the two renderings can never disagree on
/// anything but the address (Session E1).
pub fn render_classmemory_skill(daemon_addr: &str) -> String {
    CLASSMEMORY_SKILL.replace(CLASSMEMORY_SKILL_ADDR, daemon_addr)
}

/// The agent-facing `polis-memory` skill for a standalone install: how to
/// search, cite and write the record over MCP, with the daemon's HTTP API as
/// the fallback. A template with `{{ADDR}}` and `{{TOKEN_PATH}}` (the token's
/// PATH, never its value); [`render_polis_memory_skill`] fills them. Separate
/// from [`CLASSMEMORY_SKILL`], whose bytes hosts pin.
pub const POLIS_MEMORY_SKILL_TEMPLATE: &str = include_str!("../skills/polis-memory/SKILL.md");

/// The `polis-memory` skill for this daemon address and token file.
pub fn render_polis_memory_skill(daemon_addr: &str, token_path: &str) -> String {
    POLIS_MEMORY_SKILL_TEMPLATE.replace("{{ADDR}}", daemon_addr).replace("{{TOKEN_PATH}}", token_path)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_standalone_skill_renders_its_address_and_token_path_only() {
        let text = super::render_polis_memory_skill("127.0.0.1:7677", "/home/u/.polis/token");
        assert!(text.starts_with("---\nname: polis-memory\n"));
        assert!(text.contains("http://127.0.0.1:7677/v1/memory/answer-pack") && text.contains("$(cat /home/u/.polis/token)"));
        assert!(!text.contains("{{") && !text.contains("7676") && !text.contains("Redline"), "host-neutral and fully rendered");
        for tool in ["memory_search", "memory_grep", "memory_decide", "memory_forget"] {
            assert!(text.contains(tool), "{tool}");
        }
    }

    #[test]
    fn the_skill_ships_with_the_crate_and_teaches_the_ops() {
        for op in ["file", "create", "promote", "split", "merge", "collapse", "supersede"] {
            assert!(super::CLASSMEMORY_SKILL.contains(op), "skill lost the `{op}` op");
        }
        assert!(super::CLASSMEMORY_SKILL.contains("answer-pack"));
    }

    /// The const is the template rendered for the address it was written
    /// for; any other address yields the same text with only that changed.
    #[test]
    fn the_template_renders_the_const_for_its_own_address() {
        assert_eq!(super::render_classmemory_skill(super::CLASSMEMORY_SKILL_ADDR), super::CLASSMEMORY_SKILL);
        assert!(super::CLASSMEMORY_SKILL.contains(super::CLASSMEMORY_SKILL_ADDR), "the template has an address to substitute");
        let other = super::render_classmemory_skill("127.0.0.1:7677");
        assert!(other.contains("127.0.0.1:7677") && !other.contains(super::CLASSMEMORY_SKILL_ADDR));
        assert_eq!(other.len(), super::CLASSMEMORY_SKILL.len(), "same length: only the port differs");
    }
}
