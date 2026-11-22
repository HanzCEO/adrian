//! Locating and verifying the model files.
//!
//! Verification is not ceremony. A truncated `safetensors` file still loads: the header is
//! intact and the tensor data is short. The model then runs and returns plausible-looking
//! numbers that are quietly wrong, which is the worst possible failure for a retrieval
//! system. Checking sha256 at startup turns that into a refusal to boot.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The files the engine needs, relative to the weights directory.
pub const REQUIRED_FILES: [&str; 9] = [
    "model.safetensors",
    "dense_branch.safetensors",
    "1_HybridTokens/model.safetensors",
    "1_HybridTokens/config.json",
    "tokenizer.json",
    "tokenizer_config.json",
    "config.json",
    "config_sentence_transformers.json",
    "sentence_bert_config.json",
];

/// The files the engine loads, resolved to absolute paths.
#[derive(Debug, Clone)]
pub struct ModelFiles {
    pub root: PathBuf,
    pub main: PathBuf,
    pub dense_branch: PathBuf,
    pub head: PathBuf,
    pub head_config: PathBuf,
    pub config: PathBuf,
    pub tokenizer: PathBuf,
    pub tokenizer_config: PathBuf,
    /// Holds the punctuation skip list and the `[Q] `/`[D] ` prefixes.
    pub sentence_transformers_config: PathBuf,
}

impl ModelFiles {
    /// Resolve the required files under `root`, failing if any is missing.
    pub fn discover(root: impl AsRef<Path>) -> Result<ModelFiles> {
        let root = root.as_ref().to_path_buf();
        for relative in REQUIRED_FILES {
            let path = root.join(relative);
            if !path.is_file() {
                bail!(
                    "missing model file: {}\nRun scripts/fetch_weights.sh to download the \
                     model, or point ADRIAN_WEIGHTS_DIR at a directory that has it.",
                    path.display()
                );
            }
        }
        Ok(ModelFiles {
            main: root.join("model.safetensors"),
            dense_branch: root.join("dense_branch.safetensors"),
            head: root.join("1_HybridTokens/model.safetensors"),
            head_config: root.join("1_HybridTokens/config.json"),
            config: root.join("config.json"),
            tokenizer: root.join("tokenizer.json"),
            tokenizer_config: root.join("tokenizer_config.json"),
            sentence_transformers_config: root.join("config_sentence_transformers.json"),
            root,
        })
    }

    /// Verify every file against the `SHA256SUMS` manifest in the same directory.
    ///
    /// The manifest format is the one `sha256sum` writes: `<hex>  <relative path>`.
    pub fn verify(&self) -> Result<()> {
        let manifest_path = self.root.join("SHA256SUMS");
        if !manifest_path.is_file() {
            bail!(
                "missing {}: refusing to serve weights that cannot be verified",
                manifest_path.display()
            );
        }
        let manifest = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("read {}", manifest_path.display()))?;

        let expected = parse_manifest(&manifest)?;
        let mut verified = 0usize;

        for relative in REQUIRED_FILES {
            let Some(want) = expected.get(relative) else {
                bail!("SHA256SUMS has no entry for {relative}");
            };
            let path = self.root.join(relative);
            let got = sha256_file(&path).with_context(|| format!("hash {}", path.display()))?;
            if &got != want {
                bail!(
                    "checksum mismatch for {}\n  expected {want}\n  actual   {got}",
                    path.display()
                );
            }
            verified += 1;
        }

        if verified != REQUIRED_FILES.len() {
            bail!(
                "verified {verified} of {} model files",
                REQUIRED_FILES.len()
            );
        }
        Ok(())
    }
}

fn parse_manifest(text: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else { continue };
        let Some(name) = parts.next() else { continue };
        let name = name.trim_start_matches('*');
        out.insert(name.to_string(), hash.to_ascii_lowercase());
    }
    Ok(out)
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parsing_accepts_the_sha256sum_format() {
        let text = "\
abc123  model.safetensors
DEF456 *dense_branch.safetensors

# a comment
";
        let parsed = parse_manifest(text).unwrap();
        assert_eq!(parsed.get("model.safetensors").unwrap(), "abc123");
        assert_eq!(parsed.get("dense_branch.safetensors").unwrap(), "def456");
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn missing_directory_fails_with_a_useful_message() {
        let err = ModelFiles::discover("/nonexistent/adrian/weights").unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("fetch_weights.sh"), "got: {message}");
    }
}
