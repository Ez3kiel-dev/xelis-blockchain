mod mempool_state;
mod chain_state;

pub use mempool_state::MempoolState;
pub use chain_state::{
    ChainState,
    ApplicableChainState,
    StorageReference
};

use log::{trace, debug};
use xelis_common::{
    account::VersionedBalance,
    crypto::{Hash, PublicKey},
    transaction::{Reference, Transaction},
    block::BlockVersion,
    utils::format_xelis
};
use super::{
    blockchain,
    error::BlockchainError,
    storage::{AccountProvider, BalanceProvider, DagOrderProvider}
};
use crate::config::PRUNE_SAFETY_LIMIT;

// Verify a transaction before adding it to mempool/chain state
// We only verify the reference and the required fees
pub (super) async fn pre_verify_tx<P: AccountProvider + BalanceProvider>(provider: &P, tx: &Transaction, stable_topoheight: u64, topoheight: u64, block_version: BlockVersion) -> Result<(), BlockchainError> {
    if tx.get_version() != 0 {
        debug!("Invalid version: {}", tx.get_version());
        return Err(BlockchainError::InvalidTxVersion);
    }

    let required_fees = blockchain::estimate_required_tx_fees(provider, topoheight, tx, block_version).await?;
    if required_fees > tx.get_fee() {
        debug!("Invalid fees: {} required, {} provided", format_xelis(required_fees), format_xelis(tx.get_fee()));
        return Err(BlockchainError::InvalidTxFee(required_fees, tx.get_fee()));
    }

    let reference = tx.get_reference();
    // Verify that it is not a fake topoheight
    if topoheight < reference.topoheight {
        debug!("Invalid reference: topoheight {} is higher than chain {}", reference.topoheight, topoheight);
        return Err(BlockchainError::InvalidReferenceTopoheight);
    }

    // Reference is in stable topoheight
    // for fast synced nodes, we must ensure that the TX is using the latest version available
    // Problem is, this give a way to front running attacks as the TX would be forced to use the latest stable version
    // and this give a short lifetime to the TX. What we can do to increase the lifetime of the tx,
    // is to check that the reference is not too old
    // So, stable topoheight = 1000, topoheight = 1010, reference = 990
    // Alice has a balance at 800 and 950 respectively. We can't use the balance at 800 because it is too old
    // Reference at 990 is valid, but if it was 800, we must reject the TX
    // TX lifetime is PRUNE_SAFETY_LIMIT + STABLE_LIMIT, which is equal to (at least) 88 blocks.
    if block_version != BlockVersion::V0 && stable_topoheight >= PRUNE_SAFETY_LIMIT {
        let safety_stable_topoheight = stable_topoheight - PRUNE_SAFETY_LIMIT;
        if reference.topoheight <= safety_stable_topoheight {
            for asset in tx.get_assets() {
                if let Some((topo, _)) = provider.get_usable_balance_at_maximum_topoheight(tx.get_source(), asset, safety_stable_topoheight, topoheight).await? {
                    // We have a more recent version than the referenced one
                    if topo > reference.topoheight {
                        debug!("Invalid reference: last stable balance for asset {} is at topoheight {} (stable topoheight is {}) while reference is {}", asset, topo, stable_topoheight, reference.topoheight);
                        return Err(BlockchainError::InvalidReferenceTopoheight);
                    }
                } else {
                    debug!("Invalid reference: no usable balance found for asset {} at stable topoheight {}", asset, stable_topoheight);
                    return Err(BlockchainError::NoStableReferenceFound);
                }
            }
        }
    }

    Ok(())
}

// Create a sender echange
// This is where the magic happens to fix front running problems
// Returns:
// - If we should use the output balance for verification
// - is it a new version created
// - Versioned Balance to use for verification
pub (super) async fn search_versioned_balance_for_reference<S: DagOrderProvider + BalanceProvider>(storage: &S, key: &PublicKey, asset: &Hash, current_topoheight: u64, reference: &Reference) -> Result<(bool, bool, VersionedBalance), BlockchainError> {
    trace!("search versioned balance for {} at topoheight {}, reference: {}", key.as_address(storage.is_mainnet()), current_topoheight, reference.topoheight);
    // Scenario A
    // TX A has reference topo 1000
    // We are at block topo 1001
    // Because TX A is based on previous block, it is built on final balance

    // Scenario B
    // TX A has reference topo 1000
    // We are at block topo 1005
    // We got some funds in topo 1003
    // We must use the final balance of 1000

    // Scenario C
    // TX A has reference topo 1000
    // We are at block topo 1005
    // We sent another TX B at topo 1001
    // We must use the output balance if available of TX B

    // Scenario D
    // TXs have reference topo 1000
    // We are at block topo 1005
    // We sent another TX B at topo 1003
    // We sent another TX C at topo 1004
    // We must use the output balance if available

    // Retrieve the block topoheight based on reference hash
    let reference_block_topo = if storage.is_block_topological_ordered(&reference.hash).await {
        let topo = storage.get_topo_height_for_hash(&reference.hash).await?;
        if topo == reference.topoheight {
            topo
        } else if reference.topoheight < current_topoheight {
            reference.topoheight
        } else {
            current_topoheight
        }
    } else {
        current_topoheight
    };

    let mut use_output_balance = false;
    let version;
    // We must verify the last "output" balance for the asset
    // Search the last output balance
    let last_output = storage.get_output_balance_at_maximum_topoheight(key, asset, current_topoheight).await?;
    // We have a output balance
    if let Some((topo, v)) = last_output {
        trace!("Found output balance at topoheight {}", topo);
        // Verify if the output balance topo is higher than our reference
        if reference.topoheight < topo || reference_block_topo < topo {
            debug!("Scenario C");
            // We must use the output balance if possible because this TX may be built after a previous TX at same reference
            // see Scenario C
            use_output_balance = true;
            version = Some(v);
        } else if topo < reference.topoheight || topo < reference_block_topo {
            trace!("Reference is above last output balance");
            debug!("Scenario B");

            version = storage.get_balance_at_maximum_topoheight(key, asset, topo.max(reference_block_topo)).await?
                .map(|(_, v)| v);
        } else {
            debug!("Scenario A (bis)");
            version = Some(v);
        }
    } else {
        trace!("No output balance found (Scenario B)");
        version = storage.get_balance_at_maximum_topoheight(key, asset, reference_block_topo).await?
            .map(|(_, v)| v);
    }

    let (new_version, version) = if let Some(version) = version {
        (false, version)
    } else {
        // Scenario A
        debug!("Scenario A");
        (true, storage.get_new_versioned_balance(key, asset, current_topoheight).await?)
    };

    Ok((use_output_balance, new_version,  version))
}