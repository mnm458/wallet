use documented::Documented;
use jsonrpsee::core::RpcResult;
use rusqlite::named_params;
use schemars::JsonSchema;
use serde::Serialize;
use zcash_client_backend::data_api::WalletRead;
use zcash_client_sqlite::error::SqliteClientError;
use zcash_encoding::ReverseHex;
use zcash_primitives::block::BlockHash;

use super::list_transactions::{self, WalletTx};
use crate::components::{database::DbConnection, json_rpc::server::LegacyCode};

/// Response to a `listsinceblock` RPC request.
pub(crate) type Response = RpcResult<ResultType>;

/// The wallet's transactions since a given block, plus a cursor for the next poll.
#[derive(Clone, Debug, Serialize, Documented, JsonSchema)]
pub(crate) struct ResultType {
    /// Transactions affecting the wallet since (but not including) the block named by
    /// `blockhash`, or all wallet transactions if no `blockhash` was given.
    ///
    /// Entries use the same schema as `z_listtransactions`.
    transactions: Vec<WalletTx>,
    /// The hash of the block `target_confirmations - 1` back from the wallet's
    /// fully-scanned height.
    ///
    /// Passing this as the next call's `blockhash` reports every transaction that had
    /// fewer than `target_confirmations` confirmations at the time of this call at
    /// least once more, so a poller that treats `target_confirmations` as its finality
    /// depth never misses a transaction across calls. All-zeroes if the wallet has not
    /// fully scanned any blocks, or if the depth reaches past its scanned history.
    lastblock: String,
}

pub(super) const PARAM_BLOCKHASH_DESC: &str =
    "The hash of a block on the wallet's chain; only transactions after it are listed.";
pub(super) const PARAM_TARGET_CONFIRMATIONS_DESC: &str =
    "Selects the returned `lastblock` cursor as the block at this confirmation depth.";

/// Looks up the height of `hash` among the blocks the wallet has scanned.
///
/// The wallet's `blocks` table only ever describes the best chain the wallet is synced
/// to: rewinds delete rows for unwound blocks. A hash the wallet has never scanned and a
/// hash that was reorged away are therefore indistinguishable here, and both report
/// `None`.
fn scanned_block_height(
    conn: &rusqlite::Connection,
    hash: &BlockHash,
) -> Result<Option<u32>, rusqlite::Error> {
    conn.query_row(
        "SELECT height FROM blocks WHERE hash = :hash",
        named_params! {":hash": hash.0.as_slice()},
        |row| row.get(0),
    )
    .map(Some)
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(None),
        e => Err(e),
    })
}

pub(crate) async fn call(
    wallet: &DbConnection,
    blockhash: Option<String>,
    target_confirmations: Option<u32>,
) -> Response {
    let requested_block = blockhash
        .map(|s| {
            ReverseHex::decode(&s)
                .map(BlockHash)
                .ok_or_else(|| LegacyCode::InvalidParameter.with_static("invalid blockhash"))
        })
        .transpose()?;

    let target_confirmations = target_confirmations.unwrap_or(1);
    if target_confirmations < 1 {
        return Err(
            LegacyCode::InvalidParameter.with_static("target_confirmations must be at least 1")
        );
    }

    // The cursor for the caller's next poll: the block `target_confirmations - 1` back
    // from the wallet's fully-scanned height, so that (as in `zcashd`) every transaction
    // still shy of `target_confirmations` at this call is reported again by the next
    // one. The cursor is anchored to the fully-scanned height rather than the node tip:
    // the listing below is only complete up to the height the wallet has fully scanned,
    // and a node-tip cursor would let a poller on a lagging wallet skip past blocks
    // whose transactions the wallet had not yet recorded when the cursor was issued.
    let lastblock = wallet
        .with(|db| -> Result<BlockHash, SqliteClientError> {
            let cursor = db
                .block_fully_scanned()?
                .and_then(|fully_scanned| {
                    u32::from(fully_scanned.block_height()).checked_sub(target_confirmations - 1)
                })
                .map(|height| db.block_metadata(height.into()))
                .transpose()?
                .flatten();

            // An unscanned wallet, or a depth reaching past the wallet's scanned
            // history, yields the all-zeroes hash (as `zcashd` did for a depth past the
            // start of the chain). It is not a usable cursor; the caller falls back to
            // an uncursored listing.
            Ok(cursor
                .map(|metadata| metadata.block_hash())
                .unwrap_or(BlockHash([0; 32])))
        })
        .map_err(|e| LegacyCode::Database.with_message(e.to_string()))?
        .to_string();

    wallet.with_raw_mut(|conn, _| {
        let db_tx = conn
            .transaction()
            .map_err(|e| LegacyCode::Database.with_message(format!("{e}")))?;

        // Transactions are listed strictly after the named block. The exclusive lower
        // bound also makes the `lastblock` round trip safe on its own terms: the cursor
        // block's transactions were already reported by the call that returned it.
        let start_height = requested_block
            .map(|hash| {
                scanned_block_height(&db_tx, &hash)
                    .map_err(|e| LegacyCode::Database.with_message(format!("{e}")))?
                    .map(|height| height + 1)
                    // Mirrors `zcashd`'s "Block not found". Unlike `zcashd`, a block that
                    // was reorged away is also reported this way (the wallet does not
                    // retain unwound blocks to walk back to a fork point); callers
                    // recover by retrying with a cursor obtained at a safer
                    // `target_confirmations` depth.
                    .ok_or_else(|| LegacyCode::InvalidAddressOrKey.with_static("Block not found"))
            })
            .transpose()?;

        Ok(ResultType {
            transactions: list_transactions::query_transactions(
                &db_tx,
                None,
                start_height,
                None,
                None,
                None,
            )
            .map_err(|e| LegacyCode::Database.with_message(format!("{e}")))?,
            lastblock,
        })
    })
}

#[cfg(test)]
mod tests {
    use zcash_primitives::block::BlockHash;

    use super::scanned_block_height;

    fn wallet_with_blocks(blocks: &[(u32, BlockHash)]) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().expect("in-memory db opens");
        conn.execute_batch("CREATE TABLE blocks (height INTEGER PRIMARY KEY, hash BLOB NOT NULL)")
            .expect("schema applies");
        for (height, hash) in blocks {
            conn.execute(
                "INSERT INTO blocks (height, hash) VALUES (?, ?)",
                rusqlite::params![height, hash.0.as_slice()],
            )
            .expect("insert succeeds");
        }
        conn
    }

    #[test]
    fn scanned_block_resolves_to_its_height() {
        let hash = BlockHash([7; 32]);
        let conn = wallet_with_blocks(&[(100, BlockHash([1; 32])), (101, hash)]);

        assert_eq!(
            scanned_block_height(&conn, &hash).expect("query succeeds"),
            Some(101),
        );
    }

    #[test]
    fn unscanned_block_resolves_to_none() {
        let conn = wallet_with_blocks(&[(100, BlockHash([1; 32]))]);

        assert_eq!(
            scanned_block_height(&conn, &BlockHash([2; 32])).expect("query succeeds"),
            None,
        );
    }
}
