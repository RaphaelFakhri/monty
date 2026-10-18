//! The session's modules imported from `.py` files: CPython's `sys.modules`,
//! restricted to file modules.

use std::collections::BTreeMap;

use crate::{
    heap::{ContainsHeap, DropWithContext, HeapId},
    value::Value,
};

/// Modules imported from files this session, by fully-qualified dotted name.
///
/// Each file runs once: a later import of the name binds the cached module. A
/// module is cached before its body runs, so a circular import binds it partly
/// initialised, and dropped again if the body raises, so the next import retries.
/// Bundled and host modules are never cached. Every `HeapId` is OWNED.
// TODO: back a live `sys.modules` dict with this, and add `importlib.reload`.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ModuleCache(BTreeMap<String, HeapId>);

impl ModuleCache {
    /// An owned reference to the module cached as `name`.
    pub fn get(&self, name: &str, heap: &impl ContainsHeap) -> Option<Value> {
        self.0.get(name).map(|&id| {
            heap.heap().inc_ref(id);
            Value::Ref(id)
        })
    }

    /// The module cached as `name`, borrowed.
    pub fn get_id(&self, name: &str) -> Option<HeapId> {
        self.0.get(name).copied()
    }

    /// Caches the borrowed `module` as `name`, taking a reference to it.
    pub fn insert(&mut self, name: String, module: HeapId, heap: &mut impl ContainsHeap) {
        heap.heap().inc_ref(module);
        if let Some(old) = self.0.insert(name, module) {
            heap.heap_mut().dec_ref(old);
        }
    }

    /// Forgets `name` if it still caches `module`, whose body raised.
    pub fn remove_failed(&mut self, name: &str, module: HeapId, heap: &mut impl ContainsHeap) {
        if self.0.get(name) == Some(&module) {
            self.0.remove(name);
            heap.heap_mut().dec_ref(module);
        }
    }
}

impl<C: ContainsHeap> DropWithContext<C> for ModuleCache {
    fn drop_with(self, ctx: &mut C) {
        for id in self.0.into_values() {
            ctx.heap_mut().dec_ref(id);
        }
    }
}
