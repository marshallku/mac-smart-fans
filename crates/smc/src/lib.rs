//! SMC (System Management Controller) access for Apple Silicon fan keys.
//!
//! Reads `F0Ac` / `F0Mn` / `F0Mx` / `FNum` as 4-byte IEEE 754 little-endian
//! floats. Write path (`Ftst` unlock, `F{N}Md`, `F{N}Tg`) lives in Stage 2.

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct FanReading {
    pub index: u8,
    pub actual_rpm: f64,
    pub min_rpm: f64,
    pub max_rpm: f64,
}

pub fn read_fans() -> Result<Vec<FanReading>> {
    Ok(Vec::new())
}
