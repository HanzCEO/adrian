//! The output head: latent-attention pooling plus the hybrid token/slot construction.
//!
//! Transcribed from the model's `hybrid_tokens.py`. The two details that are easy to get
//! wrong and impossible to notice afterwards:
//!
//! * The feed-forward gate here is `x * gelu(gate)`, the *opposite* order from
//!   ModernBERT's own MLP, which is `gelu(x) * gate`.
//! * The pooled dense vector is L2 normalised *before* the rotation, and each 127-wide
//!   slice is normalised *after* it. Both normalisations matter.

use super::bert::Config;
use candle::{DType, Module, Result, Tensor};
use candle_nn::{layer_norm, linear, linear_no_bias, LayerNorm, Linear, VarBuilder};

/// Hyperparameters read from `1_HybridTokens/config.json`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HeadConfig {
    pub dim: usize,
    pub token_dim: usize,
    pub slots: usize,
    pub local_weight: f64,
    pub slot_weight: f64,
    pub num_latents: usize,
    pub num_heads: usize,
    pub ff_mult: usize,
    #[serde(default)]
    pub query_marker_id: i64,
    #[serde(default)]
    pub query_local_cap: usize,
}

impl HeadConfig {
    /// How many dimensions a slot vector carries, i.e. `token_dim - 1` for the tag.
    pub fn content_dim(&self) -> usize {
        self.token_dim - 1
    }
}

/// The gated unit used by the pooling block.
///
/// `x * gelu(gates)` where the input is split in half. This is deliberately not shared
/// with [`super::bert`], whose MLP applies the activation to the first half.
fn geglu(xs: &Tensor) -> Result<Tensor> {
    let chunks = xs.chunk(2, candle::D::Minus1)?;
    let gated = chunks[1].gelu_erf()?;
    &chunks[0] * gated
}

/// The whole output head.
#[derive(Clone)]
pub struct Head {
    /// The ColBERT projection, `dim -> token_dim`, bias free.
    token_projection: Linear,
    /// `(token_dim, token_dim - 1)`, orthonormal columns, shipped with the model.
    local_rotation: Tensor,
    /// `(dim, slots * (token_dim - 1))`, shipped with the model.
    dense_rotation: Tensor,

    latents: Tensor,
    norm_x: LayerNorm,
    norm_latents: LayerNorm,
    to_q: Linear,
    to_kv: Linear,
    to_out: Linear,
    norm_ff: LayerNorm,
    ff_in: Linear,
    ff_out: Linear,

    pub config: HeadConfig,
}

impl Head {
    pub fn load(vb: VarBuilder, head: HeadConfig, model: &Config) -> Result<Self> {
        let content = head.content_dim();
        let token_projection = linear_no_bias(head.dim, head.token_dim, vb.pp("linear"))?;
        let local_rotation = vb.get((head.token_dim, content), "local_rotation")?;
        let dense_rotation = vb.get((head.dim, head.slots * content), "dense_rotation")?;

        let latents = vb.get((head.num_latents, head.dim), "latents")?;
        let eps = model.layer_norm_eps;
        let norm_x = layer_norm(head.dim, eps, vb.pp("norm_x"))?;
        let norm_latents = layer_norm(head.dim, eps, vb.pp("norm_latents"))?;
        let to_q = linear_no_bias(head.dim, head.dim, vb.pp("to_q"))?;
        let to_kv = linear_no_bias(head.dim, head.dim * 2, vb.pp("to_kv"))?;
        let to_out = linear(head.dim, head.dim, vb.pp("to_out"))?;
        let norm_ff = layer_norm(head.dim, eps, vb.pp("norm_ff"))?;
        let ff_hidden = head.ff_mult * head.dim;
        let ff_in = linear(head.dim, ff_hidden * 2, vb.pp("ff.0"))?;
        let ff_out = linear(ff_hidden, head.dim, vb.pp("ff.2"))?;

        Ok(Self {
            token_projection,
            local_rotation,
            dense_rotation,
            latents,
            norm_x,
            norm_latents,
            to_q,
            to_kv,
            to_out,
            norm_ff,
            ff_in,
            ff_out,
            config: head,
        })
    }

    /// The latent-attention block, applied token by token.
    ///
    /// `xs` is `(seq, dim)`. The block is unmasked cross-attention from the sequence to a
    /// fixed set of learned latents, then a gated feed-forward, each with a residual.
    fn refine(&self, xs: &Tensor) -> Result<Tensor> {
        let (seq_len, dim) = xs.dims2()?;
        let heads = self.config.num_heads;
        let head_dim = dim / heads;

        let q = self
            .norm_x
            .forward(xs)?
            .apply(&self.to_q)?
            .reshape((seq_len, heads, head_dim))?
            .transpose(0, 1)?
            .unsqueeze(0)?;

        let kv = self
            .norm_latents
            .forward(&self.latents)?
            .apply(&self.to_kv)?;
        let chunks = kv.chunk(2, candle::D::Minus1)?;
        let k = chunks[0]
            .reshape((self.config.num_latents, heads, head_dim))?
            .transpose(0, 1)?
            .unsqueeze(0)?;
        let v = chunks[1]
            .reshape((self.config.num_latents, heads, head_dim))?
            .transpose(0, 1)?
            .unsqueeze(0)?;

        let scale = (head_dim as f64).powf(-0.5);
        let q = (q * scale)?;
        let att = q.matmul(&k.transpose(candle::D::Minus2, candle::D::Minus1)?)?;
        let att = candle_nn::ops::softmax_last_dim(&att)?;
        let attn = att.matmul(&v)?;

        let attended = attn
            .squeeze(0)?
            .transpose(0, 1)?
            .reshape((seq_len, dim))?
            .apply(&self.to_out)?;
        let xs = (xs + attended)?;

        let ff = xs.apply(&self.norm_ff)?.apply(&self.ff_in)?;
        let ff = geglu(&ff)?.apply(&self.ff_out)?;
        &xs + ff
    }

    /// The pooled dense sentence vector: the mean of the refined states over the real
    /// tokens, L2 normalised.
    pub fn pooled_dense(&self, dense_states: &Tensor) -> Result<Tensor> {
        // `dense_states` is (1, seq, dim) for a single unpadded sequence.
        let (_, seq_len, dim) = dense_states.dims3()?;
        let rows = dense_states.squeeze(0)?.reshape((seq_len, dim))?;

        // Refine in chunks so that a 2048-token document does not materialise a
        // (2048, 2048) attention matrix alongside everything else.
        const REFINE_ROWS: usize = 32_768;
        let refined = if seq_len <= REFINE_ROWS {
            self.refine(&rows)?
        } else {
            let mut parts = Vec::new();
            let mut start = 0usize;
            while start < seq_len {
                let end = (start + REFINE_ROWS).min(seq_len);
                let chunk = rows.narrow(0, start, end - start)?;
                parts.push(self.refine(&chunk)?);
                start = end;
            }
            Tensor::cat(&parts, 0)?
        };

        let pooled = (refined.sum(0)? / (seq_len as f64))?;
        normalize(&pooled, 0)
    }

    /// Build the output matrix: three slot vectors followed by one vector per token.
    ///
    /// `token_states` and `dense_states` are both `(1, seq, dim)`; the returned tensor is
    /// `(seq, token_dim)`.
    pub fn forward(&self, token_states: &Tensor, dense_states: &Tensor) -> Result<Tensor> {
        let (_, seq_len, dim) = token_states.dims3()?;
        let content = self.config.content_dim();
        let dtype = token_states.dtype();

        // Token path: project, rotate down to 127 dimensions, renormalise, then prepend
        // the positive tag.
        let rows = token_states.squeeze(0)?.reshape((seq_len, dim))?;
        let projected = rows.apply(&self.token_projection)?;
        let projected = normalize(&projected, 1)?;
        let rotated = projected.matmul(&self.local_rotation.to_dtype(dtype)?)?;
        let rotated = normalize(&rotated, 1)?;
        let token_scale = self.config.local_weight.sqrt();
        let token_tag = (1.0 - self.config.local_weight).sqrt();
        let token_vectors = build_tagged(&rotated, token_tag, token_scale, dtype)?;

        // Dense path: rotate the pooled vector into `slots` slices of 127, normalise each,
        // then prepend the negative tag.
        let pooled = self.pooled_dense(dense_states)?;
        // `pooled` is one vector; give it a leading batch axis so it can be rotated.
        let pooled = pooled.reshape((1, self.config.dim))?;
        let rotated_dense = pooled.matmul(&self.dense_rotation.to_dtype(DType::F32)?)?;
        let rotated_dense = rotated_dense.squeeze(0)?;
        let slices = rotated_dense.reshape((self.config.slots, content))?;
        let slices = normalize(&slices, 1)?.to_dtype(dtype)?;
        let slot_scale = self.config.slot_weight.sqrt();
        let slot_tag = -(1.0 - self.config.slot_weight).sqrt();
        let slot_vectors = build_tagged(&slices, slot_tag, slot_scale, dtype)?;

        // The slots overwrite the first `slots` positions.
        let mut out = token_vectors;
        for index in 0..self.config.slots {
            let slot = slot_vectors.get(index)?;
            out = replace_row(&out, index, &slot)?;
        }
        Ok(out)
    }
}

/// Concatenate a constant tag column with the scaled payload.
fn build_tagged(payload: &Tensor, tag: f64, scale: f64, dtype: DType) -> Result<Tensor> {
    let rows = payload.dim(0)?;
    let tag_column = Tensor::full(tag, (rows, 1), payload.device())?.to_dtype(dtype)?;
    let scaled = (payload * scale)?;
    Tensor::cat(&[tag_column, scaled], 1)
}

fn replace_row(matrix: &Tensor, index: usize, row: &Tensor) -> Result<Tensor> {
    let rows = matrix.dim(0)?;
    let width = matrix.dim(1)?;
    if rows == 1 {
        return row.reshape((1, width));
    }
    let before = if index == 0 {
        None
    } else {
        Some(matrix.narrow(0, 0, index)?)
    };
    let after_start = index + 1;
    let after = if after_start >= rows {
        None
    } else {
        Some(matrix.narrow(0, after_start, rows - after_start)?)
    };
    let row = row.reshape((1, width))?;
    let mut parts: Vec<Tensor> = Vec::new();
    if let Some(before) = before {
        parts.push(before);
    }
    parts.push(row);
    if let Some(after) = after {
        parts.push(after);
    }
    Tensor::cat(&parts, 0)
}

/// L2 normalise along one axis, guarding against a zero vector.
pub fn normalize(xs: &Tensor, dim: usize) -> Result<Tensor> {
    let norm = xs.sqr()?.sum_keepdim(dim)?.sqrt()?;
    let norm = norm.clamp(1e-12, f64::MAX)?;
    xs.broadcast_div(&norm)
}
