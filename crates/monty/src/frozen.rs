//! Python implementations of interpreter functions captured with each program.
//!
//! Internal functions use ordinary VM frames and suspension but are hidden from
//! tracebacks and namespace introspection. Private native helpers arrive as
//! explicit arguments, never through user globals or importable modules.

use std::sync::{Mutex, MutexGuard, OnceLock};

use monty_types::CompileOptions;
use serde::{Deserialize, Serialize};

use crate::{
    args::{ArgValues, FromArgs},
    bytecode::{Opcode, VM},
    defer_drop,
    exception_private::RunResult,
    intern::{CompileInterns, FunctionId, Interns},
    name_map::NameMap,
    run::compile_module_source,
    types::lru_cache,
    value::Value,
};

const FROZEN_FILENAME: &str = "<monty-frozen>";
const REDUCE_LOOP_ENTRY: usize = 2;
const FROZEN_SOURCE: &str = r"
def __monty_functools_reduce(function, iterator, accumulator):
    for item in iterator:
        accumulator = function(accumulator, item)
    return accumulator

def __monty_cache_miss(store, cache, key, function, args, kwargs):
    result = function(*args, **kwargs)
    store(cache, key, result)
    return result
";

/// Selects an entry in the program's captured frozen-function mapping.
/// Append variants: their indices select metadata in existing dumps.
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
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

/// Captured entry metadata for an ordinary function in `Interns`.
/// Bodies must be self-contained: global slots would address the user module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FrozenEntry {
    /// Function ID in the same table as user-defined functions.
    pub function_id: FunctionId,
    /// Bytecode offset after any native setup.
    pub entry_ip: usize,
    /// Optional local iterator to copy onto the initial operand stack.
    pub iterator_slot: Option<u16>,
}

impl FrozenEntry {
    /// Captures an entry point with fixed positional arguments and no closure cells.
    fn new(function_id: FunctionId, interns: &Interns, entry_ip: usize, iterator_slot: Option<u16>) -> Self {
        let function = interns.get_function(function_id);
        assert!(function.is_internal);
        assert!(function.signature.exact_positional_count().is_some());
        assert!(function.cell_var_slots.is_empty() && function.free_var_slots.is_empty());
        if let Some(slot) = iterator_slot {
            assert!(usize::from(slot) < function.signature.exact_positional_count().unwrap());
            assert_eq!(function.code.bytecode()[entry_ip], Opcode::ForIter as u8);
        }
        Self {
            function_id,
            entry_ip,
            iterator_slot,
        }
    }
}

/// Native primitives passed directly to internal Python functions.
/// These identities and their signatures are part of the captured bytecode's ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::Display)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum FrozenFunctions {
    /// Insert a successful result into a cache wrapper.
    Store,
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

/// Compiles trusted source before user functions are added to the same tables.
fn compile_frozen_interns() -> Interns {
    let mut interns = Interns::new(FROZEN_SOURCE);
    compile_module_source(
        FROZEN_SOURCE,
        FROZEN_FILENAME,
        &mut NameMap::new(),
        CompileInterns::for_frozen(&mut interns),
        Vec::<String>::new(),
        CompileOptions::default(),
    )
    .expect("frozen Python source should compile");
    let reduce = FunctionId::from_index(0);
    let cache_miss = FunctionId::from_index(1);
    assert_eq!(
        interns.get_function(reduce).code.bytecode().get(..REDUCE_LOOP_ENTRY),
        Some([Opcode::LoadLocal1 as u8, Opcode::GetIter as u8].as_slice()),
        "frozen reduce setup changed without updating its entry point"
    );
    let entries = vec![
        FrozenEntry::new(reduce, &interns, REDUCE_LOOP_ENTRY, Some(1)),
        FrozenEntry::new(cache_miss, &interns, 0, None),
    ];
    interns.set_frozen_entries(entries);
    interns
}
