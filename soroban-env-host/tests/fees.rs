use soroban_env_common::xdr::{Hash, LedgerEntry, LedgerEntryData, LedgerEntryExt, WriteXdr};
use soroban_env_host::{
    fees::{
        compute_rent_fee, compute_rent_write_fee_per_1kb, compute_transaction_resource_fee,
        FeeConfiguration, LedgerEntryRentChange, RentFeeConfiguration, RentWriteFeeConfiguration,
        TransactionResources, MINIMUM_RENT_WRITE_FEE_PER_1KB, TTL_ENTRY_SIZE,
    },
    xdr::TtlEntry,
    DEFAULT_XDR_RW_LIMITS,
};

#[test]
fn ttl_entry_size() {
    let expiration_entry = LedgerEntry {
        last_modified_ledger_seq: 0,
        data: LedgerEntryData::Ttl(TtlEntry {
            key_hash: Hash([0; 32]),
            live_until_ledger_seq: 0,
        }),
        ext: LedgerEntryExt::V0,
    };
    assert_eq!(
        TTL_ENTRY_SIZE,
        expiration_entry
            .to_xdr(DEFAULT_XDR_RW_LIMITS)
            .unwrap()
            .len() as u32
    );
}

fn change_resource<T>(func: T) -> TransactionResources
where
    T: FnOnce(&mut TransactionResources) -> (),
{
    let mut resources = TransactionResources {
        instructions: 0,
        disk_read_entries: 0,
        write_entries: 0,
        disk_read_bytes: 0,
        write_bytes: 0,
        contract_events_size_bytes: 0,
        transaction_size_bytes: 0,
    };
    func(&mut resources);
    resources
}

#[test]
fn resource_fee_computation_with_single_resource() {
    // Historical fee is always paid for 300 byte of transaction result.
    // ceil(6000 * 300 / 1024) == 1758
    const BASE_HISTORICAL_FEE: i64 = 1758;
    let fee_config = FeeConfiguration {
        fee_per_instruction_increment: 1000,
        fee_per_disk_read_entry: 2000,
        fee_per_write_entry: 3000,
        fee_per_disk_read_1kb: 4000,
        fee_per_write_1kb: 5000,
        fee_per_historical_1kb: 6000,
        fee_per_contract_event_1kb: 7000,
        fee_per_transaction_size_1kb: 8000,
    };

    // Instructions
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.instructions = 1;
            }),
            &fee_config,
        ),
        (1 + BASE_HISTORICAL_FEE, 0)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.instructions = 10000;
            }),
            &fee_config,
        ),
        (1000 + BASE_HISTORICAL_FEE, 0)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.instructions = 123_451_234;
            }),
            &fee_config,
        ),
        (12_345_124 + BASE_HISTORICAL_FEE, 0)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.instructions = u32::MAX;
            }),
            &fee_config,
        ),
        (429_496_730 + BASE_HISTORICAL_FEE, 0)
    );

    // Read entries
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.disk_read_entries = 1;
            }),
            &fee_config,
        ),
        (2000 + BASE_HISTORICAL_FEE, 0)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.disk_read_entries = 5;
            }),
            &fee_config,
        ),
        (2000 * 5 + BASE_HISTORICAL_FEE, 0)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.disk_read_entries = u32::MAX;
            }),
            &fee_config,
        ),
        (8_589_934_590_000 + BASE_HISTORICAL_FEE, 0)
    );

    // Write entries
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.write_entries = 1;
            }),
            &fee_config,
        ),
        // Write entries are not counted towards the read entry fee unless they were on disk.
        (3000 + BASE_HISTORICAL_FEE, 0)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.write_entries = 5;
            }),
            &fee_config,
        ),
        (3000 * 5 + BASE_HISTORICAL_FEE, 0)
    );
    // Read and Write entries
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.write_entries = 1;
                res.disk_read_entries = 1;
            }),
            &fee_config,
        ),
        (2000 + 3000 + BASE_HISTORICAL_FEE, 0)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.write_entries = u32::MAX;
            }),
            &fee_config,
        ),
        (12_884_901_885_000 + BASE_HISTORICAL_FEE, 0)
    );

    // Read bytes
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.disk_read_bytes = 1;
            }),
            &fee_config,
        ),
        // ceil(1 * 4000 / 1024) = 4
        (4 + BASE_HISTORICAL_FEE, 0)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.disk_read_bytes = 5 * 1024;
            }),
            &fee_config,
        ),
        (5 * 4000 + BASE_HISTORICAL_FEE, 0)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.disk_read_bytes = 5 * 1024 + 1;
            }),
            &fee_config,
        ),
        (20_004 + BASE_HISTORICAL_FEE, 0)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.disk_read_bytes = u32::MAX;
            }),
            &fee_config,
        ),
        (16_777_215_997 + BASE_HISTORICAL_FEE, 0)
    );

    // Write bytes
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.write_bytes = 1;
            }),
            &fee_config,
        ),
        // ceil(1 * 5000 / 1024) = 4
        (5 + BASE_HISTORICAL_FEE, 0)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.write_bytes = 5 * 1024;
            }),
            &fee_config,
        ),
        (5 * 5000 + BASE_HISTORICAL_FEE, 0)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.write_bytes = 5 * 1024 + 1;
            }),
            &fee_config,
        ),
        (25_005 + BASE_HISTORICAL_FEE, 0)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.write_bytes = u32::MAX;
            }),
            &fee_config,
        ),
        (20_971_519_996 + BASE_HISTORICAL_FEE, 0)
    );

    // Transaction size (affected by historical + tx size fees)
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.transaction_size_bytes = 1;
            }),
            &fee_config,
        ),
        // Historical fee: ceil(1 * 6000 / 1024) = 6
        // Tx size fee: ceil(1 * 8000 / 1024) = 8
        (6 + 8 + BASE_HISTORICAL_FEE, 0)
    );

    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.transaction_size_bytes = 5 * 1024;
            }),
            &fee_config,
        ),
        ((6000 + 8000) * 5 + BASE_HISTORICAL_FEE, 0)
    );

    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.transaction_size_bytes = 5 * 1024 + 1;
            }),
            &fee_config,
        ),
        (30_006 + 40_008 + BASE_HISTORICAL_FEE, 0)
    );

    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.transaction_size_bytes = u32::MAX;
            }),
            &fee_config,
        ),
        // BASE_HISTORICAL_FEE is omitted as it's saturated with overall
        // `transaction_size_bytes`.
        (25_165_823_995 + 33_554_431_993, 0)
    );

    // Events size
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.contract_events_size_bytes = 1;
            }),
            &fee_config,
        ),
        // ceil(1 * 7000 / 1024) = 7
        (BASE_HISTORICAL_FEE, 7)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.contract_events_size_bytes = 5 * 1024;
            }),
            &fee_config,
        ),
        (BASE_HISTORICAL_FEE, 5 * 7000)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.contract_events_size_bytes = 5 * 1024 + 1;
            }),
            &fee_config,
        ),
        (BASE_HISTORICAL_FEE, 35_007)
    );
    assert_eq!(
        compute_transaction_resource_fee(
            &change_resource(|res: &mut TransactionResources| {
                res.contract_events_size_bytes = u32::MAX;
            }),
            &fee_config,
        ),
        (BASE_HISTORICAL_FEE, 29_360_127_994)
    );
}

#[test]
fn resource_fee_computation() {
    // No resources
    assert_eq!(
        compute_transaction_resource_fee(
            &TransactionResources {
                instructions: 0,
                disk_read_entries: 0,
                write_entries: 0,
                disk_read_bytes: 0,
                write_bytes: 0,
                contract_events_size_bytes: 0,
                transaction_size_bytes: 0,
            },
            &FeeConfiguration {
                fee_per_instruction_increment: 100,
                fee_per_disk_read_entry: 100,
                fee_per_write_entry: 100,
                fee_per_disk_read_1kb: 100,
                fee_per_write_1kb: 100,
                fee_per_historical_1kb: 100,
                fee_per_contract_event_1kb: 100,
                fee_per_transaction_size_1kb: 100,
            },
        ),
        // 30 comes from TX_BASE_RESULT_SIZE
        (30, 0)
    );

    // Minimal resources
    assert_eq!(
        compute_transaction_resource_fee(
            &TransactionResources {
                instructions: 1,
                disk_read_entries: 1,
                write_entries: 1,
                disk_read_bytes: 1,
                write_bytes: 1,
                contract_events_size_bytes: 1,
                transaction_size_bytes: 1,
            },
            &FeeConfiguration {
                fee_per_instruction_increment: 100,
                fee_per_disk_read_entry: 100,
                fee_per_write_entry: 100,
                fee_per_disk_read_1kb: 100,
                fee_per_write_1kb: 100,
                fee_per_historical_1kb: 100,
                fee_per_contract_event_1kb: 100,
                fee_per_transaction_size_1kb: 100,
            },
        ),
        // 1 entry read + 1 write + 30 from TX_BASE_RESULT_SIZE + 1 for
        // everything else
        (234, 1)
    );

    // Different resource/fee values
    assert_eq!(
        compute_transaction_resource_fee(
            &TransactionResources {
                instructions: 10_123_456,
                disk_read_entries: 30,
                write_entries: 10,
                disk_read_bytes: 25_600,
                write_bytes: 10_340,
                contract_events_size_bytes: 321_654,
                transaction_size_bytes: 35_721,
            },
            &FeeConfiguration {
                fee_per_instruction_increment: 1000,
                fee_per_disk_read_entry: 2000,
                fee_per_write_entry: 4000,
                fee_per_disk_read_1kb: 1500,
                fee_per_write_1kb: 3000,
                fee_per_historical_1kb: 300,
                fee_per_contract_event_1kb: 200,
                fee_per_transaction_size_1kb: 900,
            },
        ),
        (1_222_089, 62824)
    );

    // Integer limits
    assert_eq!(
        compute_transaction_resource_fee(
            &TransactionResources {
                instructions: u32::MAX,
                disk_read_entries: u32::MAX,
                write_entries: u32::MAX,
                disk_read_bytes: u32::MAX,
                write_bytes: u32::MAX,
                contract_events_size_bytes: u32::MAX,
                transaction_size_bytes: u32::MAX,
            },
            &FeeConfiguration {
                fee_per_instruction_increment: i64::MAX,
                fee_per_disk_read_entry: i64::MAX,
                fee_per_write_entry: i64::MAX,
                fee_per_disk_read_1kb: i64::MAX,
                fee_per_write_1kb: i64::MAX,
                fee_per_historical_1kb: i64::MAX,
                fee_per_contract_event_1kb: i64::MAX,
                fee_per_transaction_size_1kb: i64::MAX,
            },
        ),
        // The refundable (events) fee is not i64::MAX because we do division
        // after multiplication and hence it's i64::MAX / 1024.
        // Hitting the integer size limits shouldn't be an issue in practice;
        // we need to just make sure there are no overflows.
        (i64::MAX, 9_007_199_254_740_992)
    );
}

#[test]
fn test_rent_extend_fees_with_only_extend() {
    let fee_config = RentFeeConfiguration {
        fee_per_write_entry: 10,
        fee_per_rent_1kb: 1000,
        fee_per_write_1kb: 500,
        persistent_rent_rate_denominator: 10_000,
        temporary_rent_rate_denominator: 100_000,
    };

    // Minimal size
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: true,
                is_code_entry: false,
                old_size_bytes: 1,
                new_size_bytes: 1,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 300_000,
            }],
            &fee_config,
            50_000,
        ),
        // Rent: ceil(1 * 1000 * 200_000 / (10_000 * 1024)) (=20) +
        // TTL entry write bytes: ceil(500 * 48 / 1024) (=24) +
        // TTL entry write: 10
        20 + 24 + 10
    );

    // Minimal ledgers
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: true,
                is_code_entry: false,
                old_size_bytes: 10 * 1024,
                new_size_bytes: 10 * 1024,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 100_001,
            }],
            &fee_config,
            50_000,
        ),
        // Rent: ceil(10 * 1024 * 1000 * 1 / (10_000 * 1024)) (=1) +
        // Expiration entry write entry/bytes: 34
        1 + 34
    );

    // Minimal ledgers & size
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: true,
                is_code_entry: false,
                old_size_bytes: 1,
                new_size_bytes: 1,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 100_001,
            }],
            &fee_config,
            50_000,
        ),
        // Rent: ceil(1 * 1000 * 1 / (10_000 * 1024))
        // Expiration entry write entry/bytes: 34
        1 + 34
    );

    // No size change
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: true,
                is_code_entry: false,
                old_size_bytes: 10 * 1024,
                new_size_bytes: 10 * 1024,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 300_000,
            }],
            &fee_config,
            50_000,
        ),
        // Rent: ceil(10 * 1024 * 1000 * 200_000 / (10_000 * 1024)) (=200_000)
        // Expiration entry write entry/bytes: 34
        200_000 + 34
    );

    // No size change, code entry
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: true,
                is_code_entry: true,
                old_size_bytes: 10 * 1024,
                new_size_bytes: 10 * 1024,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 300_000,
            }],
            &fee_config,
            50_000,
        ),
        // Rent: ceil(10 * 1024 * 1000 * 200_000 / (10_000 * 1024)) / 3 (=66_666)
        // Expiration entry write entry/bytes: 34
        66_666 + 34
    );

    // Size decrease
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: true,
                is_code_entry: false,
                old_size_bytes: 10 * 1024,
                new_size_bytes: 5 * 1024,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 300_000,
            }],
            &fee_config,
            50_000,
        ),
        // Rent: ceil(5 * 1024 * 1000 * 200_000 / (10_000 * 1024)) (=100_000) +
        // Expiration entry write entry/bytes: 34
        100_000 + 34
    );

    // Temp storage rate
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: false,
                is_code_entry: false,
                old_size_bytes: 10 * 1024,
                new_size_bytes: 10 * 1024,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 300_000,
            }],
            &fee_config,
            50_000,
        ),
        // Rent: ceil(10 * 1024 * 1000 * 200_000 / (100_000 * 1024)) (=20_000) +
        // Expiration entry write entry/bytes: 34
        20_000 + 34
    );

    // Multiple entries
    assert_eq!(
        compute_rent_fee(
            &[
                LedgerEntryRentChange {
                    is_persistent: false,
                    is_code_entry: false,
                    old_size_bytes: 10 * 1024,
                    new_size_bytes: 10 * 1024,
                    old_live_until_ledger: 100_000,
                    new_live_until_ledger: 300_000,
                },
                LedgerEntryRentChange {
                    is_persistent: true,
                    is_code_entry: false,
                    old_size_bytes: 10 * 1024,
                    new_size_bytes: 10 * 1024,
                    old_live_until_ledger: 100_000,
                    new_live_until_ledger: 300_000,
                },
                LedgerEntryRentChange {
                    is_persistent: true,
                    is_code_entry: true,
                    old_size_bytes: 10 * 1024,
                    new_size_bytes: 10 * 1024,
                    old_live_until_ledger: 100_000,
                    new_live_until_ledger: 300_000,
                },
                LedgerEntryRentChange {
                    is_persistent: true,
                    is_code_entry: false,
                    old_size_bytes: 1,
                    new_size_bytes: 1,
                    old_live_until_ledger: 100_000,
                    new_live_until_ledger: 100_001,
                },
                LedgerEntryRentChange {
                    is_persistent: true,
                    is_code_entry: false,
                    old_size_bytes: 1,
                    new_size_bytes: 1,
                    old_live_until_ledger: 100_000,
                    new_live_until_ledger: 300_000,
                },
                LedgerEntryRentChange {
                    is_persistent: true,
                    is_code_entry: false,
                    old_size_bytes: 10 * 1024,
                    new_size_bytes: 10 * 1024,
                    old_live_until_ledger: 100_000,
                    new_live_until_ledger: 300_000,
                },
                LedgerEntryRentChange {
                    is_persistent: false,
                    is_code_entry: false,
                    old_size_bytes: 10 * 1024,
                    new_size_bytes: 10 * 1024,
                    old_live_until_ledger: 100_000,
                    new_live_until_ledger: 300_000,
                }
            ],
            &fee_config,
            50_000,
        ),
        // Rent: 20_000 + 200_000 + 66_666 + 1 + 20 + 200_000 + 20_000 (=506_687) +
        // Expiration entry write bytes: ceil(7 * 500 * 48 / 1024) (=165) +
        // Expiration entry write: 10 * 7
        506_687 + 165 + 70
    );
}

#[test]
fn test_rent_extend_fees_with_only_size_change() {
    let fee_config = RentFeeConfiguration {
        fee_per_write_entry: 100,
        fee_per_rent_1kb: 1000,
        fee_per_write_1kb: 500,
        persistent_rent_rate_denominator: 10_000,
        temporary_rent_rate_denominator: 100_000,
    };

    // Large size increase
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: true,
                is_code_entry: false,
                old_size_bytes: 1,
                new_size_bytes: 100_000,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 100_000,
            }],
            &fee_config,
            25_000,
        ),
        // 99_999 * 1000 * (100_000 - 25_000 + 1) / (10_000 * 1024)
        732_425
    );

    // Large size increase, code entry
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: true,
                is_code_entry: true,
                old_size_bytes: 1,
                new_size_bytes: 100_000,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 100_000,
            }],
            &fee_config,
            25_000,
        ),
        // 99_999 * 1000 * (100_000 - 25_000 + 1) / (10_000 * 1024) / 3
        732_425 / 3
    );

    // Large size increase, temp storage
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: false,
                is_code_entry: false,
                old_size_bytes: 1,
                new_size_bytes: 100_000,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 100_000,
            }],
            &fee_config,
            25_000,
        ),
        // 99_999 * 1000 * (100_000 - 25_000 + 1) / (1_000 * 1024)
        73_243
    );

    // Small size increase
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: true,
                is_code_entry: false,
                old_size_bytes: 99_999,
                new_size_bytes: 100_000,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 100_000,
            }],
            &fee_config,
            25_000,
        ),
        // ceil(1 * 1000 * (100_000 - 25_000 + 1) / (10_000 * 1024))
        8
    );

    // Small ledger difference
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: true,
                is_code_entry: false,
                old_size_bytes: 1,
                new_size_bytes: 100_000,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 100_000,
            }],
            &fee_config,
            99_999,
        ),
        // ceil(99_999 * 1000 * (100_000 - 99_999 + 1) / (10_000 * 1024))
        20
    );

    // Multiple entries
    assert_eq!(
        compute_rent_fee(
            &[
                LedgerEntryRentChange {
                    is_persistent: true,
                    is_code_entry: false,
                    old_size_bytes: 1,
                    new_size_bytes: 100_000,
                    old_live_until_ledger: 100_000,
                    new_live_until_ledger: 100_000,
                },
                LedgerEntryRentChange {
                    is_persistent: true,
                    is_code_entry: true,
                    old_size_bytes: 1,
                    new_size_bytes: 100_000,
                    old_live_until_ledger: 100_000,
                    new_live_until_ledger: 100_000,
                },
                LedgerEntryRentChange {
                    is_persistent: false,
                    is_code_entry: false,
                    old_size_bytes: 1,
                    new_size_bytes: 100_000,
                    old_live_until_ledger: 100_000,
                    new_live_until_ledger: 100_000,
                }
            ],
            &fee_config,
            25_000,
        ),
        // 732_425 + 732_425 / 3 + 73_243
        1_049_809
    );
}

#[test]
fn test_rent_extend_with_size_change_and_extend() {
    let fee_config = RentFeeConfiguration {
        fee_per_write_entry: 10,
        fee_per_rent_1kb: 1000,
        fee_per_write_1kb: 500,
        persistent_rent_rate_denominator: 10_000,
        temporary_rent_rate_denominator: 100_000,
    };

    // Persistent entry
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: true,
                is_code_entry: false,
                old_size_bytes: 1,
                new_size_bytes: 100_000,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 300_000,
            }],
            &fee_config,
            25_000,
        ),
        // Rent: 100_000 * 1000 * 200_000 / (10_000 * 1024) +
        // 99_999 * 1000 * (100_000 - 25_000 + 1) / (10_000 * 1024)
        // Expiration entry write entry/bytes: 34
        2_685_550 + 34
    );

    // Persistent code entry
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: true,
                is_code_entry: true,
                old_size_bytes: 1,
                new_size_bytes: 100_000,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 300_000,
            }],
            &fee_config,
            25_000,
        ),
        // Rent: 100_000 * 1000 * 200_000 / (10_000 * 1024) / 2 +
        // 99_999 * 1000 * (100_000 - 25_000 + 1) / (10_000 * 1024)
        // Expiration entry write entry/bytes: 34
        2_685_550 / 3 + 34
    );

    // Temp entry
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: false,
                is_code_entry: false,
                old_size_bytes: 1,
                new_size_bytes: 100_000,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 300_000,
            }],
            &fee_config,
            25_000,
        ),
        // Rent: 100_000 * 1000 * 200_000 / (10_000 * 1024) +
        // 99_999 * 1000 * (100_000 - 25_000 + 1) / (10_000 * 1024)
        // Expiration entry write entry/bytes: 34
        268_556 + 34
    );

    // Multiple entries
    assert_eq!(
        compute_rent_fee(
            &[
                LedgerEntryRentChange {
                    is_persistent: true,
                    is_code_entry: false,
                    old_size_bytes: 1,
                    new_size_bytes: 100_000,
                    old_live_until_ledger: 100_000,
                    new_live_until_ledger: 300_000,
                },
                LedgerEntryRentChange {
                    is_persistent: true,
                    is_code_entry: true,
                    old_size_bytes: 1,
                    new_size_bytes: 100_000,
                    old_live_until_ledger: 100_000,
                    new_live_until_ledger: 300_000,
                },
                LedgerEntryRentChange {
                    is_persistent: false,
                    is_code_entry: false,
                    old_size_bytes: 1,
                    new_size_bytes: 100_000,
                    old_live_until_ledger: 100_000,
                    new_live_until_ledger: 300_000,
                }
            ],
            &fee_config,
            25_000,
        ),
        // Rent: 2_685_550 + 2_685_550 / 3 + 268_556
        // Expiration entry write bytes: ceil(3 * 500 * 48 / 1024) (=71) +
        // Expiration entry write: 10 * 3
        3_849_289 + 71 + 30
    );

    // Small increments
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: true,
                is_code_entry: false,
                old_size_bytes: 1,
                new_size_bytes: 2,
                old_live_until_ledger: 100_000,
                new_live_until_ledger: 100_001,
            }],
            &fee_config,
            99_999,
        ),
        // Rent: ceil(2 * 1000 * 1 / (10_000 * 1024)) +
        //       ceil(1 * 1000 * (100_000 - 99_999 + 1) / (10_000 * 1024)) (=2)
        // Expiration entry write entry/bytes: 34
        2 + 34
    );
}

#[test]
fn test_rent_extend_without_old_entry() {
    let fee_config = RentFeeConfiguration {
        fee_per_write_entry: 10,
        fee_per_rent_1kb: 1000,
        fee_per_write_1kb: 500,
        persistent_rent_rate_denominator: 10_000,
        temporary_rent_rate_denominator: 100_000,
    };

    // Persistent storage
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: true,
                is_code_entry: false,
                old_size_bytes: 0,
                new_size_bytes: 100_000,
                old_live_until_ledger: 0,
                new_live_until_ledger: 100_000,
            }],
            &fee_config,
            25_000,
        ),
        // Rent: 100_000 * 1000 * (100_000 - 25_000) / (10_000 * 1024)
        // Expiration entry write entry/bytes: 34
        732_432 + 34
    );

    // Temp storage
    assert_eq!(
        compute_rent_fee(
            &[LedgerEntryRentChange {
                is_persistent: false,
                is_code_entry: false,
                old_size_bytes: 0,
                new_size_bytes: 100_000,
                old_live_until_ledger: 0,
                new_live_until_ledger: 100_000,
            }],
            &fee_config,
            25_000,
        ),
        // Rent: 100_000 * 1000 * (100_000 - 25_000) / (10_000 * 1024)
        // Expiration entry write entry/bytes: 34
        73_244 + 34
    );
}

#[test]
fn test_compute_rent_write_fee() {
    let fee_config = RentWriteFeeConfiguration {
        state_target_size_bytes: 100_000,
        rent_fee_1kb_state_size_low: 100,
        rent_fee_1kb_state_size_high: 10_000,
        state_size_rent_fee_growth_factor: 50,
    };
    // Empty state
    assert_eq!(
        compute_rent_write_fee_per_1kb(0, &fee_config),
        MINIMUM_RENT_WRITE_FEE_PER_1KB
    );
    // Non-empty state below target
    assert_eq!(
        compute_rent_write_fee_per_1kb(50_000, &fee_config),
        100 + (10_000 - 100) / 2
    );
    assert_eq!(compute_rent_write_fee_per_1kb(56_789, &fee_config), 5723);
    // State size is at target
    assert_eq!(compute_rent_write_fee_per_1kb(100_000, &fee_config), 10_000);
    // State size is bigger than target
    assert_eq!(
        compute_rent_write_fee_per_1kb(150_000, &fee_config),
        10_000 + 50 * (10_000 - 100) / 2
    );
    // State size is several times bigger than target
    assert_eq!(
        compute_rent_write_fee_per_1kb(580_000, &fee_config),
        10_000 + 2_376_000
    );

    let large_fee_config = RentWriteFeeConfiguration {
        state_target_size_bytes: 100_000_000_000_000,
        rent_fee_1kb_state_size_low: 1_000_000,
        rent_fee_1kb_state_size_high: 1_000_000_000,
        state_size_rent_fee_growth_factor: 50,
    };
    // Large bucket list size and fees, half-filled bucket list
    assert_eq!(
        compute_rent_write_fee_per_1kb(50_000_000_000_000, &large_fee_config),
        1_000_000 + (1_000_000_000 - 1_000_000) / 2
    );
    // Large bucket list size and fees, over target bucket list
    assert_eq!(
        compute_rent_write_fee_per_1kb(150_000_000_000_000, &large_fee_config),
        1_000_000_000 + 50 * (1_000_000_000 - 1_000_000) / 2
    );
}

#[test]
fn test_compute_write_fee_with_negative_low() {
    let fee_config = RentWriteFeeConfiguration {
        state_target_size_bytes: 100_000,
        rent_fee_1kb_state_size_low: -1_000,
        rent_fee_1kb_state_size_high: 10_000,
        state_size_rent_fee_growth_factor: 50,
    };

    // clamping before target
    assert_eq!(
        compute_rent_write_fee_per_1kb(18_181, &fee_config),
        MINIMUM_RENT_WRITE_FEE_PER_1KB
    );

    //ceil(11_000 * 18_182 / 100_000) - 1000 = 1001
    assert_eq!(
        compute_rent_write_fee_per_1kb(18_182, &fee_config),
        MINIMUM_RENT_WRITE_FEE_PER_1KB + 1
    );

    // Bucket list bigger than target.
    assert_eq!(
        compute_rent_write_fee_per_1kb(150_000, &fee_config),
        10_000 + 50 * (10_000 + 1_000) / 2
    );
}

#[test]
fn test_compute_write_fee_clamp_after_target() {
    let fee_config = RentWriteFeeConfiguration {
        state_target_size_bytes: 100_000,
        rent_fee_1kb_state_size_low: 10,
        rent_fee_1kb_state_size_high: 20,
        state_size_rent_fee_growth_factor: 50,
    };

    // Bucket list bigger than target.
    assert_eq!(
        compute_rent_write_fee_per_1kb(100_001, &fee_config),
        MINIMUM_RENT_WRITE_FEE_PER_1KB
    );
}
