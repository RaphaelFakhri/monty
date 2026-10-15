//! `functools.lru_cache` / `functools.cache` — a callable that memoizes another
//! callable's results.
//!
//! Hits return directly from native lookup. Misses enter captured Python
//! bytecode which calls the wrapped callable and then a private store helper.
//! Pending keys and arguments are ordinary frame locals, including across
//! host suspension and snapshots; the VM needs no cache-specific return effects.

use std::{fmt::Write, mem::take};

use serde::{Deserialize, Serialize};

use crate::{
    args::{ArgValues, FromArgs, KwargsValues},
    builtins::type_of_ref,
    bytecode::{CallResult, VM},
    defer_drop, defer_drop_mut,
    exception_private::{ExcType, ExcTypeExt, RunError, RunResult},
    frozen::{self, FrozenFunction},
    hash::{HashValue, identity_hash},
    heap::{
        BorrowedHeapReadMut, DropGuard, DropWithContext, HeapData, HeapId, HeapItem, HeapObjectRead, HeapReadOutput,
        heap_read_ref_as_field_mut,
    },
    intern::StaticStrings,
    types::{
        Dict, LazyHeapSet, NamedTuple, PyTrait, Type,
        tuple::{TupleVec, allocate_tuple},
    },
    value::{EitherStr, Value},
};

/// A memoizing wrapper around another callable.
///
/// Owns its callable, keys, and results. Both heap cleanup and cycle tracing
/// must enumerate them: cached arguments and results can refer back to the wrapper.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct LruCache {
    /// Owned callable, exposed as `__wrapped__`; `None` for a configured
    /// decorator awaiting the function to wrap.
    func: Option<Value>,
    /// Entry ceiling; `None` is unbounded (`cache`), `Some(0)` caches nothing.
    maxsize: Option<u64>,
    /// Whether arguments of different types are cached separately.
    typed: bool,
    /// Calls answered from the cache, as reported by `cache_info()`.
    hits: u64,
    /// Calls that reached the wrapped callable, including ones it then raised
    /// from and ones `maxsize=0` never stored.
    misses: u64,
    /// Call key (see [`make_key`]) to result.
    cache: Dict,
    /// Recency stamps in dict entry order; empty for unbounded caches.
    /// Insertions and removals must maintain the parallel positions.
    stamps: Vec<u64>,
    /// Source of stamps; monotonic, so the smallest stamp is the oldest use.
    clock: u64,
}

impl LruCache {
    /// Wraps `func` (taking ownership), or, with `None`, stands in as the
    /// decorator that will wrap the next callable it is given.
    fn new(func: Option<Value>, maxsize: Option<u64>, typed: bool) -> Self {
        Self {
            func,
            maxsize,
            typed,
            hits: 0,
            misses: 0,
            cache: Dict::new(),
            stamps: Vec::new(),
            clock: 0,
        }
    }

    /// Invokes `on_child` for each heap id this wrapper owns (GC trace hook).
    ///
    /// MUST report exactly the ids [`HeapItem::py_dec_ref_ids`] releases.
    pub(crate) fn for_each_child_id(&self, mut on_child: impl FnMut(HeapId)) {
        if let Some(Value::Ref(id)) = self.func {
            on_child(id);
        }
        if !self.cache.has_refs() {
            return;
        }
        for (key, value) in &self.cache {
            if let Value::Ref(id) = key {
                on_child(*id);
            }
            if let Value::Ref(id) = value {
                on_child(*id);
            }
        }
    }

    /// Records a use of the entry at `index`; a no-op while unbounded.
    ///
    /// Ignores an index with no stamp: a user `__eq__` running during the key
    /// lookup can clear or shrink the cache before the index is used.
    fn touch(&mut self, index: usize) {
        if self.maxsize.is_some() {
            self.clock += 1;
            if let Some(stamp) = self.stamps.get_mut(index) {
                *stamp = self.clock;
            }
        }
    }

    /// Finds the least recently used entry with an O(n) scan when eviction is needed.
    fn lru_index(&self) -> Option<usize> {
        self.stamps
            .iter()
            .enumerate()
            .min_by_key(|(_, stamp)| **stamp)
            .map(|(index, _)| index)
    }
}

/// Answers a hit natively, or transfers a miss to its frozen continuation.
pub(crate) fn call(cache_id: HeapId, args: ArgValues, vm: &mut VM<'_>) -> RunResult<CallResult> {
    let HeapReadOutput::LruCache(mut cache) = vm.heap.read(cache_id) else {
        unreachable!("cache dispatch requires an LruCache")
    };
    let (func, maxsize, typed) = {
        let this = cache.get(vm.heap);
        (
            this.func.as_ref().map(|func| func.clone_with_heap(vm)),
            this.maxsize,
            this.typed,
        )
    };
    // Still a decorator: this call supplies the function to wrap.
    let Some(func) = func else {
        return decorate(maxsize, typed, args, vm).map(CallResult::Value);
    };
    defer_drop!(func, vm);

    let CallArgs { positional, keywords } = CallArgs::from_args(args, vm)?;
    defer_drop_mut!(positional, vm);
    let keywords = keywords.into_iter().collect::<Vec<_>>();
    defer_drop_mut!(keywords, vm);

    // Disabled caches must accept unhashable arguments and still use a frame,
    // rather than recursively dispatching nested wrappers on the Rust stack.
    let key = if maxsize == Some(0) {
        Value::None
    } else {
        make_key(positional, keywords, typed, vm)?
    };
    let mut key_guard = DropGuard::new(key, vm);
    let (key, vm) = key_guard.as_parts_mut();

    // Comparing keys can run a user `__eq__`, which is free to call
    // `cache_clear()` on this very wrapper, so the index is read back rather
    // than trusted; a vanished entry counts as a miss.
    let index = if maxsize == Some(0) {
        None
    } else {
        cache_mut(&mut cache).find_entry_index(key, vm)?
    };
    let hit = match index {
        Some(index) => cache
            .get(vm.heap)
            .cache
            .value_at(index)
            .map(|value| (index, value.clone_with_heap(vm))),
        None => None,
    };
    if let Some((index, value)) = hit {
        let this = cache.get_mut(vm.heap);
        this.hits += 1;
        this.touch(index);
        Ok(CallResult::Value(value))
    } else {
        cache.get_mut(vm.heap).misses += 1;
        let kwargs = Dict::from_pairs(take(keywords), vm)?;
        let kwargs = Value::Ref(vm.heap.allocate(HeapData::Dict(kwargs)));
        let args = allocate_tuple(TupleVec::from_vec(take(positional)), vm.heap);
        let helpers = frozen::cache_helpers(vm);
        let function = func.clone_with_heap(vm);
        vm.heap.inc_ref(cache_id);
        let (key, vm) = key_guard.into_parts();
        vm.call_frozen_exact(
            FrozenFunction::CacheMiss,
            [helpers, Value::Ref(cache_id), key, function, args, kwargs],
        )
    }
}

/// Captures arbitrary call arguments without interpreting the wrapped signature.
#[derive(FromArgs)]
#[from_args(name = "_lru_cache_wrapper")]
struct CallArgs {
    #[from_args(varargs)]
    positional: Vec<Value>,
    #[from_args(varkwargs)]
    keywords: KwargsValues,
}

/// Argument shape for the decorator `lru_cache(maxsize=…, typed=…)` returns.
///
/// CPython's decorator is a closure over the two settings taking just the
/// function, so this shape is what its arity errors describe; Monty keeps the
/// settings on a function-less wrapper instead.
#[derive(FromArgs)]
#[from_args(name = "lru_cache.<locals>.decorating_function", style = def)]
struct DecoratingFunctionArgs {
    user_function: Value,
}

/// Applies the settings this decorator captured to the function it is given.
fn decorate(maxsize: Option<u64>, typed: bool, args: ArgValues, vm: &mut VM<'_>) -> RunResult<Value> {
    let DecoratingFunctionArgs { user_function } = DecoratingFunctionArgs::from_args(args, vm)?;
    allocate(Some(user_function), maxsize, typed, vm)
}

/// Allocates a wrapper, taking ownership of `func` and rejecting one that
/// cannot be called. `None` allocates the decorator stage.
pub(crate) fn allocate(func: Option<Value>, maxsize: Option<u64>, typed: bool, vm: &mut VM<'_>) -> RunResult<Value> {
    if func.as_ref().is_none_or(|func| func.is_callable(vm.heap)) {
        let cache = LruCache::new(func, maxsize, typed);
        Ok(Value::Ref(vm.heap.allocate(HeapData::LruCache(Box::new(cache)))))
    } else {
        func.drop_with(vm);
        Err(ExcType::partial_not_callable())
    }
}

/// The TypeError `lru_cache()` raises for a first argument that is neither a
/// size, a callable, nor `None`.
#[cold]
pub(crate) fn bad_maxsize_error() -> RunError {
    ExcType::type_error("Expected first argument to be an integer, a callable, or None")
}

/// The cached results as a dict handle, for the `HeapRead<Dict>` methods.
fn cache_mut<'r, 'h>(cache: &'r mut HeapObjectRead<'h, LruCache>) -> BorrowedHeapReadMut<'r, 'h, Dict> {
    heap_read_ref_as_field_mut!(cache, LruCache, cache)
}

/// Stores a successful frozen call's result. Consumes the key; borrows the wrapper and value.
/// Disabled caches deliberately skip hashing and insertion.
pub(crate) fn store_result(cache: &Value, key: Value, value: &Value, vm: &mut VM<'_>) -> RunResult<()> {
    let mut key_guard = DropGuard::new(key, vm);
    let (key, vm) = key_guard.as_parts_mut();
    let cache_id = match cache {
        Value::Ref(id) if matches!(vm.heap.get(*id), HeapData::LruCache(_)) => *id,
        _ => return Err(ExcType::type_error("store() requires a cache wrapper")),
    };
    let HeapReadOutput::LruCache(mut cache) = vm.heap.read(cache_id) else {
        unreachable!("checked cache type above")
    };
    if cache.get(vm.heap).maxsize == Some(0) {
        return Ok(());
    }

    // A bounded cache preserves an entry inserted by a recursive call.
    // Unbounded caches instead overwrite it with the outer result.
    let existing = match cache_mut(&mut cache).find_entry_index(key, vm)? {
        Some(index) if index < cache.get(vm.heap).cache.len() => Some(index),
        _ => None,
    };
    if let Some(index) = existing {
        if cache.get(vm.heap).maxsize.is_none() {
            let value = value.clone_with_heap(vm);
            let old = cache_mut(&mut cache).replace_value_at(index, value, vm);
            old.drop_with(vm);
        }
        Ok(())
    } else {
        let (key, vm) = key_guard.into_parts();
        insert_new(&mut cache, key, value, vm)
    }
}

/// Inserts an owned key and a cloned result, making room in bounded caches.
fn insert_new<'h>(
    cache: &mut HeapObjectRead<'h, LruCache>,
    key: Value,
    value: &Value,
    vm: &mut VM<'h>,
) -> RunResult<()> {
    let maxsize = cache.get(vm.heap).maxsize;
    if let Some(maxsize) = maxsize
        && cache.get(vm.heap).cache.len() as u64 >= maxsize
    {
        evict_one(cache, vm);
    }

    let value = value.clone_with_heap(vm);
    // A re-entrant `__eq__` between the probe above and this insertion can have
    // stored the key already, in which case `set` hands back the old value and
    // its refcount with it.
    if let Some(old) = cache_mut(cache).set(key, value, vm)? {
        old.drop_with(vm);
    }

    let this = cache.get_mut(vm.heap);
    if let Some(maxsize) = this.maxsize {
        this.clock += 1;
        let clock = this.clock;
        this.stamps.resize(this.cache.len(), clock);
        // An equality callback during insertion may have filled the slot we
        // reserved. Enforce the bound again without invoking any user code.
        while cache.get(vm.heap).cache.len() as u64 > maxsize {
            evict_one(cache, vm);
        }
    }
    Ok(())
}

/// Removes the least recently used entry by position without invoking user code.
/// A lookup by key could run `__eq__` and mutate both the cache and its stamps.
fn evict_one<'h>(cache: &mut HeapObjectRead<'h, LruCache>, vm: &mut VM<'h>) {
    let index = {
        let this = cache.get_mut(vm.heap);
        // The lookup this insertion began with may have run a user `__eq__`
        // that shrank the cache, leaving stamps the entries no longer have.
        // The scan below reads them as positions, so drop those first.
        this.stamps.truncate(this.cache.len());
        this.lru_index()
    };
    let Some(index) = index else { return };
    // `pop_at` shifts everything after the entry down, so `stamps` shifts too.
    let (key, value) = cache_mut(cache).pop_at(index, vm);
    key.drop_with(vm);
    value.drop_with(vm);
    cache.get_mut(vm.heap).stamps.remove(index);
}

/// Builds and hashes a key, preserving keyword order and optional argument types.
/// A leading positional count separates positionals from keyword pairs without
/// CPython's private sentinel. Single untyped int/str arguments bypass the tuple.
fn make_key(positional: &[Value], keywords: &[(Value, Value)], typed: bool, vm: &mut VM<'_>) -> RunResult<Value> {
    // This shortcut distinguishes f(1) from f(1.0), despite their equal arguments.
    if !typed
        && keywords.is_empty()
        && let [only] = positional
        && matches!(only.py_type(vm), Type::Int | Type::Str)
    {
        return Ok(only.clone_with_heap(vm));
    }

    let mut items = TupleVec::new();
    items.push(Value::Int(i64::try_from(positional.len()).unwrap_or(i64::MAX)));
    for arg in positional {
        items.push(arg.clone_with_heap(vm));
    }
    for (name, value) in keywords {
        items.push(name.clone_with_heap(vm));
        items.push(value.clone_with_heap(vm));
    }
    if typed {
        for value in positional.iter().chain(keywords.iter().map(|(_, value)| value)) {
            items.push(type_of_ref(vm, value));
        }
    }

    // CPython hashes the key eagerly (that is what `_HashedSeq` is for), so an
    // unhashable argument is reported as itself; leaving it to the lookup would
    // blame the key tuple built around it instead.
    let key = allocate_tuple(items, vm.heap);
    let mut key_guard = DropGuard::new(key, vm);
    let (key, vm) = key_guard.as_parts_mut();
    if key.py_hash(vm)?.is_none() {
        return Err(unhashable_argument_error(positional, keywords, vm));
    }
    Ok(key_guard.into_inner())
}

/// Names the argument that made the key unhashable.
///
/// Falls back to the whole key's type if every argument hashes on its own,
/// which nothing currently produces.
#[cold]
fn unhashable_argument_error(positional: &[Value], keywords: &[(Value, Value)], vm: &mut VM<'_>) -> RunError {
    for value in positional.iter().chain(keywords.iter().map(|(_, value)| value)) {
        if matches!(value.py_hash(vm), Ok(None)) {
            return ExcType::type_error_unhashable(&value.py_type_name(vm));
        }
    }
    ExcType::type_error_unhashable("tuple")
}

impl HeapItem for LruCache {
    fn py_dec_ref_ids(&mut self, stack: &mut Vec<HeapId>) {
        if let Some(func) = &mut self.func {
            func.py_dec_ref_ids(stack);
        }
        self.cache.py_dec_ref_ids(stack);
    }
}

impl<'h> PyTrait<'h> for HeapObjectRead<'h, LruCache> {
    fn py_type(&self, _: &VM<'h>) -> Type {
        Type::LruCacheWrapper
    }

    fn py_len(&self, _: &VM<'h>) -> Option<usize> {
        None
    }

    fn py_eq_impl(&self, _: &Value, _: &mut VM<'h>) -> RunResult<Option<bool>> {
        Ok(None)
    }

    /// Cached functions hash by identity, as CPython's do — the wrapper defines
    /// neither `__eq__` nor `__hash__`.
    fn py_hash(&self, _: &mut VM<'h>) -> RunResult<Option<HashValue>> {
        Ok(Some(identity_hash(self.id())))
    }

    /// Fallback repr; `Value::py_repr_fmt` adds the identity through [`lru_cache_repr`].
    fn py_repr_fmt(&self, f: &mut impl Write, _: &mut VM<'h>, _: &mut LazyHeapSet) -> RunResult<()> {
        Ok(f.write_str("<functools._lru_cache_wrapper object>")?)
    }

    /// `__wrapped__` — the callable being cached.
    fn py_getattr(&self, attr: &EitherStr, vm: &mut VM<'h>) -> RunResult<Option<CallResult>> {
        if attr.static_string(vm.interns) == Some(StaticStrings::DunderWrapped)
            && let Some(func) = self.get(vm.heap).func.as_ref()
        {
            let func = func.clone_with_heap(vm);
            Ok(Some(CallResult::Value(func)))
        } else {
            Ok(None)
        }
    }

    fn py_call_attr(&mut self, vm: &mut VM<'h>, attr: &EitherStr, args: ArgValues) -> RunResult<CallResult> {
        let value = match attr.static_string(vm.interns) {
            Some(StaticStrings::CacheInfo) => {
                args.check_zero_args("_lru_cache_wrapper.cache_info", vm.heap)?;
                self.cache_info(vm)
            }
            Some(StaticStrings::CacheClear) => {
                args.check_zero_args("_lru_cache_wrapper.cache_clear", vm.heap)?;
                self.cache_clear(vm);
                Value::None
            }
            Some(StaticStrings::CacheParameters) => {
                args.check_zero_args("_lru_cache_wrapper.cache_parameters", vm.heap)?;
                self.cache_parameters(vm)?
            }
            // `f.__wrapped__(...)` is an ordinary call of the attribute's
            // value, not a method of the wrapper, so hand it straight on —
            // uncached, which is the point of reaching for `__wrapped__`.
            Some(StaticStrings::DunderWrapped) if let Some(func) = self.get(vm.heap).func.as_ref() => {
                let func = func.clone_with_heap(vm);
                defer_drop!(func, vm);
                return vm.call_function(func, args);
            }
            _ => {
                args.drop_with(vm);
                return Err(ExcType::attribute_error(Type::LruCacheWrapper, attr.as_str(vm.interns)));
            }
        };
        Ok(CallResult::Value(value))
    }
}

impl<'h> HeapObjectRead<'h, LruCache> {
    /// `CacheInfo(hits=..., misses=..., maxsize=..., currsize=...)`.
    fn cache_info(&self, vm: &mut VM<'h>) -> Value {
        let this = self.get(vm.heap);
        let items = vec![
            count_value(this.hits),
            count_value(this.misses),
            maxsize_value(this.maxsize),
            count_value(this.cache.len() as u64),
        ];
        let info = NamedTuple::new(
            EitherStr::Interned(vm.interns.intern_static(StaticStrings::CacheInfoName)),
            vec![
                EitherStr::Interned(vm.interns.intern_static(StaticStrings::Hits)),
                EitherStr::Interned(vm.interns.intern_static(StaticStrings::Misses)),
                EitherStr::Interned(vm.interns.intern_static(StaticStrings::Maxsize)),
                EitherStr::Interned(vm.interns.intern_static(StaticStrings::Currsize)),
            ],
            items,
        );
        Value::Ref(vm.heap.allocate(HeapData::NamedTuple(Box::new(info))))
    }

    /// Empties the cache and resets the hit/miss counters, as CPython does.
    fn cache_clear(&mut self, vm: &mut VM<'h>) {
        let entries = take(&mut self.get_mut(vm.heap).cache);
        entries.drop_with(vm);
        let this = self.get_mut(vm.heap);
        this.stamps.clear();
        this.hits = 0;
        this.misses = 0;
    }

    /// `{'maxsize': ..., 'typed': ...}`.
    fn cache_parameters(&self, vm: &mut VM<'h>) -> RunResult<Value> {
        let this = self.get(vm.heap);
        let pairs = vec![
            (
                Value::InternString(vm.interns.intern_static(StaticStrings::Maxsize)),
                maxsize_value(this.maxsize),
            ),
            (
                Value::InternString(vm.interns.intern_static(StaticStrings::Typed)),
                Value::Bool(this.typed),
            ),
        ];
        let dict = Dict::from_pairs(pairs, vm)?;
        Ok(Value::Ref(vm.heap.allocate(HeapData::Dict(dict))))
    }
}

/// Writes the wrapper's repr using the identity available at the `Value` level.
pub(crate) fn lru_cache_repr(self_id: HeapId, f: &mut impl Write) -> RunResult<()> {
    Ok(write!(
        f,
        "<functools._lru_cache_wrapper object at 0x{:x}>",
        self_id.index()
    )?)
}

/// A counter as a Python `int`, saturating rather than wrapping negative.
fn count_value(count: u64) -> Value {
    Value::Int(i64::try_from(count).unwrap_or(i64::MAX))
}

/// `maxsize` as Python sees it: an `int`, or `None` when unbounded.
fn maxsize_value(maxsize: Option<u64>) -> Value {
    maxsize.map_or(Value::None, count_value)
}
