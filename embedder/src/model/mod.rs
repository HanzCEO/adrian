//! The inference engine: trunk, two tops, output head.

pub mod bert;
pub mod head;
pub mod tokenizer;

use crate::weights::ModelFiles;
use anyhow::{Context, Result};
use candle::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use std::sync::Arc;

/// The revision of the model these weights come from.
pub const MODEL_REVISION: &str = "b18df82e77306006a1dd0162199f6dfeacb5492e";

/// The model name reported by `/v1/models`.
pub const MODEL_NAME: &str = "uibk-embedding-v1";

/// Token ids that mark which side of a retrieval a text is on.
///
/// The model recognises a query by this marker at position 1 and keeps only the first
/// [`QUERY_LOCAL_CAP`] token vectors for it, so the role cannot be inferred from the text.
pub const CLS_TOKEN_ID: u32 = 50281;
pub const SEP_TOKEN_ID: u32 = 50282;
pub const QUERY_TOKEN_ID: u32 = 50368;
pub const DOCUMENT_TOKEN_ID: u32 = 50369;

/// How many token vectors survive on the query side.
pub const QUERY_LOCAL_CAP: usize = 64;

/// Maximum total tokens, including `[CLS]` and `[SEP]`.
pub const MAX_DOCUMENT_TOKENS: usize = 2048;
pub const MAX_QUERY_TOKENS: usize = 256;

/// Layers shared by both tops before the branch point.
pub const SHARED_LAYERS: usize = 18;

/// Which side of a retrieval a text is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Document,
    Query,
}

impl Role {
    pub fn marker_token_id(self) -> u32 {
        match self {
            Role::Document => DOCUMENT_TOKEN_ID,
            Role::Query => QUERY_TOKEN_ID,
        }
    }

    pub fn max_tokens(self) -> usize {
        match self {
            Role::Document => MAX_DOCUMENT_TOKENS,
            Role::Query => MAX_QUERY_TOKENS,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Document => "document",
            Role::Query => "query",
        }
    }
}

/// One embedded text.
#[derive(Debug, Clone)]
pub struct Embedding {
    /// Number of tokens the tokenizer produced, including the two special tokens.
    pub tokens: usize,
    /// Whether the input was cut at the model's limit.
    pub truncated: bool,
    /// One 128-dimensional unit vector per surviving position. The first three are slots.
    pub vectors: Vec<Vec<f32>>,
    /// The pooled 768-dimensional sentence vector, unit norm.
    pub dense: Vec<f32>,
}

/// The loaded model.
pub struct Engine {
    trunk: bert::Trunk,
    token_top: bert::Top,
    dense_top: bert::Top,
    head: head::Head,
    tokenizer: tokenizer::TextTokenizer,
    config: bert::Config,
    device: Device,
    half_window: usize,
}

impl Engine {
    /// Load the model from the three safetensors files and the tokenizer.
    pub fn load(files: &ModelFiles, device: Device) -> Result<Engine> {
        let config: bert::Config =
            serde_json::from_slice(&std::fs::read(&files.config).context("read config.json")?)
                .context("parse config.json")?;

        let head_config: head::HeadConfig = serde_json::from_slice(
            &std::fs::read(&files.head_config).context("read 1_HybridTokens/config.json")?,
        )
        .context("parse 1_HybridTokens/config.json")?;

        let dtype = DType::F32;

        // The three files are separate namespaces, so each gets its own VarBuilder.
        // Doing this with one builder would require rewriting every key.
        let vb_main = unsafe {
            VarBuilder::from_mmaped_safetensors(&[&files.main], dtype, &device)
                .context("load model.safetensors")?
        };
        let vb_dense = unsafe {
            VarBuilder::from_mmaped_safetensors(&[&files.dense_branch], dtype, &device)
                .context("load dense_branch.safetensors")?
        };
        let vb_head = unsafe {
            VarBuilder::from_mmaped_safetensors(&[&files.head], dtype, &device)
                .context("load 1_HybridTokens/model.safetensors")?
        };

        let (global_rotary, local_rotary) = bert::rotary_tables(dtype, &config, &device)?;

        let trunk = bert::Trunk::load(
            vb_main.clone(),
            &config,
            SHARED_LAYERS,
            global_rotary.clone(),
            local_rotary.clone(),
        )
        .context("load trunk")?;

        let remaining = config.num_hidden_layers - SHARED_LAYERS;
        let token_top = bert::Top::load(
            vb_main.clone(),
            &config,
            "layers",
            SHARED_LAYERS,
            SHARED_LAYERS,
            remaining,
            "final_norm",
            global_rotary.clone(),
            local_rotary.clone(),
        )
        .context("load token top")?;

        let dense_top = bert::Top::load(
            vb_dense.clone(),
            &config,
            "dense_layers",
            0,
            SHARED_LAYERS,
            remaining,
            "dense_norm",
            global_rotary.clone(),
            local_rotary.clone(),
        )
        .context("load dense top")?;

        let head = head::Head::load(vb_head, head_config, &config).context("load head")?;

        let tokenizer =
            tokenizer::TextTokenizer::load(&files.tokenizer, &files.sentence_transformers_config)
                .context("load tokenizer")?;

        let half_window = config.local_attention / 2;

        Ok(Engine {
            trunk,
            token_top,
            dense_top,
            head,
            tokenizer,
            config,
            device,
            half_window,
        })
    }

    pub fn device_name(&self) -> &'static str {
        match self.device {
            Device::Cpu => "cpu",
            Device::Cuda(_) => "cuda",
            _ => "other",
        }
    }

    pub fn token_dim(&self) -> usize {
        self.head.config.token_dim
    }

    pub fn dense_dim(&self) -> usize {
        self.head.config.dim
    }

    pub fn slots(&self) -> usize {
        self.head.config.slots
    }

    pub fn query_local_cap(&self) -> usize {
        self.head.config.query_local_cap
    }

    /// How many tokens a text of this role can carry, including the special tokens.
    pub fn max_tokens(&self, role: Role) -> usize {
        role.max_tokens()
    }

    /// Embed one text in one role.
    pub fn embed(&self, text: &str, role: Role) -> Result<Embedding> {
        let encoded = self.tokenizer.encode(text, role)?;

        let seq_len = encoded.ids.len();
        let ids = Tensor::from_slice(&encoded.ids, (1, seq_len), &self.device)?;

        let local_mask = bert::sliding_window_mask(seq_len, self.half_window, &self.device)?;

        let trunk = self.trunk.forward(&ids, Some(&local_mask))?;
        let token_states = self.token_top.forward(&trunk, Some(&local_mask))?;
        let dense_states = self.dense_top.forward(&trunk, Some(&local_mask))?;

        let matrix = self.head.forward(&token_states, &dense_states)?;
        let matrix: Vec<Vec<f32>> = matrix.to_vec2()?;

        // Keep the positions that survive: the query cap zeroes everything past position
        // 63, and documents additionally drop punctuation tokens.
        let keep = self.keep_positions(&encoded.ids, role);
        let vectors: Vec<Vec<f32>> = keep.iter().map(|index| matrix[*index].clone()).collect();

        let dense = self.head.pooled_dense(&dense_states)?.to_vec1()?;

        Ok(Embedding {
            tokens: seq_len,
            truncated: encoded.truncated,
            vectors,
            dense,
        })
    }

    /// Which token positions survive into the output.
    ///
    /// Two filters apply, and they are not symmetric. The query cap applies only to
    /// queries, and the punctuation skip list only to documents. This mirrors PyLate, and
    /// the golden fixtures record the resulting positions so the two agree exactly.
    fn keep_positions(&self, ids: &[u32], role: Role) -> Vec<usize> {
        let cap = match role {
            Role::Query => self.head.config.query_local_cap,
            Role::Document => usize::MAX,
        };
        ids.iter()
            .enumerate()
            .filter(|(index, _)| *index < cap)
            .filter(|(_, token_id)| role == Role::Query || !self.tokenizer.is_skipped(**token_id))
            .map(|(index, _)| index)
            .collect()
    }

    /// Token ids that documents drop before the vectors are returned.
    pub fn skiplist(&self) -> &[u32] {
        self.tokenizer.skiplist()
    }

    /// Tokenize a text and report which positions survive, without running the model.
    ///
    /// Exposed so that the parity test can assert the token ids and the surviving
    /// positions against the oracle separately from the numbers.
    pub fn debug_encode(&self, text: &str, role: Role) -> Result<(Vec<u32>, Vec<usize>, bool)> {
        let encoded = self.tokenizer.encode(text, role)?;
        let keep = self.keep_positions(&encoded.ids, role);
        Ok((encoded.ids, keep, encoded.truncated))
    }

    /// A description of the loaded model, for `/v1/models`.
    pub fn info(&self) -> ModelInfo {
        ModelInfo {
            model: MODEL_NAME.to_string(),
            version: crate::MODEL_VERSION,
            dim: self.token_dim(),
            dense_dim: self.dense_dim(),
            max_doc_tokens: MAX_DOCUMENT_TOKENS,
            max_query_tokens: MAX_QUERY_TOKENS,
            query_local_cap: self.query_local_cap(),
            device: self.device_name().to_string(),
            revision: MODEL_REVISION.to_string(),
            vocab_size: self.config.vocab_size,
        }
    }
}

/// What `/v1/models` reports.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelInfo {
    pub model: String,
    pub version: i64,
    pub dim: usize,
    pub dense_dim: usize,
    pub max_doc_tokens: usize,
    pub max_query_tokens: usize,
    pub query_local_cap: usize,
    pub device: String,
    pub revision: String,
    pub vocab_size: usize,
}

/// Shared so that the HTTP layer and the loader agree on one instance.
pub type SharedEngine = Arc<Engine>;
