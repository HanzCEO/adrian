//! `adrian-embedder`: uibk-embedding-v1 inference, implemented with candle.
//!
//! The model is a late-interaction retriever with a branched trunk. One encoder pass
//! produces a 768-dimensional pooled sentence vector and a 128-dimensional vector per
//! token, and the pooled vector is written into the first three positions of the token
//! list behind a sign tag. A plain MaxSim over that list is therefore the fixed-weight sum
//! of a ColBERT score and a dense score, which is why one index serves both.

pub mod config;
pub mod http;
pub mod model;
pub mod weights;

/// Model version, bumped when the weights change.
pub const MODEL_VERSION: i64 = 1;

/// Pick the device to run on.
pub fn select_device(cuda: bool) -> anyhow::Result<candle::Device> {
    #[cfg(feature = "cuda")]
    if cuda {
        return Ok(candle::Device::new_cuda(0)?);
    }
    let _ = cuda;
    Ok(candle::Device::Cpu)
}
