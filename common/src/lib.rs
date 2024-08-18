pub mod state;
pub mod summary;
pub mod parameters;
pub mod delta;
pub mod util;

pub use delta::ChatRoomDelta;
pub use parameters::ChatRoomParameters;
pub use state::ChatRoomState;
pub use summary::ChatRoomSummary;

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        let result = 2 + 2;
        assert_eq!(result, 4);
    }
}