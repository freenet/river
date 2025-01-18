The purpose of the web-container-contract is to act as a container contract for the web interface.

./target/dx/river-ui/debug/web/public/index.html (and other files in the directory)
./target/dx/river-ui/release/web/public/index.html



The web-container-contract state consists of metadata and the web interface compressed. The metadata should be
encoded with ciborium - it should contain a u32 version number and a digital signature of the web interface
together with the version number, signed using a public EC key that's specified in the contract parameters.
The `validate_state` function should verify the signature and the version number. `update_state` should update
the state if and only if the updated state has a higher version number.

# An example of a contract state (in this case it's just glue code for a ComposableState trait but I don't think we want to use ComposableState for this contract)

```rust
use ciborium::{de::from_reader, ser::into_writer};
use freenet_stdlib::prelude::*;

use common::room_state::{ChatRoomParametersV1, ChatRoomStateV1Delta, ChatRoomStateV1Summary};
use common::ChatRoomStateV1;
use freenet_scaffold::ComposableState;
use freenet_stdlib::prelude::ContractError;

#[allow(dead_code)]
struct Contract;

#[contract]
impl ContractInterface for Contract {
    fn validate_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        _related: RelatedContracts<'static>,
    ) -> Result<ValidateResult, freenet_stdlib::prelude::ContractError> {
        let bytes = state.as_ref();
        // allow empty room_state
        if bytes.is_empty() {
            return Ok(ValidateResult::Valid);
        }
        let chat_state = from_reader::<ChatRoomStateV1, &[u8]>(bytes)
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        let parameters = from_reader::<ChatRoomParametersV1, &[u8]>(parameters.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        chat_state
            .verify(&chat_state, &parameters)
            .map(|_| ValidateResult::Valid)
            .map_err(|e| ContractError::InvalidState)
    }

    fn update_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        data: Vec<UpdateData<'static>>,
    ) -> Result<UpdateModification<'static>, freenet_stdlib::prelude::ContractError> {
        let parameters = from_reader::<ChatRoomParametersV1, &[u8]>(parameters.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let mut chat_state = from_reader::<ChatRoomStateV1, &[u8]>(state.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        for update in data {
            match update {
                UpdateData::State(new_state) => {
                    let new_state = from_reader::<ChatRoomStateV1, &[u8]>(new_state.as_ref())
                        .map_err(|e| ContractError::Deser(e.to_string()))?;
                    chat_state
                        .merge(&chat_state.clone(), &parameters, &new_state)
                        .map_err(|_| ContractError::InvalidUpdate)?;
                }
                UpdateData::Delta(d) => {
                    let delta = from_reader::<ChatRoomStateV1Delta, &[u8]>(d.as_ref())
                        .map_err(|e| ContractError::Deser(e.to_string()))?;
                    chat_state
                        .apply_delta(&chat_state.clone(), &parameters, &Some(delta))
                        .map_err(|_| ContractError::InvalidUpdate)?;
                }
                UpdateData::RelatedState {
                    related_to: _,
                    state: _,
                } => {
                    // TODO: related room_state handling not needed for river
                }
                _ => unreachable!(),
            }
        }

        let mut updated_state = vec![];
        into_writer(&chat_state, &mut updated_state)
            .map_err(|e| ContractError::Deser(e.to_string()))?;

        Ok(UpdateModification::valid(updated_state.into()))
    }

    fn summarize_state(
        parameters: Parameters<'static>,
        state: State<'static>,
    ) -> Result<StateSummary<'static>, freenet_stdlib::prelude::ContractError> {
        let state = state.as_ref();
        if state.is_empty() {
            return Ok(StateSummary::from(vec![]));
        }
        let parameters = from_reader::<ChatRoomParametersV1, &[u8]>(parameters.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let state = from_reader::<ChatRoomStateV1, &[u8]>(state)
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let summary = state.summarize(&state, &parameters);
        let mut summary_bytes = vec![];
        into_writer(&summary, &mut summary_bytes)
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        Ok(StateSummary::from(summary_bytes))
    }

    fn get_state_delta(
        parameters: Parameters<'static>,
        state: State<'static>,
        summary: StateSummary<'static>,
    ) -> Result<StateDelta<'static>, freenet_stdlib::prelude::ContractError> {
        let chat_state = from_reader::<ChatRoomStateV1, &[u8]>(state.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let parameters = from_reader::<ChatRoomParametersV1, &[u8]>(parameters.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let summary = from_reader::<ChatRoomStateV1Summary, &[u8]>(summary.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let delta = chat_state.delta(&chat_state, &parameters, &summary);
        let mut delta_bytes = vec![];
        into_writer(&delta, &mut delta_bytes).map_err(|e| ContractError::Deser(e.to_string()))?;
        Ok(StateDelta::from(delta_bytes))
    }
}
```

# How the state is encoded/decoded within the Freenet code (not available within this codebase)

```rust
//! Helper functions and types for dealing with HTTP gateway compatible contracts.
use std::{
    io::{Cursor, Read},
    path::Path,
};

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use tar::{Archive, Builder};
use xz2::read::{XzDecoder, XzEncoder};

#[derive(Debug, thiserror::Error)]
pub enum WebContractError {
    #[error("unpacking error: {0}")]
    UnpackingError(anyhow::Error),
    #[error("{0}")]
    StoringError(std::io::Error),
    #[error("file not found: {0}")]
    FileNotFound(String),
}

#[non_exhaustive]
pub struct WebApp {
    pub metadata: Vec<u8>,
    pub web: Vec<u8>,
}

impl WebApp {
    pub fn from_data(
        metadata: Vec<u8>,
        web: Builder<Cursor<Vec<u8>>>,
    ) -> Result<Self, WebContractError> {
        let buf = web.into_inner().unwrap().into_inner();
        let mut encoder = XzEncoder::new(Cursor::new(buf), 6);
        let mut compressed = vec![];
        encoder.read_to_end(&mut compressed).unwrap();
        Ok(Self {
            metadata,
            web: compressed,
        })
    }

    pub fn pack(mut self) -> std::io::Result<Vec<u8>> {
        let mut output = Vec::with_capacity(
            self.metadata.len() + self.web.len() + (std::mem::size_of::<u64>() * 2),
        );
        output.write_u64::<BigEndian>(self.metadata.len() as u64)?;
        output.append(&mut self.metadata);
        output.write_u64::<BigEndian>(self.web.len() as u64)?;
        output.append(&mut self.web);
        Ok(output)
    }

    pub fn unpack(&mut self, dst: impl AsRef<Path>) -> Result<(), WebContractError> {
        let mut decoded_web = self.decode_web();
        decoded_web
            .unpack(dst)
            .map_err(WebContractError::StoringError)?;
        Ok(())
    }

    pub fn get_file(&mut self, path: &str) -> Result<Vec<u8>, WebContractError> {
        let mut decoded_web = self.decode_web();
        for e in decoded_web
            .entries()
            .map_err(|e| WebContractError::UnpackingError(anyhow::anyhow!(e)))?
        {
            let mut e = e.map_err(|e| WebContractError::UnpackingError(anyhow::anyhow!(e)))?;
            if e.path()
                .ok()
                .filter(|p| p.to_string_lossy() == path)
                .is_some()
            {
                let mut bytes = vec![];
                e.read_to_end(&mut bytes)
                    .map_err(|e| WebContractError::UnpackingError(anyhow::anyhow!(e)))?;
                return Ok(bytes);
            }
        }
        Err(WebContractError::FileNotFound(path.to_owned()))
    }

    fn decode_web(&self) -> Archive<XzDecoder<&[u8]>> {
        let decoder = XzDecoder::new(self.web.as_slice());
        Archive::new(decoder)
    }
}

impl<'a> TryFrom<&'a [u8]> for WebApp {
    type Error = WebContractError;

    fn try_from(state: &'a [u8]) -> Result<Self, Self::Error> {
        const MAX_METADATA_SIZE: u64 = 1024;
        const MAX_WEB_SIZE: u64 = 1024 * 1024 * 100;
        // Decompose the state and extract the compressed web interface
        let mut state = Cursor::new(state);

        let metadata_size = state
            .read_u64::<BigEndian>()
            .map_err(|e| WebContractError::UnpackingError(anyhow::anyhow!(e)))?;
        if metadata_size > MAX_METADATA_SIZE {
            return Err(WebContractError::UnpackingError(anyhow::anyhow!(
                "Exceeded metadata size of 1kB: {} bytes",
                metadata_size
            )));
        }
        let mut metadata = vec![0; metadata_size as usize];
        state
            .read_exact(&mut metadata)
            .map_err(|e| WebContractError::UnpackingError(anyhow::anyhow!(e)))?;

        let web_size = state
            .read_u64::<BigEndian>()
            .map_err(|e| WebContractError::UnpackingError(anyhow::anyhow!(e)))?;
        if web_size > MAX_WEB_SIZE {
            return Err(WebContractError::UnpackingError(anyhow::anyhow!(
                "Exceeded packed web size of 100MB: {} bytes",
                web_size
            )));
        }
        let mut web = vec![0; web_size as usize];
        state
            .read_exact(&mut web)
            .map_err(|e| WebContractError::UnpackingError(anyhow::anyhow!(e)))?;

        Ok(Self { metadata, web })
    }
}
```