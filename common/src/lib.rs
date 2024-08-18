pub mod state;
pub mod configuration;
pub mod member;
pub mod upgrade;
pub mod message;
pub mod ban;
pub mod summary;
pub mod parameters;
pub mod delta;
pub mod util;

pub use state::ChatRoomState;
pub use summary::ChatRoomSummary;
pub use parameters::ChatRoomParameters;
pub use delta::ChatRoomDelta;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}

#[cfg(test)]
mod proptest_tests;
