use crate::builtin_contracts::testutils::AccountContractSigner;
use crate::crypto::sha256_hash_from_bytes_raw;
use crate::e2e_testutils::{
    account_entry, bytes_sc_val, contract_code_entry_with_refined_contract_cost_inputs,
    upload_wasm_host_fn,
};
use crate::testutils::simple_account_sign_fn;
use crate::vm::VersionedContractCodeCostInputs;
use crate::{
    budget::{AsBudget, Budget},
    builtin_contracts::testutils::TestSigner,
    e2e_invoke::{
        entry_size_for_rent, invoke_host_function, invoke_host_function_in_recording_mode,
        ledger_entry_to_ledger_key, LedgerEntryChange, LedgerEntryLiveUntilChange,
        RecordingInvocationAuthMode,
    },
    e2e_testutils::{
        auth_contract_invocation, create_contract_auth, default_ledger_info, get_account_id,
        get_contract_id_preimage, get_wasm_hash, get_wasm_key, ledger_entry, wasm_entry,
        AuthContractInvocationNode, CreateContractData,
    },
    testutils::MockSnapshotSource,
    xdr::{
        AccountId, ContractCodeEntryExt, ContractDataDurability, ContractDataEntry, ContractEvent,
        ContractExecutable, ContractId, ContractIdPreimage, ContractIdPreimageFromAddress,
        CreateContractArgs, DiagnosticEvent, ExtensionPoint, Hash, HashIdPreimage,
        HashIdPreimageSorobanAuthorization, HostFunction, InvokeContractArgs, LedgerEntry,
        LedgerEntryData, LedgerEntryType, LedgerFootprint, LedgerKey, LedgerKeyContractCode,
        LedgerKeyContractData, Limits, ReadXdr, ScAddress, ScContractInstance, ScErrorCode,
        ScErrorType, ScMap, ScNonceKey, ScVal, ScVec, SorobanAuthorizationEntry,
        SorobanCredentials, SorobanResources, TtlEntry, Uint256, WriteXdr,
    },
    Host, HostError, LedgerInfo,
};
use crate::{ErrorHandler, ModuleCache};
use ed25519_dalek::SigningKey;
use pretty_assertions::assert_eq;
use rand::rngs::StdRng;
use rand::SeedableRng;
use sha2::{Digest, Sha256};
use soroban_env_common::TryIntoVal;
use soroban_test_wasms::{
    ADD_F32, ADD_I32, AUTH_TEST_CONTRACT, CONTRACT_STORAGE, DEPLOYER_TEST_CONTRACT, LINEAR_MEMORY,
    NO_ARGUMENT_CONSTRUCTOR_TEST_CONTRACT_P22, SIMPLE_ACCOUNT_CONTRACT, SUM_I32,
    UPDATEABLE_CONTRACT,
};
use std::rc::Rc;

// It's tricky to get exactly the same instruction consumption
// in the recording storage/auth mode vs the enforcing mode. For
// example, frame snapshots in enforcing mode contain all the auths and
// storage entries, while in recording mode these snapshots will be
// smaller as storage/auth are populated eagerly.
// We don't anticipate this divergence to be too high though: specifically,
// we expect that:
// real_instructions <= (1 + RECORDING_MODE_INSTRUCTIONS_RANGE) * estimated_instructions
// and
// estimated_instructions <= real_instructions * (1 + RECORDING_MODE_INSTRUCTIONS_RANGE)
const RECORDING_MODE_INSTRUCTIONS_RANGE: f64 = 0.02;

fn prng_seed() -> [u8; 32] {
    [0; 32]
}

fn resources(
    instructions: u32,
    ro_footprint: Vec<LedgerKey>,
    rw_footprint: Vec<LedgerKey>,
) -> SorobanResources {
    let footprint = LedgerFootprint {
        read_only: ro_footprint.try_into().unwrap(),
        read_write: rw_footprint.try_into().unwrap(),
    };
    SorobanResources {
        footprint,
        instructions,
        disk_read_bytes: 0,
        write_bytes: 0,
    }
}

fn compute_key_hash(key: &LedgerKey) -> Vec<u8> {
    let key_xdr = key.to_xdr(Limits::none()).unwrap();
    let hash: [u8; 32] = Sha256::digest(&key_xdr).into();
    hash.to_vec()
}

fn ttl_entry(key: &LedgerKey, ttl: u32) -> TtlEntry {
    TtlEntry {
        key_hash: compute_key_hash(key).try_into().unwrap(),
        live_until_ledger_seq: ttl,
    }
}

fn symbol_sc_val(s: &str) -> ScVal {
    ScVal::Symbol(s.try_into().unwrap())
}

fn u64_sc_val(v: u64) -> ScVal {
    ScVal::U64(v)
}

fn u32_sc_val(v: u32) -> ScVal {
    ScVal::U32(v)
}

fn sign_auth_entry(
    dummy_host: &Host,
    ledger_info: &LedgerInfo,
    signers: &Vec<TestSigner>,
    auth_entry: SorobanAuthorizationEntry,
) -> SorobanAuthorizationEntry {
    let mut out = auth_entry;

    match &mut out.credentials {
        SorobanCredentials::SourceAccount => {}
        SorobanCredentials::Address(creds) => {
            let signature_payload_preimage =
                HashIdPreimage::SorobanAuthorization(HashIdPreimageSorobanAuthorization {
                    network_id: ledger_info.network_id.try_into().unwrap(),
                    invocation: out.root_invocation.clone(),
                    nonce: creds.nonce,
                    signature_expiration_ledger: ledger_info.sequence_number
                        + ledger_info.max_entry_ttl
                        - 1,
                });
            let signature_payload: [u8; 32] =
                Sha256::digest(&signature_payload_preimage.to_xdr(Limits::none()).unwrap()).into();
            let signer = signers
                .iter()
                .find(|s| s.sc_address() == creds.address)
                .unwrap();
            creds.signature = signer.sign(&dummy_host, &signature_payload);
            creds.signature_expiration_ledger =
                ledger_info.sequence_number + ledger_info.max_entry_ttl - 1;
        }
    }
    out
}

impl PartialEq<Self> for HostError {
    fn eq(&self, other: &Self) -> bool {
        self.error == other.error
    }
}

impl Eq for HostError {}

#[derive(Debug, Eq, PartialEq, Clone)]
struct LedgerEntryChangeHelper {
    read_only: bool,
    key: LedgerKey,
    old_entry_size_bytes_for_rent: u32,
    new_value: Option<LedgerEntry>,
    ttl_change: Option<LedgerEntryLiveUntilChange>,
}

impl From<LedgerEntryChange> for LedgerEntryChangeHelper {
    fn from(c: LedgerEntryChange) -> Self {
        Self {
            read_only: c.read_only,
            key: LedgerKey::from_xdr(c.encoded_key, Limits::none()).unwrap(),
            old_entry_size_bytes_for_rent: c.old_entry_size_bytes_for_rent,
            new_value: c
                .encoded_new_value
                .map(|v| LedgerEntry::from_xdr(v, Limits::none()).unwrap()),
            ttl_change: c.ttl_change,
        }
    }
}

impl LedgerEntryChangeHelper {
    fn no_op_change(entry: &LedgerEntry, live_until_ledger: u32) -> Self {
        let ledger_key = ledger_entry_to_ledger_key(entry, &Budget::default()).unwrap();
        let durability = match &ledger_key {
            LedgerKey::ContractData(cd) => Some(cd.durability),
            LedgerKey::ContractCode(_) => Some(ContractDataDurability::Persistent),
            _ => None,
        };
        let old_entry_size_bytes_for_rent = entry_size_for_rent(
            &Budget::default(),
            entry,
            entry.to_xdr(Limits::none()).unwrap().len() as u32,
        )
        .unwrap();

        Self {
            read_only: true,
            key: ledger_key.clone(),
            old_entry_size_bytes_for_rent,
            new_value: None,
            ttl_change: if let Some(durability) = durability {
                Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&ledger_key),
                    entry_type: entry.data.discriminant(),
                    durability,
                    old_live_until_ledger: live_until_ledger,
                    new_live_until_ledger: live_until_ledger,
                })
            } else {
                None
            },
        }
    }
}

struct InvokeHostFunctionHelperResult {
    invoke_result: Result<ScVal, HostError>,
    ledger_changes: Vec<LedgerEntryChangeHelper>,
    contract_events: Vec<ContractEvent>,
    diagnostic_events: Vec<DiagnosticEvent>,
    budget: Budget,
}

struct InvokeHostFunctionRecordingHelperResult {
    invoke_result: Result<ScVal, HostError>,
    resources: SorobanResources,
    restored_rw_entry_ids: Vec<u32>,
    auth: Vec<SorobanAuthorizationEntry>,
    ledger_changes: Vec<LedgerEntryChangeHelper>,
    contract_events: Vec<ContractEvent>,
    diagnostic_events: Vec<DiagnosticEvent>,
    contract_events_and_return_value_size: u32,
}

#[derive(Clone)]
struct E2eTestCompilationContext(Budget);
impl E2eTestCompilationContext {
    fn new() -> Result<Self, HostError> {
        // Compilation happens ouside the host, in an unlimited budget.
        let budget = Budget::default();
        budget.reset_unlimited()?;
        Ok(Self(budget))
    }
}
impl ErrorHandler for E2eTestCompilationContext {
    fn map_err<T, E>(&self, res: Result<T, E>) -> Result<T, HostError>
    where
        soroban_env_common::Error: From<E>,
        E: std::fmt::Debug,
    {
        res.map_err(|e| HostError::from(e))
    }

    fn error(
        &self,
        error: soroban_env_common::Error,
        _msg: &str,
        _args: &[soroban_env_common::Val],
    ) -> HostError {
        HostError::from(error)
    }
}
impl AsBudget for E2eTestCompilationContext {
    fn as_budget(&self) -> &Budget {
        &self.0
    }
}
impl crate::CompilationContext for E2eTestCompilationContext {}

fn invoke_host_function_helper_with_restored_entries(
    enable_diagnostics: bool,
    host_fn: &HostFunction,
    resources: &SorobanResources,
    source_account: &AccountId,
    auth_entries: Vec<SorobanAuthorizationEntry>,
    ledger_info: &LedgerInfo,
    ledger_entries_with_ttl: Vec<(LedgerEntry, Option<u32>)>,
    prng_seed: &[u8; 32],
    restored_entry_ids: &[u32],
) -> Result<InvokeHostFunctionHelperResult, HostError> {
    let limits = Limits::none();
    let encoded_host_fn = host_fn.to_xdr(limits.clone()).unwrap();
    let encoded_resources = resources.to_xdr(limits.clone()).unwrap();
    let encoded_source_account = source_account.to_xdr(limits.clone()).unwrap();
    let encoded_auth_entries: Vec<Vec<u8>> = auth_entries
        .iter()
        .map(|e| e.to_xdr(limits.clone()).unwrap())
        .collect();
    let encoded_ledger_entries: Vec<Vec<u8>> = ledger_entries_with_ttl
        .iter()
        .map(|e| e.0.to_xdr(limits.clone()).unwrap())
        .collect();
    let encoded_ttl_entries: Vec<Vec<u8>> = ledger_entries_with_ttl
        .iter()
        .map(|e| {
            let (le, ttl) = e;
            let key = match &le.data {
                LedgerEntryData::ContractData(cd) => {
                    LedgerKey::ContractData(LedgerKeyContractData {
                        contract: cd.contract.clone(),
                        key: cd.key.clone(),
                        durability: cd.durability,
                    })
                }
                LedgerEntryData::ContractCode(code) => {
                    LedgerKey::ContractCode(LedgerKeyContractCode {
                        hash: code.hash.clone(),
                    })
                }
                _ => {
                    return vec![];
                }
            };
            ttl_entry(&key, ttl.unwrap())
                .to_xdr(limits.clone())
                .unwrap()
        })
        .collect();

    let module_cache = build_module_cache_for_entries(ledger_info, ledger_entries_with_ttl)?;

    let budget = Budget::default();
    budget
        .reset_cpu_limit(resources.instructions as u64)
        .unwrap();
    let mut diagnostic_events = Vec::<DiagnosticEvent>::new();
    let res = invoke_host_function(
        &budget,
        enable_diagnostics,
        encoded_host_fn,
        encoded_resources,
        restored_entry_ids,
        encoded_source_account,
        encoded_auth_entries.into_iter(),
        ledger_info.clone(),
        encoded_ledger_entries.into_iter(),
        encoded_ttl_entries.into_iter(),
        prng_seed.to_vec(),
        &mut diagnostic_events,
        None,
        Some(module_cache),
    )?;
    Ok(InvokeHostFunctionHelperResult {
        invoke_result: res
            .encoded_invoke_result
            .map(|v| ScVal::from_xdr(v, limits.clone()).unwrap()),
        ledger_changes: res.ledger_changes.into_iter().map(|c| c.into()).collect(),
        contract_events: res
            .encoded_contract_events
            .iter()
            .map(|v| ContractEvent::from_xdr(v, limits.clone()).unwrap())
            .collect(),
        diagnostic_events,
        budget,
    })
}

fn invoke_host_function_helper(
    enable_diagnostics: bool,
    host_fn: &HostFunction,
    resources: &SorobanResources,
    source_account: &AccountId,
    auth_entries: Vec<SorobanAuthorizationEntry>,
    ledger_info: &LedgerInfo,
    ledger_entries_with_ttl: Vec<(LedgerEntry, Option<u32>)>,
    prng_seed: &[u8; 32],
) -> Result<InvokeHostFunctionHelperResult, HostError> {
    invoke_host_function_helper_with_restored_entries(
        enable_diagnostics,
        host_fn,
        resources,
        source_account,
        auth_entries,
        ledger_info,
        ledger_entries_with_ttl,
        prng_seed,
        &[],
    )
}

fn build_module_cache_for_entries(
    ledger_info: &LedgerInfo,
    ledger_entries_with_ttl: Vec<(LedgerEntry, Option<u32>)>,
) -> Result<ModuleCache, HostError> {
    let ctx = E2eTestCompilationContext::new()?;
    let cache = ModuleCache::new(&ctx)?;
    for (e, _) in ledger_entries_with_ttl.iter() {
        if let LedgerEntryData::ContractCode(cd) = &e.data {
            let contract_id = Hash(sha256_hash_from_bytes_raw(&cd.code, ctx.as_budget())?);
            let code_cost_inputs = match &cd.ext {
                ContractCodeEntryExt::V0 => VersionedContractCodeCostInputs::V0 {
                    wasm_bytes: cd.code.len(),
                },
                ContractCodeEntryExt::V1(v1) => {
                    VersionedContractCodeCostInputs::V1(v1.cost_inputs.clone())
                }
            };
            cache.parse_and_cache_module(
                &ctx,
                ledger_info.protocol_version,
                &contract_id,
                &cd.code,
                code_cost_inputs,
            )?;
        }
    }
    Ok(cache)
}

fn invoke_host_function_recording_helper(
    enable_diagnostics: bool,
    host_fn: &HostFunction,
    source_account: &AccountId,
    auth_mode: RecordingInvocationAuthMode,
    ledger_info: &LedgerInfo,
    ledger_entries_with_ttl: Vec<(LedgerEntry, Option<u32>)>,
    prng_seed: &[u8; 32],
    max_instructions_override: Option<u64>,
) -> Result<InvokeHostFunctionRecordingHelperResult, HostError> {
    let budget = Budget::default();
    if let Some(max_insns) = max_instructions_override {
        budget.reset_cpu_limit(max_insns).unwrap();
    }
    let snapshot = Rc::new(MockSnapshotSource::from_entries(ledger_entries_with_ttl));
    let mut diagnostic_events = vec![];
    let res = invoke_host_function_in_recording_mode(
        &budget,
        enable_diagnostics,
        host_fn,
        source_account,
        auth_mode,
        ledger_info.clone(),
        snapshot,
        *prng_seed,
        &mut diagnostic_events,
    )?;
    Ok(InvokeHostFunctionRecordingHelperResult {
        invoke_result: res.invoke_result,
        resources: res.resources,
        restored_rw_entry_ids: res.restored_rw_entry_indices,
        auth: res.auth,
        ledger_changes: res.ledger_changes.into_iter().map(|c| c.into()).collect(),
        contract_events: res.contract_events,
        diagnostic_events,
        contract_events_and_return_value_size: res.contract_events_and_return_value_size,
    })
}

fn invoke_host_function_using_simulation_with_signers(
    enable_diagnostics: bool,
    host_fn: &HostFunction,
    source_account: &AccountId,
    ledger_info: &LedgerInfo,
    ledger_entries_with_ttl: Vec<(LedgerEntry, Option<u32>)>,
    prng_seed: &[u8; 32],
    dummy_host: &Host,
    signers: &Vec<TestSigner>,
) -> Result<InvokeHostFunctionHelperResult, HostError> {
    let recording_result = invoke_host_function_recording_helper(
        enable_diagnostics,
        host_fn,
        source_account,
        RecordingInvocationAuthMode::Recording(true),
        ledger_info,
        ledger_entries_with_ttl.clone(),
        prng_seed,
        None,
    )
    .unwrap();

    let signed_auth: Vec<_> = recording_result
        .auth
        .iter()
        .map(|a| sign_auth_entry(dummy_host, ledger_info, signers, a.clone()))
        .collect();

    let recording_result_with_enforcing_auth = invoke_host_function_recording_helper(
        enable_diagnostics,
        host_fn,
        source_account,
        RecordingInvocationAuthMode::Enforcing(signed_auth.clone()),
        ledger_info,
        ledger_entries_with_ttl.clone(),
        prng_seed,
        None,
    )
    .unwrap();
    assert_eq!(
        recording_result.invoke_result,
        recording_result_with_enforcing_auth.invoke_result
    );

    let has_custom_account_signers = signers
        .iter()
        .any(|s| matches!(s, TestSigner::AccountContract(_)));
    // We can't simulate custom account verification logic without a signature, and
    // we don't have a signature in recording auth mode. Thus, the divergence in
    // simulated resources and footprint is expected.
    if !has_custom_account_signers {
        assert_eq!(
            recording_result.resources.footprint,
            recording_result_with_enforcing_auth.resources.footprint
        );
        assert_eq!(
            recording_result.resources.disk_read_bytes,
            recording_result_with_enforcing_auth
                .resources
                .disk_read_bytes
        );
        assert_eq!(
            recording_result.resources.write_bytes,
            recording_result_with_enforcing_auth.resources.write_bytes
        );

        assert_eq!(
            recording_result.ledger_changes,
            recording_result_with_enforcing_auth.ledger_changes
        );
        assert_eq!(
            recording_result.contract_events,
            recording_result_with_enforcing_auth.contract_events
        );
        assert_eq!(
            recording_result.diagnostic_events,
            recording_result_with_enforcing_auth.diagnostic_events
        );
        assert_eq!(
            recording_result.contract_events_and_return_value_size,
            recording_result_with_enforcing_auth.contract_events_and_return_value_size
        );
        let adjusted_recording_result_instructions =
            (recording_result.resources.instructions as f64
                * (1.0 + RECORDING_MODE_INSTRUCTIONS_RANGE)) as u32;
        assert!(
            adjusted_recording_result_instructions
                >= recording_result_with_enforcing_auth.resources.instructions
        );
    }
    // When no custom accounts are involved, we should be able to use the
    // simulation results from the recording auth mode to succeed in the
    // enforcing mode.
    let mut recording_result = if has_custom_account_signers {
        recording_result_with_enforcing_auth
    } else {
        recording_result
    };
    // Instructions are expected to be slightly different between recording and
    // enforcing modes, so just make sure that the estimation is within the small
    // coefficient.
    let initial_recording_result_instructions = recording_result.resources.instructions;
    recording_result.resources.instructions = (recording_result.resources.instructions as f64
        * (1.0 + RECORDING_MODE_INSTRUCTIONS_RANGE))
        as u32;

    let enforcing_result = invoke_host_function_helper(
        enable_diagnostics,
        host_fn,
        &recording_result.resources,
        source_account,
        signed_auth,
        ledger_info,
        ledger_entries_with_ttl,
        prng_seed,
    )?;

    assert_eq!(
        recording_result.invoke_result,
        enforcing_result.invoke_result
    );
    assert_eq!(
        recording_result.ledger_changes,
        enforcing_result.ledger_changes
    );
    assert_eq!(
        recording_result.contract_events,
        enforcing_result.contract_events
    );
    assert_eq!(
        recording_result.diagnostic_events,
        enforcing_result.diagnostic_events
    );
    if let Ok(res) = &enforcing_result.invoke_result {
        let mut enforcing_events_size = res.to_xdr(Limits::none()).unwrap().len();
        for e in &enforcing_result.contract_events {
            enforcing_events_size += e.to_xdr(Limits::none()).unwrap().len();
        }
        assert_eq!(
            recording_result.contract_events_and_return_value_size,
            enforcing_events_size as u32
        );
    } else {
        assert_eq!(recording_result.contract_events_and_return_value_size, 0);
    }
    let max_instructions = (enforcing_result.budget.get_cpu_insns_consumed().unwrap() as f64
        * (1.0 + RECORDING_MODE_INSTRUCTIONS_RANGE)) as u32;
    dbg!(initial_recording_result_instructions, max_instructions);
    assert!(initial_recording_result_instructions <= max_instructions);

    Ok(enforcing_result)
}

fn invoke_host_function_using_simulation(
    enable_diagnostics: bool,
    host_fn: &HostFunction,
    source_account: &AccountId,
    ledger_info: &LedgerInfo,
    ledger_entries_with_ttl: Vec<(LedgerEntry, Option<u32>)>,
    prng_seed: &[u8; 32],
) -> Result<InvokeHostFunctionHelperResult, HostError> {
    invoke_host_function_using_simulation_with_signers(
        enable_diagnostics,
        host_fn,
        source_account,
        ledger_info,
        ledger_entries_with_ttl,
        prng_seed,
        &Host::test_host(),
        &vec![],
    )
}

fn invoke_contract_host_fn(contract: &ScAddress, fn_name: &str, args: Vec<ScVal>) -> HostFunction {
    HostFunction::InvokeContract(InvokeContractArgs {
        contract_address: contract.clone(),
        function_name: fn_name.try_into().unwrap(),
        args: args.try_into().unwrap(),
    })
}

fn contract_data_key(
    contract_address: &ScAddress,
    key: &ScVal,
    durability: ContractDataDurability,
) -> LedgerKey {
    LedgerKey::ContractData(LedgerKeyContractData {
        contract: contract_address.clone(),
        key: key.clone(),
        durability,
    })
}

fn contract_data_entry(
    contract_address: &ScAddress,
    key: &ScVal,
    value: &ScVal,
    durability: ContractDataDurability,
) -> LedgerEntry {
    ledger_entry(LedgerEntryData::ContractData(ContractDataEntry {
        ext: ExtensionPoint::V0,
        contract: contract_address.clone(),
        key: key.clone(),
        durability,
        val: value.clone(),
    }))
}

#[test]
fn test_run_out_of_budget_before_calling_host() {
    let res = invoke_host_function_helper(
        true,
        &upload_wasm_host_fn(ADD_I32),
        &resources(1000, vec![], vec![]),
        &get_account_id([0; 32]),
        vec![],
        &default_ledger_info(),
        vec![],
        &prng_seed(),
    );
    assert!(HostError::result_matches_err(
        res,
        (ScErrorType::Budget, ScErrorCode::ExceededLimit)
    ));
}

#[test]
fn test_run_out_of_budget_before_calling_host_in_recording_mode() {
    let res = invoke_host_function_recording_helper(
        true,
        &upload_wasm_host_fn(ADD_I32),
        &get_account_id([0; 32]),
        RecordingInvocationAuthMode::Recording(true),
        &default_ledger_info(),
        vec![],
        &prng_seed(),
        Some(1000),
    );
    assert!(HostError::result_matches_err(
        res,
        (ScErrorType::Budget, ScErrorCode::ExceededLimit)
    ));
}

#[test]
fn test_wasm_upload_success() {
    let ledger_key = get_wasm_key(ADD_I32);
    let ledger_info = default_ledger_info();

    let res = invoke_host_function_helper(
        false,
        &upload_wasm_host_fn(ADD_I32),
        &resources(10_000_000, vec![], vec![ledger_key.clone()]),
        &get_account_id([123; 32]),
        vec![],
        &ledger_info,
        vec![],
        &prng_seed(),
    )
    .unwrap();
    assert!(res.contract_events.is_empty());
    assert_eq!(
        res.invoke_result.unwrap(),
        bytes_sc_val(&get_wasm_hash(ADD_I32))
    );
    assert_eq!(
        res.ledger_changes,
        vec![LedgerEntryChangeHelper {
            read_only: false,
            key: ledger_key.clone(),
            old_entry_size_bytes_for_rent: 0,
            new_value: Some(wasm_entry(ADD_I32)),
            ttl_change: Some(LedgerEntryLiveUntilChange {
                key_hash: compute_key_hash(&ledger_key),
                entry_type: LedgerEntryType::ContractCode,
                durability: ContractDataDurability::Persistent,
                old_live_until_ledger: 0,
                new_live_until_ledger: ledger_info.sequence_number
                    + ledger_info.min_persistent_entry_ttl
                    - 1,
            }),
        }]
    );
    assert!(res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(res.budget.get_mem_bytes_consumed().unwrap() > 0);
}

#[test]
fn test_wasm_upload_failure_due_to_unsupported_wasm_features() {
    let ledger_key = get_wasm_key(ADD_F32);
    let ledger_info = default_ledger_info();

    let res = invoke_host_function_helper(
        false,
        &upload_wasm_host_fn(ADD_F32),
        &resources(10_000_000, vec![], vec![ledger_key.clone()]),
        &get_account_id([123; 32]),
        vec![],
        &ledger_info,
        vec![],
        &prng_seed(),
    )
    .unwrap();
    assert!(res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(res.budget.get_mem_bytes_consumed().unwrap() > 0);

    assert!(res.invoke_result.is_err());
    assert!(HostError::result_matches_err(
        res.invoke_result,
        (ScErrorType::WasmVm, ScErrorCode::InvalidAction)
    ));
}

#[test]
fn test_wasm_upload_success_in_recording_mode() {
    let ledger_key = get_wasm_key(ADD_I32);
    let ledger_info = default_ledger_info();

    let res = invoke_host_function_recording_helper(
        false,
        &upload_wasm_host_fn(ADD_I32),
        &get_account_id([123; 32]),
        RecordingInvocationAuthMode::Recording(true),
        &ledger_info,
        vec![],
        &prng_seed(),
        None,
    )
    .unwrap();
    assert!(res.contract_events.is_empty());
    assert_eq!(
        res.invoke_result.unwrap(),
        bytes_sc_val(&get_wasm_hash(ADD_I32))
    );
    assert_eq!(
        res.ledger_changes,
        vec![LedgerEntryChangeHelper {
            read_only: false,
            key: ledger_key.clone(),
            old_entry_size_bytes_for_rent: 0,
            new_value: Some(wasm_entry(ADD_I32)),
            ttl_change: Some(LedgerEntryLiveUntilChange {
                key_hash: compute_key_hash(&ledger_key),
                entry_type: LedgerEntryType::ContractCode,
                durability: ContractDataDurability::Persistent,
                old_live_until_ledger: 0,
                new_live_until_ledger: ledger_info.sequence_number
                    + ledger_info.min_persistent_entry_ttl
                    - 1,
            }),
        }]
    );
    assert!(res.auth.is_empty());
    let (expected_insns, expected_write_bytes) = (1767593, 684);
    assert_eq!(
        res.resources,
        SorobanResources {
            footprint: LedgerFootprint {
                read_only: Default::default(),
                read_write: vec![ledger_key.clone()].try_into().unwrap()
            },
            instructions: expected_insns,
            disk_read_bytes: 0,
            write_bytes: expected_write_bytes,
        }
    );
}

#[test]
fn test_wasm_upload_failure_in_recording_mode() {
    let ledger_info = default_ledger_info();

    let res = invoke_host_function_recording_helper(
        true,
        &upload_wasm_host_fn(&[0_u8; 1000]),
        &get_account_id([123; 32]),
        RecordingInvocationAuthMode::Recording(true),
        &ledger_info,
        vec![],
        &prng_seed(),
        None,
    )
    .unwrap();
    assert!(res.diagnostic_events.len() >= 1);
    assert!(res.contract_events.is_empty());
    assert!(HostError::result_matches_err(
        res.invoke_result,
        (ScErrorType::WasmVm, ScErrorCode::InvalidAction)
    ));
    assert!(res.ledger_changes.is_empty());
    assert!(res.auth.is_empty());
    let expected_instructions = 1093647;
    assert_eq!(
        res.resources,
        SorobanResources {
            footprint: LedgerFootprint {
                read_only: Default::default(),
                read_write: Default::default(),
            },
            instructions: expected_instructions,
            disk_read_bytes: 0,
            write_bytes: 0,
        }
    );
}

#[test]
fn test_unsupported_wasm_upload_failure_in_recording_mode() {
    let ledger_info = default_ledger_info();

    let res = invoke_host_function_recording_helper(
        true,
        &upload_wasm_host_fn(ADD_F32),
        &get_account_id([123; 32]),
        RecordingInvocationAuthMode::Recording(true),
        &ledger_info,
        vec![],
        &prng_seed(),
        None,
    )
    .unwrap();
    assert!(res.diagnostic_events.len() >= 1);
    assert!(res.contract_events.is_empty());
    assert!(HostError::result_matches_err(
        res.invoke_result,
        (ScErrorType::WasmVm, ScErrorCode::InvalidAction)
    ));
}

#[test]
fn test_wasm_upload_success_using_simulation() {
    let res = invoke_host_function_using_simulation_with_signers(
        true,
        &upload_wasm_host_fn(ADD_I32),
        &get_account_id([123; 32]),
        &default_ledger_info(),
        vec![],
        &prng_seed(),
        &Host::test_host(),
        &vec![],
    );
    assert!(res.is_ok());
}

#[test]
fn test_wasm_upload_failure_using_simulation() {
    let res = invoke_host_function_using_simulation(
        false,
        &upload_wasm_host_fn(&[0_u8; 1000]),
        &get_account_id([123; 32]),
        &default_ledger_info(),
        vec![],
        &prng_seed(),
    )
    .unwrap();
    assert!(HostError::result_matches_err(
        res.invoke_result,
        (ScErrorType::WasmVm, ScErrorCode::InvalidAction)
    ));
}

#[test]
fn test_wasm_upload_budget_exceeded() {
    let ledger_key = get_wasm_key(CONTRACT_STORAGE);
    let ledger_info = default_ledger_info();

    let res = invoke_host_function_helper(
        true,
        &upload_wasm_host_fn(CONTRACT_STORAGE),
        &resources(1_000_000, vec![], vec![ledger_key.clone()]),
        &get_account_id([123; 32]),
        vec![],
        &ledger_info,
        vec![],
        &prng_seed(),
    )
    .unwrap();
    assert!(HostError::result_matches_err(
        res.invoke_result,
        (ScErrorType::Budget, ScErrorCode::ExceededLimit)
    ));
    assert!(res.ledger_changes.is_empty());
    assert!(res.contract_events.is_empty());
    assert_eq!(res.budget.get_cpu_insns_remaining().unwrap(), 0);
}

#[test]
fn test_wasm_upload_with_incorrect_footprint_fails() {
    // RO footprint instead of RW
    let res = invoke_host_function_helper(
        false,
        &upload_wasm_host_fn(ADD_I32),
        &resources(10_000_000, vec![get_wasm_key(ADD_I32)], vec![]),
        &get_account_id([111; 32]),
        vec![],
        &default_ledger_info(),
        vec![],
        &prng_seed(),
    )
    .unwrap();
    assert!(HostError::result_matches_err(
        res.invoke_result,
        (ScErrorType::Storage, ScErrorCode::ExceededLimit)
    ));

    assert!(res.ledger_changes.is_empty());
    // Diagnostics is disabled for this case
    assert!(res.diagnostic_events.is_empty());
    assert!(res.contract_events.is_empty());
    assert!(res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(res.budget.get_mem_bytes_consumed().unwrap() > 0);
}

#[test]
fn test_wasm_upload_without_footprint_fails() {
    let res = invoke_host_function_helper(
        true,
        &upload_wasm_host_fn(ADD_I32),
        &resources(10_000_000, vec![], vec![]),
        &get_account_id([123; 32]),
        vec![],
        &default_ledger_info(),
        vec![],
        &prng_seed(),
    )
    .unwrap();
    assert!(HostError::result_matches_err(
        res.invoke_result,
        (ScErrorType::Storage, ScErrorCode::ExceededLimit)
    ));
    assert!(res.ledger_changes.is_empty());
    assert!(res.diagnostic_events.len() >= 1);
    assert!(res.contract_events.is_empty());
    assert!(res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(res.budget.get_mem_bytes_consumed().unwrap() > 0);
}

#[test]
fn test_wasm_reupload_is_no_op() {
    let ledger_info = default_ledger_info();
    let res = invoke_host_function_helper(
        false,
        &upload_wasm_host_fn(ADD_I32),
        &resources(10_000_000, vec![], vec![get_wasm_key(ADD_I32)]),
        &get_account_id([123; 32]),
        vec![],
        &ledger_info,
        vec![(wasm_entry(ADD_I32), Some(ledger_info.sequence_number))],
        &prng_seed(),
    )
    .unwrap();
    assert_eq!(
        res.invoke_result.unwrap(),
        bytes_sc_val(&get_wasm_hash(ADD_I32))
    );
    let code_entry = ledger_entry(LedgerEntryData::ContractCode(
        contract_code_entry_with_refined_contract_cost_inputs(ADD_I32, true),
    ));
    assert_eq!(
        res.ledger_changes,
        vec![LedgerEntryChangeHelper {
            read_only: false,
            key: get_wasm_key(ADD_I32),
            old_entry_size_bytes_for_rent: entry_size_for_rent(
                &Budget::default(),
                &code_entry,
                code_entry.to_xdr(Limits::none()).unwrap().len() as u32,
            )
            .unwrap() as u32,
            new_value: Some(wasm_entry(ADD_I32)),
            ttl_change: Some(LedgerEntryLiveUntilChange {
                key_hash: compute_key_hash(&get_wasm_key(ADD_I32)),
                entry_type: LedgerEntryType::ContractCode,
                durability: ContractDataDurability::Persistent,
                old_live_until_ledger: ledger_info.sequence_number,
                new_live_until_ledger: ledger_info.sequence_number,
            })
        }]
    );
    assert!(res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(res.budget.get_mem_bytes_consumed().unwrap() > 0);
}

#[test]
fn test_wasm_upload_success_with_extra_footprint_entries() {
    let ledger_info = default_ledger_info();

    let res = invoke_host_function_helper(
        true,
        &upload_wasm_host_fn(ADD_I32),
        &resources(
            10_000_000,
            vec![get_wasm_key(CONTRACT_STORAGE)],
            vec![get_wasm_key(ADD_I32), get_wasm_key(LINEAR_MEMORY)],
        ),
        &get_account_id([123; 32]),
        vec![],
        &ledger_info,
        vec![(
            wasm_entry(LINEAR_MEMORY),
            Some(ledger_info.sequence_number + 1000),
        )],
        &prng_seed(),
    )
    .unwrap();
    assert_eq!(
        res.invoke_result.unwrap(),
        bytes_sc_val(&get_wasm_hash(ADD_I32))
    );
    let code_entry = ledger_entry(LedgerEntryData::ContractCode(
        contract_code_entry_with_refined_contract_cost_inputs(LINEAR_MEMORY, true),
    ));
    assert_eq!(
        res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: false,
                key: get_wasm_key(ADD_I32),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(wasm_entry(ADD_I32)),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&get_wasm_key(ADD_I32)),
                    entry_type: LedgerEntryType::ContractCode,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper {
                read_only: false,
                key: get_wasm_key(LINEAR_MEMORY),
                old_entry_size_bytes_for_rent: entry_size_for_rent(
                    &Budget::default(),
                    &code_entry,
                    code_entry.to_xdr(Limits::none()).unwrap().len() as u32,
                )
                .unwrap() as u32,
                new_value: Some(wasm_entry(LINEAR_MEMORY)),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&get_wasm_key(LINEAR_MEMORY)),
                    entry_type: LedgerEntryType::ContractCode,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: ledger_info.sequence_number + 1000,
                    new_live_until_ledger: ledger_info.sequence_number + 1000,
                }),
            },
            LedgerEntryChangeHelper {
                read_only: true,
                key: get_wasm_key(CONTRACT_STORAGE),
                old_entry_size_bytes_for_rent: 0,
                new_value: None,
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&get_wasm_key(CONTRACT_STORAGE)),
                    entry_type: LedgerEntryType::ContractCode,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: 0,
                }),
            },
        ]
    );
    assert!(res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(res.budget.get_mem_bytes_consumed().unwrap() > 0);
}

#[test]
fn test_create_contract_success() {
    let cd = CreateContractData::new([111; 32], ADD_I32);
    let ledger_info = default_ledger_info();
    let res = invoke_host_function_helper(
        true,
        &cd.host_fn,
        &resources(
            10_000_000,
            vec![cd.wasm_key.clone()],
            vec![cd.contract_key.clone()],
        ),
        &cd.deployer,
        vec![cd.auth_entry],
        &ledger_info,
        vec![(
            cd.wasm_entry.clone(),
            Some(ledger_info.sequence_number + 100),
        )],
        &prng_seed(),
    )
    .unwrap();
    assert_eq!(
        res.invoke_result.unwrap(),
        ScVal::Address(cd.contract_address.clone())
    );
    assert!(res.contract_events.is_empty());
    assert_eq!(
        res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: false,
                key: cd.contract_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(cd.contract_entry),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&cd.contract_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper::no_op_change(
                &cd.wasm_entry,
                ledger_info.sequence_number + 100
            ),
        ]
    );
    assert!(res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(res.budget.get_mem_bytes_consumed().unwrap() > 0);
}

#[test]
fn test_create_contract_with_no_argument_constructor_success() {
    let cd = CreateContractData::new([111; 32], NO_ARGUMENT_CONSTRUCTOR_TEST_CONTRACT_P22);
    let ledger_info = default_ledger_info();
    let persistent_entry_key = contract_data_key(
        &cd.contract_address,
        &symbol_sc_val("key"),
        ContractDataDurability::Persistent,
    );
    let temp_entry_key = contract_data_key(
        &cd.contract_address,
        &symbol_sc_val("key"),
        ContractDataDurability::Temporary,
    );
    let res = invoke_host_function_helper(
        true,
        &cd.host_fn,
        &resources(
            10_000_000,
            vec![cd.wasm_key.clone()],
            vec![
                cd.contract_key.clone(),
                persistent_entry_key.clone(),
                temp_entry_key.clone(),
            ],
        ),
        &cd.deployer,
        vec![cd.auth_entry],
        &ledger_info,
        vec![(
            cd.wasm_entry.clone(),
            Some(ledger_info.sequence_number + 100),
        )],
        &prng_seed(),
    )
    .unwrap();
    assert_eq!(
        res.invoke_result.unwrap(),
        ScVal::Address(cd.contract_address.clone())
    );
    assert!(res.contract_events.is_empty());
    let mut expected_contract_entry = cd.contract_entry.clone();
    if let LedgerEntryData::ContractData(cd) = &mut expected_contract_entry.data {
        if let ScVal::ContractInstance(instance) = &mut cd.val {
            instance.storage =
                Some(ScMap::sorted_from(vec![(symbol_sc_val("key"), u32_sc_val(2))]).unwrap());
        } else {
            unreachable!();
        }
    } else {
        unreachable!();
    };
    assert_eq!(
        res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: false,
                key: temp_entry_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(contract_data_entry(
                    &cd.contract_address,
                    &symbol_sc_val("key"),
                    &u32_sc_val(3),
                    ContractDataDurability::Temporary
                )),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&temp_entry_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Temporary,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_temp_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper {
                read_only: false,
                key: persistent_entry_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(contract_data_entry(
                    &cd.contract_address,
                    &symbol_sc_val("key"),
                    &u32_sc_val(1),
                    ContractDataDurability::Persistent
                )),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&persistent_entry_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper {
                read_only: false,
                key: cd.contract_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(expected_contract_entry),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&cd.contract_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper::no_op_change(
                &cd.wasm_entry,
                ledger_info.sequence_number + 100
            ),
        ]
    );
    assert!(res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(res.budget.get_mem_bytes_consumed().unwrap() > 0);
}

#[test]
fn test_create_contract_success_in_recording_mode() {
    let cd = CreateContractData::new([111; 32], ADD_I32);
    let ledger_info = default_ledger_info();
    let res = invoke_host_function_recording_helper(
        true,
        &cd.host_fn,
        &cd.deployer,
        RecordingInvocationAuthMode::Recording(true),
        &ledger_info,
        vec![(
            cd.wasm_entry.clone(),
            Some(ledger_info.sequence_number + 100),
        )],
        &prng_seed(),
        None,
    )
    .unwrap();
    assert_eq!(
        res.invoke_result.unwrap(),
        ScVal::Address(cd.contract_address.clone())
    );
    assert!(res.contract_events.is_empty());
    assert_eq!(
        res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: false,
                key: cd.contract_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(cd.contract_entry),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&cd.contract_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper::no_op_change(
                &cd.wasm_entry,
                ledger_info.sequence_number + 100
            ),
        ]
    );
    assert_eq!(res.auth, vec![cd.auth_entry]);
    assert_eq!(
        res.resources,
        SorobanResources {
            footprint: LedgerFootprint {
                read_only: vec![cd.wasm_key].try_into().unwrap(),
                read_write: vec![cd.contract_key].try_into().unwrap()
            },
            instructions: 663583,
            disk_read_bytes: 0,
            write_bytes: 104,
        }
    );
}

#[test]
fn test_create_contract_success_in_recording_mode_with_custom_account() {
    // We don't try to invoke `__check_auth` in recording mode in order to not output confusing
    // side-effects. Thus any Wasm can stand for a custom account.
    let custom_account_wasm = CONTRACT_STORAGE;
    let custom_account_address = ScAddress::Contract(ContractId([222; 32].into()));
    let expected_nonce = 801925984706572462_i64;

    let cd = CreateContractData::new_with_refined_contract_cost_inputs_and_deployer(
        Some((custom_account_address.clone(), expected_nonce)),
        [111; 32],
        ADD_I32,
        true,
    );

    let custom_account_instance_entry =
        ledger_entry(LedgerEntryData::ContractData(ContractDataEntry {
            ext: ExtensionPoint::V0,
            contract: custom_account_address.clone(),
            key: ScVal::LedgerKeyContractInstance,
            durability: ContractDataDurability::Persistent,
            val: ScVal::ContractInstance(ScContractInstance {
                executable: ContractExecutable::Wasm(
                    get_wasm_hash(custom_account_wasm).try_into().unwrap(),
                ),
                storage: None,
            }),
        }));
    let ledger_info = default_ledger_info();
    let res = invoke_host_function_recording_helper(
        true,
        &cd.host_fn,
        &cd.deployer,
        RecordingInvocationAuthMode::Recording(true),
        &ledger_info,
        vec![
            (
                cd.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                wasm_entry(custom_account_wasm),
                Some(ledger_info.sequence_number + 1000),
            ),
            (
                custom_account_instance_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
        ],
        &prng_seed(),
        None,
    )
    .unwrap();
    assert_eq!(
        res.invoke_result.unwrap(),
        ScVal::Address(cd.contract_address.clone())
    );
    assert!(res.contract_events.is_empty());

    let nonce_key = ScVal::LedgerKeyNonce(ScNonceKey {
        nonce: expected_nonce,
    });
    let nonce_entry_key = LedgerKey::ContractData(LedgerKeyContractData {
        contract: custom_account_address.clone(),
        key: nonce_key.clone(),
        durability: ContractDataDurability::Temporary,
    });
    assert_eq!(
        res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: false,
                key: cd.contract_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(cd.contract_entry),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&cd.contract_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper::no_op_change(
                &custom_account_instance_entry,
                ledger_info.sequence_number + 1000,
            ),
            LedgerEntryChangeHelper {
                read_only: false,
                key: nonce_entry_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(ledger_entry(LedgerEntryData::ContractData(
                    ContractDataEntry {
                        ext: ExtensionPoint::V0,
                        contract: custom_account_address.clone(),
                        key: nonce_key.clone(),
                        durability: ContractDataDurability::Temporary,
                        val: ScVal::Void,
                    }
                ))),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&nonce_entry_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Temporary,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number + ledger_info.max_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper::no_op_change(
                &cd.wasm_entry,
                ledger_info.sequence_number + 100
            ),
            LedgerEntryChangeHelper::no_op_change(
                &wasm_entry(custom_account_wasm),
                ledger_info.sequence_number + 1000
            ),
        ]
    );
    assert_eq!(res.auth, vec![cd.auth_entry]);
    assert_eq!(
        res.resources,
        SorobanResources {
            footprint: LedgerFootprint {
                read_only: vec![
                    ledger_entry_to_ledger_key(&custom_account_instance_entry, &Budget::default())
                        .unwrap(),
                    cd.wasm_key,
                    get_wasm_key(custom_account_wasm),
                ]
                .try_into()
                .unwrap(),
                read_write: vec![cd.contract_key, nonce_entry_key].try_into().unwrap()
            },
            instructions: 1070741,
            disk_read_bytes: 0,
            write_bytes: 176,
        }
    );
}

#[test]
fn test_create_contract_success_in_recording_mode_with_enforced_auth() {
    let cd = CreateContractData::new([111; 32], ADD_I32);
    let ledger_info = default_ledger_info();
    let res = invoke_host_function_recording_helper(
        true,
        &cd.host_fn,
        &cd.deployer,
        RecordingInvocationAuthMode::Enforcing(vec![cd.auth_entry.clone()]),
        &ledger_info,
        vec![(
            cd.wasm_entry.clone(),
            Some(ledger_info.sequence_number + 100),
        )],
        &prng_seed(),
        None,
    )
    .unwrap();
    assert_eq!(
        res.invoke_result.unwrap(),
        ScVal::Address(cd.contract_address.clone())
    );
    assert!(res.contract_events.is_empty());
    assert_eq!(
        res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: false,
                key: cd.contract_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(cd.contract_entry),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&cd.contract_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper::no_op_change(
                &cd.wasm_entry,
                ledger_info.sequence_number + 100
            ),
        ]
    );
    assert_eq!(res.auth, vec![cd.auth_entry]);
    assert_eq!(
        res.resources,
        SorobanResources {
            footprint: LedgerFootprint {
                read_only: vec![cd.wasm_key].try_into().unwrap(),
                read_write: vec![cd.contract_key].try_into().unwrap()
            },
            instructions: 665030,
            disk_read_bytes: 0,
            write_bytes: 104,
        }
    );
}

#[test]
fn test_create_contract_success_using_simulation() {
    let cd = CreateContractData::new([111; 32], ADD_I32);
    let ledger_info = default_ledger_info();
    let res = invoke_host_function_using_simulation(
        true,
        &cd.host_fn,
        &cd.deployer,
        &ledger_info,
        vec![(
            cd.wasm_entry.clone(),
            Some(ledger_info.sequence_number + 100),
        )],
        &prng_seed(),
    )
    .unwrap();
    assert_eq!(
        res.invoke_result.unwrap(),
        ScVal::Address(cd.contract_address.clone())
    );
}

#[test]
fn test_create_contract_success_with_extra_footprint_entries() {
    let cd = CreateContractData::new([111; 32], ADD_I32);
    let cd2 = CreateContractData::new([222; 32], CONTRACT_STORAGE);
    let ledger_info = default_ledger_info();
    let res = invoke_host_function_helper(
        true,
        &cd.host_fn,
        &resources(
            10_000_000,
            vec![cd.wasm_key.clone(), cd2.wasm_key.clone()],
            vec![cd.contract_key.clone(), cd2.contract_key.clone()],
        ),
        &cd.deployer,
        vec![cd.auth_entry],
        &ledger_info,
        vec![
            (
                cd.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                cd2.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 200),
            ),
        ],
        &prng_seed(),
    )
    .unwrap();
    assert_eq!(
        res.invoke_result.unwrap(),
        ScVal::Address(cd.contract_address.clone())
    );
    assert_eq!(
        res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: false,
                key: cd.contract_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(cd.contract_entry),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&cd.contract_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper {
                read_only: false,
                key: cd2.contract_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: None,
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&cd2.contract_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: 0,
                }),
            },
            LedgerEntryChangeHelper::no_op_change(
                &cd.wasm_entry,
                ledger_info.sequence_number + 100
            ),
            LedgerEntryChangeHelper::no_op_change(
                &cd2.wasm_entry,
                ledger_info.sequence_number + 200
            ),
        ]
    );
    assert!(res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(res.budget.get_mem_bytes_consumed().unwrap() > 0);
}

#[test]
fn test_create_contract_without_footprint_fails() {
    let cd = CreateContractData::new([111; 32], ADD_I32);

    let res = invoke_host_function_helper(
        true,
        &cd.host_fn,
        &resources(10_000_000, vec![], vec![]),
        &cd.deployer,
        vec![cd.auth_entry],
        &default_ledger_info(),
        vec![],
        &prng_seed(),
    )
    .unwrap();
    assert!(HostError::result_matches_err(
        res.invoke_result,
        (ScErrorType::Storage, ScErrorCode::ExceededLimit)
    ));
    assert!(res.ledger_changes.is_empty());
    assert!(res.diagnostic_events.len() >= 1);
    assert!(res.contract_events.is_empty());
    assert!(res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(res.budget.get_mem_bytes_consumed().unwrap() > 0);
}

#[test]
fn test_create_contract_without_auth_fails() {
    let cd = CreateContractData::new([111; 32], ADD_I32);
    // No auth
    let res = invoke_host_function_helper(
        true,
        &cd.host_fn,
        &resources(10_000_000, vec![cd.wasm_key], vec![cd.contract_key]),
        &cd.deployer,
        vec![],
        &default_ledger_info(),
        vec![],
        &prng_seed(),
    )
    .unwrap();
    assert!(HostError::result_matches_err(
        res.invoke_result,
        (ScErrorType::Auth, ScErrorCode::InvalidAction)
    ));
    assert!(res.ledger_changes.is_empty());
    assert!(res.diagnostic_events.len() >= 1);
    assert!(res.contract_events.is_empty());
    assert!(res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(res.budget.get_mem_bytes_consumed().unwrap() > 0);
}

#[test]
fn test_create_contract_without_wasm_entry_fails() {
    let cd = CreateContractData::new([111; 32], ADD_I32);

    // No Wasm entry
    let res = invoke_host_function_helper(
        true,
        &cd.host_fn,
        &resources(10_000_000, vec![cd.wasm_key], vec![cd.contract_key]),
        &cd.deployer,
        vec![cd.auth_entry],
        &default_ledger_info(),
        vec![],
        &prng_seed(),
    )
    .unwrap();
    assert!(HostError::result_matches_err(
        res.invoke_result,
        (ScErrorType::Storage, ScErrorCode::MissingValue)
    ));
    assert!(res.ledger_changes.is_empty());
    assert!(res.diagnostic_events.len() >= 1);
    assert!(res.contract_events.is_empty());
}

#[test]
fn test_create_contract_with_incorrect_auth_fails() {
    let cd = CreateContractData::new([111; 32], ADD_I32);
    let res = invoke_host_function_helper(
        true,
        &cd.host_fn,
        &resources(10_000_000, vec![cd.wasm_key], vec![cd.contract_key]),
        &cd.deployer,
        // Auth entry is for the different salt
        vec![create_contract_auth(
            &get_contract_id_preimage(&cd.deployer, &[1; 32]),
            ADD_I32,
        )],
        &default_ledger_info(),
        vec![(
            wasm_entry(ADD_I32),
            Some(default_ledger_info().sequence_number + 100),
        )],
        &prng_seed(),
    )
    .unwrap();
    assert!(HostError::result_matches_err(
        res.invoke_result,
        (ScErrorType::Auth, ScErrorCode::InvalidAction)
    ));
    assert!(res.ledger_changes.is_empty());
    assert!(res.diagnostic_events.len() >= 1);
    assert!(res.contract_events.is_empty());
}

#[test]
fn test_invoke_contract_with_storage_ops_success() {
    let cd = CreateContractData::new([111; 32], CONTRACT_STORAGE);
    let ledger_info = default_ledger_info();
    let key = symbol_sc_val("key");
    let val = u64_sc_val(u64::MAX);
    let host_fn = invoke_contract_host_fn(
        &cd.contract_address,
        "put_temporary",
        vec![key.clone(), val.clone()],
    );
    let data_key = contract_data_key(
        &cd.contract_address,
        &key,
        ContractDataDurability::Temporary,
    );
    let res = invoke_host_function_helper(
        true,
        &host_fn,
        &resources(
            10_000_000,
            vec![cd.contract_key.clone(), cd.wasm_key.clone()],
            vec![data_key.clone()],
        ),
        &cd.deployer,
        vec![],
        &ledger_info,
        vec![
            (
                cd.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                cd.contract_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
        ],
        &prng_seed(),
    )
    .unwrap();
    assert_eq!(res.invoke_result.unwrap(), ScVal::Void);
    assert!(res.contract_events.is_empty());
    assert!(res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(res.budget.get_mem_bytes_consumed().unwrap() > 0);

    let contract_entry_change = LedgerEntryChangeHelper::no_op_change(
        &cd.contract_entry,
        ledger_info.sequence_number + 1000,
    );
    let wasm_entry_change =
        LedgerEntryChangeHelper::no_op_change(&cd.wasm_entry, ledger_info.sequence_number + 100);
    let new_entry = contract_data_entry(
        &cd.contract_address,
        &key,
        &val,
        ContractDataDurability::Temporary,
    );
    assert_eq!(
        res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: false,
                key: data_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(new_entry.clone()),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&data_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Temporary,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_temp_entry_ttl
                        - 1,
                }),
            },
            contract_entry_change.clone(),
            wasm_entry_change.clone()
        ]
    );

    let extend_host_fn = invoke_contract_host_fn(
        &cd.contract_address,
        "extend_temporary",
        vec![key.clone(), u32_sc_val(501), u32_sc_val(5000)],
    );
    let extend_res = invoke_host_function_helper(
        true,
        &extend_host_fn,
        &resources(
            10_000_000,
            vec![
                cd.contract_key.clone(),
                cd.wasm_key.clone(),
                data_key.clone(),
            ],
            vec![],
        ),
        &cd.deployer,
        vec![],
        &ledger_info,
        vec![
            (
                cd.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                cd.contract_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
            (new_entry.clone(), Some(ledger_info.sequence_number + 500)),
        ],
        &prng_seed(),
    )
    .unwrap();
    assert_eq!(extend_res.invoke_result.unwrap(), ScVal::Void);
    assert!(extend_res.contract_events.is_empty());
    assert_eq!(
        extend_res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: true,
                key: data_key.clone(),
                old_entry_size_bytes_for_rent: new_entry.to_xdr(Limits::none()).unwrap().len()
                    as u32,
                new_value: None,
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&data_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Temporary,
                    old_live_until_ledger: ledger_info.sequence_number + 500,
                    new_live_until_ledger: ledger_info.sequence_number + 5000,
                }),
            },
            contract_entry_change.clone(),
            wasm_entry_change.clone()
        ]
    );
    assert!(extend_res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(extend_res.budget.get_mem_bytes_consumed().unwrap() > 0);
}

#[test]
fn test_invoke_contract_with_storage_ops_success_in_recording_mode() {
    let cd = CreateContractData::new([111; 32], CONTRACT_STORAGE);
    let ledger_info = default_ledger_info();
    let key = symbol_sc_val("key");
    let val = u64_sc_val(u64::MAX);
    let host_fn = invoke_contract_host_fn(
        &cd.contract_address,
        "put_temporary",
        vec![key.clone(), val.clone()],
    );
    let data_key = contract_data_key(
        &cd.contract_address,
        &key,
        ContractDataDurability::Temporary,
    );
    let res = invoke_host_function_recording_helper(
        true,
        &host_fn,
        &cd.deployer,
        RecordingInvocationAuthMode::Recording(true),
        &ledger_info,
        vec![
            (
                cd.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                cd.contract_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
        ],
        &prng_seed(),
        None,
    )
    .unwrap();
    assert_eq!(res.invoke_result.unwrap(), ScVal::Void);
    assert!(res.contract_events.is_empty());
    assert!(res.restored_rw_entry_ids.is_empty());

    let contract_entry_change = LedgerEntryChangeHelper::no_op_change(
        &cd.contract_entry,
        ledger_info.sequence_number + 1000,
    );
    let wasm_entry_change =
        LedgerEntryChangeHelper::no_op_change(&cd.wasm_entry, ledger_info.sequence_number + 100);
    let new_entry = contract_data_entry(
        &cd.contract_address,
        &key,
        &val,
        ContractDataDurability::Temporary,
    );
    assert_eq!(
        res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: false,
                key: data_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(new_entry.clone()),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&data_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Temporary,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_temp_entry_ttl
                        - 1,
                }),
            },
            contract_entry_change.clone(),
            wasm_entry_change.clone()
        ]
    );
    assert!(res.restored_rw_entry_ids.is_empty());
    assert_eq!(
        res.resources,
        SorobanResources {
            footprint: LedgerFootprint {
                read_only: vec![cd.contract_key.clone(), cd.wasm_key.clone()]
                    .try_into()
                    .unwrap(),
                read_write: vec![data_key.clone()].try_into().unwrap(),
            },
            instructions: 898006,
            disk_read_bytes: 0,
            write_bytes: 80,
        }
    );

    let extend_host_fn = invoke_contract_host_fn(
        &cd.contract_address,
        "extend_temporary",
        vec![key.clone(), u32_sc_val(501), u32_sc_val(5000)],
    );
    let extend_res = invoke_host_function_recording_helper(
        true,
        &extend_host_fn,
        &cd.deployer,
        RecordingInvocationAuthMode::Recording(true),
        &ledger_info,
        vec![
            (
                cd.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                cd.contract_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
            (new_entry.clone(), Some(ledger_info.sequence_number + 500)),
        ],
        &prng_seed(),
        None,
    )
    .unwrap();
    assert_eq!(extend_res.invoke_result.unwrap(), ScVal::Void);
    assert!(extend_res.contract_events.is_empty());
    assert!(res.restored_rw_entry_ids.is_empty());
    assert_eq!(
        extend_res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: true,
                key: data_key.clone(),
                old_entry_size_bytes_for_rent: new_entry.to_xdr(Limits::none()).unwrap().len()
                    as u32,
                new_value: None,
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&data_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Temporary,
                    old_live_until_ledger: ledger_info.sequence_number + 500,
                    new_live_until_ledger: ledger_info.sequence_number + 5000,
                }),
            },
            contract_entry_change.clone(),
            wasm_entry_change.clone()
        ]
    );
    assert_eq!(
        extend_res.resources,
        SorobanResources {
            footprint: LedgerFootprint {
                read_only: vec![
                    data_key.clone(),
                    cd.contract_key.clone(),
                    cd.wasm_key.clone(),
                ]
                .try_into()
                .unwrap(),
                read_write: Default::default(),
            },
            instructions: 1009860,
            disk_read_bytes: 0,
            write_bytes: 0,
        }
    );
}

#[test]
fn test_create_contract_success_with_autorestore() {
    let cd = CreateContractData::new([111; 32], ADD_I32);
    let ledger_info = default_ledger_info();
    let res = invoke_host_function_helper_with_restored_entries(
        true,
        &cd.host_fn,
        &resources(
            10_000_000,
            vec![],
            vec![cd.contract_key.clone(), cd.wasm_key.clone()],
        ),
        &cd.deployer,
        vec![cd.auth_entry],
        &ledger_info,
        vec![(
            cd.wasm_entry.clone(),
            // The restored entries will be automatically extended to min
            // persistent TTL.
            Some(ledger_info.sequence_number),
        )],
        &prng_seed(),
        &[1],
    )
    .unwrap();
    assert_eq!(
        res.invoke_result.unwrap(),
        ScVal::Address(cd.contract_address.clone())
    );
    assert!(res.contract_events.is_empty());
    assert_eq!(
        res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: false,
                key: cd.contract_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(cd.contract_entry),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&cd.contract_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper {
                read_only: false,
                key: cd.wasm_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(cd.wasm_entry),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&cd.wasm_key),
                    entry_type: LedgerEntryType::ContractCode,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
        ]
    );
    assert!(res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(res.budget.get_mem_bytes_consumed().unwrap() > 0);
}

#[test]
fn test_invoke_contract_with_storage_extension_and_autorestore() {
    let cd = CreateContractData::new([111; 32], CONTRACT_STORAGE);
    let ledger_info = default_ledger_info();
    let key = symbol_sc_val("key");
    let val = u64_sc_val(u64::MAX);
    let ttl_extension = ledger_info.min_persistent_entry_ttl * 2;
    let host_fn = invoke_contract_host_fn(
        &cd.contract_address,
        "extend_persistent",
        vec![key.clone(), u32_sc_val(1), u32_sc_val(ttl_extension)],
    );
    let data_key = contract_data_key(
        &cd.contract_address,
        &key,
        ContractDataDurability::Persistent,
    );
    let data_entry = contract_data_entry(
        &cd.contract_address,
        &key,
        &val,
        ContractDataDurability::Persistent,
    );
    let res = invoke_host_function_helper_with_restored_entries(
        true,
        &host_fn,
        &resources(
            10_000_000,
            vec![],
            vec![
                cd.contract_key.clone(),
                cd.wasm_key.clone(),
                data_key.clone(),
            ],
        ),
        &cd.deployer,
        vec![],
        &ledger_info,
        vec![
            (
                cd.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (cd.contract_entry.clone(), Some(ledger_info.sequence_number)),
            (data_entry.clone(), Some(ledger_info.sequence_number)),
        ],
        &prng_seed(),
        // Contract key and data key are restored, Wasm key is not restored, but
        // is present in RW footprint.
        &[0, 2],
    )
    .unwrap();
    assert_eq!(res.invoke_result.unwrap(), ScVal::Void);
    assert!(res.contract_events.is_empty());
    assert!(res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(res.budget.get_mem_bytes_consumed().unwrap() > 0);

    let mut wasm_entry_change =
        LedgerEntryChangeHelper::no_op_change(&cd.wasm_entry, ledger_info.sequence_number + 100);
    // Wasm change is no-op, but it's not read-only.
    wasm_entry_change.read_only = false;
    wasm_entry_change.new_value = Some(cd.wasm_entry.clone());
    let contract_entry_change = LedgerEntryChangeHelper {
        read_only: false,
        key: cd.contract_key.clone(),
        old_entry_size_bytes_for_rent: 0,
        new_value: Some(cd.contract_entry.clone()),
        ttl_change: Some(LedgerEntryLiveUntilChange {
            key_hash: compute_key_hash(&cd.contract_key),
            entry_type: LedgerEntryType::ContractData,
            durability: ContractDataDurability::Persistent,
            old_live_until_ledger: 0,
            new_live_until_ledger: ledger_info.sequence_number
                + ledger_info.min_persistent_entry_ttl
                - 1,
        }),
    };
    let contract_data_change = LedgerEntryChangeHelper {
        read_only: false,
        key: data_key.clone(),
        old_entry_size_bytes_for_rent: 0,
        new_value: Some(data_entry.clone()),
        ttl_change: Some(LedgerEntryLiveUntilChange {
            key_hash: compute_key_hash(&data_key),
            entry_type: LedgerEntryType::ContractData,
            durability: ContractDataDurability::Persistent,
            old_live_until_ledger: 0,
            new_live_until_ledger: ledger_info.sequence_number + ttl_extension,
        }),
    };
    assert_eq!(
        res.ledger_changes,
        vec![
            contract_data_change,
            contract_entry_change.clone(),
            wasm_entry_change.clone()
        ]
    );
}

#[test]
fn test_auto_restore_with_extension_in_recording_mode() {
    let cd = CreateContractData::new([111; 32], CONTRACT_STORAGE);
    let ledger_info = default_ledger_info();

    let key = symbol_sc_val("key");
    let val = u64_sc_val(u64::MAX);
    let host_fn = invoke_contract_host_fn(
        &cd.contract_address,
        "extend_persistent",
        vec![
            key.clone(),
            u32_sc_val(5 * ledger_info.min_persistent_entry_ttl),
            u32_sc_val(5 * ledger_info.min_persistent_entry_ttl),
        ],
    );

    let persistent_data_entry = contract_data_entry(
        &cd.contract_address,
        &key,
        &val,
        ContractDataDurability::Persistent,
    );
    let res = invoke_host_function_recording_helper(
        true,
        &host_fn,
        &cd.deployer,
        RecordingInvocationAuthMode::Recording(true),
        &ledger_info,
        vec![
            (
                cd.wasm_entry.clone(),
                Some(ledger_info.sequence_number - 100_000),
            ),
            (
                cd.contract_entry.clone(),
                Some(ledger_info.sequence_number - 1),
            ),
            (
                persistent_data_entry.clone(),
                Some(ledger_info.sequence_number - 10),
            ),
        ],
        &prng_seed(),
        None,
    )
    .unwrap();
    assert_eq!(res.invoke_result.unwrap(), ScVal::Void);
    assert!(res.contract_events.is_empty());
    assert_eq!(res.restored_rw_entry_ids.as_slice(), &[0, 1, 2]);

    let data_key = contract_data_key(
        &cd.contract_address,
        &key,
        ContractDataDurability::Persistent,
    );
    let data_entry_size = persistent_data_entry.to_xdr(Limits::none()).unwrap().len() as u32;
    let wasm_entry_size = cd.wasm_entry.to_xdr(Limits::none()).unwrap().len() as u32;
    let instance_entry_size = cd.contract_entry.to_xdr(Limits::none()).unwrap().len() as u32;
    assert_eq!(
        res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: false,
                key: data_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(persistent_data_entry.clone()),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&data_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + 5 * ledger_info.min_persistent_entry_ttl,
                }),
            },
            LedgerEntryChangeHelper {
                read_only: false,
                key: cd.contract_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(cd.contract_entry.clone()),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&cd.contract_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper {
                read_only: false,
                key: cd.wasm_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(cd.wasm_entry.clone()),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&cd.wasm_key),
                    entry_type: LedgerEntryType::ContractCode,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
        ]
    );

    assert_eq!(
        res.resources,
        SorobanResources {
            footprint: LedgerFootprint {
                read_only: Default::default(),
                read_write: vec![
                    data_key.clone(),
                    cd.contract_key.clone(),
                    cd.wasm_key.clone(),
                ]
                .try_into()
                .unwrap(),
            },
            instructions: 1027906,
            disk_read_bytes: data_entry_size + wasm_entry_size + instance_entry_size,
            write_bytes: data_entry_size + wasm_entry_size + instance_entry_size,
        }
    );
}

#[test]
fn test_auto_restore_with_overwrite_in_recording_mode() {
    let cd = CreateContractData::new([111; 32], CONTRACT_STORAGE);
    let ledger_info = default_ledger_info();

    let key = symbol_sc_val("key");
    let val = u64_sc_val(u64::MAX);
    let host_fn = invoke_contract_host_fn(
        &cd.contract_address,
        "put_persistent",
        vec![key.clone(), val.clone()],
    );

    let persistent_data_entry = contract_data_entry(
        &cd.contract_address,
        &key,
        &u64_sc_val(321),
        ContractDataDurability::Persistent,
    );
    let data_key = contract_data_key(
        &cd.contract_address,
        &key,
        ContractDataDurability::Persistent,
    );
    let res = invoke_host_function_recording_helper(
        true,
        &host_fn,
        &cd.deployer,
        RecordingInvocationAuthMode::Recording(true),
        &ledger_info,
        vec![
            (
                cd.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100_000),
            ),
            (
                cd.contract_entry.clone(),
                Some(ledger_info.sequence_number - 100),
            ),
            (
                persistent_data_entry.clone(),
                Some(ledger_info.sequence_number - 1),
            ),
        ],
        &prng_seed(),
        None,
    )
    .unwrap();
    assert_eq!(res.invoke_result.unwrap(), ScVal::Void);
    assert!(res.contract_events.is_empty());
    assert_eq!(res.restored_rw_entry_ids.len(), 2);
    assert_eq!(
        res.resources.footprint.read_write[res.restored_rw_entry_ids[0] as usize],
        data_key
    );
    assert_eq!(
        res.resources.footprint.read_write[res.restored_rw_entry_ids[1] as usize],
        cd.contract_key
    );

    let new_persistent_data_entry = contract_data_entry(
        &cd.contract_address,
        &key,
        &val,
        ContractDataDurability::Persistent,
    );
    let data_entry_size = new_persistent_data_entry
        .to_xdr(Limits::none())
        .unwrap()
        .len() as u32;
    let instance_entry_size = cd.contract_entry.to_xdr(Limits::none()).unwrap().len() as u32;
    assert_eq!(
        res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: false,
                key: data_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(new_persistent_data_entry.clone()),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&data_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper {
                read_only: false,
                key: cd.contract_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(cd.contract_entry.clone()),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&cd.contract_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper::no_op_change(
                &cd.wasm_entry,
                ledger_info.sequence_number + 100_000
            ),
        ]
    );

    assert_eq!(
        res.resources,
        SorobanResources {
            footprint: LedgerFootprint {
                read_only: vec![cd.wasm_key.clone(),].try_into().unwrap(),
                read_write: vec![data_key.clone(), cd.contract_key.clone(),]
                    .try_into()
                    .unwrap(),
            },
            instructions: 1028344,
            disk_read_bytes: data_entry_size + instance_entry_size,
            write_bytes: data_entry_size + instance_entry_size,
        }
    );
}

#[test]
fn test_auto_restore_with_expired_temp_entry_in_recording_mode() {
    let cd = CreateContractData::new([111; 32], CONTRACT_STORAGE);
    let ledger_info = default_ledger_info();

    let key = symbol_sc_val("key");
    let val = u64_sc_val(0);
    let host_fn = invoke_contract_host_fn(&cd.contract_address, "has_temporary", vec![key.clone()]);

    let temp_data_entry = contract_data_entry(
        &cd.contract_address,
        &key,
        &val,
        ContractDataDurability::Temporary,
    );
    let res = invoke_host_function_recording_helper(
        true,
        &host_fn,
        &cd.deployer,
        RecordingInvocationAuthMode::Recording(true),
        &ledger_info,
        vec![
            (
                cd.wasm_entry.clone(),
                Some(ledger_info.sequence_number - 100_000),
            ),
            (
                cd.contract_entry.clone(),
                Some(ledger_info.sequence_number - 100),
            ),
            (
                temp_data_entry.clone(),
                Some(ledger_info.sequence_number - 1),
            ),
        ],
        &prng_seed(),
        None,
    )
    .unwrap();
    assert_eq!(res.invoke_result.unwrap(), ScVal::Bool(false));
    assert!(res.contract_events.is_empty());
    assert_eq!(res.restored_rw_entry_ids.len(), 2);
    assert_eq!(
        res.resources.footprint.read_write[res.restored_rw_entry_ids[0] as usize],
        cd.contract_key
    );
    assert_eq!(
        res.resources.footprint.read_write[res.restored_rw_entry_ids[1] as usize],
        cd.wasm_key
    );
    let data_key = contract_data_key(
        &cd.contract_address,
        &key,
        ContractDataDurability::Temporary,
    );
    let wasm_entry_size = cd.wasm_entry.to_xdr(Limits::none()).unwrap().len() as u32;
    let instance_entry_size = cd.contract_entry.to_xdr(Limits::none()).unwrap().len() as u32;
    assert_eq!(
        res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: true,
                key: data_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: None,
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&data_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Temporary,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: 0,
                }),
            },
            LedgerEntryChangeHelper {
                read_only: false,
                key: cd.contract_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(cd.contract_entry.clone()),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&cd.contract_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper {
                read_only: false,
                key: cd.wasm_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(cd.wasm_entry.clone()),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&cd.wasm_key),
                    entry_type: LedgerEntryType::ContractCode,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
        ]
    );

    assert_eq!(
        res.resources,
        SorobanResources {
            footprint: LedgerFootprint {
                read_only: vec![data_key].try_into().unwrap(),
                read_write: vec![cd.contract_key.clone(), cd.wasm_key.clone(),]
                    .try_into()
                    .unwrap(),
            },
            instructions: 1026761,
            disk_read_bytes: wasm_entry_size + instance_entry_size,
            write_bytes: wasm_entry_size + instance_entry_size,
        }
    );
}

#[test]
fn test_auto_restore_with_recreated_temp_entry_in_recording_mode() {
    let cd = CreateContractData::new([111; 32], CONTRACT_STORAGE);
    let ledger_info = default_ledger_info();

    let key = symbol_sc_val("key");
    let val = u64_sc_val(0);
    let host_fn = invoke_contract_host_fn(
        &cd.contract_address,
        "put_temporary",
        vec![key.clone(), val.clone()],
    );

    let temp_data_entry = contract_data_entry(
        &cd.contract_address,
        &key,
        &u64_sc_val(123),
        ContractDataDurability::Temporary,
    );
    let res = invoke_host_function_recording_helper(
        true,
        &host_fn,
        &cd.deployer,
        RecordingInvocationAuthMode::Recording(true),
        &ledger_info,
        vec![
            (
                cd.wasm_entry.clone(),
                Some(ledger_info.sequence_number - 100_000),
            ),
            (
                cd.contract_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                temp_data_entry.clone(),
                Some(ledger_info.sequence_number - 1000),
            ),
        ],
        &prng_seed(),
        None,
    )
    .unwrap();
    assert_eq!(res.invoke_result.unwrap(), ScVal::Void);
    assert!(res.contract_events.is_empty());
    assert_eq!(res.restored_rw_entry_ids.len(), 1);
    assert_eq!(
        res.resources.footprint.read_write[res.restored_rw_entry_ids[0] as usize],
        cd.wasm_key
    );
    let data_key = contract_data_key(
        &cd.contract_address,
        &key,
        ContractDataDurability::Temporary,
    );
    let updated_temp_data_entry = contract_data_entry(
        &cd.contract_address,
        &key,
        &val,
        ContractDataDurability::Temporary,
    );
    let wasm_entry_size = cd.wasm_entry.to_xdr(Limits::none()).unwrap().len() as u32;
    let temp_entry_size = updated_temp_data_entry
        .to_xdr(Limits::none())
        .unwrap()
        .len() as u32;
    assert_eq!(
        res.ledger_changes,
        vec![
            LedgerEntryChangeHelper {
                read_only: false,
                key: data_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(updated_temp_data_entry),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&data_key),
                    entry_type: LedgerEntryType::ContractData,
                    durability: ContractDataDurability::Temporary,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_temp_entry_ttl
                        - 1,
                }),
            },
            LedgerEntryChangeHelper::no_op_change(
                &cd.contract_entry,
                ledger_info.sequence_number + 100
            ),
            LedgerEntryChangeHelper {
                read_only: false,
                key: cd.wasm_key.clone(),
                old_entry_size_bytes_for_rent: 0,
                new_value: Some(cd.wasm_entry.clone()),
                ttl_change: Some(LedgerEntryLiveUntilChange {
                    key_hash: compute_key_hash(&cd.wasm_key),
                    entry_type: LedgerEntryType::ContractCode,
                    durability: ContractDataDurability::Persistent,
                    old_live_until_ledger: 0,
                    new_live_until_ledger: ledger_info.sequence_number
                        + ledger_info.min_persistent_entry_ttl
                        - 1,
                }),
            },
        ]
    );

    assert_eq!(
        res.resources,
        SorobanResources {
            footprint: LedgerFootprint {
                read_only: vec![cd.contract_key.clone()].try_into().unwrap(),
                read_write: vec![data_key, cd.wasm_key.clone()].try_into().unwrap(),
            },
            instructions: 1028934,
            disk_read_bytes: wasm_entry_size,
            write_bytes: wasm_entry_size + temp_entry_size,
        }
    );
}

#[test]
fn test_invoke_contract_with_storage_ops_success_using_simulation() {
    let cd = CreateContractData::new([111; 32], CONTRACT_STORAGE);
    let ledger_info = default_ledger_info();
    let key = symbol_sc_val("key");
    let val = u64_sc_val(u64::MAX);
    let host_fn = invoke_contract_host_fn(
        &cd.contract_address,
        "put_temporary",
        vec![key.clone(), val.clone()],
    );
    let res = invoke_host_function_using_simulation(
        true,
        &host_fn,
        &cd.deployer,
        &ledger_info,
        vec![
            (
                cd.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                cd.contract_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
        ],
        &prng_seed(),
    )
    .unwrap();
    assert_eq!(res.invoke_result.unwrap(), ScVal::Void);
    let extend_host_fn = invoke_contract_host_fn(
        &cd.contract_address,
        "extend_temporary",
        vec![key.clone(), u32_sc_val(501), u32_sc_val(5000)],
    );
    let new_entry = contract_data_entry(
        &cd.contract_address,
        &key,
        &val,
        ContractDataDurability::Temporary,
    );
    let extend_res = invoke_host_function_recording_helper(
        true,
        &extend_host_fn,
        &cd.deployer,
        RecordingInvocationAuthMode::Recording(true),
        &ledger_info,
        vec![
            (
                cd.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                cd.contract_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
            (new_entry, Some(ledger_info.sequence_number + 500)),
        ],
        &prng_seed(),
        None,
    )
    .unwrap();
    assert_eq!(extend_res.invoke_result.unwrap(), ScVal::Void);
}

#[test]
fn test_invoke_contract_without_footprint_fails() {
    let cd = CreateContractData::new([111; 32], CONTRACT_STORAGE);
    let ledger_info = default_ledger_info();
    let host_fn = invoke_contract_host_fn(
        &cd.contract_address,
        "put_temporary",
        vec![symbol_sc_val("key"), u64_sc_val(u64::MAX)],
    );
    let res = invoke_host_function_helper(
        true,
        &host_fn,
        &resources(10_000_000, vec![], vec![]),
        &cd.deployer,
        vec![],
        &ledger_info,
        vec![],
        &prng_seed(),
    )
    .unwrap();
    assert!(HostError::result_matches_err(
        res.invoke_result,
        (ScErrorType::Storage, ScErrorCode::ExceededLimit)
    ));
    assert!(res.ledger_changes.is_empty());
    assert!(res.diagnostic_events.len() >= 1);
    assert!(res.contract_events.is_empty());
    assert!(res.budget.get_cpu_insns_consumed().unwrap() > 0);
    assert!(res.budget.get_mem_bytes_consumed().unwrap() > 0);
}

#[test]
fn test_classic_account_auth_using_simulation() {
    let mut prng = StdRng::from_seed([123; 32]);
    let keys = vec![
        SigningKey::generate(&mut prng),
        SigningKey::generate(&mut prng),
    ];
    let signers = vec![TestSigner::account(&keys[0]), TestSigner::account(&keys[1])];

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
    let host_fn = auth_contract_invocation(
        vec![
            ScAddress::Account(source_account.clone()),
            signers[0].sc_address(),
            signers[1].sc_address(),
        ],
        tree,
    );
    let ledger_info = default_ledger_info();
    let res = invoke_host_function_using_simulation_with_signers(
        true,
        &host_fn,
        &source_account,
        &ledger_info,
        vec![
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
            (account_entry(&signers[0].account_id()), None),
            (account_entry(&signers[1].account_id()), None),
        ],
        &prng_seed(),
        &Host::test_host(),
        &signers,
    )
    .unwrap();
    assert!(res.invoke_result.is_ok());
}

// Test that when running on a protocol that supports the ModuleCache, when
// doing work that would be significantly different under cached instantiation,
// we get a cost estimate from recording mode that still matches the cost of the
// actual execution.
#[test]
fn test_cap_54_55_56_module_cache_recording_fidelity() {
    for refined_cost_inputs in [false, true] {
        let add_cd = CreateContractData::new_with_refined_contract_cost_inputs(
            [111; 32],
            ADD_I32,
            refined_cost_inputs,
        );
        let sum_cd = CreateContractData::new_with_refined_contract_cost_inputs(
            [222; 32],
            SUM_I32,
            refined_cost_inputs,
        );
        let ledger_info = default_ledger_info();
        let host_fn = invoke_contract_host_fn(
            &sum_cd.contract_address,
            "sum",
            vec![
                ScVal::Address(add_cd.contract_address.clone()),
                ScVal::Vec(Some(ScVec(
                    vec![
                        ScVal::I32(1),
                        ScVal::I32(2),
                        ScVal::I32(3),
                        ScVal::I32(4),
                        ScVal::I32(5),
                    ]
                    .try_into()
                    .unwrap(),
                ))),
            ],
        );
        let ledger_entries_with_ttl = vec![
            (
                add_cd.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                add_cd.contract_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
            (
                sum_cd.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                sum_cd.contract_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
        ];
        let res = invoke_host_function_using_simulation(
            true,
            &host_fn,
            &sum_cd.deployer,
            &ledger_info,
            ledger_entries_with_ttl,
            &prng_seed(),
        )
        .unwrap();
        assert_eq!(res.invoke_result.unwrap(), ScVal::I32(15));
    }
}

#[test]
fn test_deployer_operations_using_simulation() {
    let deployer_contract = CreateContractData::new([1; 32], DEPLOYER_TEST_CONTRACT);

    let source_account = get_account_id([123; 32]);
    let host_fn = invoke_contract_host_fn(
        &deployer_contract.contract_address,
        "deploy",
        vec![
            bytes_sc_val(UPDATEABLE_CONTRACT),
            bytes_sc_val(ADD_I32),
            bytes_sc_val(&[5; 32]),
        ],
    );
    let ledger_info = default_ledger_info();
    let res = invoke_host_function_using_simulation(
        true,
        &host_fn,
        &source_account,
        &ledger_info,
        vec![
            (
                deployer_contract.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                deployer_contract.contract_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
        ],
        &prng_seed(),
    )
    .unwrap();
    assert!(res.invoke_result.is_ok());
}

#[test]
fn test_create_contract_authorized_by_custom_account() {
    let ledger_info = default_ledger_info();
    let account_contract = CreateContractData::new([1; 32], SIMPLE_ACCOUNT_CONTRACT);
    let mut prng = StdRng::from_seed(prng_seed());
    let account_key = SigningKey::generate(&mut prng);
    let dummy_host = Host::test_host();
    let signers = vec![TestSigner::AccountContract(AccountContractSigner {
        address: dummy_host
            .add_host_object(account_contract.contract_address.clone())
            .unwrap()
            .try_into_val(&dummy_host)
            .unwrap(),
        sign: simple_account_sign_fn(&dummy_host, &account_key),
    })];

    let source_account = get_account_id([123; 32]);
    let contract_id_preimage = ContractIdPreimage::Address(ContractIdPreimageFromAddress {
        address: account_contract.contract_address.clone(),
        salt: Uint256([2; 32]),
    });
    let executable = ContractExecutable::Wasm(get_wasm_hash(ADD_I32).try_into().unwrap());
    let host_fn = HostFunction::CreateContract(CreateContractArgs {
        contract_id_preimage: contract_id_preimage.clone(),
        executable: executable.clone(),
    });
    let account_key_entry = ledger_entry(LedgerEntryData::ContractData(ContractDataEntry {
        ext: ExtensionPoint::V0,
        contract: account_contract.contract_address.clone(),
        key: ScVal::Vec(Some(
            vec![ScVal::Symbol("Owner".try_into().unwrap())]
                .try_into()
                .unwrap(),
        )),
        durability: ContractDataDurability::Persistent,
        val: ScVal::Bytes(
            account_key
                .verifying_key()
                .as_bytes()
                .to_vec()
                .try_into()
                .unwrap(),
        ),
    }));

    let res = invoke_host_function_using_simulation_with_signers(
        true,
        &host_fn,
        &source_account,
        &ledger_info,
        vec![
            (
                account_contract.wasm_entry.clone(),
                Some(ledger_info.sequence_number + 100),
            ),
            (
                account_contract.contract_entry.clone(),
                Some(ledger_info.sequence_number + 1000),
            ),
            (
                wasm_entry(ADD_I32),
                Some(ledger_info.sequence_number + 1000),
            ),
            (account_key_entry, Some(ledger_info.sequence_number + 100)),
        ],
        &prng_seed(),
        &dummy_host,
        &signers,
    )
    .unwrap();
    assert!(res.invoke_result.is_ok());
}
