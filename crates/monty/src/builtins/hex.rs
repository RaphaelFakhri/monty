//! Implementation of the hex() builtin function.

use num_traits::Signed;

use crate::{
    args::ArgValues,
    bytecode::VM,
    defer_drop,
    exception_private::{ExcType, ExcTypeExt, RunResult},
    types::str::allocate_string_no_interning,
    value::Value,
};

/// Implementation of the hex() builtin function.
///
/// Converts an integer to a lowercase hexadecimal string prefixed with '0x'.
/// Supports both i64 and BigInt integers.
pub fn builtin_hex(vm: &mut VM<'_>, args: ArgValues) -> RunResult<Value> {
    let value = args.get_one_arg("hex", vm.heap)?;
    defer_drop!(value, vm);

    let s = match value {
        Value::Int(n) => {
            format!("{}0x{:x}", if *n < 0 { "-" } else { "" }, n.unsigned_abs())
        }
        Value::Bool(b) => {
            let s = if *b { "0x1" } else { "0x0" };
            s.to_string()
        }
        _ if let Some(bi) = value.as_long_int(vm) => {
            format!("{}0x{:x}", if bi.is_negative() { "-" } else { "" }, bi.magnitude())
        }
        _ => return Err(ExcType::type_error_not_integer(&value.py_type_name(vm))),
    };
    Ok(allocate_string_no_interning(s, vm.heap))
}
