/// This module contains functionality to invoke host functions in embedder
/// environments using a clean host instance.
/// Also contains helpers for processing the ledger changes caused by these
/// host functions.
use std::{cmp::max, rc::Rc};

#[cfg(any(test, feature = "recording_mode"))]
use crate::{
    auth::RecordedAuthPayload,
    storage::is_persistent_key,
    xdr::{ContractEvent, ReadXdr, ScVal, SorobanAddressCredentials, SorobanCredentials, WriteXdr},
    DEFAULT_XDR_RW_LIMITS,
};
use crate::{
    budget::{AsBudget, Budget},
    crypto::sha256_hash_from_bytes,
    events::Events,
    fees::LedgerEntryRentChange,
    host::{
        metered_clone::{MeteredAlloc, MeteredClone, MeteredContainer, MeteredIterator},
        metered_xdr::{metered_from_xdr_with_budget, metered_write_xdr},
        TraceHook,
    },
    storage::{AccessType, Footprint, FootprintMap, SnapshotSource, Storage, StorageMap},
    xdr::{
        AccountId, ContractDataDurability, ContractEventType, DiagnosticEvent, HostFunction,
        LedgerEntry, LedgerEntryData, LedgerEntryType, LedgerFootprint, LedgerKey,
        LedgerKeyAccount, LedgerKeyContractCode, LedgerKeyContractData, LedgerKeyTrustLine,
        ScErrorCode, ScErrorType, SorobanAuthorizationEntry, SorobanResources, TtlEntry,
    },
    DiagnosticLevel, Error, Host, HostError, LedgerInfo, MeteredOrdMap,
};
use crate::{ledger_info::get_key_durability, ModuleCache};
use crate::{storage::EntryWithLiveUntil, vm::wasm_module_memory_cost};
#[cfg(any(test, feature = "recording_mode"))]
use sha2::{Digest, Sha256};

type TtlEntryMap = MeteredOrdMap<Rc<LedgerKey>, Rc<TtlEntry>, Budget>;
type RestoredKeySet = MeteredOrdMap<Rc<LedgerKey>, (), Budget>;

/// Result of invoking a single host function prepared for embedder consumption.
pub struct InvokeHostFunctionResult {
    /// Result value of the function, encoded `ScVal` XDR on success, or error.
    pub encoded_invoke_result: Result<Vec<u8>, HostError>,
    /// All the ledger changes caused by this invocation, including no-ops.
    /// This contains an entry for *every* item in the input footprint, even if
    /// it wasn't modified at all.
    ///
    /// Read-only entry can only have their live until ledger increased.
    /// Read-write entries can be modified arbitrarily or removed.
    ///
    /// Empty when invocation fails.
    pub ledger_changes: Vec<LedgerEntryChange>,
    /// All the events that contracts emitted during invocation, encoded as
    /// `ContractEvent` XDR.
    ///
    /// Empty when invocation fails.
    pub encoded_contract_events: Vec<Vec<u8>>,
}

/// Result of invoking a single host function prepared for embedder consumption.
#[cfg(any(test, feature = "recording_mode"))]
pub struct InvokeHostFunctionRecordingModeResult {
    /// Result value of the invoked function or error returned for invocation.
    pub invoke_result: Result<ScVal, HostError>,
    /// Resources recorded during the invocation, including the footprint.
    pub resources: SorobanResources,
    /// Indices of the entries in read-write footprint that are to be
    /// auto-restored when transaction is executed.
    ///
    /// Specifically, these are indices of ledger entry keys in
    /// `resources.footprint.read_write` vector. Thus additional care should
    /// be taken if the read-write footprint is re-arranged in any way (that
    /// shouldn't normally be necessary though).
    pub restored_rw_entry_indices: Vec<u32>,
    /// Authorization data, either passed through from the call (when provided),
    /// or recorded during the invocation.
    pub auth: Vec<SorobanAuthorizationEntry>,
    /// All the ledger changes caused by this invocation, including no-ops.
    ///
    /// Read-only entry can only have their live until ledger increased.
    /// Read-write entries can be modified arbitrarily or removed.
    ///
    /// Empty when invocation fails.
    pub ledger_changes: Vec<LedgerEntryChange>,
    /// All the events that contracts emitted during invocation.
    ///
    /// Empty when invocation fails.
    pub contract_events: Vec<ContractEvent>,
    /// Size of the encoded contract events and the return value.
    /// Non-zero only when invocation has succeeded.
    pub contract_events_and_return_value_size: u32,
}

/// Represents a change of the ledger entry from 'old' value to the 'new' one.
/// Only contains the final value of the entry (if any) and some minimal
/// information about the old entry for convenience.
#[derive(Default)]
pub struct LedgerEntryChange {
    /// Whether the ledger entry is read-only, as defined by the footprint.
    pub read_only: bool,
    /// Entry key encoded as `LedgerKey` XDR.
    pub encoded_key: Vec<u8>,
    /// Size of the 'old' entry to use in the rent computations.
    /// This is the size of the encoded entry XDR for all of the entries besides
    /// contract code, for which the module in-memory size is used instead.
    pub old_entry_size_bytes_for_rent: u32,
    /// New value of the ledger entry encoded as `LedgerEntry` XDR.
    /// Only set for non-removed, non-readonly values, otherwise `None`.
    pub encoded_new_value: Option<Vec<u8>>,
    /// Size of the 'new' entry to use in the rent computations.
    /// This is the size of the encoded entry XDR (i.e. length of `encoded_new_value`)
    /// for all of the entries besides contract code, for which the module
    /// in-memory size is used instead.
    pub new_entry_size_bytes_for_rent: u32,
    /// Change of the live until state of the entry.
    /// Only set for entries that have a TTL, otherwise `None`.
    pub ttl_change: Option<LedgerEntryLiveUntilChange>,
}
/// Represents the live until-related state of the entry.
#[derive(Debug, Eq, PartialEq, Clone)]
pub struct LedgerEntryLiveUntilChange {
    /// Hash of the LedgerKey for the entry that this live until ledger change is tied to
    pub key_hash: Vec<u8>,
    /// Durability of the entry.    
    pub durability: ContractDataDurability,
    /// Type of the entry.
    pub entry_type: LedgerEntryType,
    /// Live until ledger of the old entry.
    pub old_live_until_ledger: u32,
    /// Live until ledger of the new entry. Guaranteed to always be greater than
    /// or equal to `old_live_until_ledger`.
    pub new_live_until_ledger: u32,
}

// Builds a set for metered lookups of keys for entries that were restored from
// the archived state.
// This returns an `Option` instead of an empty set because most of the
// invocations won't have this populated and it makes sense to not run
// unnecessary metered work for them (even empty map lookups have some cost
// charged).
fn build_restored_key_set(
    budget: &Budget,
    resources: &SorobanResources,
    restored_rw_entry_indices: &[u32],
) -> Result<Option<RestoredKeySet>, HostError> {
    if restored_rw_entry_indices.is_empty() {
        return Ok(None);
    }
    let rw_footprint = &resources.footprint.read_write;
    let mut key_set = RestoredKeySet::default();
    for e in restored_rw_entry_indices {
        key_set = key_set.insert(
            Rc::new(
                rw_footprint
                    .get(*e as usize)
                    .ok_or_else(|| {
                        HostError::from(Error::from_type_and_code(
                            ScErrorType::Storage,
                            ScErrorCode::InternalError,
                        ))
                    })?
                    .metered_clone(budget)?,
            ),
            (),
            budget,
        )?;
    }
    Ok(Some(key_set))
}

/// Returns the difference between the `storage` and its initial snapshot as
/// `LedgerEntryChanges`.
/// Returns an entry for every item in `storage` footprint.
fn get_ledger_changes(
    budget: &Budget,
    storage: &Storage,
    init_storage_snapshot: &(impl SnapshotSource + ?Sized),
    init_ttl_entries: TtlEntryMap,
    min_live_until_ledger: u32,
    restored_keys: &Option<RestoredKeySet>,
    #[cfg(any(test, feature = "recording_mode"))] current_ledger_seq: u32,
) -> Result<Vec<LedgerEntryChange>, HostError> {
    // Skip allocation metering for this for the sake of simplicity - the
    // bounding factor here is XDR decoding which is metered.
    let mut changes = Vec::with_capacity(storage.map.len());

    let footprint_map = &storage.footprint.0;
    // We return any invariant errors here as internal errors, as they would
    // typically mean inconsistency between storage and snapshot that shouldn't
    // happen in embedder environments, or simply fundamental invariant bugs.
    let internal_error = || {
        HostError::from(Error::from_type_and_code(
            ScErrorType::Storage,
            ScErrorCode::InternalError,
        ))
    };
    for (key, entry_with_live_until_ledger) in storage.map.iter(budget)? {
        let mut entry_change = LedgerEntryChange::default();
        metered_write_xdr(budget, key.as_ref(), &mut entry_change.encoded_key)?;
        let durability = get_key_durability(key);

        if let Some(durability) = durability {
            let key_hash = match init_ttl_entries.get::<Rc<LedgerKey>>(key, budget)? {
                Some(ttl_entry) => ttl_entry.key_hash.0.to_vec(),
                None => sha256_hash_from_bytes(entry_change.encoded_key.as_slice(), budget)?,
            };

            entry_change.ttl_change = Some(LedgerEntryLiveUntilChange {
                key_hash,
                entry_type: key.discriminant(),
                durability,
                old_live_until_ledger: 0,
                new_live_until_ledger: 0,
            });
        }
        let entry_with_live_until = init_storage_snapshot.get(key)?;
        if let Some((old_entry, old_live_until_ledger)) = entry_with_live_until {
            let mut buf = vec![];
            metered_write_xdr(budget, old_entry.as_ref(), &mut buf)?;

            entry_change.old_entry_size_bytes_for_rent =
                entry_size_for_rent(budget, &old_entry, buf.len() as u32)?;

            if let Some(ref mut ttl_change) = &mut entry_change.ttl_change {
                ttl_change.old_live_until_ledger =
                    old_live_until_ledger.ok_or_else(internal_error)?;
                // In recording mode we might encounter ledger changes that have an expired 'old'
                // entry. In that case we should treat it as non-existent instead.
                // Note, that this should only be necessary for the temporary
                // entries, the auto-restored persistent entries are handled below
                // via `restored_keys` check.
                #[cfg(any(test, feature = "recording_mode"))]
                if ttl_change.old_live_until_ledger < current_ledger_seq {
                    ttl_change.old_live_until_ledger = 0;
                    entry_change.old_entry_size_bytes_for_rent = 0;
                }
            }
        }
        if let Some((_, new_live_until_ledger)) = entry_with_live_until_ledger {
            if let Some(ref mut ttl_change) = &mut entry_change.ttl_change {
                // Never reduce the final live until ledger.
                ttl_change.new_live_until_ledger = max(
                    new_live_until_ledger.ok_or_else(internal_error)?,
                    ttl_change.old_live_until_ledger,
                );
            }
        }
        let maybe_access_type: Option<AccessType> =
            footprint_map.get::<Rc<LedgerKey>>(key, budget)?.copied();
        match maybe_access_type {
            Some(AccessType::ReadOnly) => {
                entry_change.read_only = true;
            }
            Some(AccessType::ReadWrite) => {
                if let Some((entry, _)) = entry_with_live_until_ledger {
                    let mut entry_buf = vec![];
                    metered_write_xdr(budget, entry.as_ref(), &mut entry_buf)?;
                    entry_change.new_entry_size_bytes_for_rent =
                        entry_size_for_rent(budget, &entry, entry_buf.len() as u32)?;
                    entry_change.encoded_new_value = Some(entry_buf);

                    if let Some(restored_keys) = &restored_keys {
                        if restored_keys.contains_key::<LedgerKey>(key, budget)? {
                            entry_change.old_entry_size_bytes_for_rent = 0;
                            if let Some(ref mut ttl_change) = &mut entry_change.ttl_change {
                                ttl_change.old_live_until_ledger = 0;
                                ttl_change.new_live_until_ledger =
                                    max(ttl_change.new_live_until_ledger, min_live_until_ledger);
                            }
                        }
                    }
                }
            }
            None => {
                return Err(internal_error());
            }
        }
        changes.push(entry_change);
    }
    Ok(changes)
}

/// Creates ledger changes for entries that don't exist in the storage.
///
/// In recording mode it's possible to have discrepancies between the storage
/// and the footprint. Specifically, if an entry is only accessed from a
/// function that has failed and had its failure handled gracefully (via
/// `try_call`), then the storage map will get rolled back and the access will
/// only be recorded in the footprint. However, we still need to account for
/// these in the ledger entry changes, as downstream consumers (simulation) rely
/// on that to determine the fees.
#[cfg(any(test, feature = "recording_mode"))]
fn add_footprint_only_ledger_changes(
    budget: &Budget,
    storage: &Storage,
    changes: &mut Vec<LedgerEntryChange>,
) -> Result<(), HostError> {
    for (key, access_type) in storage.footprint.0.iter(budget)? {
        // We have to check if the entry exists in the internal storage map
        // because `has` check on storage affects the footprint.
        if storage.map.contains_key::<Rc<LedgerKey>>(key, budget)? {
            continue;
        }
        let mut entry_change = LedgerEntryChange::default();
        metered_write_xdr(budget, key.as_ref(), &mut entry_change.encoded_key)?;
        entry_change.read_only = matches!(*access_type, AccessType::ReadOnly);
        changes.push(entry_change);
    }
    Ok(())
}

/// Extracts the rent-related changes from the provided ledger changes.
///
/// Only meaningful changes are returned (i.e. no-op changes are skipped).
///
/// Extracted changes can be used to compute the rent fee via `fees::compute_rent_fee`.
pub fn extract_rent_changes(ledger_changes: &[LedgerEntryChange]) -> Vec<LedgerEntryRentChange> {
    ledger_changes
        .iter()
        .filter_map(|entry_change| {
            // Rent changes are only relevant to non-removed entries with
            // a ttl.
            if let (Some(ttl_change), optional_encoded_new_value) =
                (&entry_change.ttl_change, &entry_change.encoded_new_value)
            {
                let new_size_bytes_for_rent = if optional_encoded_new_value.is_some() {
                    entry_change.new_entry_size_bytes_for_rent
                } else {
                    entry_change.old_entry_size_bytes_for_rent
                };

                // Skip the entry if 1. it is not extended and 2. the entry size has not increased
                if ttl_change.old_live_until_ledger >= ttl_change.new_live_until_ledger
                    && entry_change.old_entry_size_bytes_for_rent >= new_size_bytes_for_rent
                {
                    return None;
                }
                Some(LedgerEntryRentChange {
                    is_persistent: matches!(
                        ttl_change.durability,
                        ContractDataDurability::Persistent
                    ),
                    is_code_entry: matches!(ttl_change.entry_type, LedgerEntryType::ContractCode),
                    old_size_bytes: entry_change.old_entry_size_bytes_for_rent,
                    new_size_bytes: new_size_bytes_for_rent,
                    old_live_until_ledger: ttl_change.old_live_until_ledger,
                    new_live_until_ledger: ttl_change.new_live_until_ledger,
                })
            } else {
                None
            }
        })
        .collect()
}

/// Helper for computing the size of the ledger entry to be used in rent
/// computations.
///
/// This returns the size of the Wasm module in memory for the contract code
/// entries and the provided XDR size of the entry otherwise.
///
/// Note, that this doesn't compute the XDR size because the recomputation
/// might be costly.
pub fn entry_size_for_rent(
    budget: &Budget,
    entry: &LedgerEntry,
    entry_xdr_size: u32,
) -> Result<u32, HostError> {
    Ok(match &entry.data {
        LedgerEntryData::ContractCode(contract_code_entry) => entry_xdr_size.saturating_add(
            wasm_module_memory_cost(budget, contract_code_entry)?.min(u32::MAX as u64) as u32,
        ),
        _ => entry_xdr_size,
    })
}

/// Invokes a host function within a fresh host instance.
///
/// This collects the necessary inputs as encoded XDR and returns the outputs
/// as encoded XDR as well. This is supposed to encapsulate all the metered
/// operations needed to invoke a host function, including the input/output
/// decoding/encoding.
///
/// In order to get clean budget metering data, a clean budget has to be
/// provided as an input. It can then be examined immediately after execution in
/// order to get the precise metering data. Budget is not reset in case of
/// errors.
///
/// This may only fail when budget is exceeded or if there is an internal error.
/// Host function invocation errors are stored within
///  `Ok(InvokeHostFunctionResult)`.
///
/// When diagnostics are enabled, we try to populate `diagnostic_events`
/// even if the `InvokeHostFunctionResult` fails for any reason.
#[allow(clippy::too_many_arguments)]
pub fn invoke_host_function<T: AsRef<[u8]>, I: ExactSizeIterator<Item = T>>(
    budget: &Budget,
    enable_diagnostics: bool,
    encoded_host_fn: T,
    encoded_resources: T,
    restored_rw_entry_indices: &[u32],
    encoded_source_account: T,
    encoded_auth_entries: I,
    ledger_info: LedgerInfo,
    encoded_ledger_entries: I,
    encoded_ttl_entries: I,
    base_prng_seed: T,
    diagnostic_events: &mut Vec<DiagnosticEvent>,
    trace_hook: Option<TraceHook>,
    module_cache: Option<ModuleCache>,
) -> Result<InvokeHostFunctionResult, HostError> {
    let _span0 = tracy_span!("invoke_host_function");

    let resources: SorobanResources =
        metered_from_xdr_with_budget(encoded_resources.as_ref(), &budget)?;
    let restored_keys = build_restored_key_set(&budget, &resources, &restored_rw_entry_indices)?;
    let footprint = build_storage_footprint_from_xdr(&budget, resources.footprint)?;
    let current_ledger_seq = ledger_info.sequence_number;
    let min_live_until_ledger = ledger_info
        .min_live_until_ledger_checked(ContractDataDurability::Persistent)
        .ok_or_else(|| {
            HostError::from(Error::from_type_and_code(
                ScErrorType::Context,
                ScErrorCode::InternalError,
            ))
        })?;
    let (storage_map, init_ttl_map) = build_storage_map_from_xdr_ledger_entries(
        &budget,
        &footprint,
        encoded_ledger_entries,
        encoded_ttl_entries,
        current_ledger_seq,
        #[cfg(any(test, feature = "recording_mode"))]
        false,
    )?;

    let init_storage_map = storage_map.metered_clone(budget)?;

    let storage = Storage::with_enforcing_footprint_and_map(footprint, storage_map);
    let host = Host::with_storage_and_budget(storage, budget.clone());
    let have_trace_hook = trace_hook.is_some();
    if let Some(th) = trace_hook {
        host.set_trace_hook(Some(th))?;
    }
    let auth_entries = host.build_auth_entries_from_xdr(encoded_auth_entries)?;
    let host_function: HostFunction = host.metered_from_xdr(encoded_host_fn.as_ref())?;
    let source_account: AccountId = host.metered_from_xdr(encoded_source_account.as_ref())?;
    host.set_source_account(source_account)?;
    host.set_ledger_info(ledger_info)?;
    host.set_authorization_entries(auth_entries)?;
    let seed32: [u8; 32] = base_prng_seed.as_ref().try_into().map_err(|_| {
        host.err(
            ScErrorType::Context,
            ScErrorCode::InternalError,
            "base PRNG seed is not 32-bytes long",
            &[],
        )
    })?;
    host.set_base_prng_seed(seed32)?;
    if enable_diagnostics {
        host.set_diagnostic_level(DiagnosticLevel::Debug)?;
    }
    if let Some(module_cache) = module_cache {
        host.set_module_cache(module_cache)?;
    }
    let result = {
        let _span1 = tracy_span!("Host::invoke_function");
        host.invoke_function(host_function)
    };
    if have_trace_hook {
        host.set_trace_hook(None)?;
    }
    let (storage, events) = host.try_finish()?;
    if enable_diagnostics {
        extract_diagnostic_events(&events, diagnostic_events);
    }
    let encoded_invoke_result = result.and_then(|res| {
        let mut encoded_result_sc_val = vec![];
        metered_write_xdr(&budget, &res, &mut encoded_result_sc_val).map(|_| encoded_result_sc_val)
    });
    if encoded_invoke_result.is_ok() {
        let init_storage_snapshot = StorageMapSnapshotSource {
            budget: &budget,
            map: &init_storage_map,
        };
        let ledger_changes = get_ledger_changes(
            &budget,
            &storage,
            &init_storage_snapshot,
            init_ttl_map,
            min_live_until_ledger,
            &restored_keys,
            #[cfg(any(test, feature = "recording_mode"))]
            current_ledger_seq,
        )?;
        let encoded_contract_events = encode_contract_events(budget, &events)?;
        Ok(InvokeHostFunctionResult {
            encoded_invoke_result,
            ledger_changes,
            encoded_contract_events,
        })
    } else {
        Ok(InvokeHostFunctionResult {
            encoded_invoke_result,
            ledger_changes: vec![],
            encoded_contract_events: vec![],
        })
    }
}

#[cfg(any(test, feature = "recording_mode"))]
impl Host {
    fn to_xdr_non_metered(&self, v: &impl WriteXdr) -> Result<Vec<u8>, HostError> {
        v.to_xdr(DEFAULT_XDR_RW_LIMITS).map_err(|_| {
            self.err(
                ScErrorType::Value,
                ScErrorCode::InvalidInput,
                "could not convert XDR struct to bytes - the input is too deep or too large",
                &[],
            )
        })
    }

    fn xdr_roundtrip<T>(&self, v: &T) -> Result<T, HostError>
    where
        T: WriteXdr + ReadXdr,
    {
        self.metered_from_xdr(self.to_xdr_non_metered(v)?.as_slice())
    }
}

#[cfg(any(test, feature = "recording_mode"))]
fn storage_footprint_to_ledger_footprint(
    footprint: &Footprint,
) -> Result<LedgerFootprint, HostError> {
    let mut read_only: Vec<LedgerKey> = Vec::with_capacity(footprint.0.len());
    let mut read_write: Vec<LedgerKey> = Vec::with_capacity(footprint.0.len());
    for (key, access_type) in &footprint.0 {
        match access_type {
            AccessType::ReadOnly => read_only.push((**key).clone()),
            AccessType::ReadWrite => read_write.push((**key).clone()),
        }
    }
    Ok(LedgerFootprint {
        read_only: read_only
            .try_into()
            .map_err(|_| HostError::from((ScErrorType::Storage, ScErrorCode::InternalError)))?,
        read_write: read_write
            .try_into()
            .map_err(|_| HostError::from((ScErrorType::Storage, ScErrorCode::InternalError)))?,
    })
}

#[cfg(any(test, feature = "recording_mode"))]
impl RecordedAuthPayload {
    fn into_auth_entry_with_emulated_signature(
        self,
    ) -> Result<SorobanAuthorizationEntry, HostError> {
        const EMULATED_SIGNATURE_SIZE: usize = 512;

        match (self.address, self.nonce) {
            (Some(address), Some(nonce)) => Ok(SorobanAuthorizationEntry {
                credentials: SorobanCredentials::Address(SorobanAddressCredentials {
                    address,
                    nonce,
                    signature_expiration_ledger: 0,
                    signature: ScVal::Bytes(
                        vec![0_u8; EMULATED_SIGNATURE_SIZE].try_into().unwrap(),
                    ),
                }),
                root_invocation: self.invocation,
            }),
            (None, None) => Ok(SorobanAuthorizationEntry {
                credentials: SorobanCredentials::SourceAccount,
                root_invocation: self.invocation,
            }),
            (_, _) => Err((ScErrorType::Auth, ScErrorCode::InternalError).into()),
        }
    }
}

#[cfg(any(test, feature = "recording_mode"))]
fn clear_signature(auth_entry: &mut SorobanAuthorizationEntry) {
    match &mut auth_entry.credentials {
        SorobanCredentials::Address(address_creds) => {
            address_creds.signature = ScVal::Void;
        }
        SorobanCredentials::SourceAccount => {}
    }
}

#[cfg(any(test, feature = "recording_mode"))]
/// Defines the authorization mode for the `invoke_host_function_in_recording_mode`.
pub enum RecordingInvocationAuthMode {
    /// Use enforcing auth and pass the signed authorization entries to be used.
    Enforcing(Vec<SorobanAuthorizationEntry>),
    /// Use recording auth and determine whether non-root authorization is
    /// disabled (i.e. non-root auth is not allowed when `true` is passed to
    /// the enum).
    Recording(bool),
}

/// Invokes a host function within a fresh host instance in 'recording' mode.
///
/// The purpose of recording mode is to measure the resources necessary for
/// the invocation to succeed in the 'enforcing' mode (i.e. via
/// `invoke_host_function`). The following information is recorded:
///
/// - Footprint - this is based on the ledger entries accessed.
/// - Read/write bytes - this is based on the sizes of ledger entries read
/// from the provided `ledger_snapshot`
/// - Authorization mode - when the input `auth_mode` is `None`, Host
/// switches to recording auth mode and fills the recorded data in the output.
/// When `auth_mode` is not `None`, the authorization is performed in
/// enforcing mode and entries from `auth_mode` are passed through to the
/// output.
/// - Instructions - this simply measures the instructions measured by the
/// provided `budget`. While this function makes the best effort to emulate
/// the work performed by `invoke_host_function`, the measured value might
/// still be slightly lower than the actual value used during the enforcing
/// call. Typically this difference should be within 1% from the correct
/// value, but in scenarios where recording auth is used it might be
/// significantly higher (e.g. if the user uses multisig with classic
/// accounts or custom accounts - there is currently no way to emulate that
/// in recording auth mode).
///
/// The input `Budget` should normally be configured to match the network
/// limits. Exceeding the budget is the only error condition for this
/// function, otherwise we try to populate
/// `InvokeHostFunctionRecordingModeResult` as much as possible (e.g.
/// if host function invocation fails, we would still populate the resources).
///
/// When diagnostics are enabled, we try to populate `diagnostic_events`
/// even if the `InvokeHostFunctionResult` fails for any reason.
#[cfg(any(test, feature = "recording_mode"))]
#[allow(clippy::too_many_arguments)]
pub fn invoke_host_function_in_recording_mode(
    budget: &Budget,
    enable_diagnostics: bool,
    host_fn: &HostFunction,
    source_account: &AccountId,
    auth_mode: RecordingInvocationAuthMode,
    ledger_info: LedgerInfo,
    ledger_snapshot: Rc<dyn SnapshotSource>,
    base_prng_seed: [u8; 32],
    diagnostic_events: &mut Vec<DiagnosticEvent>,
) -> Result<InvokeHostFunctionRecordingModeResult, HostError> {
    let storage = Storage::with_recording_footprint(ledger_snapshot.clone());
    let host = Host::with_storage_and_budget(storage, budget.clone());
    let is_recording_auth = matches!(auth_mode, RecordingInvocationAuthMode::Recording(_));
    let ledger_seq = ledger_info.sequence_number;
    let min_live_until_ledger = ledger_info
        .min_live_until_ledger_checked(ContractDataDurability::Persistent)
        .ok_or_else(|| {
            HostError::from(Error::from_type_and_code(
                ScErrorType::Context,
                ScErrorCode::InternalError,
            ))
        })?;
    let host_function = host.xdr_roundtrip(host_fn)?;
    let source_account: AccountId = host.xdr_roundtrip(source_account)?;
    host.set_source_account(source_account)?;
    host.set_ledger_info(ledger_info)?;
    host.set_base_prng_seed(base_prng_seed)?;

    match &auth_mode {
        RecordingInvocationAuthMode::Enforcing(auth_entries) => {
            host.set_authorization_entries(auth_entries.clone())?;
        }
        RecordingInvocationAuthMode::Recording(disable_non_root_auth) => {
            host.switch_to_recording_auth(*disable_non_root_auth)?;
        }
    }

    if enable_diagnostics {
        host.set_diagnostic_level(DiagnosticLevel::Debug)?;
    }
    let invoke_result = host.invoke_function(host_function);
    let mut contract_events_and_return_value_size = 0_u32;
    if let Ok(res) = &invoke_result {
        let mut encoded_result_sc_val = vec![];
        metered_write_xdr(&budget, res, &mut encoded_result_sc_val)?;
        contract_events_and_return_value_size = contract_events_and_return_value_size
            .saturating_add(encoded_result_sc_val.len() as u32);
    }

    let mut output_auth = if let RecordingInvocationAuthMode::Enforcing(auth_entries) = auth_mode {
        auth_entries
    } else {
        let recorded_auth = host.get_recorded_auth_payloads()?;
        recorded_auth
            .into_iter()
            .map(|a| a.into_auth_entry_with_emulated_signature())
            .collect::<Result<Vec<SorobanAuthorizationEntry>, HostError>>()?
    };

    let encoded_auth_entries = output_auth
        .iter()
        .map(|e| host.to_xdr_non_metered(e))
        .collect::<Result<Vec<Vec<u8>>, HostError>>()?;
    let decoded_auth_entries = host.build_auth_entries_from_xdr(encoded_auth_entries.iter())?;
    if is_recording_auth {
        host.set_authorization_entries(decoded_auth_entries)?;
        for auth_entry in &mut output_auth {
            clear_signature(auth_entry);
        }
    }

    let (footprint, disk_read_bytes, init_ttl_map, restored_rw_entry_ids, restored_keys) = host
        .with_mut_storage(|storage| {
            let footprint = storage_footprint_to_ledger_footprint(&storage.footprint)?;
            let _footprint_from_xdr = build_storage_footprint_from_xdr(&budget, footprint.clone())?;

            let mut encoded_ledger_entries = Vec::with_capacity(storage.footprint.0.len());
            let mut encoded_ttl_entries = Vec::with_capacity(storage.footprint.0.len());
            let mut disk_read_bytes = 0_u32;
            let mut current_rw_id = 0;
            let mut restored_rw_entry_ids = vec![];
            let mut restored_keys = RestoredKeySet::default();

            for (lk, access_type) in &storage.footprint.0 {
                let entry_with_live_until = ledger_snapshot.get(lk)?;
                if let Some((le, live_until)) = entry_with_live_until {
                    let encoded_le = host.to_xdr_non_metered(&*le)?;
                    match &le.data {
                        LedgerEntryData::ContractData(_) | LedgerEntryData::ContractCode(_) => {
                            if let Some(live_until) = live_until {
                                // Check if entry has been auto-restored (only persistent entries
                                // can be auto-restored)
                                if live_until < ledger_seq && is_persistent_key(lk.as_ref()) {
                                    // Auto-restored entries are expected to be in RW footprint.
                                    if !matches!(*access_type, AccessType::ReadWrite) {
                                        return Err(HostError::from(Error::from_type_and_code(
                                            ScErrorType::Storage,
                                            ScErrorCode::InternalError,
                                        )));
                                    }
                                    // Auto-restored entries are counted towards disk read bytes.
                                    disk_read_bytes =
                                        disk_read_bytes.saturating_add(encoded_le.len() as u32);
                                    restored_rw_entry_ids.push(current_rw_id);
                                    restored_keys =
                                        restored_keys.insert(lk.clone(), (), &budget)?;
                                }
                            }
                        }
                        _ => {
                            // Non-Soroban entries are counted towards disk read bytes.
                            disk_read_bytes =
                                disk_read_bytes.saturating_add(encoded_le.len() as u32);
                        }
                    }

                    encoded_ledger_entries.push(encoded_le);
                    if let Some(live_until_ledger) = live_until {
                        let key_xdr = host.to_xdr_non_metered(lk.as_ref())?;
                        let key_hash: [u8; 32] = Sha256::digest(&key_xdr).into();
                        let ttl_entry = TtlEntry {
                            key_hash: key_hash.try_into().map_err(|_| {
                                HostError::from((ScErrorType::Context, ScErrorCode::InternalError))
                            })?,
                            live_until_ledger_seq: live_until_ledger,
                        };
                        encoded_ttl_entries.push(host.to_xdr_non_metered(&ttl_entry)?);
                    } else {
                        encoded_ttl_entries.push(vec![]);
                    }
                    if matches!(*access_type, AccessType::ReadWrite) {
                        current_rw_id += 1;
                    }
                }
            }
            let (init_storage, init_ttl_map) = build_storage_map_from_xdr_ledger_entries(
                &budget,
                &storage.footprint,
                encoded_ledger_entries.iter(),
                encoded_ttl_entries.iter(),
                ledger_seq,
                true,
            )?;
            let _init_storage_clone = init_storage.metered_clone(budget)?;
            Ok((
                footprint,
                disk_read_bytes,
                init_ttl_map,
                restored_rw_entry_ids,
                restored_keys,
            ))
        })?;
    let mut resources = SorobanResources {
        footprint,
        instructions: 0,
        disk_read_bytes,
        write_bytes: 0,
    };
    let _resources_roundtrip: SorobanResources =
        host.metered_from_xdr(host.to_xdr_non_metered(&resources)?.as_slice())?;
    let (storage, events) = host.try_finish()?;
    if enable_diagnostics {
        extract_diagnostic_events(&events, diagnostic_events);
    }
    let restored_keys = if restored_keys.map.is_empty() {
        None
    } else {
        Some(restored_keys)
    };
    let (ledger_changes, contract_events) = if invoke_result.is_ok() {
        let mut ledger_changes = get_ledger_changes(
            &budget,
            &storage,
            &*ledger_snapshot,
            init_ttl_map,
            min_live_until_ledger,
            &restored_keys,
            ledger_seq,
        )?;
        // Add the keys that only exist in the footprint, but not in the
        // storage. This doesn't resemble anything in the enforcing mode, so use
        // the shadow budget for this.
        budget.with_shadow_mode(|| {
            add_footprint_only_ledger_changes(budget, &storage, &mut ledger_changes)
        });

        let encoded_contract_events = encode_contract_events(budget, &events)?;
        for e in &encoded_contract_events {
            contract_events_and_return_value_size =
                contract_events_and_return_value_size.saturating_add(e.len() as u32);
        }
        let contract_events: Vec<ContractEvent> = events
            .0
            .into_iter()
            .filter(|e| !e.failed_call && e.event.type_ != ContractEventType::Diagnostic)
            .map(|e| e.event)
            .collect();

        (ledger_changes, contract_events)
    } else {
        (vec![], vec![])
    };
    resources.instructions = budget.get_cpu_insns_consumed()? as u32;
    for ledger_change in &ledger_changes {
        if !ledger_change.read_only {
            if let Some(new_entry) = &ledger_change.encoded_new_value {
                resources.write_bytes =
                    resources.write_bytes.saturating_add(new_entry.len() as u32);
            }
        }
    }

    Ok(InvokeHostFunctionRecordingModeResult {
        invoke_result,
        resources,
        restored_rw_entry_indices: restored_rw_entry_ids,
        auth: output_auth,
        ledger_changes,
        contract_events,
        contract_events_and_return_value_size,
    })
}

/// Encodes host events as `ContractEvent` XDR.
pub fn encode_contract_events(budget: &Budget, events: &Events) -> Result<Vec<Vec<u8>>, HostError> {
    let ce = events
        .0
        .iter()
        .filter(|e| !e.failed_call && e.event.type_ != ContractEventType::Diagnostic)
        .map(|e| {
            let mut buf = vec![];
            metered_write_xdr(budget, &e.event, &mut buf)?;
            Ok(buf)
        })
        .collect::<Result<Vec<Vec<u8>>, HostError>>()?;
    // Here we collect first then charge, so that the input size excludes the diagnostic events.
    // This means we may temporarily go over the budget limit but should be okay.
    Vec::<Vec<u8>>::charge_bulk_init_cpy(ce.len() as u64, budget)?;
    Ok(ce)
}

fn extract_diagnostic_events(events: &Events, diagnostic_events: &mut Vec<DiagnosticEvent>) {
    // Important: diagnostic events should be non-metered and not fallible in
    // order to not cause unitentional change in transaction result.
    for event in &events.0 {
        diagnostic_events.push(DiagnosticEvent {
            in_successful_contract_call: !event.failed_call,
            event: event.event.clone(),
        });
    }
}

pub(crate) fn ledger_entry_to_ledger_key(
    le: &LedgerEntry,
    budget: &Budget,
) -> Result<LedgerKey, HostError> {
    match &le.data {
        LedgerEntryData::Account(a) => Ok(LedgerKey::Account(LedgerKeyAccount {
            account_id: a.account_id.metered_clone(budget)?,
        })),
        LedgerEntryData::Trustline(tl) => Ok(LedgerKey::Trustline(LedgerKeyTrustLine {
            account_id: tl.account_id.metered_clone(budget)?,
            asset: tl.asset.metered_clone(budget)?,
        })),
        LedgerEntryData::ContractData(cd) => Ok(LedgerKey::ContractData(LedgerKeyContractData {
            contract: cd.contract.metered_clone(budget)?,
            key: cd.key.metered_clone(budget)?,
            durability: cd.durability,
        })),
        LedgerEntryData::ContractCode(code) => Ok(LedgerKey::ContractCode(LedgerKeyContractCode {
            hash: code.hash.metered_clone(budget)?,
        })),
        _ => {
            return Err(Error::from_type_and_code(
                ScErrorType::Storage,
                ScErrorCode::InternalError,
            )
            .into());
        }
    }
}

fn build_storage_footprint_from_xdr(
    budget: &Budget,
    footprint: LedgerFootprint,
) -> Result<Footprint, HostError> {
    let mut footprint_map = FootprintMap::new();

    for key in footprint.read_write.as_vec() {
        Storage::check_supported_ledger_key_type(&key)?;
        footprint_map = footprint_map.insert(
            Rc::metered_new(key.metered_clone(budget)?, budget)?,
            AccessType::ReadWrite,
            budget,
        )?;
    }

    for key in footprint.read_only.as_vec() {
        Storage::check_supported_ledger_key_type(&key)?;
        footprint_map = footprint_map.insert(
            Rc::metered_new(key.metered_clone(budget)?, budget)?,
            AccessType::ReadOnly,
            budget,
        )?;
    }
    Ok(Footprint(footprint_map))
}

fn build_storage_map_from_xdr_ledger_entries<T: AsRef<[u8]>, I: ExactSizeIterator<Item = T>>(
    budget: &Budget,
    footprint: &Footprint,
    encoded_ledger_entries: I,
    encoded_ttl_entries: I,
    ledger_num: u32,
    #[cfg(any(test, feature = "recording_mode"))] is_recording_mode: bool,
) -> Result<(StorageMap, TtlEntryMap), HostError> {
    let mut storage_map = StorageMap::new();
    let mut ttl_map = TtlEntryMap::new();

    if encoded_ledger_entries.len() != encoded_ttl_entries.len() {
        return Err(
            Error::from_type_and_code(ScErrorType::Storage, ScErrorCode::InternalError).into(),
        );
    }

    for (entry_buf, ttl_buf) in encoded_ledger_entries.zip(encoded_ttl_entries) {
        let mut live_until_ledger: Option<u32> = None;

        let le = Rc::metered_new(
            metered_from_xdr_with_budget::<LedgerEntry>(entry_buf.as_ref(), budget)?,
            budget,
        )?;
        let key = Rc::metered_new(ledger_entry_to_ledger_key(&le, budget)?, budget)?;
        if !ttl_buf.as_ref().is_empty() {
            let ttl_entry = Rc::metered_new(
                metered_from_xdr_with_budget::<TtlEntry>(ttl_buf.as_ref(), budget)?,
                budget,
            )?;
            // In the default host flow (i.e. enforcing storage only) we don't
            // expect expired entries to ever appear in the storage map, so
            // that's always an internal error.
            #[cfg(not(any(test, feature = "recording_mode")))]
            if ttl_entry.live_until_ledger_seq < ledger_num {
                #[cfg(any(test, feature = "recording_mode"))]
                if !is_recording_mode {
                    return Err(Error::from_type_and_code(
                        ScErrorType::Storage,
                        ScErrorCode::InternalError,
                    )
                    .into());
                }
            }
            // In the recording mode we still compile both recording and
            // enforcing functions, and we do allow expired entries in the
            // recording mode when allow_expired_entries is true.
            #[cfg(any(test, feature = "recording_mode"))]
            if ttl_entry.live_until_ledger_seq < ledger_num {
                #[cfg(any(test, feature = "recording_mode"))]
                if !is_recording_mode {
                    return Err(Error::from_type_and_code(
                        ScErrorType::Storage,
                        ScErrorCode::InternalError,
                    )
                    .into());
                }
                // Skip expired temp entries, as these can't actually appear in
                // storage.
                if !crate::storage::is_persistent_key(key.as_ref()) {
                    continue;
                }
            }

            live_until_ledger = Some(ttl_entry.live_until_ledger_seq);

            ttl_map = ttl_map.insert(key.clone(), ttl_entry, budget)?;
        } else if matches!(le.as_ref().data, LedgerEntryData::ContractData(_))
            || matches!(le.as_ref().data, LedgerEntryData::ContractCode(_))
        {
            return Err(Error::from_type_and_code(
                ScErrorType::Storage,
                ScErrorCode::InternalError,
            )
            .into());
        }

        if !footprint.0.contains_key::<LedgerKey>(&key, budget)? {
            return Err(Error::from_type_and_code(
                ScErrorType::Storage,
                ScErrorCode::InternalError,
            )
            .into());
        }
        storage_map = storage_map.insert(key, Some((le, live_until_ledger)), budget)?;
    }

    // Add non-existing entries from the footprint to the storage.
    for k in footprint.0.keys(budget)? {
        if !storage_map.contains_key::<LedgerKey>(k, budget)? {
            storage_map = storage_map.insert(Rc::clone(k), None, budget)?;
        }
    }
    Ok((storage_map, ttl_map))
}

impl Host {
    fn build_auth_entries_from_xdr<T: AsRef<[u8]>, I: ExactSizeIterator<Item = T>>(
        &self,
        encoded_contract_auth_entries: I,
    ) -> Result<Vec<SorobanAuthorizationEntry>, HostError> {
        encoded_contract_auth_entries
            .map(|buf| self.metered_from_xdr::<SorobanAuthorizationEntry>(buf.as_ref()))
            .metered_collect::<Result<Vec<SorobanAuthorizationEntry>, HostError>>(
                self.as_budget(),
            )?
    }
}

struct StorageMapSnapshotSource<'a> {
    budget: &'a Budget,
    map: &'a StorageMap,
}

impl SnapshotSource for StorageMapSnapshotSource<'_> {
    fn get(&self, key: &Rc<LedgerKey>) -> Result<Option<EntryWithLiveUntil>, HostError> {
        if let Some(Some((entry, live_until_ledger))) =
            self.map.get::<Rc<LedgerKey>>(key, self.budget)?
        {
            Ok(Some((Rc::clone(entry), *live_until_ledger)))
        } else {
            Ok(None)
        }
    }
}
