//! Tokenization, including the two things the model's contract adds on top of the
//! tokenizer file: the role prefix and the punctuation skip list.

use crate::model::{Role, SEP_TOKEN_ID};
use anyhow::{Context, Result};
use std::path::Path;
use tokenizers::Tokenizer;

/// A tokenized text, ready for the model.
#[derive(Debug, Clone)]
pub struct Encoded {
    pub ids: Vec<u32>,
    pub truncated: bool,
}

/// Wraps the Hugging Face tokenizer with the model's conventions.
pub struct TextTokenizer {
    tokenizer: Tokenizer,
    /// Token ids that documents drop before the vectors are returned.
    skiplist: Vec<u32>,
}

impl TextTokenizer {
    pub fn load(tokenizer_path: &Path, config_path: &Path) -> Result<Self> {
        let tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|err| anyhow::anyhow!("{err}"))
            .with_context(|| format!("load {}", tokenizer_path.display()))?;

        let config_text = std::fs::read_to_string(config_path)
            .with_context(|| format!("read {}", config_path.display()))?;
        let config: serde_json::Value = serde_json::from_str(&config_text)
            .context("parse config_sentence_transformers.json")?;

        let words: Vec<String> = config
            .get("skiplist_words")
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        // The oracle resolves these with `convert_tokens_to_ids`, so the same lookup is
        // used here. A word the vocabulary does not contain is simply not skipped.
        let skiplist: Vec<u32> = words
            .iter()
            .filter_map(|word| tokenizer.token_to_id(word))
            .collect();

        Ok(Self {
            tokenizer,
            skiplist,
        })
    }

    /// Token ids that documents drop.
    pub fn skiplist(&self) -> &[u32] {
        &self.skiplist
    }

    pub fn is_skipped(&self, token_id: u32) -> bool {
        self.skiplist.contains(&token_id)
    }

    /// Tokenize one text in one role.
    ///
    /// The role prefix is not decoration: `[Q]` and `[D]` are single added tokens, and the
    /// model reads the one at position 1 to decide how to treat the rest.
    pub fn encode(&self, text: &str, role: Role) -> Result<Encoded> {
        let prefix = match role {
            Role::Document => "[D] ",
            Role::Query => "[Q] ",
        };
        let prefixed = format!("{prefix}{text}");

        let encoding = self
            .tokenizer
            .encode(prefixed, true)
            .map_err(|err| anyhow::anyhow!("tokenize: {err}"))?;

        let mut ids: Vec<u32> = encoding.get_ids().to_vec();
        let limit = role.max_tokens();
        let mut truncated = false;

        if ids.len() > limit {
            // Right truncation, keeping the leading special token and re-terminating.
            truncated = true;
            ids.truncate(limit.saturating_sub(1));
            ids.push(SEP_TOKEN_ID);
        }

        Ok(Encoded { ids, truncated })
    }

    /// The id for a literal token string, used by the tests and by the skiplist check.
    pub fn token_to_id(&self, token: &str) -> Option<u32> {
        self.tokenizer.token_to_id(token)
    }
}
