use thiserror::Error;

#[derive(Error, Debug)]
pub enum CliError {
    #[error("WebSocket error: {0}")]
    WebSocket(String),
    
    #[error("API error: {0}")]
    Api(String),
    
    #[error("Configuration error: {0}")]
    Config(String),
    
    #[error("Invalid input: {0}")]
    InvalidInput(String),
    
    #[error("Room not found: {0}")]
    RoomNotFound(String),
    
    #[error("Not a member of room: {0}")]
    NotMember(String),
}