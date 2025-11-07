use anyhow::{Context, Result};
use bs58::Alphabet;
use ciborium::de::from_reader;
use redb::{
    Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition, TableHandle,
};
use river_core::room_state::ChatRoomStateV1;
use std::env;
use std::time::SystemTime;

const STATE_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("state");
const CONTRACT_PARAMS_TABLE: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("contract_params");

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let db_path = args
        .next()
        .context("Usage: dump_state <db-path> [contract_prefix]")?;
    let filter = args.next();

    let db = Database::builder().open(&db_path)?;
    let txn = db.begin_read()?;
    println!("Available tables:");
    for name in txn.list_tables()? {
        println!("  - {}", name.name());
    }

    let table = match txn.open_table(STATE_TABLE) {
        Ok(t) => t,
        Err(_) => {
            eprintln!("STATE_TABLE not found");
            return Ok(());
        }
    };

    println!("state entries reported by redb: {}", table.len()?);

    let mut count = 0usize;
    for entry in table.iter()? {
        let (key, value) = entry?;
        let key_bytes = key.value();
        let key_b58 = bs58::encode(key_bytes)
            .with_alphabet(Alphabet::BITCOIN)
            .into_string();

        if let Some(prefix) = &filter {
            if !key_b58.starts_with(prefix) {
                continue;
            }
        }

        let state_bytes = value.value();
        let room_state: ChatRoomStateV1 =
            from_reader(state_bytes).context("Failed to deserialize room state")?;

        println!(
            "Contract {} -> {} messages, {} members",
            key_b58,
            room_state.recent_messages.messages.len(),
            room_state.members.members.len()
        );

        for msg in &room_state.recent_messages.messages {
            let timestamp = msg
                .message
                .time
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default();
            match &msg.message.content {
                river_core::room_state::message::RoomMessageBody::Public { plaintext } => {
                    println!("  [{}] {}...", timestamp, truncate(plaintext.as_str(), 80));
                }
                river_core::room_state::message::RoomMessageBody::Private { .. } => {
                    println!("  [{}] <private message>", timestamp);
                }
            }
        }
        count += 1;
    }

    if count == 0 {
        println!("No contract states stored in table");
    }

    if let Ok(params_table) = txn.open_table(CONTRACT_PARAMS_TABLE) {
        let params_count = params_table.len()?;
        println!("contract_params entries reported by redb: {}", params_count);
    }

    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}
