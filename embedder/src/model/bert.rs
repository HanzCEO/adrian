//! ModernBERT, restructured for the branched trunk that `uibk-embedding-v1` needs.
//!
//! Vendored from `candle-transformers/src/models/modernbert.rs` at candle 0.11.0, which
//! is dual licensed MIT / Apache-2.0. The upstream attribution is preserved below.
//!
//! Three changes were necessary, and nothing else was touched:
//!
//! 1. The upstream `ModernBert` keeps all 22 layers private and always runs every one of
//!    them. This model splits the trunk after layer 18: the token head and the dense head
//!    each get their own copy of layers 18..22, so the layers have to be separable.
//! 2. `global_rope_theta` and `local_rope_theta` are `null` in the model's `config.json`.
//!    Upstream deserialises them as `f64` and fails; here they are `Option<f64>` with the
//!    ModernBERT defaults.
//! 3. Inputs are a single unpadded sequence, so the global attention mask is unnecessary
//!    and only the sliding-window mask is built.
//!
//! Everything else, including the sliding-window convention (`|i - j| <= local_attention / 2`,
//! which matches Hugging Face's `sliding_window_bidirectional_overlay`), is upstream code.
//!
//! ---
//! Upstream header:
//!
//! ModernBERT is a modernized bidirectional encoder-only Transformer model.
//! - [Arxiv](https://arxiv.org/abs/2412.13663)
//! - Upstream [GitHub repo](https://github.com/AnswerDotAI/ModernBERT).

use candle::{DType, Device, IndexOp, Result, Tensor, D};
use candle_nn::{
    embedding, layer_norm_no_bias, linear_no_bias, ops::softmax, Embedding, LayerNorm, Linear,
    Module, VarBuilder,
};
use serde::Deserialize;
use std::sync::Arc;

/// The model's `config.json`, with the two rope thetas made optional because they are
/// written as `null`.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub vocab_size: usize,
    pub hidden_size: usize,
    pub num_hidden_layers: usize,
    pub num_attention_heads: usize,
    pub intermediate_size: usize,
    pub max_position_embeddings: usize,
    pub layer_norm_eps: f64,
    pub global_attn_every_n_layers: usize,
    pub local_attention: usize,
    #[serde(default)]
    pub global_rope_theta: Option<f64>,
    #[serde(default)]
    pub local_rope_theta: Option<f64>,
}

impl Config {
    pub fn global_rope_theta(&self) -> f64 {
        self.global_rope_theta.unwrap_or(160_000.0)
    }

    pub fn local_rope_theta(&self) -> f64 {
        self.local_rope_theta.unwrap_or(10_000.0)
    }

    /// Layers whose index is a multiple of `global_attn_every_n_layers` use full attention;
    /// the rest use a sliding window.
    pub fn uses_local_attention(&self, layer_index: usize) -> bool {
        layer_index % self.global_attn_every_n_layers != 0
    }

    pub fn head_dim(&self) -> usize {
        self.hidden_size / self.num_attention_heads
    }
}

/// The precomputed rotary tables for one attention flavour.
///
/// Public because [`Trunk::load`] and [`Top::load`] take one of each: the model uses a
/// different theta for full attention than for sliding-window attention.
#[derive(Debug, Clone)]
pub struct RotaryEmbedding {
    sin: Tensor,
    cos: Tensor,
}

impl RotaryEmbedding {
    fn new(dtype: DType, config: &Config, rope_theta: f64, dev: &Device) -> Result<Self> {
        let dim = config.head_dim();
        let inv_freq: Vec<_> = (0..dim)
            .step_by(2)
            .map(|i| 1f32 / rope_theta.powf(i as f64 / dim as f64) as f32)
            .collect();
        let inv_freq_len = inv_freq.len();
        let inv_freq = Tensor::from_vec(inv_freq, (1, inv_freq_len), dev)?.to_dtype(dtype)?;
        let max_seq_len = config.max_position_embeddings;
        let t = Tensor::arange(0u32, max_seq_len as u32, dev)?
            .to_dtype(dtype)?
            .reshape((max_seq_len, 1))?;
        let freqs = t.matmul(&inv_freq)?;
        Ok(Self {
            sin: freqs.sin()?,
            cos: freqs.cos()?,
        })
    }

    fn apply_rotary_emb_qkv(&self, q: &Tensor, k: &Tensor) -> Result<(Tensor, Tensor)> {
        let q_embed = candle_nn::rotary_emb::rope(&q.contiguous()?, &self.cos, &self.sin)?;
        let k_embed = candle_nn::rotary_emb::rope(&k.contiguous()?, &self.cos, &self.sin)?;
        Ok((q_embed, k_embed))
    }
}

#[derive(Clone)]
struct Attention {
    qkv: Linear,
    proj: Linear,
    num_attention_heads: usize,
    attention_head_size: usize,
    rotary_emb: Arc<RotaryEmbedding>,
}

impl Attention {
    fn load(vb: VarBuilder, config: &Config, rotary_emb: Arc<RotaryEmbedding>) -> Result<Self> {
        let num_attention_heads = config.num_attention_heads;
        let attention_head_size = config.head_dim();
        let qkv = linear_no_bias(config.hidden_size, config.hidden_size * 3, vb.pp("Wqkv"))?;
        let proj = linear_no_bias(config.hidden_size, config.hidden_size, vb.pp("Wo"))?;
        Ok(Self {
            qkv,
            proj,
            num_attention_heads,
            attention_head_size,
            rotary_emb,
        })
    }

    fn forward(&self, hidden_states: &Tensor, attention_mask: Option<&Tensor>) -> Result<Tensor> {
        let xs = hidden_states.clone();
        let (b, seq_len, d) = xs.dims3()?;
        let qkv = xs
            .apply(&self.qkv)?
            .reshape((
                b,
                seq_len,
                3,
                self.num_attention_heads,
                self.attention_head_size,
            ))?
            .permute((2, 0, 3, 1, 4))?;

        let q = qkv.get(0)?;
        let k = qkv.get(1)?;
        let v = qkv.get(2)?;

        let (q, k) = self.rotary_emb.apply_rotary_emb_qkv(&q, &k)?;

        let scale = (self.attention_head_size as f64).powf(-0.5);
        let q = (q * scale)?;

        let att = q.matmul(&k.transpose(D::Minus2, D::Minus1)?)?;

        let att = match attention_mask {
            Some(mask) => att.broadcast_add(mask)?,
            None => att,
        };
        let att = softmax(&att, D::Minus1)?;

        let xs = att.matmul(&v)?;
        let xs = xs.transpose(1, 2)?.reshape((b, seq_len, d))?;
        let xs = xs.apply(&self.proj)?;
        Ok(xs)
    }
}

/// ModernBERT's feed-forward: a gated linear unit with the gate applied to the second half.
///
/// Note that this is `gelu(first) * second`, which is the *opposite* order from the
/// latent-attention block's GEGLU in [`super::head`]. Getting them the same way round
/// produces a model that runs and returns wrong numbers.
#[derive(Clone)]
struct Mlp {
    wi: Linear,
    wo: Linear,
}

impl Mlp {
    fn load(vb: VarBuilder, config: &Config) -> Result<Self> {
        let wi = linear_no_bias(
            config.hidden_size,
            config.intermediate_size * 2,
            vb.pp("Wi"),
        )?;
        let wo = linear_no_bias(config.intermediate_size, config.hidden_size, vb.pp("Wo"))?;
        Ok(Self { wi, wo })
    }
}

impl Module for Mlp {
    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        let xs = xs.apply(&self.wi)?;
        let xs = xs.chunk(2, D::Minus1)?;
        let xs = (xs[0].gelu_erf()? * &xs[1])?.apply(&self.wo)?;
        Ok(xs)
    }
}

/// One transformer layer. Layer 0 of ModernBERT has no pre-attention norm.
#[derive(Clone)]
pub struct Layer {
    attn: Attention,
    mlp: Mlp,
    attn_norm: Option<LayerNorm>,
    mlp_norm: LayerNorm,
    uses_local_attention: bool,
}

impl Layer {
    fn load(
        vb: VarBuilder,
        config: &Config,
        rotary_emb: Arc<RotaryEmbedding>,
        uses_local_attention: bool,
    ) -> Result<Self> {
        let attn = Attention::load(vb.pp("attn"), config, rotary_emb)?;
        let mlp = Mlp::load(vb.pp("mlp"), config)?;
        let attn_norm = layer_norm_no_bias(
            config.hidden_size,
            config.layer_norm_eps,
            vb.pp("attn_norm"),
        )
        .ok();
        let mlp_norm =
            layer_norm_no_bias(config.hidden_size, config.layer_norm_eps, vb.pp("mlp_norm"))?;
        Ok(Self {
            attn,
            mlp,
            attn_norm,
            mlp_norm,
            uses_local_attention,
        })
    }

    fn forward(&self, xs: &Tensor, local_mask: Option<&Tensor>) -> Result<Tensor> {
        let residual = xs.clone();
        let mut xs = xs.clone();
        if let Some(norm) = &self.attn_norm {
            xs = xs.apply(norm)?;
        }
        let mask = if self.uses_local_attention {
            local_mask
        } else {
            None
        };
        let xs = self.attn.forward(&xs, mask)?;
        let xs = (xs + residual)?;
        let mlp_out = xs.apply(&self.mlp_norm)?.apply(&self.mlp)?;
        let xs = (xs + mlp_out)?;
        Ok(xs)
    }
}

/// Build the additive sliding-window mask, matching Hugging Face's
/// `abs(q_idx - kv_idx) <= sliding_window` convention where the window is half the
/// configured local attention size.
pub fn sliding_window_mask(seq_len: usize, half_window: usize, device: &Device) -> Result<Tensor> {
    let mask: Vec<f32> = (0..seq_len)
        .flat_map(|i| {
            (0..seq_len).map(move |j| {
                if (j as i64 - i as i64).abs() > half_window as i64 {
                    f32::NEG_INFINITY
                } else {
                    0.
                }
            })
        })
        .collect();
    Tensor::from_slice(&mask, (1, 1, seq_len, seq_len), device)
}

/// The shared part of the trunk: embeddings plus layers `0..shared_layers`.
#[derive(Clone)]
pub struct Trunk {
    word_embeddings: Embedding,
    norm: LayerNorm,
    layers: Vec<Layer>,
}

impl Trunk {
    pub fn load(
        vb: VarBuilder,
        config: &Config,
        shared_layers: usize,
        global_rotary: Arc<RotaryEmbedding>,
        local_rotary: Arc<RotaryEmbedding>,
    ) -> Result<Self> {
        let word_embeddings = embedding(
            config.vocab_size,
            config.hidden_size,
            vb.pp("embeddings.tok_embeddings"),
        )?;
        let norm = layer_norm_no_bias(
            config.hidden_size,
            config.layer_norm_eps,
            vb.pp("embeddings.norm"),
        )?;

        let mut layers = Vec::with_capacity(shared_layers);
        for index in 0..shared_layers {
            let local = config.uses_local_attention(index);
            layers.push(Layer::load(
                vb.pp(format!("layers.{index}")),
                config,
                if local {
                    local_rotary.clone()
                } else {
                    global_rotary.clone()
                },
                local,
            )?);
        }

        Ok(Self {
            word_embeddings,
            norm,
            layers,
        })
    }

    /// Run the shared trunk over a single unpadded sequence.
    pub fn forward(&self, input_ids: &Tensor, local_mask: Option<&Tensor>) -> Result<Tensor> {
        let mut xs = input_ids.apply(&self.word_embeddings)?.apply(&self.norm)?;
        for layer in &self.layers {
            xs = layer.forward(&xs, local_mask)?;
        }
        Ok(xs)
    }
}

/// One of the two tops: layers `from..from + count`, followed by its own final norm.
///
/// The layer indices passed to the config stay absolute, because which layers use a
/// sliding window depends on the original position, not the position within the top.
#[derive(Clone)]
pub struct Top {
    layers: Vec<Layer>,
    norm: LayerNorm,
}

impl Top {
    /// Load a top.
    ///
    /// `name_from` is the first index to use in the weight names, and `position_from` is
    /// the first index to use when deciding which layers use a sliding window. They differ
    /// for the two tops: the token top is `layers.18..22` (both are 18), while the dense
    /// top is `dense_layers.0..4` but occupies positions 18..22 in the network, so its
    /// attention pattern is the one of layers 18 onwards.
    #[allow(clippy::too_many_arguments)]
    pub fn load(
        vb: VarBuilder,
        config: &Config,
        name_prefix: &str,
        name_from: usize,
        position_from: usize,
        count: usize,
        norm_name: &str,
        global_rotary: Arc<RotaryEmbedding>,
        local_rotary: Arc<RotaryEmbedding>,
    ) -> Result<Self> {
        let mut layers = Vec::with_capacity(count);
        for offset in 0..count {
            let local = config.uses_local_attention(position_from + offset);
            layers.push(Layer::load(
                vb.pp(format!("{name_prefix}.{}", name_from + offset)),
                config,
                if local {
                    local_rotary.clone()
                } else {
                    global_rotary.clone()
                },
                local,
            )?);
        }
        let norm = layer_norm_no_bias(config.hidden_size, config.layer_norm_eps, vb.pp(norm_name))?;
        Ok(Self { layers, norm })
    }

    pub fn forward(&self, xs: &Tensor, local_mask: Option<&Tensor>) -> Result<Tensor> {
        let mut xs = xs.clone();
        for layer in &self.layers {
            xs = layer.forward(&xs, local_mask)?;
        }
        xs.apply(&self.norm)
    }
}

/// Construct the pair of rotary tables once and share them across every layer.
pub fn rotary_tables(
    dtype: DType,
    config: &Config,
    device: &Device,
) -> Result<(Arc<RotaryEmbedding>, Arc<RotaryEmbedding>)> {
    let global = Arc::new(RotaryEmbedding::new(
        dtype,
        config,
        config.global_rope_theta(),
        device,
    )?);
    let local = Arc::new(RotaryEmbedding::new(
        dtype,
        config,
        config.local_rope_theta(),
        device,
    )?);
    Ok((global, local))
}

/// Kept so that the unused-import lint does not fire on `IndexOp`, which the vendored
/// attention code needs for `qkv.get(n)`.
#[allow(dead_code)]
fn _index_op_is_used(t: &Tensor) -> Result<Tensor> {
    t.i(0)
}
