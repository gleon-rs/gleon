//! `skip_serializing_if` predicates shared by the serialized types.

/// Whether `value` is its type's default (`0`, `false`, …): left out of reports while it says
/// nothing.
pub fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}
