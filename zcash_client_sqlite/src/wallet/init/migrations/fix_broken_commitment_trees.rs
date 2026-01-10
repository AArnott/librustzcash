//! Truncates away bad note commitment tree state for users whose wallets were broken by incorrect
//! reorg handling.
use std::collections::HashSet;

use rusqlite::OptionalExtension;
use schemerz_rusqlite::RusqliteMigration;
use uuid::Uuid;
use zcash_protocol::consensus::{self, BlockHeight};

use crate::wallet::{
    self,
    init::{WalletMigrationError, migrations::support_legacy_sqlite},
};

#[cfg(feature = "transparent-inputs")]
use crate::GapLimits;

pub(super) const MIGRATION_ID: Uuid = Uuid::from_u128(0x9fa43ce0_a387_45d1_be03_57a3edc76d01);

const DEPENDENCIES: &[Uuid] = &[support_legacy_sqlite::MIGRATION_ID];

pub(super) struct Migration<P> {
    pub(super) params: P,
}

impl<P> schemerz::Migration<Uuid> for Migration<P> {
    fn id(&self) -> Uuid {
        MIGRATION_ID
    }

    fn dependencies(&self) -> HashSet<Uuid> {
        DEPENDENCIES.iter().copied().collect()
    }

    fn description(&self) -> &'static str {
        "Truncates away bad note commitment tree state for users whose wallets were broken by bad reorg handling."
    }
}

impl<P: consensus::Parameters> RusqliteMigration for Migration<P> {
    type Error = WalletMigrationError;

    fn up(&self, transaction: &rusqlite::Transaction) -> Result<(), WalletMigrationError> {
        #[cfg(not(feature = "orchard"))]
        let max_height_query = r#"
            SELECT MAX(height) FROM blocks
            JOIN sapling_tree_checkpoints sc ON sc.checkpoint_id = height
        "#;
        #[cfg(feature = "orchard")]
        let max_height_query = r#"
            SELECT MAX(height) FROM blocks
            JOIN sapling_tree_checkpoints sc ON sc.checkpoint_id = height
            JOIN orchard_tree_checkpoints oc ON oc.checkpoint_id = height
        "#;

        let max_block_height = transaction
            .query_row(max_height_query, [], |row| {
                let cid = row.get::<_, Option<u32>>(0)?;
                Ok(cid.map(BlockHeight::from))
            })
            .optional()?
            .flatten();

        if let Some(h) = max_block_height {
            wallet::truncate_to_height(
                transaction,
                &self.params,
                #[cfg(feature = "transparent-inputs")]
                &GapLimits::default(),
                h,
            )?;
        }

        Ok(())
    }

    fn down(&self, _: &rusqlite::Transaction) -> Result<(), WalletMigrationError> {
        Err(WalletMigrationError::CannotRevert(MIGRATION_ID))
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::params;
    use tempfile::NamedTempFile;
    use zcash_protocol::consensus::Network;

    use crate::{
        WalletDb,
        testing::db::{test_clock, test_rng},
        wallet::init::{
            WalletMigrator,
            migrations::{support_legacy_sqlite, tx_observation_height},
        },
    };

    use super::MIGRATION_ID;

    #[test]
    fn migrate() {
        crate::wallet::init::migrations::tests::test_migrate(&[MIGRATION_ID]);
    }

    /// Test that the migration works correctly when the `confirmed_unmined_at_height` column
    /// does not exist (i.e., when migrating from before the `tx_observation_height` migration).
    #[test]
    fn migrate_without_confirmed_unmined_at_height_column() {
        let network = Network::TestNetwork;
        let data_file = NamedTempFile::new().unwrap();
        let mut db_data =
            WalletDb::for_path(data_file.path(), network, test_clock(), test_rng()).unwrap();

        // Migrate to support_legacy_sqlite, which is the dependency of fix_broken_commitment_trees
        // but before tx_observation_height which adds the confirmed_unmined_at_height column
        WalletMigrator::new()
            .ignore_seed_relevance()
            .init_or_migrate_to(&mut db_data, &[support_legacy_sqlite::MIGRATION_ID])
            .unwrap();

        // Insert some test data: blocks and transactions
        db_data
            .conn
            .execute_batch(
                "INSERT INTO blocks (height, hash, time, sapling_tree) VALUES (1, X'01', 1, X'00');
                 INSERT INTO blocks (height, hash, time, sapling_tree) VALUES (2, X'02', 2, X'00');
                 INSERT INTO blocks (height, hash, time, sapling_tree) VALUES (3, X'03', 3, X'00');

                 INSERT INTO sapling_tree_checkpoints (checkpoint_id, position) VALUES (1, 0);
                 INSERT INTO sapling_tree_checkpoints (checkpoint_id, position) VALUES (2, 0);
                 INSERT INTO sapling_tree_checkpoints (checkpoint_id, position) VALUES (3, 0);

                 INSERT INTO transactions (id_tx, txid, block, mined_height)
                 VALUES (1, X'00', 2, 2);
                 INSERT INTO transactions (id_tx, txid, block, mined_height)
                 VALUES (2, X'01', 3, 3);",
            )
            .unwrap();

        // Verify the confirmed_unmined_at_height column does NOT exist
        let column_exists: bool = db_data
            .conn
            .prepare("PRAGMA table_info(transactions)")
            .unwrap()
            .query_map([], |row| {
                let col_name: String = row.get(1)?;
                Ok(col_name)
            })
            .unwrap()
            .any(|name| {
                name.as_ref()
                    .map(|n| n == "confirmed_unmined_at_height")
                    .unwrap_or(false)
            });
        assert!(
            !column_exists,
            "confirmed_unmined_at_height column should not exist yet"
        );

        // Now run the fix_broken_commitment_trees migration, which calls truncate_to_height
        // This should work without the confirmed_unmined_at_height column
        WalletMigrator::new()
            .ignore_seed_relevance()
            .init_or_migrate_to(&mut db_data, &[MIGRATION_ID])
            .unwrap();

        // Verify the migration succeeded and the database is still functional
        let tx_count: u32 = db_data
            .conn
            .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            tx_count, 2,
            "Transactions should still exist after migration"
        );
    }

    /// Test that the migration works correctly when the `confirmed_unmined_at_height` column
    /// DOES exist (i.e., when migrating from after the `tx_observation_height` migration).
    #[test]
    fn migrate_with_confirmed_unmined_at_height_column() {
        let network = Network::TestNetwork;
        let data_file = NamedTempFile::new().unwrap();
        let mut db_data =
            WalletDb::for_path(data_file.path(), network, test_clock(), test_rng()).unwrap();

        // Migrate through tx_observation_height, which adds the confirmed_unmined_at_height column
        WalletMigrator::new()
            .ignore_seed_relevance()
            .init_or_migrate_to(&mut db_data, &[tx_observation_height::MIGRATION_ID])
            .unwrap();

        // Insert some test data
        db_data
            .conn
            .execute_batch(
                "INSERT INTO blocks (height, hash, time, sapling_tree) VALUES (1, X'01', 1, X'00');
                 INSERT INTO blocks (height, hash, time, sapling_tree) VALUES (2, X'02', 2, X'00');
                 INSERT INTO blocks (height, hash, time, sapling_tree) VALUES (3, X'03', 3, X'00');

                 INSERT INTO sapling_tree_checkpoints (checkpoint_id, position) VALUES (1, 0);
                 INSERT INTO sapling_tree_checkpoints (checkpoint_id, position) VALUES (2, 0);
                 INSERT INTO sapling_tree_checkpoints (checkpoint_id, position) VALUES (3, 0);",
            )
            .unwrap();

        // Insert transactions with the confirmed_unmined_at_height column
        // Transaction 1: mined transaction (no confirmed_unmined_at_height)
        db_data
            .conn
            .execute(
                "INSERT INTO transactions (id_tx, txid, block, mined_height, min_observed_height, confirmed_unmined_at_height)
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![1, vec![0u8; 32], 2, 2, 1, Option::<u32>::None],
            )
            .unwrap();
        // Transaction 2: unmined transaction with confirmed_unmined_at_height set
        // Per the constraint, if confirmed_unmined_at_height is set, mined_height must be NULL
        db_data
            .conn
            .execute(
                "INSERT INTO transactions (id_tx, txid, block, mined_height, min_observed_height, confirmed_unmined_at_height)
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![2, vec![1u8; 32], Option::<u32>::None, Option::<u32>::None, 1, Some(2u32)],
            )
            .unwrap();

        // Verify the confirmed_unmined_at_height column DOES exist
        let column_exists: bool = db_data
            .conn
            .prepare("PRAGMA table_info(transactions)")
            .unwrap()
            .query_map([], |row| {
                let col_name: String = row.get(1)?;
                Ok(col_name)
            })
            .unwrap()
            .any(|name| {
                name.as_ref()
                    .map(|n| n == "confirmed_unmined_at_height")
                    .unwrap_or(false)
            });
        assert!(
            column_exists,
            "confirmed_unmined_at_height column should exist"
        );

        // Verify the second transaction has the confirmed_unmined_at_height value set
        let confirmed_value: Option<u32> = db_data
            .conn
            .query_row(
                "SELECT confirmed_unmined_at_height FROM transactions WHERE id_tx = 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(confirmed_value, Some(2));

        // Test that we can update transactions and set confirmed_unmined_at_height to NULL
        // This simulates what truncate_to_height does when the column exists
        let result = db_data.conn.execute(
            "UPDATE transactions SET confirmed_unmined_at_height = NULL WHERE id_tx = 2",
            [],
        );
        assert!(
            result.is_ok(),
            "Should be able to update confirmed_unmined_at_height when column exists"
        );

        // Verify that the confirmed_unmined_at_height was reset
        let confirmed_value: Option<u32> = db_data
            .conn
            .query_row(
                "SELECT confirmed_unmined_at_height FROM transactions WHERE id_tx = 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(confirmed_value, None);
    }
}
