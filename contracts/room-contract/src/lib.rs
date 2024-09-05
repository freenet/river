use ciborium::de::from_reader;
use freenet_stdlib::prelude::*;
use common::ChatRoomStateV1;
use common::state::{ChatRoomParametersV1, ChatRoomStateV1Delta, ChatRoomStateV1Summary};

struct Contract;

#[contract]
impl ContractInterface for Contract {
    fn validate_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        _related: RelatedContracts<'static>,
    ) -> Result<ValidateResult, ContractError> {
        let bytes = state.as_ref();
        // allow empty state
        if bytes.is_empty() {
            return Ok(ValidateResult::Valid);
        }
        let chat_state = from_reader::<ChatRoomStateV1, &[u8]>(bytes)
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        
        let parameters = from_reader::<ChatRoomParametersV1, &[u8]>(parameters.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;        
        
        chat_state.verify(&chat_state, &parameters)
            .map(|_| ValidateResult::Valid)
            .map_err(|e| ContractError::InvalidState)
    }

    fn validate_delta(
        parameters: Parameters<'static>,
        delta: StateDelta<'static>,
    ) -> Result<bool, ContractError> {
        // validate_delta is obsolete
        Ok(true)
    }

    fn update_state(
        parameters: Parameters<'static>,
        state: State<'static>,
        data: Vec<UpdateData<'static>>,
    ) -> Result<UpdateModification<'static>, ContractError> {
        let parameters = from_reader::<ChatRoomParametersV1, &[u8]>(parameters.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
    }

    fn summarize_state(
        parameters: Parameters<'static>,
        state: State<'static>,
    ) -> Result<StateSummary<'static>, ContractError> {
        let state = state.as_ref();
        if state.is_empty() {
            return Ok(StateSummary::from(vec![]));
        }
        let parameters = from_reader::<ChatRoomParametersV1, &[u8]>(parameters.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let state = from_reader::<ChatRoomStateV1, &[u8]>(state)?;
        let summary = state.summarize(&state, &parameters)
            .map(|summary| StateSummary::from(summary))
            .map_err(|e| ContractError::InvalidState)?;
        let mut summary_bytes = vec![];
        ciborium::into_writer(summary, &mut summary_bytes)
            .map_err(|e| ContractError::InvalidState)?;
        Ok(StateSummary::from(summary_bytes))
    }

    fn get_state_delta(
        parameters: Parameters<'static>,
        state: State<'static>,
        summary: StateSummary<'static>,
    ) -> Result<StateDelta<'static>, ContractError> {
        let chat_state = from_reader::<ChatRoomStateV1, &[u8]>(state.as_ref())
            .map_err(|e| ContractError::Deser(e.to_string()))?;
        let parameters = from_reader::<ChatRoomParametersV1, &[u8]>(parameters.as_ref())?;
        let summary = from_reader::<ChatRoomStateV1Summary, &[u8]>(summary.as_ref())?;
        let delta = chat_state.delta(&chat_state, &parameters, &summary)
            .map_err(|e| ContractError::InvalidState)?;
        let mut delta_bytes = vec![];
        ciborium::into_writer(delta, &mut delta_bytes)
            .map_err(|e| ContractError::InvalidDelta)?;
        Ok(StateDelta::from(delta_bytes))
        
    }
}
