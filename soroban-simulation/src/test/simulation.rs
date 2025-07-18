use crate::simulation::{
    simulate_extend_ttl_op, simulate_invoke_host_function_op, simulate_restore_op,
    ExtendTtlOpSimulationResult, LedgerEntryDiff, RestoreOpSimulationResult,
    SimulationAdjustmentConfig, SimulationAdjustmentFactor,
};
use crate::testutils::{ledger_entry_to_ledger_key, temp_entry, MockSnapshotSource};
use crate::NetworkConfig;
use pretty_assertions::assert_eq;
use soroban_env_host::e2e_invoke::RecordingInvocationAuthMode;
use soroban_env_host::e2e_testutils::{
    account_entry, auth_contract_invocation, bytes_sc_val, create_contract_auth,
    default_ledger_info, get_account_id, get_contract_id_preimage, get_wasm_hash, get_wasm_key,
    ledger_entry, upload_wasm_host_fn, wasm_entry, wasm_entry_non_validated,
    AuthContractInvocationNode, CreateContractData,
};
use soroban_env_host::fees::{FeeConfiguration, RentFeeConfiguration};
use soroban_env_host::xdr::{
    AccountId, AlphaNum4, AssetCode4, ContractCostParamEntry, ContractCostParams, ContractCostType,
    ContractDataDurability, ContractDataEntry, ContractExecutable, ContractId, ExtensionPoint,
    Hash, HostFunction, Int128Parts, InvokeContractArgs, LedgerEntry, LedgerEntryData,
    LedgerFootprint, LedgerKey, LedgerKeyContractData, LedgerKeyTrustLine, Limits, PublicKey,
    ScAddress, ScBytes, ScContractInstance, ScErrorCode, ScErrorType, ScMap, ScNonceKey, ScString,
    ScSymbol, ScVal, SorobanAddressCredentials, SorobanAuthorizationEntry,
    SorobanAuthorizedFunction, SorobanAuthorizedInvocation, SorobanCredentials, SorobanResources,
    SorobanResourcesExtV0, SorobanTransactionData, SorobanTransactionDataExt, TrustLineAsset,
    TrustLineEntry, TrustLineEntryExt, TrustLineFlags, Uint256, VecM, WriteXdr,
};
use soroban_env_host::HostError;
use soroban_test_wasms::{ADD_I32, AUTH_TEST_CONTRACT, TRY_CALL_SAC};
use std::rc::Rc;
use tap::prelude::*;

fn default_network_config() -> NetworkConfig {
    let default_entry = ContractCostParamEntry {
        ext: ExtensionPoint::V0,
        const_term: 0,
        linear_term: 0,
    };
    let mut cpu_cost_params = vec![default_entry.clone(); ContractCostType::variants().len()];
    let mut mem_cost_params = vec![default_entry; ContractCostType::variants().len()];
    for i in 0..ContractCostType::variants().len() {
        let v = i as i64;
        cpu_cost_params[i].const_term = (v + 1) * 1000;
        cpu_cost_params[i].linear_term = v << 7;
        mem_cost_params[i].const_term = (v + 1) * 500;
        mem_cost_params[i].linear_term = v << 6;
    }
    let ledger_info = default_ledger_info();

    NetworkConfig {
        fee_configuration: FeeConfiguration {
            fee_per_instruction_increment: 10,
            fee_per_disk_read_entry: 20,
            fee_per_write_entry: 30,
            fee_per_disk_read_1kb: 40,
            fee_per_write_1kb: 50,
            fee_per_historical_1kb: 60,
            fee_per_contract_event_1kb: 70,
            fee_per_transaction_size_1kb: 80,
        },
        rent_fee_configuration: RentFeeConfiguration {
            fee_per_rent_1kb: 100,
            fee_per_write_1kb: 50,
            fee_per_write_entry: 30,
            persistent_rent_rate_denominator: 100,
            temporary_rent_rate_denominator: 1000,
        },
        tx_max_instructions: 100_000_000,
        tx_memory_limit: 40_000_000,
        cpu_cost_params: ContractCostParams(cpu_cost_params.try_into().unwrap()),
        memory_cost_params: ContractCostParams(mem_cost_params.try_into().unwrap()),
        min_temp_entry_ttl: ledger_info.min_temp_entry_ttl,
        min_persistent_entry_ttl: ledger_info.min_persistent_entry_ttl,
        max_entry_ttl: ledger_info.max_entry_ttl,
    }
}

fn test_adjustment_config() -> SimulationAdjustmentConfig {
    SimulationAdjustmentConfig {
        instructions: SimulationAdjustmentFactor::new(1.1, 100_000),
        read_bytes: SimulationAdjustmentFactor::new(1.2, 500),
        write_bytes: SimulationAdjustmentFactor::new(1.3, 300),
        tx_size: SimulationAdjustmentFactor::new(1.4, 1000),
        refundable_fee: SimulationAdjustmentFactor::new(1.5, 100_000),
    }
}

fn nonce_key(address: ScAddress, nonce: i64) -> LedgerKey {
    LedgerKey::ContractData(LedgerKeyContractData {
        contract: address,
        key: ScVal::LedgerKeyNonce(ScNonceKey { nonce }),
        durability: ContractDataDurability::Temporary,
    })
}

fn nonce_entry(address: ScAddress, nonce: i64) -> LedgerEntry {
    ledger_entry(LedgerEntryData::ContractData(ContractDataEntry {
        ext: ExtensionPoint::V0,
        contract: address,
        key: ScVal::LedgerKeyNonce(ScNonceKey { nonce }),
        durability: ContractDataDurability::Temporary,
        val: ScVal::Void,
    }))
}

#[test]
fn test_simulate_upload_wasm() {
    let source_account = get_account_id([123; 32]);
    let ledger_info = default_ledger_info();
    let network_config = default_network_config();
    let snapshot_source = Rc::new(MockSnapshotSource::from_entries(vec![]).unwrap());

    let res = simulate_invoke_host_function_op(
        snapshot_source.clone(),
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        upload_wasm_host_fn(ADD_I32),
        RecordingInvocationAuthMode::Recording(true),
        &source_account,
        [1; 32],
        true,
    )
    .unwrap();
    assert_eq!(
        res.invoke_result.unwrap(),
        bytes_sc_val(&get_wasm_hash(ADD_I32))
    );

    assert_eq!(res.auth, vec![]);
    assert!(res.contract_events.is_empty());
    assert!(res.diagnostic_events.is_empty());

    let expected_instructions = 1676095;
    let expected_write_bytes = 684;
    assert_eq!(
        res.transaction_data,
        Some(SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: Default::default(),
                    read_write: vec![get_wasm_key(ADD_I32)].try_into().unwrap()
                },
                instructions: expected_instructions,
                disk_read_bytes: 0,
                write_bytes: expected_write_bytes,
            },
            resource_fee: 4714773,
        })
    );
    assert_eq!(res.simulated_instructions, expected_instructions);
    assert_eq!(res.simulated_memory, 838046);
    assert_eq!(
        res.modified_entries,
        vec![LedgerEntryDiff {
            state_before: None,
            state_after: Some(wasm_entry(ADD_I32))
        }]
    );

    let res_with_adjustments = simulate_invoke_host_function_op(
        snapshot_source,
        &network_config,
        &test_adjustment_config(),
        &ledger_info,
        upload_wasm_host_fn(ADD_I32),
        RecordingInvocationAuthMode::Recording(true),
        &source_account,
        [1; 32],
        true,
    )
    .unwrap();
    assert_eq!(
        res_with_adjustments.invoke_result.unwrap(),
        bytes_sc_val(&get_wasm_hash(ADD_I32))
    );

    assert_eq!(res_with_adjustments.auth, res.auth);
    assert_eq!(res_with_adjustments.contract_events, res.contract_events);
    assert_eq!(
        res_with_adjustments.diagnostic_events,
        res.diagnostic_events
    );
    assert_eq!(
        res_with_adjustments.simulated_instructions,
        res.simulated_instructions
    );
    assert_eq!(res_with_adjustments.simulated_memory, res.simulated_memory);
    assert_eq!(
        res_with_adjustments.transaction_data,
        Some(SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: Default::default(),
                    read_write: vec![get_wasm_key(ADD_I32)].try_into().unwrap()
                },
                instructions: (expected_instructions as f64 * 1.1) as u32,
                disk_read_bytes: 0,
                write_bytes: expected_write_bytes + 300,
            },
            resource_fee: 7071426,
        })
    );
}

#[test]
fn test_simulation_returns_insufficient_budget_error() {
    let source_account = get_account_id([123; 32]);
    let ledger_info = default_ledger_info();
    let mut network_config = default_network_config();
    network_config.tx_max_instructions = 100_000;
    let snapshot_source = Rc::new(MockSnapshotSource::from_entries(vec![]).unwrap());

    let res = simulate_invoke_host_function_op(
        snapshot_source.clone(),
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        upload_wasm_host_fn(ADD_I32),
        RecordingInvocationAuthMode::Recording(true),
        &source_account,
        [1; 32],
        true,
    )
    .unwrap();
    assert!(HostError::result_matches_err(
        res.invoke_result,
        (ScErrorType::Budget, ScErrorCode::ExceededLimit)
    ));
    assert_eq!(res.auth, vec![]);
    assert!(res.contract_events.is_empty());
    assert!(res.diagnostic_events.is_empty());

    assert_eq!(res.transaction_data, None);
    assert_eq!(res.simulated_instructions, 111516);
    assert_eq!(res.simulated_memory, 45006);
    assert_eq!(res.modified_entries, vec![]);
}

#[test]
fn test_simulation_returns_logic_error() {
    let source_account = get_account_id([123; 32]);
    let ledger_info = default_ledger_info();
    let network_config = default_network_config();
    let snapshot_source = Rc::new(MockSnapshotSource::from_entries(vec![]).unwrap());
    let bad_wasm = [0; 1000];

    let res = simulate_invoke_host_function_op(
        snapshot_source.clone(),
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        upload_wasm_host_fn(&bad_wasm),
        RecordingInvocationAuthMode::Recording(true),
        &source_account,
        [1; 32],
        true,
    )
    .unwrap();
    assert!(HostError::result_matches_err(
        res.invoke_result,
        (ScErrorType::WasmVm, ScErrorCode::InvalidAction)
    ));
    assert_eq!(res.auth, vec![]);
    assert!(res.contract_events.is_empty());
    assert!(!res.diagnostic_events.is_empty());

    assert_eq!(res.transaction_data, None);
    assert_eq!(res.simulated_instructions, 154568);
    assert_eq!(res.simulated_memory, 77284);
    assert_eq!(res.modified_entries, vec![]);
}

#[test]
fn test_simulate_create_contract() {
    let source_account = get_account_id([123; 32]);
    let ledger_info = default_ledger_info();
    let network_config = default_network_config();
    let contract = CreateContractData::new([1; 32], ADD_I32);

    let snapshot_source = Rc::new(
        MockSnapshotSource::from_entries(vec![(
            contract.wasm_entry,
            Some(ledger_info.sequence_number + 1000),
        )])
        .unwrap(),
    );

    let res = simulate_invoke_host_function_op(
        snapshot_source.clone(),
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        contract.host_fn.clone(),
        RecordingInvocationAuthMode::Recording(true),
        &source_account,
        [1; 32],
        true,
    )
    .unwrap();
    assert_eq!(
        res.invoke_result.unwrap(),
        ScVal::Address(contract.contract_address)
    );

    assert_eq!(
        res.auth,
        vec![create_contract_auth(
            &get_contract_id_preimage(&contract.deployer, &[1; 32]),
            ADD_I32,
        )]
    );
    assert!(res.contract_events.is_empty());
    assert!(res.diagnostic_events.is_empty());
    let expected_instructions = 2739690;
    assert_eq!(
        res.transaction_data,
        Some(SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![contract.wasm_key.clone()].try_into().unwrap(),
                    read_write: vec![contract.contract_key.clone()].try_into().unwrap()
                },
                instructions: expected_instructions,
                disk_read_bytes: 0,
                write_bytes: 104,
            },
            resource_fee: 13293,
        })
    );
    assert_eq!(res.simulated_instructions, expected_instructions);
    assert_eq!(res.simulated_memory, 1369843);
    assert_eq!(
        res.modified_entries,
        vec![LedgerEntryDiff {
            state_before: None,
            state_after: Some(contract.contract_entry)
        }]
    );
}

#[test]
fn test_simulate_invoke_contract_with_auth() {
    let contracts = vec![
        CreateContractData::new([1; 32], AUTH_TEST_CONTRACT),
        CreateContractData::new([2; 32], AUTH_TEST_CONTRACT),
        CreateContractData::new([3; 32], AUTH_TEST_CONTRACT),
        CreateContractData::new([4; 32], AUTH_TEST_CONTRACT),
    ];

    let tree = AuthContractInvocationNode {
        address: contracts[0].contract_address.clone(),
        children: vec![
            AuthContractInvocationNode {
                address: contracts[1].contract_address.clone(),
                children: vec![AuthContractInvocationNode {
                    address: contracts[2].contract_address.clone(),
                    children: vec![AuthContractInvocationNode {
                        address: contracts[3].contract_address.clone(),
                        children: vec![],
                    }],
                }],
            },
            AuthContractInvocationNode {
                address: contracts[2].contract_address.clone(),
                children: vec![
                    AuthContractInvocationNode {
                        address: contracts[1].contract_address.clone(),
                        children: vec![],
                    },
                    AuthContractInvocationNode {
                        address: contracts[3].contract_address.clone(),
                        children: vec![],
                    },
                ],
            },
        ],
    };
    let source_account = get_account_id([123; 32]);
    let other_account = get_account_id([124; 32]);
    let host_fn = auth_contract_invocation(
        vec![
            ScAddress::Account(source_account.clone()),
            ScAddress::Account(other_account.clone()),
        ],
        tree.clone(),
    );
    let ledger_info = default_ledger_info();
    let network_config = default_network_config();
    let snapshot_source = Rc::new(
        MockSnapshotSource::from_entries(vec![
            (
                contracts[0].wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                contracts[0].contract_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
            (
                contracts[1].contract_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
            (
                contracts[2].contract_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
            (
                contracts[3].contract_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
            // Source account doesn't need to be accessed
            (account_entry(&other_account), None),
        ])
        .unwrap(),
    );

    let res = simulate_invoke_host_function_op(
        snapshot_source,
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        host_fn,
        RecordingInvocationAuthMode::Recording(true),
        &source_account,
        [1; 32],
        true,
    )
    .unwrap();
    assert_eq!(res.invoke_result.unwrap(), ScVal::Void);

    let other_account_address = ScAddress::Account(other_account.clone());
    // This value is stable thanks to hardcoded RNG seed.
    let other_account_nonce = 1039859045797838027;
    let expected_auth_tree = tree.into_authorized_invocation();
    assert_eq!(
        res.auth,
        vec![
            SorobanAuthorizationEntry {
                credentials: SorobanCredentials::SourceAccount,
                root_invocation: expected_auth_tree.clone(),
            },
            SorobanAuthorizationEntry {
                credentials: SorobanCredentials::Address(SorobanAddressCredentials {
                    address: other_account_address.clone(),
                    nonce: other_account_nonce,
                    signature_expiration_ledger: 0,
                    signature: ScVal::Void,
                }),
                root_invocation: expected_auth_tree,
            }
        ]
    );
    assert!(res.contract_events.is_empty());
    assert!(!res.diagnostic_events.is_empty());

    let expected_instructions = 40782813;
    assert_eq!(
        res.transaction_data,
        Some(SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![
                        ledger_entry_to_ledger_key(&account_entry(&other_account)).unwrap(),
                        contracts[0].contract_key.clone(),
                        contracts[1].contract_key.clone(),
                        contracts[2].contract_key.clone(),
                        contracts[3].contract_key.clone(),
                        contracts[0].wasm_key.clone(),
                    ]
                    .tap_mut(|v| v.sort())
                    .try_into()
                    .unwrap(),
                    read_write: vec![nonce_key(
                        other_account_address.clone(),
                        other_account_nonce
                    )]
                    .try_into()
                    .unwrap()
                },
                instructions: expected_instructions,
                disk_read_bytes: 144,
                write_bytes: 76,
            },
            resource_fee: 115726,
        })
    );
    assert_eq!(res.simulated_instructions, expected_instructions);
    assert_eq!(res.simulated_memory, 20391380);
    assert_eq!(
        res.modified_entries,
        vec![LedgerEntryDiff {
            state_before: None,
            state_after: Some(nonce_entry(other_account_address, other_account_nonce))
        }]
    );
}

#[test]
fn test_simulate_invoke_contract_with_autorestore() {
    let contracts = vec![
        CreateContractData::new([1; 32], AUTH_TEST_CONTRACT),
        CreateContractData::new([2; 32], AUTH_TEST_CONTRACT),
    ];

    let tree = AuthContractInvocationNode {
        address: contracts[0].contract_address.clone(),
        children: vec![AuthContractInvocationNode {
            address: contracts[1].contract_address.clone(),
            children: vec![],
        }],
    };
    let source_account = get_account_id([123; 32]);
    let host_fn = auth_contract_invocation(
        vec![ScAddress::Account(source_account.clone())],
        tree.clone(),
    );
    let ledger_info = default_ledger_info();
    let network_config = default_network_config();
    let snapshot_source = Rc::new(
        MockSnapshotSource::from_entries(vec![
            (
                contracts[0].wasm_entry.clone(),
                Some(ledger_info.sequence_number - 100),
            ),
            (
                contracts[0].contract_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
            (
                contracts[1].contract_entry.clone(),
                Some(ledger_info.sequence_number - 1),
            ),
            // Source account doesn't need to be accessed
        ])
        .unwrap(),
    );

    let res = simulate_invoke_host_function_op(
        snapshot_source,
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        host_fn,
        RecordingInvocationAuthMode::Recording(true),
        &source_account,
        [1; 32],
        true,
    )
    .unwrap();
    assert_eq!(res.invoke_result.unwrap(), ScVal::Void);

    assert!(res.contract_events.is_empty());
    assert!(!res.diagnostic_events.is_empty());

    let expected_instructions = 9936120;
    let wasm_entry_size = contracts[0]
        .wasm_entry
        .to_xdr(Limits::none())
        .unwrap()
        .len() as u32;
    let contract_1_size = contracts[1]
        .contract_entry
        .to_xdr(Limits::none())
        .unwrap()
        .len() as u32;
    assert_eq!(
        res.transaction_data,
        Some(SorobanTransactionData {
            ext: SorobanTransactionDataExt::V1(SorobanResourcesExtV0 {
                archived_soroban_entries: vec![0, 1].try_into().unwrap()
            }),
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![contracts[0].contract_key.clone(),].try_into().unwrap(),
                    read_write: vec![
                        contracts[1].contract_key.clone(),
                        contracts[0].wasm_key.clone(),
                    ]
                    .tap_mut(|v| v.sort())
                    .try_into()
                    .unwrap(),
                },
                instructions: expected_instructions,
                disk_read_bytes: wasm_entry_size + contract_1_size,
                write_bytes: wasm_entry_size + contract_1_size,
            },
            resource_fee: 6230340,
        })
    );
    assert_eq!(res.simulated_instructions, expected_instructions);
    assert_eq!(res.simulated_memory, 4968053);
    assert_eq!(
        res.modified_entries,
        vec![
            LedgerEntryDiff {
                state_before: None,
                state_after: Some(contracts[1].contract_entry.clone())
            },
            LedgerEntryDiff {
                state_before: None,
                state_after: Some(contracts[0].wasm_entry.clone())
            }
        ]
    );
}

#[test]
fn test_simulate_extend_ttl_op() {
    let ledger_info = default_ledger_info();
    let network_config = default_network_config();
    let contract_entry = CreateContractData::new([111; 32], ADD_I32).contract_entry;
    let entries = vec![
        (
            wasm_entry(ADD_I32),
            Some(ledger_info.sequence_number + 100_000),
        ),
        (
            wasm_entry(AUTH_TEST_CONTRACT),
            Some(ledger_info.sequence_number + 100),
        ),
        (contract_entry, Some(ledger_info.sequence_number + 500_000)),
        (
            wasm_entry_non_validated(b"123"),
            Some(ledger_info.sequence_number + 1_000_000),
        ),
        (temp_entry(b"321"), Some(ledger_info.sequence_number + 100)),
        (
            temp_entry(b"123"),
            Some(ledger_info.sequence_number + 100_000),
        ),
        (
            temp_entry(b"456"),
            Some(ledger_info.sequence_number + 1_000_000),
        ),
    ];
    let mut keys: Vec<LedgerKey> = entries
        .iter()
        .map(|e| ledger_entry_to_ledger_key(&e.0).unwrap())
        .collect();
    let snapshot_source = MockSnapshotSource::from_entries(entries).unwrap();

    let no_op_extension = simulate_extend_ttl_op(
        &snapshot_source,
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        &keys,
        100,
    )
    .unwrap();
    assert_eq!(
        no_op_extension,
        ExtendTtlOpSimulationResult {
            transaction_data: SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources: SorobanResources {
                    footprint: LedgerFootprint {
                        read_only: Default::default(),
                        read_write: Default::default()
                    },
                    instructions: 0,
                    disk_read_bytes: 0,
                    write_bytes: 0,
                },
                resource_fee: 280,
            }
        }
    );

    let extension_for_some_entries = simulate_extend_ttl_op(
        &snapshot_source,
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        &keys,
        100_001,
    )
    .unwrap();

    assert_eq!(
        extension_for_some_entries,
        ExtendTtlOpSimulationResult {
            transaction_data: SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources: SorobanResources {
                    footprint: LedgerFootprint {
                        read_only: vec![
                            keys[0].clone(),
                            keys[1].clone(),
                            keys[4].clone(),
                            keys[5].clone(),
                        ]
                        .tap_mut(|v| v.sort())
                        .try_into()
                        .unwrap(),
                        read_write: Default::default()
                    },
                    instructions: 0,
                    disk_read_bytes: 0,
                    write_bytes: 0,
                },
                resource_fee: 6204121,
            }
        }
    );

    let extension_for_all_entries = simulate_extend_ttl_op(
        &snapshot_source,
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        &keys,
        1_000_001,
    )
    .unwrap();
    assert_eq!(
        extension_for_all_entries,
        ExtendTtlOpSimulationResult {
            transaction_data: SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources: SorobanResources {
                    footprint: LedgerFootprint {
                        read_only: keys.clone().tap_mut(|v| v.sort()).try_into().unwrap(),
                        read_write: Default::default()
                    },
                    instructions: 0,
                    disk_read_bytes: 0,
                    write_bytes: 0,
                },
                resource_fee: 104563088,
            }
        }
    );

    // Non-existent entry should be just skipped.
    keys.push(get_wasm_key(b"abc"));
    let extension_for_all_entries_with_non_existent = simulate_extend_ttl_op(
        &snapshot_source,
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        &keys,
        1_000_001,
    )
    .unwrap();
    assert_eq!(
        extension_for_all_entries,
        extension_for_all_entries_with_non_existent
    );

    let extension_for_all_entries_with_adjustment = simulate_extend_ttl_op(
        &snapshot_source,
        &network_config,
        &test_adjustment_config(),
        &ledger_info,
        &keys,
        1_000_001,
    )
    .unwrap();

    assert_eq!(
        extension_for_all_entries_with_adjustment,
        ExtendTtlOpSimulationResult {
            transaction_data: SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources: SorobanResources {
                    footprint: extension_for_all_entries
                        .transaction_data
                        .resources
                        .footprint,
                    instructions: 0,
                    disk_read_bytes: 0,
                    write_bytes: 0,
                },
                resource_fee: 156844607,
            }
        }
    );

    // Extending expired entries is not allowed.
    let mut ledger_info_with_increased_ledger_seq = ledger_info;
    ledger_info_with_increased_ledger_seq.sequence_number += 101;
    assert!(simulate_extend_ttl_op(
        &snapshot_source,
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info_with_increased_ledger_seq,
        &keys,
        100_001,
    )
    .is_err());
}

#[test]
fn test_simulate_restore_op() {
    let mut ledger_info = default_ledger_info();
    let network_config = default_network_config();
    let contract_entry = CreateContractData::new([111; 32], ADD_I32).contract_entry;
    let entries = vec![
        (
            wasm_entry(ADD_I32),
            Some(ledger_info.sequence_number + 100_000),
        ),
        (
            wasm_entry(AUTH_TEST_CONTRACT),
            Some(ledger_info.sequence_number + 100),
        ),
        (contract_entry, Some(ledger_info.sequence_number + 500_000)),
        (
            wasm_entry_non_validated(b"123"),
            Some(ledger_info.sequence_number + 1_000_000),
        ),
    ];
    let keys: Vec<LedgerKey> = entries
        .iter()
        .map(|e| ledger_entry_to_ledger_key(&e.0).unwrap())
        .collect();
    let snapshot_source = MockSnapshotSource::from_entries(entries).unwrap();

    let no_op_restoration = simulate_restore_op(
        &snapshot_source,
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        &keys,
    )
    .unwrap();

    assert_eq!(
        no_op_restoration,
        RestoreOpSimulationResult {
            transaction_data: SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources: SorobanResources {
                    footprint: LedgerFootprint {
                        read_only: Default::default(),
                        read_write: Default::default()
                    },
                    instructions: 0,
                    disk_read_bytes: 0,
                    write_bytes: 0,
                },
                resource_fee: 280,
            }
        }
    );

    let init_seq_num = ledger_info.sequence_number;
    ledger_info.sequence_number = init_seq_num + 100_001;
    let restoration_for_some_entries = simulate_restore_op(
        &snapshot_source,
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        &keys,
    )
    .unwrap();
    let expected_rw_bytes = 7664;
    assert_eq!(
        restoration_for_some_entries,
        RestoreOpSimulationResult {
            transaction_data: SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources: SorobanResources {
                    footprint: LedgerFootprint {
                        read_only: Default::default(),
                        read_write: vec![keys[0].clone(), keys[1].clone(),]
                            .tap_mut(|v| v.sort())
                            .try_into()
                            .unwrap()
                    },
                    instructions: 0,
                    disk_read_bytes: expected_rw_bytes,
                    write_bytes: expected_rw_bytes,
                },
                resource_fee: 10922801,
            }
        }
    );

    ledger_info.sequence_number = init_seq_num + 1_000_001;
    let extension_for_all_entries = simulate_restore_op(
        &snapshot_source,
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        &keys,
    )
    .unwrap();
    let expected_rw_bytes = 7824;
    assert_eq!(
        extension_for_all_entries,
        RestoreOpSimulationResult {
            transaction_data: SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources: SorobanResources {
                    footprint: LedgerFootprint {
                        read_only: Default::default(),
                        read_write: keys.clone().tap_mut(|v| v.sort()).try_into().unwrap()
                    },
                    instructions: 0,
                    disk_read_bytes: expected_rw_bytes,
                    write_bytes: expected_rw_bytes,
                },
                resource_fee: 11130765,
            }
        }
    );

    let extension_for_all_entries_with_adjustment = simulate_restore_op(
        &snapshot_source,
        &network_config,
        &test_adjustment_config(),
        &ledger_info,
        &keys,
    )
    .unwrap();

    assert_eq!(
        extension_for_all_entries_with_adjustment,
        RestoreOpSimulationResult {
            transaction_data: SorobanTransactionData {
                ext: SorobanTransactionDataExt::V0,
                resources: SorobanResources {
                    footprint: LedgerFootprint {
                        read_only: Default::default(),
                        read_write: keys.clone().tap_mut(|v| v.sort()).try_into().unwrap()
                    },
                    instructions: 0,
                    disk_read_bytes: (expected_rw_bytes as f64 * 1.2) as u32,
                    write_bytes: (expected_rw_bytes as f64 * 1.3) as u32,
                },
                resource_fee: 16695904,
            }
        }
    );
}

#[test]
fn test_simulate_restore_op_returns_error_for_temp_entries() {
    let ledger_info = default_ledger_info();
    let network_config = default_network_config();

    let snapshot_source = MockSnapshotSource::from_entries(vec![(
        temp_entry(b"123"),
        Some(ledger_info.sequence_number - 10),
    )])
    .unwrap();

    let res = simulate_restore_op(
        &snapshot_source,
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        &[ledger_entry_to_ledger_key(&temp_entry(b"123")).unwrap()],
    );
    assert!(res.is_err());
}

#[test]
fn test_simulate_restore_op_returns_error_for_non_existent_entry() {
    let ledger_info = default_ledger_info();
    let network_config = default_network_config();

    let snapshot_source = MockSnapshotSource::from_entries(vec![]).unwrap();

    let res = simulate_restore_op(
        &snapshot_source,
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        &[get_wasm_key(b"123")],
    );
    assert!(res.is_err());
}

fn sc_symbol(s: &str) -> ScVal {
    ScVal::Symbol(s.try_into().unwrap())
}

fn sc_symbol_vec(s: &str) -> ScVal {
    ScVal::Vec(Some(vec![sc_symbol(s)].try_into().unwrap()))
}

fn create_sac_ledger_entry(sac_address: &ScAddress, admin_address: &ScAddress) -> LedgerEntry {
    let contract_instance_entry = ContractDataEntry {
        ext: ExtensionPoint::V0,
        contract: sac_address.clone(),
        key: ScVal::LedgerKeyContractInstance,
        durability: ContractDataDurability::Persistent,
        val: ScVal::ContractInstance(ScContractInstance {
            executable: ContractExecutable::StellarAsset,
            storage: Some(
                ScMap::sorted_from_pairs(
                    [
                        (
                            sc_symbol_vec("Admin"),
                            ScVal::Address(admin_address.clone()),
                        ),
                        (
                            sc_symbol("METADATA"),
                            ScVal::Map(Some(
                                ScMap::sorted_from_pairs(
                                    [
                                        (
                                            sc_symbol("name"),
                                            ScVal::String(ScString("aaaa".try_into().unwrap())),
                                        ),
                                        (sc_symbol("decimal"), ScVal::U32(7)),
                                        (
                                            sc_symbol("symbol"),
                                            ScVal::String(ScString("aaaa".try_into().unwrap())),
                                        ),
                                    ]
                                    .into_iter(),
                                )
                                .unwrap(),
                            )),
                        ),
                        (
                            sc_symbol_vec("AssetInfo"),
                            ScVal::Vec(Some(
                                vec![
                                    sc_symbol("AlphaNum4"),
                                    ScVal::Map(Some(
                                        ScMap::sorted_from_pairs(
                                            [
                                                (
                                                    sc_symbol("asset_code"),
                                                    ScVal::String(ScString(
                                                        "aaaa".try_into().unwrap(),
                                                    )),
                                                ),
                                                (
                                                    sc_symbol("issuer"),
                                                    ScVal::Bytes(ScBytes(
                                                        vec![0; 32].try_into().unwrap(),
                                                    )),
                                                ),
                                            ]
                                            .into_iter(),
                                        )
                                        .unwrap(),
                                    )),
                                ]
                                .try_into()
                                .unwrap(),
                            )),
                        ),
                    ]
                    .into_iter(),
                )
                .unwrap(),
            ),
        }),
    };
    ledger_entry(LedgerEntryData::ContractData(contract_instance_entry))
}

#[test]
fn test_simulate_successful_sac_call() {
    let source_account = get_account_id([123; 32]);
    let other_account = get_account_id([124; 32]);
    let sac_address = ScAddress::Contract(ContractId(Hash([111; 32])));
    let call_args: VecM<_> = vec![
        ScVal::Address(ScAddress::Account(other_account.clone())),
        ScVal::I128(Int128Parts { hi: 0, lo: 1 }),
    ]
    .try_into()
    .unwrap();
    let host_fn = HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: sac_address.clone(),
        function_name: "mint".try_into().unwrap(),
        args: call_args.clone(),
    });
    let contract_instance_le =
        create_sac_ledger_entry(&sac_address, &ScAddress::Account(source_account.clone()));
    let trustline = TrustLineEntry {
        account_id: other_account.clone(),
        asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'a'; 4]),
            issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0; 32]))),
        }),
        balance: 0,
        limit: 1_000_000_000,
        flags: TrustLineFlags::AuthorizedFlag as u32,
        ext: TrustLineEntryExt::V0,
    };
    let trustline_le = ledger_entry(LedgerEntryData::Trustline(trustline));
    let ledger_info = default_ledger_info();
    let network_config = default_network_config();
    let snapshot_source = Rc::new(
        MockSnapshotSource::from_entries(vec![
            (
                contract_instance_le.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (trustline_le.clone(), None),
            (account_entry(&source_account), None),
            (account_entry(&other_account), None),
        ])
        .unwrap(),
    );
    let res = simulate_invoke_host_function_op(
        snapshot_source,
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        host_fn,
        RecordingInvocationAuthMode::Recording(true),
        &source_account,
        [1; 32],
        true,
    )
    .unwrap();
    assert_eq!(res.invoke_result.unwrap(), ScVal::Void);
    assert_eq!(res.contract_events.len(), 1);
    assert_eq!(
        res.auth,
        vec![SorobanAuthorizationEntry {
            credentials: SorobanCredentials::SourceAccount,
            root_invocation: SorobanAuthorizedInvocation {
                function: SorobanAuthorizedFunction::ContractFn(InvokeContractArgs {
                    contract_address: sac_address,
                    function_name: ScSymbol("mint".try_into().unwrap()),
                    args: call_args,
                },),
                sub_invocations: Default::default(),
            },
        },]
    );
    assert_eq!(
        res.transaction_data,
        Some(SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![ledger_entry_to_ledger_key(&contract_instance_le).unwrap(),]
                        .try_into()
                        .unwrap(),
                    read_write: vec![ledger_entry_to_ledger_key(&trustline_le).unwrap()]
                        .try_into()
                        .unwrap()
                },
                instructions: 3443303,
                disk_read_bytes: 116,
                write_bytes: 116,
            },
            resource_fee: 52979,
        })
    );
}

// This test covers an edge-case scenario of a SAC failure due to missing
// trustline handled with `try_call`, which had an issue in recording mode that
// led to incorrect footprint.
// While this doesn't have to be a SAC failure, the issue has been discovered
// in SAC specifically and seems more likely to occur compared to the regular
// contracts (as the regular contracts can normally create their entries, unlike
// the SAC/trustline case).
#[test]
fn test_simulate_unsuccessful_sac_call_with_try_call() {
    let source_account = get_account_id([123; 32]);
    let other_account = get_account_id([124; 32]);
    let sac_address = ScAddress::Contract(ContractId(Hash([111; 32])));
    let contract = CreateContractData::new([1; 32], TRY_CALL_SAC);
    let host_fn = HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: contract.contract_address.clone(),
        function_name: "mint".try_into().unwrap(),
        args: vec![
            ScVal::Address(sac_address.clone()),
            ScVal::Address(ScAddress::Account(other_account.clone())),
        ]
        .try_into()
        .unwrap(),
    });
    let sac_instance_le = create_sac_ledger_entry(&sac_address, &contract.contract_address);
    let ledger_info = default_ledger_info();
    let network_config = default_network_config();

    let snapshot_source = Rc::new(
        MockSnapshotSource::from_entries(vec![
            (
                sac_instance_le.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                contract.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                contract.contract_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (account_entry(&source_account), None),
            (account_entry(&other_account), None),
        ])
        .unwrap(),
    );

    let res = simulate_invoke_host_function_op(
        snapshot_source,
        &network_config,
        &SimulationAdjustmentConfig::no_adjustments(),
        &ledger_info,
        host_fn,
        RecordingInvocationAuthMode::Recording(true),
        &source_account,
        [1; 32],
        true,
    )
    .unwrap();
    // The return value indicates the whether the internal `mint` call has
    // succeeded.
    assert_eq!(res.invoke_result.unwrap(), ScVal::Bool(false));
    assert!(res.contract_events.is_empty());
    assert_eq!(res.auth, vec![]);
    let trustline_key = LedgerKey::Trustline(LedgerKeyTrustLine {
        account_id: other_account.clone(),
        asset: TrustLineAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4([b'a'; 4]),
            issuer: AccountId(PublicKey::PublicKeyTypeEd25519(Uint256([0; 32]))),
        }),
    });
    assert_eq!(
        res.transaction_data,
        Some(SorobanTransactionData {
            ext: SorobanTransactionDataExt::V0,
            resources: SorobanResources {
                footprint: LedgerFootprint {
                    read_only: vec![
                        // Trustline key must appear in the footprint, even
                        // though it's not present in the storage.
                        trustline_key,
                        contract.wasm_key.clone(),
                        contract.contract_key.clone(),
                        ledger_entry_to_ledger_key(&sac_instance_le).unwrap(),
                    ]
                    .tap_mut(|v| v.sort())
                    .try_into()
                    .unwrap(),
                    // No entries should be actually modified.
                    read_write: Default::default(),
                },
                instructions: 5564767,
                disk_read_bytes: 0,
                write_bytes: 0,
            },
            resource_fee: 5913,
        })
    );
}
