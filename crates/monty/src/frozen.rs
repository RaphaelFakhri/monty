//! Python implementations of interpreter functions captured with each program.
//!
//! Frozen frames use ordinary VM suspension and cleanup but are hidden from
//! tracebacks. Private native helpers arrive as explicit arguments, never
//! through the user module's globals or importable modules.

use std::sync::{Mutex, MutexGuard, OnceLock};

use monty_types::CompileOptions;
use serde::{Deserialize, Serialize};

use crate::{
    args::{ArgValues, FromArgs},
    bytecode::{Opcode, VM},
    defer_drop,
    exception_private::RunResult,
    function::Function,
    heap::HeapData,
    intern::{CompileInterns, FunctionId, Interns, StaticStrings},
    modules::ModuleFunctions,
    name_map::NameMap,
    run::compile_module_source,
    types::{Module, lru_cache},
    value::Value,
};

const FROZEN_FILENAME: &str = "<monty-frozen>";
const REDUCE_LOOP_ENTRY: usize = 2;
const FROZEN_SOURCE: &str = r"
def __monty_functools_reduce(function, iterator, accumulator):
    for item in iterator:
        accumulator = function(accumulator, item)
    return accumulator

def __monty_cache_miss(helpers, cache, key, function, args, kwargs):
    result = function(*args, **kwargs)
    helpers.store(cache, key, result)
    return result
";

/// Stable identities selecting captured frozen bytecode.
/// Append new variants; reordering or removing them requires a dump-version bump.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FrozenFunction {
    /// Suspendable fold loop behind `functools.reduce`.
    FunctoolsReduce = 0,
    /// Store a cached call's result only after it returns successfully.
    CacheMiss = 1,
}

impl FrozenFunction {
    /// Returns this frozen function's stable index in captured metadata.
    pub(crate) fn index(self) -> usize {
        self as usize
    }
}

/// Entry metadata travels with frozen code, including in idle session dumps.
/// Bodies must be self-contained: global slots would address the user module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FrozenFunctionCode {
    /// Ordinary compiled Python function metadata and bytecode.
    function: Function,
    /// Bytecode offset after any native setup.
    entry_ip: usize,
    /// Optional local iterator to copy onto the initial operand stack.
    iterator_slot: Option<u16>,
}

impl FrozenFunctionCode {
    /// Captures an entry point with fixed positional arguments and no closure cells.
    fn new(function: Function, entry_ip: usize, iterator_slot: Option<u16>) -> Self {
        assert!(function.signature.exact_positional_count().is_some());
        assert!(function.cell_var_slots.is_empty() && function.free_var_slots.is_empty());
        if let Some(slot) = iterator_slot {
            assert!(usize::from(slot) < function.signature.exact_positional_count().unwrap());
            assert_eq!(function.code.bytecode()[entry_ip], Opcode::ForIter as u8);
        }
        Self {
            function,
            entry_ip,
            iterator_slot,
        }
    }

    /// Returns the compiled Python function.
    pub(crate) fn function(&self) -> &Function {
        &self.function
    }

    /// Returns the post-setup bytecode entry point.
    pub(crate) fn entry_ip(&self) -> usize {
        self.entry_ip
    }

    /// Returns the iterator slot used to seed the operand stack, if needed.
    pub(crate) fn iterator_slot(&self) -> Option<u16> {
        self.iterator_slot
    }
}

/// Native primitives available only through an explicitly passed helper namespace.
/// These identities and their signatures are part of the captured bytecode's ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum FrozenFunctions {
    /// Insert a successful result into a cache wrapper.
    Store,
}

/// Creates a private namespace for a cache-miss frame; never installed in globals.
pub(crate) fn cache_helpers(vm: &mut VM<'_>) -> Value {
    let mut helpers = Module::new(StaticStrings::FrozenHelpers, vm.interns);
    helpers.set_attr(
        StaticStrings::Store,
        Value::ModuleFunction(ModuleFunctions::Frozen(FrozenFunctions::Store)),
        vm,
    );
    Value::Ref(vm.heap.allocate(HeapData::Module(Box::new(helpers))))
}

/// Dispatches private helpers using the same argument ownership rules as public natives.
pub(crate) fn call_helper(function: FrozenFunctions, args: ArgValues, vm: &mut VM<'_>) -> RunResult<Value> {
    match function {
        FrozenFunctions::Store => {
            let StoreArgs { cache, key, value } = StoreArgs::from_args(args, vm)?;
            defer_drop!(cache, vm);
            defer_drop!(value, vm);
            lru_cache::store_result(cache, key, value, vm)?;
            Ok(Value::None)
        }
    }
}

/// Arguments to the private cache insertion primitive.
#[derive(FromArgs)]
#[from_args(name = "store")]
struct StoreArgs {
    #[from_args(pos_only)]
    cache: Value,
    #[from_args(pos_only)]
    key: Value,
    #[from_args(pos_only)]
    value: Value,
}

/// Seeds a program or REPL with captured frozen code and its interned literals.
pub(crate) fn interns() -> Interns {
    frozen_interns().clone()
}

/// Returns the process-wide bundle, compiling trusted source once.
fn frozen_interns() -> MutexGuard<'static, Interns> {
    static INTERNS: OnceLock<Mutex<Interns>> = OnceLock::new();
    INTERNS
        .get_or_init(|| Mutex::new(compile_frozen_interns()))
        .lock()
        .expect("frozen intern lock should not be poisoned")
}

/// Captures trusted source separately from user function IDs.
fn compile_frozen_interns() -> Interns {
    let mut interns = Interns::new(FROZEN_SOURCE);
    compile_module_source(
        FROZEN_SOURCE,
        FROZEN_FILENAME,
        &mut NameMap::new(),
        CompileInterns::direct(&mut interns),
        Vec::<String>::new(),
        CompileOptions::default(),
    )
    .expect("frozen Python source should compile");
    let reduce = interns.get_function(FunctionId::from_index(0)).clone();
    let cache_miss = interns.get_function(FunctionId::from_index(1)).clone();
    assert_eq!(
        reduce.code.bytecode().get(..REDUCE_LOOP_ENTRY),
        Some([Opcode::LoadLocal1 as u8, Opcode::GetIter as u8].as_slice()),
        "frozen reduce setup changed without updating its entry point"
    );
    assert!(
        reduce.code.bytecode().contains(&(Opcode::CallLocal2 as u8)),
        "frozen reduce callback should use CallLocal2"
    );
    interns.set_frozen_functions(vec![
        FrozenFunctionCode::new(reduce, REDUCE_LOOP_ENTRY, Some(1)),
        FrozenFunctionCode::new(cache_miss, 0, None),
    ]);
    interns
}
