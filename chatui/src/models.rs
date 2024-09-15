use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct Message {
    pub id: usize,
    pub user: String,
    pub content: String,
    pub timestamp: String,
}

#[derive(Clone, Debug)]
pub struct Room {
    pub id: String,
    pub name: String,
    pub users: Vec<String>,
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug)]
#[derive(Default)]
pub struct ChatState {
    pub rooms: HashMap<String, Room>,
    pub current_room: String,
}

pub fn init_chat_state() -> ChatState {
    let mut rooms = HashMap::new();
    
    // General room
    rooms.insert("general".to_string(), Room {
        id: "general".to_string(),
        name: "General".to_string(),
        users: vec!["Alice".to_string(), "Bob".to_string()],
        messages: vec![
            Message { id: 1, user: "Alice".to_string(), content: "Hello, everyone!".to_string(), timestamp: "2023-09-13 10:00:00".to_string() },
            Message { id: 2, user: "Bob".to_string(), content: "Hi Alice!".to_string(), timestamp: "2023-09-13 10:01:00".to_string() },
        ],
    });

    // Freenet Dev room
    rooms.insert("freenet_dev".to_string(), Room {
        id: "freenet_dev".to_string(),
        name: "Freenet Dev".to_string(),
        users: vec!["Charlie".to_string(), "David".to_string()],
        messages: vec![
            Message { id: 1, user: "Charlie".to_string(), content: "Any updates on the new feature?".to_string(), timestamp: "2023-09-13 09:30:00".to_string() },
            Message { id: 2, user: "David".to_string(), content: "Working on it, should be ready by tomorrow.".to_string(), timestamp: "2023-09-13 09:35:00".to_string() },
        ],
    });

    // Privacy Talk room
    rooms.insert("privacy_talk".to_string(), Room {
        id: "privacy_talk".to_string(),
        name: "Privacy Talk".to_string(),
        users: vec!["Eve".to_string(), "Frank".to_string()],
        messages: vec![
            Message { id: 1, user: "Eve".to_string(), content: "What do you think about the latest encryption standards?".to_string(), timestamp: "2023-09-13 11:00:00".to_string() },
            Message { id: 2, user: "Frank".to_string(), content: "They look promising, but we need more analysis.".to_string(), timestamp: "2023-09-13 11:05:00".to_string() },
        ],
    });

    // Decentralization room
    rooms.insert("decentralization".to_string(), Room {
        id: "decentralization".to_string(),
        name: "Decentralization".to_string(),
        users: vec!["Grace".to_string(), "Henry".to_string()],
        messages: vec![
            Message { id: 1, user: "Grace".to_string(), content: "Decentralized systems are the future!".to_string(), timestamp: "2023-09-13 12:00:00".to_string() },
            Message { id: 2, user: "Henry".to_string(), content: "Agreed! We need to focus on scalability though.".to_string(), timestamp: "2023-09-13 12:05:00".to_string() },
        ],
    });

    ChatState {
        rooms,
        current_room: "general".to_string(),
    }
}
