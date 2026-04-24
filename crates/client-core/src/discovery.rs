//! Chain selection — pick the best chain from client configuration.

use mirage_core::config::{ChainConfig, ClientConfig};

/// Select the highest-priority chain from the client configuration.
///
/// Priority is represented as a `u8` where a **lower** value means lower
/// priority (0 = lowest).  We return the chain with the smallest `priority`
/// value that is still the most preferred (i.e. `min_by_key` gives us the
/// one with the smallest number, so we actually want `max_by_key`).
///
/// The config comment says "Higher priority chains are preferred (0 = lowest)",
/// so we pick the chain with the **maximum** `priority` value.
///
/// Returns `None` if there are no chains configured.
pub fn select_chain(config: &ClientConfig) -> Option<&ChainConfig> {
    config.chains.iter().max_by_key(|c| c.priority)
}

/// Return all chains sorted by descending priority (highest-priority first).
///
/// Used by the multi-pool startup path to build one [`TunnelPool`] per chain
/// and to drive cross-chain failover ordering.
///
/// [`TunnelPool`]: crate::pool::TunnelPool
pub fn select_all_chains(config: &ClientConfig) -> Vec<&ChainConfig> {
    let mut chains: Vec<&ChainConfig> = config.chains.iter().collect();
    chains.sort_by(|a, b| b.priority.cmp(&a.priority));
    chains
}
