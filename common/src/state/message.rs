use crate::state::member::MemberId;
use crate::util::{fast_hash, truncated_base64};
use ed25519_dalek::{Signature, SigningKey, VerifyingKey, Signer, Verifier};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::SystemTime;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Message {
    pub time: SystemTime,
    pub content: String,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorizedMessage {
    pub message: Message,
    pub author: MemberId,
    pub signature: Signature,
}


impl fmt::Debug for AuthorizedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedMessage")
            .field("message", &self.message)
            .field("author", &self.author)
            .field("signature", &format_args!("{}", truncated_base64(self.signature.to_bytes())))
            .finish()
    }
}

#[derive(Eq, PartialEq, Hash, Serialize, Deserialize, Clone, Debug, Ord, PartialOrd)]
pub struct MessageId(pub i32);

impl AuthorizedMessage {
    pub fn new(message: Message, author: MemberId, signing_key: &SigningKey) -> Self {
        let mut serialized_message = Vec::new();
        ciborium::ser::into_writer(&message, &mut serialized_message).expect("Serialization should not fail");
        let signature = signing_key.sign(&serialized_message);
        
        Self {
            message,
            author,
            signature,
        }
    }

    pub fn validate(&self, verifying_key: &VerifyingKey) -> Result<(), ed25519_dalek::SignatureError> {
        let mut serialized_message = Vec::new();
        ciborium::ser::into_writer(&self.message, &mut serialized_message).expect("Serialization should not fail");
        verifying_key.verify(&serialized_message, &self.signature)
    }

    pub fn id(&self) -> MessageId {
        MessageId(fast_hash(&self.signature.to_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{SigningKey, SecretKey};
    use rand::rngs::OsRng;

    #[test]
    fn test_message_creation_and_validation() {
        let mut csprng = OsRng;
        let secret_key = SecretKey::generate(&mut csprng);
        let signing_key = SigningKey::from_bytes(&secret_key.to_bytes());
        let verifying_key = signing_key.verifying_key();

        let message = Message {
            time: SystemTime::UNIX_EPOCH,
            content: "Test message".to_string(),
        };

        let author = MemberId(1);

        let authorized_message = AuthorizedMessage::new(message.clone(), author, &signing_key);

        // Test that the message was correctly stored
        assert_eq!(authorized_message.message, message);
        assert_eq!(authorized_message.author, author);

        // Test that the signature is valid
        assert!(authorized_message.validate(&verifying_key).is_ok());

        // Test with an incorrect verifying key
        let wrong_secret_key = SecretKey::generate(&mut csprng);
        let wrong_signing_key = SigningKey::from_bytes(&wrong_secret_key.to_bytes());
        let wrong_verifying_key = wrong_signing_key.verifying_key();
        assert!(authorized_message.validate(&wrong_verifying_key).is_err());

        // Test message ID generation
        let id1 = authorized_message.id();
        let id2 = authorized_message.id();
        assert_eq!(id1, id2);
    }
}
