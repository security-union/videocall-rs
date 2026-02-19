/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Framework-agnostic callback type.
//!
//! Originally based on the `Callback` type from the Yew framework (MIT licensed),
//! extracted here so that `videocall-client` and its consumers do not depend on any
//! specific UI framework.

use std::fmt;
use std::rc::Rc;

/// Universal callback wrapper.
///
/// An `Rc` wrapper is used to make it cloneable.
pub struct Callback<IN, OUT = ()> {
    cb: Rc<dyn Fn(IN) -> OUT>,
}

impl<IN, OUT, F: Fn(IN) -> OUT + 'static> From<F> for Callback<IN, OUT> {
    fn from(func: F) -> Self {
        Callback { cb: Rc::new(func) }
    }
}

impl<IN, OUT> Clone for Callback<IN, OUT> {
    fn clone(&self) -> Self {
        Self {
            cb: self.cb.clone(),
        }
    }
}

#[allow(clippy::vtable_address_comparisons)]
impl<IN, OUT> PartialEq for Callback<IN, OUT> {
    fn eq(&self, other: &Callback<IN, OUT>) -> bool {
        Rc::ptr_eq(&self.cb, &other.cb)
    }
}

impl<IN, OUT> fmt::Debug for Callback<IN, OUT> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Callback<_>")
    }
}

impl<IN, OUT> Callback<IN, OUT> {
    /// Calls the callback's function.
    pub fn emit(&self, value: IN) -> OUT {
        (*self.cb)(value)
    }
}

impl<IN> Callback<IN> {
    /// Creates a "no-op" callback which can be used when it is not suitable to use an
    /// `Option<Callback>`.
    pub fn noop() -> Self {
        Self::from(|_| ())
    }
}

impl<IN> Default for Callback<IN> {
    fn default() -> Self {
        Self::noop()
    }
}

impl<IN: 'static, OUT: 'static> Callback<IN, OUT> {
    /// Creates a new callback from another callback and a function.
    /// When emitted, calls `func` first, then emits the result to the original callback.
    pub fn reform<F, T>(&self, func: F) -> Callback<T, OUT>
    where
        F: Fn(T) -> IN + 'static,
    {
        let this = self.clone();
        let func = move |input| {
            let output = func(input);
            this.emit(output)
        };
        Callback::from(func)
    }

    /// Creates a new callback from another callback and a function.
    /// When emitted will call the function and, only if it returns `Some(value)`, will emit
    /// `value` to the original callback.
    pub fn filter_reform<F, T>(&self, func: F) -> Callback<T, Option<OUT>>
    where
        F: Fn(T) -> Option<IN> + 'static,
    {
        let this = self.clone();
        let func = move |input| func(input).map(|output| this.emit(output));
        Callback::from(func)
    }
}

#[cfg(test)]
mod test {
    use std::sync::Mutex;

    use super::*;

    fn emit<T, I, R: 'static + Clone, F, OUT>(values: I, f: F) -> Vec<R>
    where
        I: IntoIterator<Item = T>,
        F: FnOnce(Callback<R, ()>) -> Callback<T, OUT>,
    {
        let result = Rc::new(Mutex::new(Vec::new()));
        let cb_result = result.clone();
        let cb = f(Callback::<R, ()>::from(move |v| {
            cb_result.lock().unwrap().push(v);
        }));
        for value in values {
            cb.emit(value);
        }
        let x = result.lock().unwrap().clone();
        x
    }

    #[test]
    fn test_callback() {
        assert_eq!(*emit([true, false], |cb| cb), vec![true, false]);
    }

    #[test]
    fn test_reform() {
        assert_eq!(
            *emit([true, false], |cb| cb.reform(|v: bool| !v)),
            vec![false, true]
        );
    }

    #[test]
    fn test_filter_reform() {
        assert_eq!(
            *emit([1, 2, 3], |cb| cb.filter_reform(|v| match v {
                1 => Some(true),
                2 => Some(false),
                _ => None,
            })),
            vec![true, false]
        );
    }
}
