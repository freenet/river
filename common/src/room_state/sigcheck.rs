//! The one place `common` runs an ed25519 check.
//!
//! Inside a [`scope`] a check is remembered, keyed on its whole input (kind,
//! key, BLAKE3 of the exact bytes handed to ed25519, signature), so a
//! remembered answer is the answer the check would give. Outside a scope
//! nothing is remembered and every call does the crypto, as it always did.
//!
//! [`seed_from`] lets the room contract's `update_state` treat the stored
//! state as already validated: it runs the stored state's `verify` in assume
//! mode, recording each signature that `verify` relies on as verified without
//! doing the crypto, so an update only pays for signatures it adds.

use ed25519_dalek::{Signature, SignatureError, Verifier, VerifyingKey};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

/// Which ed25519 check: a key that passes `Plain` can fail `Strict`, so the
/// two are never mixed up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Plain = 0,
    Strict = 1,
}

type Fact = (u8, [u8; 32], [u8; 32], [u8; 64]);

thread_local! {
    static MEMO: RefCell<Option<HashMap<Fact, bool>>> = const { RefCell::new(None) };
    /// Facts recorded while assuming; merged into `MEMO` only if the seeding
    /// run returned `Ok`.
    static ASSUMED: RefCell<Option<HashMap<Fact, bool>>> = const { RefCell::new(None) };
    static REAL_CHECKS: Cell<usize> = const { Cell::new(0) };
}

/// Real ed25519 checks run on this thread so far: a deterministic cost
/// counter for tests and benchmarks.
pub fn real_checks() -> usize {
    REAL_CHECKS.with(|c| c.get())
}

/// Whether a memo scope is active on this thread.
pub fn is_active() -> bool {
    MEMO.with(|m| m.borrow().is_some())
}

/// The one ed25519 check. With a scope active, a remembered answer is
/// returned without crypto; otherwise the check runs and, in a scope, is
/// remembered.
pub(crate) fn verify(
    kind: Kind,
    vk: &VerifyingKey,
    msg: &[u8],
    sig: &Signature,
) -> Result<(), SignatureError> {
    let in_scope = is_active();
    let fact = in_scope.then(|| {
        (
            kind as u8,
            vk.to_bytes(),
            *blake3::hash(msg).as_bytes(),
            sig.to_bytes(),
        )
    });
    if let Some(fact) = &fact {
        if let Some(ok) = MEMO.with(|m| m.borrow().as_ref().and_then(|m| m.get(fact).copied())) {
            return answer(ok);
        }
        let assuming = ASSUMED.with(|a| {
            a.borrow_mut().as_mut().map(|a| {
                a.insert(*fact, true);
            })
        });
        if assuming.is_some() {
            return Ok(());
        }
    }
    REAL_CHECKS.with(|c| c.set(c.get() + 1));
    let result = match kind {
        Kind::Plain => vk.verify(msg, sig),
        Kind::Strict => vk.verify_strict(msg, sig),
    };
    if let Some(fact) = fact {
        MEMO.with(|m| {
            if let Some(m) = m.borrow_mut().as_mut() {
                m.insert(fact, result.is_ok());
            }
        });
    }
    result
}

fn answer(ok: bool) -> Result<(), SignatureError> {
    if ok {
        Ok(())
    } else {
        Err(SignatureError::new())
    }
}

/// Run `f` with a memo active. A nested call shares the outermost memo; the
/// outermost call clears it when it returns, also if `f` panics.
pub fn scope<R>(f: impl FnOnce() -> R) -> R {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            MEMO.with(|m| *m.borrow_mut() = None);
            ASSUMED.with(|a| *a.borrow_mut() = None);
        }
    }
    let outermost = MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.is_none() {
            *m = Some(HashMap::new());
            true
        } else {
            false
        }
    });
    let _clear = outermost.then_some(Clear);
    f()
}

/// Run `f`, which must be `stored.verify(stored, params)` on a state this
/// node already validated, with every signature check recorded as true
/// instead of run. Returns `f`'s result. The facts, and every other memo's
/// entries made meanwhile, are kept only if `f` returned `Ok`; on `Err` they
/// are all discarded, so a failed seeding leaves no unchecked fact behind.
///
/// Sound only because every signature check inside `verify` is mandatory (a
/// false result makes `verify` return `Err`). Do not add a check inside any
/// field's `verify` that tolerates a bad signature. Called from exactly one
/// place, the room contract's `update_state`.
pub fn seed_from<R, E>(f: impl FnOnce() -> Result<R, E>) -> Result<R, E> {
    struct Stop;
    impl Drop for Stop {
        fn drop(&mut self) {
            ASSUMED.with(|a| *a.borrow_mut() = None);
        }
    }
    if !is_active() {
        return f();
    }
    let before = (
        MEMO.with(|m| m.borrow().clone()),
        crate::room_state::ban::signature_memo::snapshot(),
    );
    ASSUMED.with(|a| *a.borrow_mut() = Some(HashMap::new()));
    let guard = Stop;
    let result = f();
    let assumed = ASSUMED.with(|a| a.borrow_mut().take());
    drop(guard);
    match (&result, assumed) {
        (Ok(_), Some(assumed)) => MEMO.with(|m| {
            if let Some(m) = m.borrow_mut().as_mut() {
                m.extend(assumed);
            }
        }),
        _ => {
            MEMO.with(|m| *m.borrow_mut() = before.0);
            crate::room_state::ban::signature_memo::restore(before.1);
        }
    }
    result
}

/// Run `f` with the memo set aside (neither read nor written), restoring it
/// afterwards: for the debug-build cross-check in cleanup.
#[cfg(debug_assertions)]
pub(crate) fn set_aside<R>(f: impl FnOnce() -> R) -> R {
    let held = MEMO.with(|m| m.borrow_mut().take());
    // The cross-check exists only in debug builds, so its checks are not
    // counted: the counter reports what a release build does.
    let counted = REAL_CHECKS.with(|c| c.get());
    let r = f();
    REAL_CHECKS.with(|c| c.set(counted));
    MEMO.with(|m| *m.borrow_mut() = held);
    r
}
