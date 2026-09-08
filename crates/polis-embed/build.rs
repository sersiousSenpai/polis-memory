// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Yusuf Al-Bazian
//! Feature `bundled-model`: copy the three pinned potion-base-8M files from
//! the directory `POLIS_BUNDLED_MODEL_DIR` names into `OUT_DIR`, where the
//! crate `include_bytes!`s them. Without the variable the build still
//! succeeds — with zero-length placeholders that fail the runtime hash check
//! (`Model2Vec::bundled()` returns an error naming the variable) — so a
//! `--all-features` CI build needs no model directory and no network. The
//! hashes are checked here too: a wrong file never reaches the binary.

use std::io::Write;
use std::path::PathBuf;

const PINNED: [(&str, &str); 3] = [
    ("model.safetensors", "f65d0f325faadc1e121c319e2faa41170d3fa07d8c89abd48ca5358d9a223de2"),
    ("tokenizer.json", "e67e803f624fb4d67dea1c730d06e1067e1b14d830e2c2202569e3ef0f70bb50"),
    ("config.json", "2a6ac0e9aaa356a68a5688070db78fc3a464fefe85d2f06a1905ce3718687553"),
];

fn main() {
    println!("cargo:rerun-if-env-changed=POLIS_BUNDLED_MODEL_DIR");
    if std::env::var("CARGO_FEATURE_BUNDLED_MODEL").is_err() {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let dir = std::env::var_os("POLIS_BUNDLED_MODEL_DIR").map(PathBuf::from);
    for (name, want) in PINNED {
        let dst = out.join(name);
        match &dir {
            Some(d) => {
                let src = d.join(name);
                println!("cargo:rerun-if-changed={}", src.display());
                let bytes = std::fs::read(&src).unwrap_or_else(|e| panic!("bundled-model: {}: {e}", src.display()));
                let got = sha256_hex(&bytes);
                assert!(got == want, "bundled-model: {} hashes to {got}, the pinned digest is {want}", src.display());
                std::fs::write(&dst, bytes).expect("write the bundled file");
            }
            None => {
                println!("cargo:warning=bundled-model: POLIS_BUNDLED_MODEL_DIR is unset; {name} is an empty placeholder and Model2Vec::bundled() will refuse it");
                std::fs::File::create(&dst).expect("placeholder").write_all(b"").unwrap();
            }
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}
