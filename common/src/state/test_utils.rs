use super::*;
use crate::parameters::ChatRoomParameters;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use rand::prelude::*;
use std::sync::Mutex;

lazy_static::lazy_static! {
    static ref LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
}

macro_rules! log {
    ($($arg:tt)*) => {
        LOG.lock().unwrap().push(format!($($arg)*));
    };
}

pub fn create_test_parameters() -> ChatRoomParameters {
    let mut rng = rand::thread_rng();
    let mut secret_key = [0u8; 32];
    rng.fill_bytes(&mut secret_key);
    let signing_key = SigningKey::from_bytes(&secret_key);
    ChatRoomParameters {
        owner: signing_key.verifying_key(),
    }
}

pub fn test_apply_deltas<F>(
    initial_state: ChatRoomState,
    deltas: Vec<ChatRoomDelta>,
    state_validator: F,
    parameters: &ChatRoomParameters,
) where
    F: Fn(&ChatRoomState) -> bool,
{
    LOG.lock().unwrap().clear();
    let mut current_state = initial_state.clone();

    if deltas.len() > 1 {
        let mut rng = thread_rng();
        for i in 0..10 {  // Run 10 random permutations
            log!("Permutation {}", i + 1);
            current_state = initial_state.clone();
            let mut permuted_deltas = deltas.clone();
            permuted_deltas.shuffle(&mut rng);

            for (j, delta) in permuted_deltas.iter().enumerate() {
                log!("Applying delta {}", j + 1);
                log!("Delta: {:?}", delta);
                log!("Before state: {:?}", current_state);
                match current_state.apply_delta(delta, parameters) {
                    Ok(_) => {
                        log!("After state: {:?}", current_state);
                        if let Err(e) = current_state.validate(parameters) {
                            panic!("State became invalid after applying delta {}: {}. Log:\n{}", j + 1, e, LOG.lock().unwrap().join("\n"));
                        }
                    },
                    Err(e) => {
                        panic!("Error applying delta {}: {}. Log:\n{}", j + 1, e, LOG.lock().unwrap().join("\n"));
                    }
                }
                log!("");
            }

            assert!(state_validator(&current_state), "State does not meet expected criteria after applying deltas in permutation {}. Log:\n{}", i + 1, LOG.lock().unwrap().join("\n"));
            log!("Permutation {} successful", i + 1);
            log!("");
        }

        log!("All permutations successful");
    } else {
        // If there's only one delta, just apply it once
        log!("Applying single delta");
        log!("Delta: {:?}", deltas[0]);
        log!("Before state: {:?}", current_state);
        match current_state.apply_delta(&deltas[0], parameters) {
            Ok(_) => {
                log!("After state: {:?}", current_state);
                if let Err(e) = current_state.validate(parameters) {
                    panic!("State became invalid after applying delta: {}. Log:\n{}", e, LOG.lock().unwrap().join("\n"));
                }
            },
            Err(e) => {
                panic!("Error applying delta: {}. Log:\n{}", e, LOG.lock().unwrap().join("\n"));
            }
        }
        log!("");

        assert!(state_validator(&current_state), "State does not meet expected criteria after applying delta. Log:\n{}", LOG.lock().unwrap().join("\n"));
        log!("Single delta application successful");
    }

    // Create a delta from one of the valid final states relative to the initial state
    let mut final_state = initial_state.clone();
    for delta in deltas.iter() {
        final_state.apply_delta(delta, parameters).unwrap();
    }
    let initial_summary = initial_state.summarize();
    let final_delta = final_state.create_delta(&initial_summary);

    // Apply this delta to the initial state
    let mut new_state = initial_state.clone();
    new_state.apply_delta(&final_delta, parameters).unwrap();

    // Verify that the new state passes the state_validator
    assert!(
        state_validator(&new_state),
        "State created from delta does not meet expected criteria. Log:\n{}",
        LOG.lock().unwrap().join("\n")
    );

    log!("Delta creation and application successful");
}
use super::*;
use crate::parameters::ChatRoomParameters;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use rand::prelude::*;
use std::sync::Mutex;

lazy_static::lazy_static! {
    static ref LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
}

macro_rules! log {
    ($($arg:tt)*) => {
        LOG.lock().unwrap().push(format!($($arg)*));
    };
}

pub fn create_test_parameters() -> ChatRoomParameters {
    let mut rng = rand::thread_rng();
    let mut secret_key = [0u8; 32];
    rng.fill_bytes(&mut secret_key);
    let signing_key = SigningKey::from_bytes(&secret_key);
    ChatRoomParameters {
        owner: signing_key.verifying_key(),
    }
}

pub fn test_apply_deltas<F>(
    initial_state: ChatRoomState,
    deltas: Vec<ChatRoomDelta>,
    state_validator: F,
    parameters: &ChatRoomParameters,
) where
    F: Fn(&ChatRoomState) -> bool,
{
    LOG.lock().unwrap().clear();
    let mut current_state = initial_state.clone();

    if deltas.len() > 1 {
        let mut rng = thread_rng();
        for i in 0..10 {  // Run 10 random permutations
            log!("Permutation {}", i + 1);
            current_state = initial_state.clone();
            let mut permuted_deltas = deltas.clone();
            permuted_deltas.shuffle(&mut rng);

            for (j, delta) in permuted_deltas.iter().enumerate() {
                log!("Applying delta {}", j + 1);
                log!("Delta: {:?}", delta);
                log!("Before state: {:?}", current_state);
                match current_state.apply_delta(delta, parameters) {
                    Ok(_) => {
                        log!("After state: {:?}", current_state);
                        if let Err(e) = current_state.validate(parameters) {
                            panic!("State became invalid after applying delta {}: {}. Log:\n{}", j + 1, e, LOG.lock().unwrap().join("\n"));
                        }
                    },
                    Err(e) => {
                        panic!("Error applying delta {}: {}. Log:\n{}", j + 1, e, LOG.lock().unwrap().join("\n"));
                    }
                }
                log!("");
            }

            assert!(state_validator(&current_state), "State does not meet expected criteria after applying deltas in permutation {}. Log:\n{}", i + 1, LOG.lock().unwrap().join("\n"));
            log!("Permutation {} successful", i + 1);
            log!("");
        }

        log!("All permutations successful");
    } else {
        // If there's only one delta, just apply it once
        log!("Applying single delta");
        log!("Delta: {:?}", deltas[0]);
        log!("Before state: {:?}", current_state);
        match current_state.apply_delta(&deltas[0], parameters) {
            Ok(_) => {
                log!("After state: {:?}", current_state);
                if let Err(e) = current_state.validate(parameters) {
                    panic!("State became invalid after applying delta: {}. Log:\n{}", e, LOG.lock().unwrap().join("\n"));
                }
            },
            Err(e) => {
                panic!("Error applying delta: {}. Log:\n{}", e, LOG.lock().unwrap().join("\n"));
            }
        }
        log!("");

        assert!(state_validator(&current_state), "State does not meet expected criteria after applying delta. Log:\n{}", LOG.lock().unwrap().join("\n"));
        log!("Single delta application successful");
    }

    // Create a delta from one of the valid final states relative to the initial state
    let mut final_state = initial_state.clone();
    for delta in deltas.iter() {
        final_state.apply_delta(delta, parameters).unwrap();
    }
    let initial_summary = initial_state.summarize();
    let final_delta = final_state.create_delta(&initial_summary);

    // Apply this delta to the initial state
    let mut new_state = initial_state.clone();
    new_state.apply_delta(&final_delta, parameters).unwrap();

    // Verify that the new state passes the state_validator
    assert!(
        state_validator(&new_state),
        "State created from delta does not meet expected criteria. Log:\n{}",
        LOG.lock().unwrap().join("\n")
    );

    log!("Delta creation and application successful");
}
