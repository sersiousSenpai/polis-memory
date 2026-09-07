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

#[cfg(test)]
mod tests {
    #[test]
    fn the_skill_ships_with_the_crate_and_teaches_the_ops() {
        for op in ["file", "create", "promote", "split", "merge", "collapse", "supersede"] {
            assert!(super::CLASSMEMORY_SKILL.contains(op), "skill lost the `{op}` op");
        }
        assert!(super::CLASSMEMORY_SKILL.contains("answer-pack"));
    }
}
