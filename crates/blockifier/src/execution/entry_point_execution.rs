use std::collections::HashSet;

use cairo_vm::types::builtin_name::BuiltinName;
use cairo_vm::types::layout_name::LayoutName;
use cairo_vm::types::relocatable::{MaybeRelocatable, Relocatable};
use cairo_vm::vm::errors::cairo_run_errors::CairoRunError;
use cairo_vm::vm::errors::memory_errors::MemoryError;
use cairo_vm::vm::errors::vm_errors::VirtualMachineError;
use cairo_vm::vm::runners::builtin_runner::BuiltinRunner;
use cairo_vm::vm::runners::cairo_runner::{CairoArg, CairoRunner, ExecutionResources};
use cairo_vm::vm::security::verify_secure_runner;
use num_traits::{ToPrimitive, Zero};
use starknet_api::execution_resources::GasAmount;
use starknet_types_core::felt::Felt;

use crate::execution::call_info::{CallExecution, CallInfo, ChargedResources, Retdata};
use crate::execution::contract_class::{CompiledClassV1, EntryPointV1, TrackedResource};
use crate::execution::entry_point::{
    CallEntryPoint,
    EntryPointExecutionContext,
    EntryPointExecutionResult,
};
use crate::execution::errors::{EntryPointExecutionError, PostExecutionError, PreExecutionError};
use crate::execution::execution_utils::{
    read_execution_retdata,
    write_felt,
    write_maybe_relocatable,
    Args,
    ReadOnlySegments,
    SEGMENT_ARENA_BUILTIN_SIZE,
};
use crate::execution::syscalls::hint_processor::SyscallHintProcessor;
use crate::state::state_api::State;
use crate::versioned_constants::GasCosts;

#[cfg(test)]
#[path = "entry_point_execution_test.rs"]
mod test;

// TODO(spapini): Try to refactor this file into a StarknetRunner struct.

pub struct VmExecutionContext<'a> {
    pub runner: CairoRunner,
    pub syscall_handler: SyscallHintProcessor<'a>,
    pub initial_syscall_ptr: Relocatable,
    pub entry_point: EntryPointV1,
    // Additional data required for execution is appended after the program bytecode.
    pub program_extra_data_length: usize,
}

pub struct CallResult {
    pub failed: bool,
    pub retdata: Retdata,
    pub gas_consumed: u64,
}

/// Executes a specific call to a contract entry point and returns its output.
pub fn execute_entry_point_call(
    call: CallEntryPoint,
    compiled_class: CompiledClassV1,
    state: &mut dyn State,
    context: &mut EntryPointExecutionContext,
) -> EntryPointExecutionResult<CallInfo> {
    // Fetch the class hash from `call`.
    let class_hash = call.class_hash.ok_or(EntryPointExecutionError::InternalError(
        "Class hash must not be None when executing an entry point.".into(),
    ))?;

    let tracked_resource =
        *context.tracked_resource_stack.last().expect("Unexpected empty tracked resource.");
    let VmExecutionContext {
        mut runner,
        mut syscall_handler,
        initial_syscall_ptr,
        entry_point,
        program_extra_data_length,
    } = initialize_execution_context(call, &compiled_class, state, context)?;

    let args = prepare_call_arguments(
        &syscall_handler.base.call,
        &mut runner,
        initial_syscall_ptr,
        &mut syscall_handler.read_only_segments,
        &entry_point,
    )?;
    let n_total_args = args.len();

    // Execute.
    let bytecode_length = compiled_class.bytecode_length();
    let program_segment_size = bytecode_length + program_extra_data_length;
    run_entry_point(&mut runner, &mut syscall_handler, entry_point, args, program_segment_size)?;

    // Collect the set PC values that were visited during the entry point execution.
    register_visited_pcs(
        &mut runner,
        syscall_handler.base.state,
        class_hash,
        program_segment_size,
        bytecode_length,
    )?;

    Ok(finalize_execution(
        runner,
        syscall_handler,
        n_total_args,
        program_extra_data_length,
        tracked_resource,
    )?)
}

// Collects the set PC values that were visited during the entry point execution.
fn register_visited_pcs(
    runner: &mut CairoRunner,
    state: &mut dyn State,
    class_hash: starknet_api::core::ClassHash,
    program_segment_size: usize,
    bytecode_length: usize,
) -> EntryPointExecutionResult<()> {
    let mut class_visited_pcs = HashSet::new();
    // Relocate the trace, putting the program segment at address 1 and the execution segment right
    // after it.
    // TODO(lior): Avoid unnecessary relocation once the VM has a non-relocated `get_trace()`
    //   function.
    runner.relocate_trace(&[1, 1 + program_segment_size])?;
    for trace_entry in runner.relocated_trace.as_ref().expect("Relocated trace not found") {
        let pc = trace_entry.pc;
        if pc < 1 {
            return Err(EntryPointExecutionError::InternalError(format!(
                "Invalid PC value {pc} in trace."
            )));
        }
        let real_pc = pc - 1;
        // Jumping to a PC that is not inside the bytecode is possible. For example, to obtain
        // the builtin costs. Filter out these values.
        if real_pc < bytecode_length {
            class_visited_pcs.insert(real_pc);
        }
    }
    state.add_visited_pcs(class_hash, &class_visited_pcs);
    Ok(())
}

pub fn initialize_execution_context<'a>(
    call: CallEntryPoint,
    compiled_class: &'a CompiledClassV1,
    state: &'a mut dyn State,
    context: &'a mut EntryPointExecutionContext,
) -> Result<VmExecutionContext<'a>, PreExecutionError> {
    let entry_point = compiled_class.get_entry_point(&call)?;

    // Instantiate Cairo runner.
    let proof_mode = false;
    let trace_enabled = true;
    let mut runner = CairoRunner::new(
        &compiled_class.0.program,
        LayoutName::starknet,
        proof_mode,
        trace_enabled,
    )?;

    runner.initialize_function_runner_cairo_1(&entry_point.builtins)?;
    let mut read_only_segments = ReadOnlySegments::default();
    let program_extra_data_length = prepare_program_extra_data(
        &mut runner,
        compiled_class,
        &mut read_only_segments,
        &context.versioned_constants().os_constants.gas_costs,
    )?;

    // Instantiate syscall handler.
    let initial_syscall_ptr = runner.vm.add_memory_segment();
    let syscall_handler = SyscallHintProcessor::new(
        state,
        context,
        initial_syscall_ptr,
        call,
        &compiled_class.hints,
        read_only_segments,
    );

    Ok(VmExecutionContext {
        runner,
        syscall_handler,
        initial_syscall_ptr,
        entry_point,
        program_extra_data_length,
    })
}

fn prepare_program_extra_data(
    runner: &mut CairoRunner,
    contract_class: &CompiledClassV1,
    read_only_segments: &mut ReadOnlySegments,
    gas_costs: &GasCosts,
) -> Result<usize, PreExecutionError> {
    // Create the builtin cost segment, the builtin order should be the same as the price builtin
    // array in the os in compiled_class.cairo in load_compiled_class_facts.
    let builtin_price_array = [
        gas_costs.base.pedersen_gas_cost,
        gas_costs.base.bitwise_builtin_gas_cost,
        gas_costs.base.ecop_gas_cost,
        gas_costs.base.poseidon_gas_cost,
        gas_costs.base.add_mod_gas_cost,
        gas_costs.base.mul_mod_gas_cost,
    ];

    let data = builtin_price_array
        .iter()
        .map(|&x| MaybeRelocatable::from(Felt::from(x)))
        .collect::<Vec<_>>();
    let builtin_cost_segment_start = read_only_segments.allocate(&mut runner.vm, &data)?;

    // Put a pointer to the builtin cost segment at the end of the program (after the
    // additional `ret` statement).
    let mut ptr = (runner.vm.get_pc() + contract_class.bytecode_length())?;
    // Push a `ret` opcode.
    write_felt(&mut runner.vm, &mut ptr, Felt::from(0x208b7fff7fff7ffe_u128))?;
    // Push a pointer to the builtin cost segment.
    write_maybe_relocatable(&mut runner.vm, &mut ptr, builtin_cost_segment_start)?;

    let program_extra_data_length = 2;
    Ok(program_extra_data_length)
}

pub fn prepare_call_arguments(
    call: &CallEntryPoint,
    runner: &mut CairoRunner,
    initial_syscall_ptr: Relocatable,
    read_only_segments: &mut ReadOnlySegments,
    entrypoint: &EntryPointV1,
) -> Result<Args, PreExecutionError> {
    let mut args: Args = vec![];

    // Push builtins.
    for builtin_name in &entrypoint.builtins {
        if let Some(builtin) =
            runner.vm.get_builtin_runners().iter().find(|builtin| builtin.name() == *builtin_name)
        {
            args.extend(builtin.initial_stack().into_iter().map(CairoArg::Single));
            continue;
        }
        if builtin_name == &BuiltinName::segment_arena {
            let segment_arena = runner.vm.add_memory_segment();

            // Write into segment_arena.
            let mut ptr = segment_arena;
            let info_segment = runner.vm.add_memory_segment();
            let n_constructed = Felt::default();
            let n_destructed = Felt::default();
            write_maybe_relocatable(&mut runner.vm, &mut ptr, info_segment)?;
            write_felt(&mut runner.vm, &mut ptr, n_constructed)?;
            write_felt(&mut runner.vm, &mut ptr, n_destructed)?;

            args.push(CairoArg::Single(MaybeRelocatable::from(ptr)));
            continue;
        }
        return Err(PreExecutionError::InvalidBuiltin(*builtin_name));
    }
    // Push gas counter.
    args.push(CairoArg::Single(MaybeRelocatable::from(Felt::from(call.initial_gas))));
    // Push syscall ptr.
    args.push(CairoArg::Single(MaybeRelocatable::from(initial_syscall_ptr)));

    // Prepare calldata arguments.
    let calldata = &call.calldata.0;
    let calldata: Vec<MaybeRelocatable> =
        calldata.iter().map(|&arg| MaybeRelocatable::from(arg)).collect();

    let calldata_start_ptr = read_only_segments.allocate(&mut runner.vm, &calldata)?;
    let calldata_end_ptr = MaybeRelocatable::from((calldata_start_ptr + calldata.len())?);
    args.push(CairoArg::Single(MaybeRelocatable::from(calldata_start_ptr)));
    args.push(CairoArg::Single(calldata_end_ptr));

    Ok(args)
}
/// Runs the runner from the given PC.
pub fn run_entry_point(
    runner: &mut CairoRunner,
    hint_processor: &mut SyscallHintProcessor<'_>,
    entry_point: EntryPointV1,
    args: Args,
    program_segment_size: usize,
) -> EntryPointExecutionResult<()> {
    // Note that we run `verify_secure_runner` manually after filling the holes in the rc96 segment.
    let verify_secure = false;
    let args: Vec<&CairoArg> = args.iter().collect();
    runner.run_from_entrypoint(
        entry_point.pc(),
        &args,
        verify_secure,
        Some(program_segment_size),
        hint_processor,
    )?;

    maybe_fill_holes(entry_point, runner)?;

    verify_secure_runner(runner, false, Some(program_segment_size))
        .map_err(CairoRunError::VirtualMachine)?;

    Ok(())
}

/// Fills the holes after running the entry point.
/// Currently only fills the holes in the rc96 segment.
fn maybe_fill_holes(
    entry_point: EntryPointV1,
    runner: &mut CairoRunner,
) -> Result<(), EntryPointExecutionError> {
    let Some(rc96_offset) =
        entry_point.builtins.iter().rev().position(|name| *name == BuiltinName::range_check96)
    else {
        return Ok(());
    };
    let rc96_builtin_runner = runner
        .vm
        .get_builtin_runners()
        .iter()
        .find_map(|builtin| {
            if let BuiltinRunner::RangeCheck96(rc96_builtin_runner) = builtin {
                Some(rc96_builtin_runner)
            } else {
                None
            }
        })
        .expect("RangeCheck96 builtin runner not found.");

    // 'EntryPointReturnValues' is returned after the implicits and its size is 5,
    // So the last implicit is at offset 5 + 1.
    const IMPLICITS_OFFSET: usize = 6;
    let rc_96_stop_ptr = (runner.vm.get_ap() - (IMPLICITS_OFFSET + rc96_offset))
        .map_err(|err| CairoRunError::VirtualMachine(VirtualMachineError::Math(err)))?;

    let rc96_base = rc96_builtin_runner.base();
    let rc96_segment: isize =
        rc96_base.try_into().expect("Builtin segment index must fit in isize.");

    let Relocatable { segment_index: rc96_stop_segment, offset: stop_offset } =
        runner.vm.get_relocatable(rc_96_stop_ptr).map_err(CairoRunError::MemoryError)?;
    assert_eq!(rc96_stop_segment, rc96_segment);

    // Update `segment_used_sizes` to include the holes.
    runner
        .vm
        .segments
        .segment_used_sizes
        .as_mut()
        .expect("Segments used sizes should be calculated at this point")[rc96_base] = stop_offset;

    for offset in 0..stop_offset {
        match runner
            .vm
            .insert_value(Relocatable { segment_index: rc96_segment, offset }, Felt::zero())
        {
            // If the value is already set, ignore the error.
            Ok(()) | Err(MemoryError::InconsistentMemory(_)) => {}
            Err(err) => panic!("Unexpected error when filling holes: {err}."),
        }
    }

    Ok(())
}

/// Calculates the gas consumed in the current call.
pub fn gas_consumed_without_inner_calls(
    tracked_resource: &TrackedResource,
    gas_consumed: u64,
    inner_calls: &[CallInfo],
) -> GasAmount {
    GasAmount(match tracked_resource {
        TrackedResource::CairoSteps => 0,
        TrackedResource::SierraGas => gas_consumed
            .checked_sub(inner_calls.iter().map(|call| call.execution.gas_consumed).sum::<u64>())
            .expect("gas_consumed unexpectedly underflowed."),
    })
}

pub fn finalize_execution(
    mut runner: CairoRunner,
    mut syscall_handler: SyscallHintProcessor<'_>,
    n_total_args: usize,
    program_extra_data_length: usize,
    tracked_resource: TrackedResource,
) -> Result<CallInfo, PostExecutionError> {
    // Close memory holes in segments (OS code touches those memory cells, we simulate it).
    let program_start_ptr = runner
        .program_base
        .expect("The `program_base` field should be initialized after running the entry point.");
    let program_end_ptr = (program_start_ptr + runner.get_program().data_len())?;
    runner.vm.mark_address_range_as_accessed(program_end_ptr, program_extra_data_length)?;

    let initial_fp = runner
        .get_initial_fp()
        .expect("The `initial_fp` field should be initialized after running the entry point.");
    // When execution starts the stack holds the EP arguments + [ret_fp, ret_pc].
    let args_ptr = (initial_fp - (n_total_args + 2))?;
    runner.vm.mark_address_range_as_accessed(args_ptr, n_total_args)?;
    syscall_handler.read_only_segments.mark_as_accessed(&mut runner)?;

    let call_result = get_call_result(&runner, &syscall_handler, &tracked_resource)?;

    let vm_resources_without_inner_calls = match tracked_resource {
        TrackedResource::CairoSteps => {
            // Take into account the resources of the current call, without inner calls.
            // Has to happen after marking holes in segments as accessed.
            let mut vm_resources_without_inner_calls = runner
                .get_execution_resources()
                .map_err(VirtualMachineError::RunnerError)?
                .filter_unused_builtins();
            let versioned_constants = syscall_handler.base.context.versioned_constants();
            if versioned_constants.segment_arena_cells {
                vm_resources_without_inner_calls
                    .builtin_instance_counter
                    .get_mut(&BuiltinName::segment_arena)
                    .map_or_else(|| {}, |val| *val *= SEGMENT_ARENA_BUILTIN_SIZE);
            }
            // Take into account the syscall resources of the current call.
            vm_resources_without_inner_calls += &versioned_constants
                .get_additional_os_syscall_resources(&syscall_handler.syscall_counter);
            vm_resources_without_inner_calls
        }
        TrackedResource::SierraGas => ExecutionResources::default(),
    };

    syscall_handler.finalize();

    let charged_resources_without_inner_calls = ChargedResources {
        vm_resources: vm_resources_without_inner_calls,
        gas_for_fee: gas_consumed_without_inner_calls(
            &tracked_resource,
            call_result.gas_consumed,
            &syscall_handler.base.inner_calls,
        ),
    };

    let charged_resources = &charged_resources_without_inner_calls
        + &CallInfo::summarize_charged_resources(syscall_handler.base.inner_calls.iter());

    let syscall_handler_base = syscall_handler.base;
    Ok(CallInfo {
        call: syscall_handler_base.call,
        execution: CallExecution {
            retdata: call_result.retdata,
            events: syscall_handler_base.events,
            l2_to_l1_messages: syscall_handler_base.l2_to_l1_messages,
            failed: call_result.failed,
            gas_consumed: call_result.gas_consumed,
        },
        inner_calls: syscall_handler_base.inner_calls,
        tracked_resource,
        charged_resources,
        storage_read_values: syscall_handler_base.read_values,
        accessed_storage_keys: syscall_handler_base.accessed_keys,
        read_class_hash_values: syscall_handler_base.read_class_hash_values,
        accessed_contract_addresses: syscall_handler_base.accessed_contract_addresses,
    })
}

fn get_call_result(
    runner: &CairoRunner,
    syscall_handler: &SyscallHintProcessor<'_>,
    tracked_resource: &TrackedResource,
) -> Result<CallResult, PostExecutionError> {
    let return_result = runner.vm.get_return_values(5)?;
    // Corresponds to the Cairo 1.0 enum:
    // enum PanicResult<Array::<felt>> { Ok: Array::<felt>, Err: Array::<felt>, }.
    let [failure_flag, retdata_start, retdata_end]: &[MaybeRelocatable; 3] =
        (&return_result[2..]).try_into().expect("Return values must be of size 3.");

    let failed = if *failure_flag == MaybeRelocatable::from(0) {
        false
    } else if *failure_flag == MaybeRelocatable::from(1) {
        true
    } else {
        return Err(PostExecutionError::MalformedReturnData {
            error_message: "Failure flag expected to be either 0 or 1.".to_string(),
        });
    };

    let retdata_size = retdata_end.sub(retdata_start)?;
    // TODO(spapini): Validate implicits.

    let gas = &return_result[0];
    let MaybeRelocatable::Int(gas) = gas else {
        return Err(PostExecutionError::MalformedReturnData {
            error_message: "Error extracting return data.".to_string(),
        });
    };
    let gas = gas.to_u64().ok_or(PostExecutionError::MalformedReturnData {
        error_message: format!("Unexpected remaining gas: {gas}."),
    })?;

    if gas > syscall_handler.base.call.initial_gas {
        return Err(PostExecutionError::MalformedReturnData {
            error_message: format!("Unexpected remaining gas: {gas}."),
        });
    }

    let gas_consumed = match tracked_resource {
        // Do not count Sierra gas in CairoSteps mode.
        TrackedResource::CairoSteps => 0,
        TrackedResource::SierraGas => syscall_handler.base.call.initial_gas - gas,
    };
    Ok(CallResult {
        failed,
        retdata: read_execution_retdata(runner, retdata_size, retdata_start)?,
        gas_consumed,
    })
}
