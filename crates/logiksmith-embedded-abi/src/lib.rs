//! Small, versioned C ABI for hosting the portable LogikSmith runtime.
//!
//! The ABI deliberately stops at logical endpoint effects. An OpenKNX host
//! owns group-address bindings and translates each effect to a KNX telegram;
//! no KNX or OpenKNX type is used here. Strings crossing the boundary are
//! pointer/length byte slices and outputs use caller-owned fixed records.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    slice, str,
};

use logiksmith_core::{
    BlockConfig, BlockId, Dpt, Endpoint, EndpointDirection, EndpointName, InputEvent,
    InputSnapshot, MonotonicMs, Runtime, RuntimeActivation, RuntimeConfig, RuntimeEventError,
    SimulationInput, SimulationScenario, SimulationTrigger, TimerSimulationScenario, TypedValue,
    Value,
};
use logiksmith_runtime_api::{RuntimeApi, parse_automation_toml};

pub const ABI_VERSION: u32 = 2;
pub const MAX_IDENTIFIER_BYTES: usize = 64;
pub const MAX_SOURCE_BYTES: usize = logiksmith_core::MAX_LOGIC_SOURCE_BYTES;
pub const MAX_BLOCKS: usize = 64;
pub const MAX_ENDPOINTS_PER_BLOCK: usize = 64;
pub const MAX_EFFECTS: usize = 256;
pub const MAX_INPUTS_PER_BLOCK: usize = MAX_ENDPOINTS_PER_BLOCK;
pub const MAX_TIMERS_PER_BLOCK: usize = logiksmith_core::MAX_PENDING_TIMERS;
pub const MAX_BLOCK_PROJECTION_BYTES: usize = 16 * 1024;
pub const MAX_DOCUMENT_BYTES: usize = 16 * 1024;

// The ESP32 Arduino linker does not pull Rust's panic runtime as a separate
// crate when this library is embedded in a C++ image. The abort profile still
// leaves a personality reference in Rust `.eh_frame` records, so provide the
// required no-op symbol from the same archive. No unwinding is enabled for the
// embedded build.
#[cfg(target_arch = "xtensa")]
#[unsafe(no_mangle)]
pub extern "C" fn rust_eh_personality() {}

const INPUT: u8 = 0;
const OUTPUT: u8 = 1;
const VALUE_BOOL: u8 = 1;
const VALUE_PERCENT: u8 = 2;
const VALUE_TEMPERATURE_CENTI_DEGREES: u8 = 3;
const MAX_KNX_GROUP_ADDRESS: u16 = 0x7fff;

/// ABI result values. Numeric discriminants are part of the public contract.
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Ok = 0,
    NullPointer = 1,
    InvalidLength = 2,
    InvalidUtf8 = 3,
    InvalidBlockId = 4,
    InvalidEndpointName = 5,
    InvalidDpt = 6,
    InvalidValue = 7,
    InvalidConfiguration = 8,
    UnknownBlock = 9,
    UnknownEndpoint = 10,
    EndpointNotInput = 11,
    DptMismatch = 12,
    TimeWentBackwards = 13,
    LogicError = 14,
    OutputBufferTooSmall = 15,
    Panic = 16,
    InvalidGroupAddress = 17,
    RuntimeLimit = 18,
    InvalidSource = 19,
    RevisionConflict = 20,
    UnknownTimer = 21,
    BufferTooSmall = 22,
}

impl Status {
    const fn code(self) -> i32 {
        self as i32
    }
}

/// ABI value representation. `scalar` is interpreted according to `kind`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ValueRepr {
    pub dpt_major: u16,
    pub dpt_subtype: u16,
    pub kind: u8,
    pub reserved: u8,
    pub reserved2: u16,
    pub scalar: i32,
}

/// Endpoint declaration used when constructing an opaque runtime.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EndpointConfig {
    pub name: *const u8,
    pub name_len: usize,
    pub direction: u8,
    pub reserved: [u8; 3],
    pub dpt_major: u16,
    pub dpt_subtype: u16,
}

/// One block declaration used when constructing an opaque runtime.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BlockConfigRepr {
    pub block_id: *const u8,
    pub block_id_len: usize,
    pub logic_source: *const u8,
    pub logic_source_len: usize,
    pub endpoints: *const EndpointConfig,
    pub endpoint_count: usize,
}

/// Complete configuration for [`runtime_create`].
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RuntimeConfigRepr {
    pub blocks: *const BlockConfigRepr,
    pub block_count: usize,
}

/// A triggering logical input event. Address fields are metadata for the host
/// and are not interpreted by the transport-neutral core.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct InputEventRepr {
    pub block_id: *const u8,
    pub block_id_len: usize,
    pub endpoint: *const u8,
    pub endpoint_len: usize,
    pub value: ValueRepr,
    pub source_address: u16,
    pub group_address: u16,
    pub monotonic_ms: u64,
}

/// Caller-owned effect record. Names are copied into these fixed arrays.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EffectRepr {
    pub block_id: [u8; MAX_IDENTIFIER_BYTES],
    pub block_id_len: u16,
    pub endpoint: [u8; MAX_IDENTIFIER_BYTES],
    pub endpoint_len: u16,
    pub value: ValueRepr,
}

impl Default for EffectRepr {
    fn default() -> Self {
        Self {
            block_id: [0; MAX_IDENTIFIER_BYTES],
            block_id_len: 0,
            endpoint: [0; MAX_IDENTIFIER_BYTES],
            endpoint_len: 0,
            value: ValueRepr::default(),
        }
    }
}

/// Opaque runtime allocated by [`runtime_create`].
#[repr(C)]
pub struct RuntimeHandle {
    runtime: Runtime,
    document_revision: u64,
    structural_revision: u64,
}

/// A document wrapper used by hosts which already validated the configuration
/// through the shared document layer. The ABI copies all data before returning.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RuntimeDocumentRepr {
    pub config: *const RuntimeConfigRepr,
    pub document_revision: u64,
    pub structural_revision: u64,
}

/// Caller-owned TOML document envelope. Parsing and embedded capability
/// validation are delegated to the shared runtime API before any handle is
/// created.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RuntimeDocumentBytesRepr {
    pub bytes: *const u8,
    pub len: usize,
    pub document_revision: u64,
    pub structural_revision: u64,
}

/// One bounded source/enabled activation update.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ActivationRepr {
    pub block_id: *const u8,
    pub block_id_len: usize,
    pub source: *const u8,
    pub source_len: usize,
    pub has_source: u8,
    pub enabled: u8,
    pub has_enabled: u8,
    pub reserved: u8,
    pub expected_document_revision: u64,
    pub expected_structural_revision: u64,
    pub expected_block_revision: u64,
}

/// Result of one activation. Timer names are copied into caller-owned slots.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ActivationResultRepr {
    pub block_id: [u8; MAX_IDENTIFIER_BYTES],
    pub block_id_len: u16,
    pub reserved: u16,
    pub logic_revision: u64,
    pub document_revision: u64,
    pub structural_revision: u64,
    pub enabled: u8,
    pub source_changed: u8,
    pub enabled_changed: u8,
    pub reserved2: u8,
    pub cancelled_timer_count: usize,
}

impl Default for ActivationResultRepr {
    fn default() -> Self {
        Self {
            block_id: [0; MAX_IDENTIFIER_BYTES],
            block_id_len: 0,
            reserved: 0,
            logic_revision: 0,
            document_revision: 0,
            structural_revision: 0,
            enabled: 0,
            source_changed: 0,
            enabled_changed: 0,
            reserved2: 0,
            cancelled_timer_count: 0,
        }
    }
}

/// One pending timer projection. It has no borrowed runtime memory.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TimerRepr {
    pub name: [u8; MAX_IDENTIFIER_BYTES],
    pub name_len: u16,
    pub reserved: u16,
    pub scheduled_at: u64,
    pub due_at: u64,
    pub scheduled_logic_revision: u64,
}

impl Default for TimerRepr {
    fn default() -> Self {
        Self {
            name: [0; MAX_IDENTIFIER_BYTES],
            name_len: 0,
            reserved: 0,
            scheduled_at: 0,
            due_at: 0,
            scheduled_logic_revision: 0,
        }
    }
}

/// One input projection. `valid` is zero when the input is unknown.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct InputRepr {
    pub endpoint: [u8; MAX_IDENTIFIER_BYTES],
    pub endpoint_len: u16,
    pub valid: u8,
    pub reserved: u8,
    pub value: ValueRepr,
    pub age_ms: u64,
}

impl Default for InputRepr {
    fn default() -> Self {
        Self {
            endpoint: [0; MAX_IDENTIFIER_BYTES],
            endpoint_len: 0,
            valid: 0,
            reserved: 0,
            value: ValueRepr::default(),
            age_ms: 0,
        }
    }
}

/// Compact block projection with bounded input and timer arrays.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BlockProjectionRepr {
    pub block_id: [u8; MAX_IDENTIFIER_BYTES],
    pub block_id_len: u16,
    pub health: u8,
    pub enabled: u8,
    pub logic_revision: u64,
    pub consecutive_failures: u32,
    pub input_count: usize,
    pub timer_count: usize,
    pub inputs: [InputRepr; MAX_INPUTS_PER_BLOCK],
    pub timers: [TimerRepr; MAX_TIMERS_PER_BLOCK],
}

impl Default for BlockProjectionRepr {
    fn default() -> Self {
        Self {
            block_id: [0; MAX_IDENTIFIER_BYTES],
            block_id_len: 0,
            health: 0,
            enabled: 0,
            logic_revision: 0,
            consecutive_failures: 0,
            input_count: 0,
            timer_count: 0,
            inputs: [InputRepr::default(); MAX_INPUTS_PER_BLOCK],
            timers: [TimerRepr::default(); MAX_TIMERS_PER_BLOCK],
        }
    }
}

/// Runtime usage and revision projection.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RuntimeProjectionRepr {
    pub document_revision: u64,
    pub structural_revision: u64,
    pub last_accepted_ms: u64,
    pub has_last_accepted: u8,
    pub reserved: [u8; 7],
    pub block_count: usize,
    pub signal_count: usize,
    pub signal_bindings: usize,
    pub logic_source_bytes: usize,
    pub state_entries: usize,
    pub state_bytes: usize,
    pub pending_timers: usize,
}

/// Return the ABI version expected by the C header.
#[unsafe(no_mangle)]
pub extern "C" fn logiksmith_abi_version() -> u32 {
    ABI_VERSION
}

/// Construct a runtime from bounded C arrays and UTF-8 byte strings.
///
/// # Safety
/// `config`, every non-empty array, and every non-empty byte string must point
/// to readable memory for the duration of the call. `out_runtime` must be a
/// writable pointer to one runtime handle slot. A successful handle is owned
/// by the caller and must be destroyed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn logiksmith_runtime_create(
    config: *const RuntimeConfigRepr,
    out_runtime: *mut *mut RuntimeHandle,
) -> i32 {
    if out_runtime.is_null() {
        return Status::NullPointer.code();
    }
    // SAFETY: checked above; initialize the caller's result before doing any
    // fallible work so every non-success path has a deterministic result.
    unsafe { *out_runtime = std::ptr::null_mut() };
    ffi_status(|| {
        // SAFETY: caller guarantees config points to a readable struct.
        let config = unsafe { config.as_ref() }.ok_or(Status::NullPointer)?;
        let blocks = read_array(config.blocks, config.block_count, MAX_BLOCKS)?;
        if blocks.is_empty() {
            return Err(Status::InvalidLength);
        }
        let mut block_configs = Vec::with_capacity(blocks.len());
        for block in blocks {
            block_configs.push(decode_block_config(block)?);
        }
        let runtime = Runtime::try_new_with_profile(
            RuntimeConfig::new(block_configs),
            logiksmith_core::RuntimeProfile::EmbeddedBaseline,
        )
        .map_err(|_| Status::InvalidConfiguration)?;
        let handle = Box::new(RuntimeHandle {
            runtime,
            document_revision: 1,
            structural_revision: 1,
        });
        // SAFETY: out_runtime is non-null and caller supplied writable storage.
        unsafe { *out_runtime = Box::into_raw(handle) };
        Ok(Status::Ok)
    })
}

/// Construct a runtime from a validated document/configuration wrapper.
///
/// The document's bytes and nested arrays are copied into the Rust runtime;
/// no C-owned allocation is retained after this call returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn logiksmith_runtime_create_from_document(
    document: *const RuntimeDocumentRepr,
    out_runtime: *mut *mut RuntimeHandle,
) -> i32 {
    if document.is_null() {
        return Status::NullPointer.code();
    }
    // SAFETY: the caller supplies a readable document wrapper.
    let document = unsafe { &*document };
    if document.document_revision == 0 || document.structural_revision == 0 {
        return Status::InvalidConfiguration.code();
    }
    let status = unsafe { logiksmith_runtime_create(document.config, out_runtime) };
    if status != Status::Ok.code() {
        return status;
    }
    // SAFETY: create returned a valid handle in the caller's slot.
    unsafe {
        (*out_runtime)
            .as_mut()
            .expect("successful create handle")
            .document_revision = document.document_revision;
        (*out_runtime)
            .as_mut()
            .expect("successful create handle")
            .structural_revision = document.structural_revision;
    }
    Status::Ok.code()
}

/// Parse and validate one complete automation.toml document using the shared
/// embedded profile, then construct the ABI runtime from its core projection.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn logiksmith_runtime_create_from_toml(
    document: *const RuntimeDocumentBytesRepr,
    out_runtime: *mut *mut RuntimeHandle,
) -> i32 {
    if document.is_null() || out_runtime.is_null() {
        return Status::NullPointer.code();
    }
    unsafe { *out_runtime = std::ptr::null_mut() };
    ffi_status(|| {
        let document = unsafe { &*document };
        if document.document_revision == 0
            || document.structural_revision == 0
            || document.len > MAX_DOCUMENT_BYTES
        {
            return Err(Status::InvalidLength);
        }
        let parsed = with_bytes(document.bytes, document.len, MAX_DOCUMENT_BYTES, |bytes| {
            let source = str::from_utf8(bytes).map_err(|_| Status::InvalidUtf8)?;
            parse_automation_toml(source).map_err(|_| Status::InvalidConfiguration)
        })?;
        let api =
            RuntimeApi::from_document(parsed, logiksmith_core::RuntimeProfile::EmbeddedBaseline)
                .map_err(|_| Status::InvalidConfiguration)?;
        let handle = Box::new(RuntimeHandle {
            runtime: api.runtime().clone(),
            document_revision: document.document_revision,
            structural_revision: document.structural_revision,
        });
        unsafe { *out_runtime = Box::into_raw(handle) };
        Ok(Status::Ok)
    })
}

/// Destroy a handle previously returned by [`runtime_create`].
///
/// A null handle is accepted as an idempotent no-op. Any non-null pointer must
/// be the live pointer returned by this crate and must not be used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn logiksmith_runtime_destroy(runtime: *mut RuntimeHandle) -> i32 {
    if runtime.is_null() {
        return Status::Ok.code();
    }
    ffi_status(|| {
        // SAFETY: caller contract requires an owned pointer from create.
        unsafe { drop(Box::from_raw(runtime)) };
        Ok(Status::Ok)
    })
}

/// Process a triggering input and copy logical effects into a caller buffer.
///
/// The operation is transactional with respect to output capacity: when the
/// buffer is too small, the runtime is restored and `written` reports the
/// required number of records. The runtime may still accept an input whose Lua
/// program returns a logic error; in that case no effects are emitted and the
/// status is [`Status::LogicError`].
///
/// # Safety
/// `runtime`, `event`, and `written` must be valid pointers. `effects` must be
/// writable for `capacity` records, unless `capacity` is zero (where null is
/// accepted). Pointers inside `event` must remain readable for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn logiksmith_runtime_process_input(
    runtime: *mut RuntimeHandle,
    event: *const InputEventRepr,
    effects: *mut EffectRepr,
    capacity: usize,
    written: *mut usize,
) -> i32 {
    if written.is_null() {
        return Status::NullPointer.code();
    }
    // SAFETY: checked above; all return paths below leave this initialized.
    unsafe { *written = 0 };
    if runtime.is_null() || event.is_null() {
        return Status::NullPointer.code();
    }
    if capacity > 0 && effects.is_null() {
        return Status::NullPointer.code();
    }
    if capacity > MAX_EFFECTS {
        return Status::InvalidLength.code();
    }
    ffi_status(|| {
        // SAFETY: pointers were checked and caller owns their readable data.
        let event = unsafe { event.as_ref() }.ok_or(Status::NullPointer)?;
        let block_id = decode_block_id(event.block_id, event.block_id_len)?;
        let endpoint = decode_endpoint_name(event.endpoint, event.endpoint_len)?;
        if event.group_address == 0 || event.group_address > MAX_KNX_GROUP_ADDRESS {
            return Err(Status::InvalidGroupAddress);
        }
        let value = decode_value(event.value)?;
        let input = InputEvent::new(endpoint, value);

        // Core events can assign input state and then traverse a signal
        // cascade. Keep a clone so a caller can retry after buffer sizing or a
        // transport-facing routing error without a partially committed state.
        let handle = unsafe { &mut *runtime };
        let before = handle.runtime.clone();
        let processed = catch_unwind(AssertUnwindSafe(|| {
            handle
                .runtime
                .process_input_cascade(&block_id, input, MonotonicMs(event.monotonic_ms))
        }));
        let executions = match processed {
            Ok(Ok(executions)) => executions,
            Ok(Err(error)) => {
                handle.runtime = before;
                return Err(status_for_runtime_event(&error));
            }
            Err(_) => {
                handle.runtime = before;
                return Err(Status::Panic);
            }
        };

        let output_count = match count_effects(&executions) {
            Ok(count) => count,
            Err(status) => {
                // A logic error is a contained execution outcome. The input
                // observation remains accepted, but there is no effect list.
                // Do not roll back that semantic observation.
                return Err(status);
            }
        };
        if output_count > capacity {
            handle.runtime = before;
            // SAFETY: written was checked and points to writable storage.
            unsafe { *written = output_count };
            return Err(Status::OutputBufferTooSmall);
        }
        if output_count > 0 {
            // SAFETY: capacity was checked against output_count and the caller
            // supplied a writable buffer for all records.
            let effects = unsafe { slice::from_raw_parts_mut(effects, output_count) };
            if let Err(status) = write_effects(&executions, effects) {
                handle.runtime = before;
                return Err(status);
            }
        }
        // SAFETY: written was checked and points to writable storage.
        unsafe { *written = output_count };
        Ok(Status::Ok)
    })
}

/// Process at most one globally earliest due timer and return its effects.
/// Buffer exhaustion restores the complete runtime, including the consumed
/// timer, so the caller can retry with a larger buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn logiksmith_runtime_process_due_timer(
    runtime: *mut RuntimeHandle,
    monotonic_ms: u64,
    effects: *mut EffectRepr,
    capacity: usize,
    written: *mut usize,
) -> i32 {
    process_runtime_operation(runtime, effects, capacity, written, |runtime| {
        runtime
            .process_next_due_timer_cascade(MonotonicMs(monotonic_ms))
            .map(|executions| executions)
    })
}

/// Validate a source without changing the runtime. The revision is the core's
/// deterministic source-content revision, not a document revision.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn logiksmith_runtime_validate_source(
    source: *const u8,
    source_len: usize,
    revision: *mut u64,
) -> i32 {
    if revision.is_null() {
        return Status::NullPointer.code();
    }
    // SAFETY: checked output pointer; initialize all failure paths.
    unsafe { *revision = 0 };
    ffi_status(|| {
        let revision_value = with_utf8(source, source_len, MAX_SOURCE_BYTES, |source| {
            Runtime::validate_source(source).map_err(|_| Status::InvalidSource)
        })?;
        // SAFETY: revision is non-null and caller-owned writable storage.
        unsafe { *revision = revision_value };
        Ok(Status::Ok)
    })
}

/// Simulate an input against a copy of the block's current inputs, state and
/// pending timers. The optional source is installed only on a throwaway core
/// clone, and effects are never sent through the live output path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn logiksmith_runtime_simulate_input(
    runtime: *mut RuntimeHandle,
    event: *const InputEventRepr,
    source: *const u8,
    source_len: usize,
    effects: *mut EffectRepr,
    capacity: usize,
    written: *mut usize,
) -> i32 {
    if written.is_null() {
        return Status::NullPointer.code();
    }
    unsafe { *written = 0 };
    if runtime.is_null() || event.is_null() {
        return Status::NullPointer.code();
    }
    if capacity > MAX_EFFECTS || (capacity > 0 && effects.is_null()) {
        return if capacity > MAX_EFFECTS {
            Status::InvalidLength.code()
        } else {
            Status::NullPointer.code()
        };
    }
    ffi_status(|| {
        // SAFETY: pointers checked above and readable for this call.
        let event = unsafe { &*event };
        let block_id = decode_block_id(event.block_id, event.block_id_len)?;
        let endpoint = decode_endpoint_name(event.endpoint, event.endpoint_len)?;
        let value = decode_value(event.value)?;
        let handle = unsafe { &*runtime };
        let snapshot = handle
            .runtime
            .block(&block_id)
            .ok_or(Status::UnknownBlock)?
            .snapshot_at(MonotonicMs(event.monotonic_ms));
        let mut inputs: Vec<SimulationInput> = snapshot
            .inputs
            .iter()
            .map(simulation_input_from_snapshot)
            .collect();
        let Some(trigger_input) = inputs.iter_mut().find(|input| input.endpoint == endpoint) else {
            return Err(Status::UnknownEndpoint);
        };
        trigger_input.value = Some(value);
        trigger_input.valid = true;
        trigger_input.age_ms = Some(0);
        let scenario = SimulationScenario {
            trigger: SimulationTrigger {
                endpoint: endpoint.clone(),
                value,
                previous: snapshot
                    .inputs
                    .iter()
                    .find(|input| input.endpoint == endpoint)
                    .and_then(|input| input.value),
            },
            inputs,
        };
        let result = if source_len == 0 && source.is_null() {
            handle.runtime.simulate_input(&block_id, scenario)
        } else {
            let source = with_utf8(source, source_len, MAX_SOURCE_BYTES, |source| {
                Ok(source.to_owned())
            })?;
            handle.runtime.simulate_input_with_source(
                &block_id,
                source,
                scenario,
                snapshot.state,
                snapshot.pending_timers,
                MonotonicMs(event.monotonic_ms),
            )
        };
        let execution = result.map_err(|_| Status::InvalidConfiguration)?;
        write_execution_effects(&[execution], effects, capacity, written)
    })
}

/// Simulate one named timer against copied block state. `timer` is a bounded
/// UTF-8 name; no live timer is consumed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn logiksmith_runtime_simulate_timer(
    runtime: *mut RuntimeHandle,
    block_id: *const u8,
    block_id_len: usize,
    timer: *const u8,
    timer_len: usize,
    fired_at_ms: u64,
    source: *const u8,
    source_len: usize,
    effects: *mut EffectRepr,
    capacity: usize,
    written: *mut usize,
) -> i32 {
    if written.is_null() {
        return Status::NullPointer.code();
    }
    unsafe { *written = 0 };
    if runtime.is_null() || (capacity > 0 && effects.is_null()) {
        return Status::NullPointer.code();
    }
    if capacity > MAX_EFFECTS {
        return Status::InvalidLength.code();
    }
    ffi_status(|| {
        let block_id = decode_block_id(block_id, block_id_len)?;
        let timer = with_utf8(timer, timer_len, MAX_IDENTIFIER_BYTES, |timer| {
            timer.parse().map_err(|_| Status::UnknownTimer)
        })?;
        let handle = unsafe { &*runtime };
        let snapshot = handle
            .runtime
            .block(&block_id)
            .ok_or(Status::UnknownBlock)?
            .snapshot_at(MonotonicMs(fired_at_ms));
        if !snapshot
            .pending_timers
            .iter()
            .any(|pending| pending.name == timer)
        {
            return Err(Status::UnknownTimer);
        }
        let scenario = TimerSimulationScenario {
            timer,
            fired_at: MonotonicMs(fired_at_ms),
            inputs: snapshot
                .inputs
                .iter()
                .map(simulation_input_from_snapshot)
                .collect(),
            state: snapshot.state.clone(),
            pending_timers: snapshot.pending_timers.clone(),
        };
        let execution = if source_len == 0 && source.is_null() {
            handle.runtime.simulate_timer(&block_id, scenario)
        } else {
            let source = with_utf8(source, source_len, MAX_SOURCE_BYTES, |source| {
                Ok(source.to_owned())
            })?;
            handle
                .runtime
                .simulate_timer_with_source(&block_id, source, scenario)
        }
        .map_err(|_| Status::InvalidConfiguration)?;
        write_execution_effects(&[execution], effects, capacity, written)
    })
}

/// Activate source and/or enabled state after CAS checks. Core validation is
/// completed before mutation; document revision is advanced only on success.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn logiksmith_runtime_activate(
    runtime: *mut RuntimeHandle,
    activation: *const ActivationRepr,
    result: *mut ActivationResultRepr,
) -> i32 {
    if result.is_null() {
        return Status::NullPointer.code();
    }
    unsafe { *result = ActivationResultRepr::default() };
    if runtime.is_null() || activation.is_null() {
        return Status::NullPointer.code();
    }
    ffi_status(|| {
        // SAFETY: pointers checked above and caller owns their readable data.
        let activation = unsafe { &*activation };
        if activation.reserved != 0
            || activation.has_source > 1
            || activation.has_enabled > 1
            || (activation.has_enabled == 1 && activation.enabled > 1)
            || (activation.has_source == 0 && activation.has_enabled == 0)
        {
            return Err(Status::InvalidConfiguration);
        }
        let handle = unsafe { &mut *runtime };
        check_revision(
            handle.document_revision,
            activation.expected_document_revision,
        )?;
        check_revision(
            handle.structural_revision,
            activation.expected_structural_revision,
        )?;
        let block_id = decode_block_id(activation.block_id, activation.block_id_len)?;
        let expected_block = handle
            .runtime
            .block(&block_id)
            .ok_or(Status::UnknownBlock)?
            .active_logic_revision();
        check_revision(expected_block, activation.expected_block_revision)?;
        let source = if activation.has_source == 1 {
            Some(with_utf8(
                activation.source,
                activation.source_len,
                MAX_SOURCE_BYTES,
                |source| Ok(source.to_owned()),
            )?)
        } else {
            None
        };
        let candidate = RuntimeActivation::single(logiksmith_core::BlockActivation::new(
            block_id.clone(),
            source,
            (activation.has_enabled == 1).then_some(activation.enabled != 0),
        ));
        let activated = handle
            .runtime
            .activate(candidate)
            .map_err(|_| Status::InvalidSource)?;
        let block = activated
            .blocks
            .first()
            .ok_or(Status::InvalidConfiguration)?;
        if block.source_changed || block.enabled_changed {
            handle.document_revision = handle
                .document_revision
                .checked_add(1)
                .ok_or(Status::RuntimeLimit)?;
        }
        // SAFETY: result is non-null and caller-owned writable storage.
        unsafe {
            (*result).block_id_len =
                copy_identifier_len(&mut (*result).block_id, block.block_id.as_str())?;
            (*result).logic_revision = block.logic_revision;
            (*result).document_revision = handle.document_revision;
            (*result).structural_revision = handle.structural_revision;
            (*result).enabled = u8::from(block.enabled);
            (*result).source_changed = u8::from(block.source_changed);
            (*result).enabled_changed = u8::from(block.enabled_changed);
            (*result).cancelled_timer_count = block.cancelled_timers.len();
        }
        Ok(Status::Ok)
    })
}

/// Enable/disable one block with the same revision and cancellation semantics
/// as [`logiksmith_runtime_activate`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn logiksmith_runtime_set_enabled(
    runtime: *mut RuntimeHandle,
    block_id: *const u8,
    block_id_len: usize,
    enabled: u8,
    expected_document_revision: u64,
    expected_structural_revision: u64,
    expected_block_revision: u64,
    result: *mut ActivationResultRepr,
) -> i32 {
    if enabled > 1 {
        return Status::InvalidValue.code();
    }
    let block = ActivationRepr {
        block_id,
        block_id_len,
        source: std::ptr::null(),
        source_len: 0,
        has_source: 0,
        enabled,
        has_enabled: 1,
        reserved: 0,
        expected_document_revision,
        expected_structural_revision,
        expected_block_revision,
    };
    // SAFETY: this forwards the caller's validated pointers and result slot.
    unsafe { logiksmith_runtime_activate(runtime, &block, result) }
}

/// Clear operational suspension for one block. Resuming never restores timers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn logiksmith_runtime_resume(
    runtime: *mut RuntimeHandle,
    block_id: *const u8,
    block_id_len: usize,
    expected_document_revision: u64,
    expected_structural_revision: u64,
    expected_block_revision: u64,
) -> i32 {
    if runtime.is_null() {
        return Status::NullPointer.code();
    }
    ffi_status(|| {
        let handle = unsafe { &mut *runtime };
        check_revision(handle.document_revision, expected_document_revision)?;
        check_revision(handle.structural_revision, expected_structural_revision)?;
        let block_id = decode_block_id(block_id, block_id_len)?;
        let current = handle
            .runtime
            .block(&block_id)
            .ok_or(Status::UnknownBlock)?
            .active_logic_revision();
        check_revision(current, expected_block_revision)?;
        handle
            .runtime
            .resume_block(&block_id)
            .map_err(|_| Status::UnknownBlock)?;
        Ok(Status::Ok)
    })
}

/// Return the bounded runtime usage/revision projection without allocating in C.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn logiksmith_runtime_project(
    runtime: *const RuntimeHandle,
    result: *mut RuntimeProjectionRepr,
) -> i32 {
    if runtime.is_null() || result.is_null() {
        return Status::NullPointer.code();
    }
    ffi_status(|| {
        let handle = unsafe { &*runtime };
        let usage = handle.runtime.usage();
        let last = handle.runtime.last_accepted_at();
        // SAFETY: result is non-null and caller-owned writable storage.
        unsafe {
            *result = RuntimeProjectionRepr {
                document_revision: handle.document_revision,
                structural_revision: handle.structural_revision,
                last_accepted_ms: last.map_or(0, |value| value.0),
                has_last_accepted: u8::from(last.is_some()),
                reserved: [0; 7],
                block_count: usage.logic_blocks,
                signal_count: usage.signals,
                signal_bindings: usage.signal_bindings,
                logic_source_bytes: usage.logic_source_bytes,
                state_entries: usage.state_entries,
                state_bytes: usage.state_bytes,
                pending_timers: usage.pending_timers,
            };
        }
        Ok(Status::Ok)
    })
}

/// Return one compact block projection. If `result` is null the required
/// serialized projection size is returned through `written` without mutation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn logiksmith_runtime_project_block(
    runtime: *const RuntimeHandle,
    block_id: *const u8,
    block_id_len: usize,
    result: *mut BlockProjectionRepr,
    written: *mut usize,
) -> i32 {
    if written.is_null() || runtime.is_null() {
        return Status::NullPointer.code();
    }
    unsafe { *written = 0 };
    ffi_status(|| {
        let block_id = decode_block_id(block_id, block_id_len)?;
        let handle = unsafe { &*runtime };
        let snapshot = handle
            .runtime
            .block(&block_id)
            .ok_or(Status::UnknownBlock)?
            .snapshot_at(handle.runtime.last_accepted_at().unwrap_or_default());
        let required = snapshot
            .inputs
            .len()
            .saturating_add(snapshot.pending_timers.len());
        if snapshot.inputs.len() > MAX_INPUTS_PER_BLOCK
            || snapshot.pending_timers.len() > MAX_TIMERS_PER_BLOCK
        {
            unsafe { *written = required };
            return Err(Status::RuntimeLimit);
        }
        if result.is_null() {
            unsafe { *written = 1 };
            return Err(Status::BufferTooSmall);
        }
        let destination = unsafe { &mut *result };
        *destination = BlockProjectionRepr::default();
        destination.block_id_len =
            copy_identifier_len(&mut destination.block_id, snapshot.id.as_str())?;
        destination.health = health_code(snapshot.health);
        destination.enabled = u8::from(snapshot.enabled);
        destination.logic_revision = snapshot.logic_revision;
        destination.consecutive_failures = snapshot.consecutive_failures;
        destination.input_count = snapshot.inputs.len();
        destination.timer_count = snapshot.pending_timers.len();
        for (index, input) in snapshot.inputs.iter().enumerate() {
            destination.inputs[index] = input_repr(input)?;
        }
        for (index, timer) in snapshot.pending_timers.iter().enumerate() {
            destination.timers[index] = timer_repr(timer)?;
        }
        unsafe { *written = 1 };
        Ok(Status::Ok)
    })
}

fn process_runtime_operation(
    runtime: *mut RuntimeHandle,
    effects: *mut EffectRepr,
    capacity: usize,
    written: *mut usize,
    operation: impl FnOnce(
        &mut Runtime,
    ) -> Result<Vec<logiksmith_core::BlockExecution>, RuntimeEventError>,
) -> i32 {
    if written.is_null() {
        return Status::NullPointer.code();
    }
    unsafe { *written = 0 };
    if runtime.is_null() || (capacity > 0 && effects.is_null()) {
        return Status::NullPointer.code();
    }
    if capacity > MAX_EFFECTS {
        return Status::InvalidLength.code();
    }
    ffi_status(|| {
        let handle = unsafe { &mut *runtime };
        let before = handle.runtime.clone();
        let executions = match operation(&mut handle.runtime) {
            Ok(executions) => executions,
            Err(error) => {
                handle.runtime = before;
                return Err(status_for_runtime_event(&error));
            }
        };
        write_execution_effects(&executions, effects, capacity, written).map_err(|status| {
            handle.runtime = before;
            status
        })
    })
}

fn write_execution_effects(
    executions: &[logiksmith_core::BlockExecution],
    effects: *mut EffectRepr,
    capacity: usize,
    written: *mut usize,
) -> Result<Status, Status> {
    let output_count = count_effects(executions)?;
    if output_count > MAX_EFFECTS {
        return Err(Status::RuntimeLimit);
    }
    if output_count > capacity {
        // SAFETY: caller checked this pointer before entering the operation;
        // zero-capacity requests may use null and never write records.
        unsafe { *written = output_count };
        return Err(Status::OutputBufferTooSmall);
    }
    if output_count > 0 {
        // SAFETY: capacity is at least output_count and the caller supplied
        // writable storage for all records.
        let destination = unsafe { slice::from_raw_parts_mut(effects, output_count) };
        write_effects(executions, destination)?;
    }
    // SAFETY: written is non-null by every public caller.
    unsafe { *written = output_count };
    Ok(Status::Ok)
}

fn check_revision(current: u64, expected: u64) -> Result<(), Status> {
    if expected != 0 && expected != current {
        return Err(Status::RevisionConflict);
    }
    Ok(())
}

fn simulation_input_from_snapshot(input: &InputSnapshot) -> SimulationInput {
    SimulationInput {
        endpoint: input.endpoint.clone(),
        value: input.value,
        valid: input.valid,
        age_ms: input.age_ms,
    }
}

fn input_repr(input: &InputSnapshot) -> Result<InputRepr, Status> {
    let mut result = InputRepr::default();
    result.endpoint_len = copy_identifier_len(&mut result.endpoint, input.endpoint.as_str())?;
    result.valid = u8::from(input.valid);
    result.value = input
        .value
        .map(encode_value)
        .transpose()?
        .unwrap_or_default();
    result.age_ms = input.age_ms.unwrap_or(0);
    Ok(result)
}

fn timer_repr(timer: &logiksmith_core::PendingTimer) -> Result<TimerRepr, Status> {
    let mut result = TimerRepr::default();
    result.name_len = copy_identifier_len(&mut result.name, timer.name.as_str())?;
    result.scheduled_at = timer.scheduled_at.0;
    result.due_at = timer.due_at.0;
    result.scheduled_logic_revision = timer.scheduled_logic_revision;
    Ok(result)
}

fn copy_identifier_len(
    destination: &mut [u8; MAX_IDENTIFIER_BYTES],
    value: &str,
) -> Result<u16, Status> {
    if value.len() > MAX_IDENTIFIER_BYTES {
        return Err(Status::InvalidLength);
    }
    destination[..value.len()].copy_from_slice(value.as_bytes());
    destination[value.len()..].fill(0);
    Ok(value.len() as u16)
}

fn health_code(health: logiksmith_core::BlockHealth) -> u8 {
    match health {
        logiksmith_core::BlockHealth::Disabled => 0,
        logiksmith_core::BlockHealth::Active => 1,
        logiksmith_core::BlockHealth::SuspendedScriptFailures => 2,
        logiksmith_core::BlockHealth::SuspendedEventRate => 3,
    }
}

fn ffi_status(operation: impl FnOnce() -> Result<Status, Status>) -> i32 {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(status)) | Ok(Err(status)) => status.code(),
        Err(_) => Status::Panic.code(),
    }
}

fn read_array<'a, T>(pointer: *const T, length: usize, maximum: usize) -> Result<&'a [T], Status> {
    if length > maximum {
        return Err(Status::InvalidLength);
    }
    if length == 0 {
        return Ok(&[]);
    }
    if pointer.is_null() {
        return Err(Status::NullPointer);
    }
    // SAFETY: caller contract supplies a readable array of `length` entries.
    Ok(unsafe { slice::from_raw_parts(pointer, length) })
}

fn with_bytes<T>(
    pointer: *const u8,
    length: usize,
    maximum: usize,
    operation: impl FnOnce(&[u8]) -> Result<T, Status>,
) -> Result<T, Status> {
    if length > maximum {
        return Err(Status::InvalidLength);
    }
    if length == 0 {
        return operation(&[]);
    }
    if pointer.is_null() {
        return Err(Status::NullPointer);
    }
    // SAFETY: caller contract supplies a readable byte string of `length`.
    operation(unsafe { slice::from_raw_parts(pointer, length) })
}

fn with_utf8<T>(
    pointer: *const u8,
    length: usize,
    maximum: usize,
    operation: impl FnOnce(&str) -> Result<T, Status>,
) -> Result<T, Status> {
    with_bytes(pointer, length, maximum, |bytes| {
        let value = str::from_utf8(bytes).map_err(|_| Status::InvalidUtf8)?;
        operation(value)
    })
}

fn decode_block_id(pointer: *const u8, length: usize) -> Result<BlockId, Status> {
    with_utf8(pointer, length, MAX_IDENTIFIER_BYTES, |value| {
        value.parse().map_err(|_| Status::InvalidBlockId)
    })
}

fn decode_endpoint_name(pointer: *const u8, length: usize) -> Result<EndpointName, Status> {
    with_utf8(pointer, length, MAX_IDENTIFIER_BYTES, |value| {
        value.parse().map_err(|_| Status::InvalidEndpointName)
    })
}

fn decode_block_config(config: &BlockConfigRepr) -> Result<BlockConfig, Status> {
    let block_id = decode_block_id(config.block_id, config.block_id_len)?;
    let source = with_utf8(
        config.logic_source,
        config.logic_source_len,
        MAX_SOURCE_BYTES,
        |value| Ok(value.to_owned()),
    )?;
    let endpoint_configs = read_array(
        config.endpoints,
        config.endpoint_count,
        MAX_ENDPOINTS_PER_BLOCK,
    )?;
    if endpoint_configs.is_empty() {
        return Err(Status::InvalidLength);
    }
    let mut endpoints = Vec::with_capacity(endpoint_configs.len());
    for endpoint in endpoint_configs {
        if endpoint.reserved != [0; 3] {
            return Err(Status::InvalidConfiguration);
        }
        let name = decode_endpoint_name(endpoint.name, endpoint.name_len)?;
        let direction = match endpoint.direction {
            INPUT => EndpointDirection::Input,
            OUTPUT => EndpointDirection::Output,
            _ => return Err(Status::InvalidConfiguration),
        };
        let dpt =
            Dpt::new(endpoint.dpt_major, endpoint.dpt_subtype).map_err(|_| Status::InvalidDpt)?;
        if !dpt.is_supported() {
            return Err(Status::InvalidDpt);
        }
        endpoints.push(Endpoint::new(name, direction, dpt));
    }
    Ok(BlockConfig::new(block_id, true, endpoints, source))
}

fn decode_value(value: ValueRepr) -> Result<TypedValue, Status> {
    if value.reserved != 0 || value.reserved2 != 0 {
        return Err(Status::InvalidValue);
    }
    let dpt = Dpt::new(value.dpt_major, value.dpt_subtype).map_err(|_| Status::InvalidDpt)?;
    let semantic = match value.kind {
        VALUE_BOOL if value.scalar == 0 || value.scalar == 1 => Value::Bool(value.scalar == 1),
        VALUE_PERCENT if (0..=100).contains(&value.scalar) => Value::Percent(value.scalar as u8),
        VALUE_TEMPERATURE_CENTI_DEGREES => Value::Temperature(value.scalar),
        _ => return Err(Status::InvalidValue),
    };
    TypedValue::new(dpt, semantic).map_err(|_| Status::InvalidValue)
}

fn count_effects(executions: &[logiksmith_core::BlockExecution]) -> Result<usize, Status> {
    let mut count = 0usize;
    for execution in executions {
        let transition = execution
            .execution
            .outcome
            .as_ref()
            .map_err(|_| Status::LogicError)?;
        count = count
            .checked_add(transition.outputs.len())
            .ok_or(Status::InvalidLength)?;
    }
    Ok(count)
}

fn write_effects(
    executions: &[logiksmith_core::BlockExecution],
    destination: &mut [EffectRepr],
) -> Result<(), Status> {
    let mut index = 0;
    for execution in executions {
        let transition = execution
            .execution
            .outcome
            .as_ref()
            .map_err(|_| Status::LogicError)?;
        for output in &transition.outputs {
            let record = destination.get_mut(index).ok_or(Status::InvalidLength)?;
            copy_identifier(
                &mut record.block_id,
                &mut record.block_id_len,
                execution.block_id.as_str(),
            )?;
            copy_identifier(
                &mut record.endpoint,
                &mut record.endpoint_len,
                output.endpoint.as_str(),
            )?;
            record.value = encode_value(output.value)?;
            index += 1;
        }
    }
    Ok(())
}

fn copy_identifier(
    destination: &mut [u8; MAX_IDENTIFIER_BYTES],
    length: &mut u16,
    value: &str,
) -> Result<(), Status> {
    if value.len() > MAX_IDENTIFIER_BYTES {
        return Err(Status::InvalidLength);
    }
    destination[..value.len()].copy_from_slice(value.as_bytes());
    destination[value.len()..].fill(0);
    *length = value.len() as u16;
    Ok(())
}

fn encode_value(value: TypedValue) -> Result<ValueRepr, Status> {
    let (kind, scalar) = match value.value() {
        Value::Bool(value) => (VALUE_BOOL, i32::from(value)),
        Value::Percent(value) => (VALUE_PERCENT, i32::from(value)),
        Value::Temperature(value) => (VALUE_TEMPERATURE_CENTI_DEGREES, value),
    };
    Ok(ValueRepr {
        dpt_major: value.dpt().major(),
        dpt_subtype: value.dpt().subtype(),
        kind,
        reserved: 0,
        reserved2: 0,
        scalar,
    })
}

fn status_for_runtime_event(error: &RuntimeEventError) -> Status {
    match error {
        RuntimeEventError::UnknownBlock(_) => Status::UnknownBlock,
        RuntimeEventError::TimeWentBackwards { .. } => Status::TimeWentBackwards,
        RuntimeEventError::CascadeLimit { .. }
        | RuntimeEventError::ResourceLimit { .. }
        | RuntimeEventError::CascadeTimeLimit { .. } => Status::RuntimeLimit,
        RuntimeEventError::Block { error, .. } => match error {
            logiksmith_core::EventError::UnknownEndpoint(_) => Status::UnknownEndpoint,
            logiksmith_core::EventError::EndpointNotInput { .. } => Status::EndpointNotInput,
            logiksmith_core::EventError::DptMismatch { .. } => Status::DptMismatch,
            logiksmith_core::EventError::InvalidValue(_) => Status::InvalidValue,
            logiksmith_core::EventError::TimeWentBackwards { .. } => Status::TimeWentBackwards,
            logiksmith_core::EventError::StaleTimer { .. } => Status::InvalidConfiguration,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{panic::catch_unwind, ptr};

    fn endpoint(name: &'static [u8], direction: u8) -> EndpointConfig {
        EndpointConfig {
            name: name.as_ptr(),
            name_len: name.len(),
            direction,
            reserved: [0; 3],
            dpt_major: 1,
            dpt_subtype: 1,
        }
    }

    fn fixture() -> (RuntimeConfigRepr, LogiksmithInputEventFixture) {
        let endpoints = Box::leak(Box::new([
            endpoint(b"switch", INPUT),
            endpoint(b"light", OUTPUT),
        ]));
        let source = Box::leak(
            b"function handle(event, input)\n  if event.input == 'switch' then\n    return { outputs = { light = event.value } }\n  end\nend\0".to_vec().into_boxed_slice(),
        );
        let block = Box::leak(Box::new(BlockConfigRepr {
            block_id: b"main".as_ptr(),
            block_id_len: 4,
            logic_source: source.as_ptr(),
            logic_source_len: source.len() - 1,
            endpoints: endpoints.as_ptr(),
            endpoint_count: endpoints.len(),
        }));
        let config = RuntimeConfigRepr {
            blocks: block,
            block_count: 1,
        };
        let event = LogiksmithInputEventFixture {
            event: InputEventRepr {
                block_id: b"main".as_ptr(),
                block_id_len: 4,
                endpoint: b"switch".as_ptr(),
                endpoint_len: 6,
                value: ValueRepr {
                    dpt_major: 1,
                    dpt_subtype: 1,
                    kind: VALUE_BOOL,
                    reserved: 0,
                    reserved2: 0,
                    scalar: 1,
                },
                source_address: 0x1101,
                group_address: 0x1234,
                monotonic_ms: 1,
            },
            _endpoints: endpoints,
            _source: source,
            _block: block,
        };
        (config, event)
    }

    struct LogiksmithInputEventFixture {
        event: InputEventRepr,
        _endpoints: &'static mut [EndpointConfig; 2],
        _source: &'static mut [u8],
        _block: &'static mut BlockConfigRepr,
    }

    fn create_fixture() -> (*mut RuntimeHandle, LogiksmithInputEventFixture) {
        let (config, event) = fixture();
        let mut runtime = ptr::null_mut();
        let status = unsafe { logiksmith_runtime_create(&config, &mut runtime) };
        assert_eq!(status, Status::Ok.code());
        (runtime, event)
    }

    #[test]
    fn abi_version_and_status_numbers_are_stable() {
        assert_eq!(logiksmith_abi_version(), ABI_VERSION);
        assert_eq!(Status::Ok.code(), 0);
        assert_eq!(Status::OutputBufferTooSmall.code(), 15);
        assert_eq!(Status::Panic.code(), 16);
        assert_eq!(Status::RuntimeLimit.code(), 18);
    }

    #[test]
    fn m13_runtime_limits_have_a_stable_abi_status() {
        assert_eq!(
            status_for_runtime_event(&RuntimeEventError::CascadeLimit {
                actual: 2,
                maximum: 1,
            }),
            Status::RuntimeLimit
        );
        assert_eq!(
            status_for_runtime_event(&RuntimeEventError::ResourceLimit {
                resource: "state",
                actual: 2,
                maximum: 1,
            }),
            Status::RuntimeLimit
        );
        assert_eq!(
            status_for_runtime_event(&RuntimeEventError::CascadeTimeLimit {
                elapsed_ms: 2,
                maximum_ms: 1,
            }),
            Status::RuntimeLimit
        );
    }

    #[test]
    fn valid_event_is_processed_into_caller_owned_typed_effect() {
        let (runtime, fixture) = create_fixture();
        let mut effects = [EffectRepr::default(); 1];
        let mut written = 0;
        let status = unsafe {
            logiksmith_runtime_process_input(
                runtime,
                &fixture.event,
                effects.as_mut_ptr(),
                effects.len(),
                &mut written,
            )
        };
        assert_eq!(status, Status::Ok.code());
        assert_eq!(written, 1);
        assert_eq!(
            &effects[0].block_id[..effects[0].block_id_len as usize],
            b"main"
        );
        assert_eq!(
            &effects[0].endpoint[..effects[0].endpoint_len as usize],
            b"light"
        );
        assert_eq!(effects[0].value, fixture.event.value);
        assert_eq!(
            unsafe { logiksmith_runtime_destroy(runtime) },
            Status::Ok.code()
        );
    }

    #[test]
    fn malformed_pointers_and_values_are_rejected_without_unwinding() {
        let (runtime, fixture) = create_fixture();
        let mut written = 99;
        let mut event = fixture.event;
        event.endpoint = ptr::null();
        event.endpoint_len = 1;
        let result = catch_unwind(AssertUnwindSafe(|| unsafe {
            logiksmith_runtime_process_input(runtime, &event, ptr::null_mut(), 0, &mut written)
        }));
        assert_eq!(result.unwrap(), Status::NullPointer.code());
        assert_eq!(written, 0);

        event = fixture.event;
        event.endpoint_len = MAX_IDENTIFIER_BYTES + 1;
        assert_eq!(
            unsafe {
                logiksmith_runtime_process_input(runtime, &event, ptr::null_mut(), 0, &mut written)
            },
            Status::InvalidLength.code()
        );

        event = fixture.event;
        event.endpoint = b"Switch".as_ptr();
        event.endpoint_len = 6;
        assert_eq!(
            unsafe {
                logiksmith_runtime_process_input(runtime, &event, ptr::null_mut(), 0, &mut written)
            },
            Status::InvalidEndpointName.code()
        );

        event = fixture.event;
        event.endpoint = [0xff].as_ptr();
        event.endpoint_len = 1;
        assert_eq!(
            unsafe {
                logiksmith_runtime_process_input(runtime, &event, ptr::null_mut(), 0, &mut written)
            },
            Status::InvalidUtf8.code()
        );

        event = fixture.event;
        event.value.scalar = 2;
        assert_eq!(
            unsafe {
                logiksmith_runtime_process_input(runtime, &event, ptr::null_mut(), 0, &mut written)
            },
            Status::InvalidValue.code()
        );

        event = fixture.event;
        event.group_address = MAX_KNX_GROUP_ADDRESS + 1;
        assert_eq!(
            unsafe {
                logiksmith_runtime_process_input(runtime, &event, ptr::null_mut(), 0, &mut written)
            },
            Status::InvalidGroupAddress.code()
        );

        event = fixture.event;
        event.value = ValueRepr {
            dpt_major: 5,
            dpt_subtype: 1,
            kind: VALUE_PERCENT,
            reserved: 0,
            reserved2: 0,
            scalar: 50,
        };
        assert_eq!(
            unsafe {
                logiksmith_runtime_process_input(runtime, &event, ptr::null_mut(), 0, &mut written)
            },
            Status::DptMismatch.code()
        );

        let mut oversized_effects = [EffectRepr::default(); MAX_EFFECTS + 1];
        assert_eq!(
            unsafe {
                logiksmith_runtime_process_input(
                    runtime,
                    &fixture.event,
                    oversized_effects.as_mut_ptr(),
                    MAX_EFFECTS + 1,
                    &mut written,
                )
            },
            Status::InvalidLength.code()
        );
        assert_eq!(
            unsafe { logiksmith_runtime_destroy(runtime) },
            Status::Ok.code()
        );
    }

    #[test]
    fn undersized_effect_buffer_rolls_back_and_reports_required_capacity() {
        let (runtime, fixture) = create_fixture();
        let mut written = 0;
        let status = unsafe {
            logiksmith_runtime_process_input(
                runtime,
                &fixture.event,
                ptr::null_mut(),
                0,
                &mut written,
            )
        };
        assert_eq!(status, Status::OutputBufferTooSmall.code());
        assert_eq!(written, 1);

        let mut effects = [EffectRepr::default(); 1];
        let status = unsafe {
            logiksmith_runtime_process_input(
                runtime,
                &fixture.event,
                effects.as_mut_ptr(),
                effects.len(),
                &mut written,
            )
        };
        assert_eq!(status, Status::Ok.code());
        assert_eq!(written, 1);
        assert_eq!(
            unsafe { logiksmith_runtime_destroy(runtime) },
            Status::Ok.code()
        );
    }

    #[test]
    fn malformed_configuration_is_rejected_before_handle_creation() {
        let (mut oversized_config, _fixture) = fixture();
        oversized_config.block_count = MAX_BLOCKS + 1;
        let mut runtime = ptr::null_mut();
        assert_eq!(
            unsafe { logiksmith_runtime_create(&oversized_config, &mut runtime) },
            Status::InvalidLength.code()
        );
        assert!(runtime.is_null());

        let config = RuntimeConfigRepr {
            blocks: ptr::null(),
            block_count: 1,
        };
        let mut runtime = ptr::null_mut();
        assert_eq!(
            unsafe { logiksmith_runtime_create(&config, &mut runtime) },
            Status::NullPointer.code()
        );
        assert!(runtime.is_null());
    }

    #[test]
    fn projection_validation_simulation_and_revision_gate_are_bounded() {
        let (runtime, fixture) = create_fixture();
        let mut projection = RuntimeProjectionRepr::default();
        assert_eq!(
            unsafe { logiksmith_runtime_project(runtime, &mut projection) },
            Status::Ok.code()
        );
        assert_eq!(projection.document_revision, 1);
        assert_eq!(projection.structural_revision, 1);
        assert_eq!(projection.block_count, 1);
        assert_eq!(projection.has_last_accepted, 0);

        let mut block = BlockProjectionRepr::default();
        let mut written = 0;
        assert_eq!(
            unsafe {
                logiksmith_runtime_project_block(
                    runtime,
                    b"main".as_ptr(),
                    4,
                    &mut block,
                    &mut written,
                )
            },
            Status::Ok.code()
        );
        assert_eq!(written, 1);
        assert_eq!(block.input_count, 1);
        assert_eq!(&block.inputs[0].endpoint[..6], b"switch");

        let mut effects = [EffectRepr::default(); 1];
        assert_eq!(
            unsafe {
                logiksmith_runtime_simulate_input(
                    runtime,
                    &fixture.event,
                    ptr::null(),
                    0,
                    effects.as_mut_ptr(),
                    effects.len(),
                    &mut written,
                )
            },
            Status::Ok.code()
        );
        assert_eq!(written, 1);
        assert_eq!(projection_after(runtime).has_last_accepted, 0);

        let source = b"function handle(event) return {} end";
        let activation = ActivationRepr {
            block_id: b"main".as_ptr(),
            block_id_len: 4,
            source: source.as_ptr(),
            source_len: source.len(),
            has_source: 1,
            enabled: 0,
            has_enabled: 0,
            reserved: 0,
            expected_document_revision: 1,
            expected_structural_revision: 1,
            expected_block_revision: block.logic_revision,
        };
        let mut result = ActivationResultRepr::default();
        assert_eq!(
            unsafe { logiksmith_runtime_activate(runtime, &activation, &mut result) },
            Status::Ok.code()
        );
        assert_eq!(result.document_revision, 2);
        assert_eq!(result.source_changed, 1);
        assert_eq!(
            unsafe { logiksmith_runtime_activate(runtime, &activation, &mut result) },
            Status::RevisionConflict.code()
        );
        assert_eq!(projection_after(runtime).document_revision, 2);
        assert_eq!(
            unsafe { logiksmith_runtime_destroy(runtime) },
            Status::Ok.code()
        );
    }

    #[test]
    fn toml_creation_uses_shared_embedded_document_validation() {
        let source = br#"[[blocks]]
id = "main"
revision = 4
enabled = true
source = "function handle(event) return { outputs = { light = event.value } } end"

[[blocks.inputs]]
name = "trigger"
dpt = "1.001"

[[blocks.outputs]]
name = "light"
dpt = "1.001"

[[blocks.knx_bindings]]
endpoint = "trigger"
group_address = "1/2/3"

[[blocks.knx_bindings]]
endpoint = "light"
group_address = "1/2/4"
"#;
        let document = RuntimeDocumentBytesRepr {
            bytes: source.as_ptr(),
            len: source.len(),
            document_revision: 7,
            structural_revision: 8,
        };
        let mut runtime = ptr::null_mut();
        assert_eq!(
            unsafe { logiksmith_runtime_create_from_toml(&document, &mut runtime) },
            Status::Ok.code()
        );
        let projection = projection_after(runtime);
        assert_eq!(projection.document_revision, 7);
        assert_eq!(projection.structural_revision, 8);
        assert_eq!(projection.block_count, 1);
        assert_eq!(
            unsafe { logiksmith_runtime_destroy(runtime) },
            Status::Ok.code()
        );
    }

    fn projection_after(runtime: *const RuntimeHandle) -> RuntimeProjectionRepr {
        let mut projection = RuntimeProjectionRepr::default();
        assert_eq!(
            unsafe { logiksmith_runtime_project(runtime, &mut projection) },
            Status::Ok.code()
        );
        projection
    }
}
