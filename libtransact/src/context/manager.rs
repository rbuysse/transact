/*
 * Copyright 2019 Bitwise IO, Inc.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 * -----------------------------------------------------------------------------
 */

use std::collections::HashMap;
use std::collections::VecDeque;
use std::error::Error as StdError;
use std::fmt::Debug;
use std::hash::Hash;
use std::str;

use crate::context::{Context, ContextId};
use crate::receipts::{
    Event, StateChange, TransactionReceipt, TransactionReceiptBuilder,
    TransactionReceiptBuilderError,
};
use crate::state::error::StateReadError;
use crate::state::Read;

#[derive(Debug)]
pub enum ContextManagerError {
    MissingContextError(String),
    TransactionReceiptBuilderError(TransactionReceiptBuilderError),
    StateReadError(StateReadError),
}

impl StdError for ContextManagerError {
    fn description(&self) -> &str {
        match *self {
            ContextManagerError::MissingContextError(ref msg) => msg,
            ContextManagerError::TransactionReceiptBuilderError(ref err) => err.description(),
            ContextManagerError::StateReadError(ref err) => err.description(),
        }
    }
}

impl std::fmt::Display for ContextManagerError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match *self {
            ContextManagerError::MissingContextError(ref s) => {
                write!(f, "Unable to find specified Context: {:?}", s)
            }
            ContextManagerError::TransactionReceiptBuilderError(ref err) => {
                write!(f, "A TransactionReceiptBuilder error occured: {}", err)
            }
            ContextManagerError::StateReadError(ref err) => {
                write!(f, "A State Read error occured: {}", err)
            }
        }
    }
}

impl From<TransactionReceiptBuilderError> for ContextManagerError {
    fn from(err: TransactionReceiptBuilderError) -> Self {
        ContextManagerError::TransactionReceiptBuilderError(err)
    }
}

impl From<StateReadError> for ContextManagerError {
    fn from(err: StateReadError) -> Self {
        ContextManagerError::StateReadError(err)
    }
}

pub struct ContextManager<K, V, R: Read<StateId = String, Key = K, Value = V>> {
    contexts: HashMap<ContextId, Context<K, V>>,
    database: R,
}
impl<
        K: Hash + Eq + Clone + Debug + Default,
        V: Clone + Debug + Default,
        R: Read<StateId = String, Key = K, Value = V>,
    > ContextManager<K, V, R>
{
    pub fn new(database: R) -> Self {
        ContextManager {
            contexts: HashMap::new(),
            database,
        }
    }

    /// Returns a mutable Context within the ContextManager's Context list specified by the ContextId
    fn get_context_mut(
        &mut self,
        context_id: &ContextId,
    ) -> Result<&mut Context<K, V>, ContextManagerError> {
        self.contexts.get_mut(context_id).ok_or_else(|| {
            ContextManagerError::MissingContextError(
                str::from_utf8(context_id)
                    .expect("Unable to generate string from ContextId")
                    .to_string(),
            )
        })
    }

    /// Returns a Context within the ContextManager's Context list specified by the ContextId
    fn get_context(&self, context_id: &ContextId) -> Result<&Context<K, V>, ContextManagerError> {
        self.contexts.get(context_id).ok_or_else(|| {
            ContextManagerError::MissingContextError(
                str::from_utf8(context_id)
                    .expect("Unable to generate string from ContextId")
                    .to_string(),
            )
        })
    }

    /// Get the values associated with list of keys, from a specific Context.
    /// If a key is not found in the context, State is then checked for these keys.
    pub fn get(
        &self,
        context_id: &ContextId,
        keys: &[K],
    ) -> Result<Vec<(K, Option<V>)>, ContextManagerError> {
        let mut key_values = Vec::new();
        for key in keys.iter().rev() {
            let mut context = self.get_context(context_id)?;
            let mut contexts = VecDeque::new();
            for context_id in context.base_contexts().iter() {
                contexts.push_back(self.get_context(context_id)?);
            }
            if !context.contains(&key) && !contexts.is_empty() {
                while let Some(current_context) = contexts.pop_front() {
                    if current_context.contains(&key) {
                        context = current_context;
                        break;
                    } else {
                        context = current_context;
                        for context_id in context.base_contexts().iter() {
                            contexts.push_back(self.get_context(context_id)?);
                        }
                    }
                }
            }
            if context.contains(&key) {
                match context
                    .state_changes()
                    .iter()
                    .rev()
                    .find(|state_change| state_change.has_key(&key))
                {
                    Some(StateChange::Set { key: k, value: v }) => {
                        key_values.push((k.clone(), Some(v.clone())))
                    }
                    _ => {
                        key_values.push((key.clone(), None));
                    }
                }
            } else if let Some(v) = self
                .database
                .get(context.state_id(), &[key.clone()])?
                .get(&key)
            {
                key_values.push((key.clone(), Some(v.clone())))
            }
        }
        Ok(key_values)
    }

    /// Adds a StateChange::Set to the specified Context
    pub fn set_state(
        &mut self,
        context_id: &ContextId,
        key: K,
        value: V,
    ) -> Result<(), ContextManagerError> {
        let context = self.get_context_mut(context_id)?;
        context.set_state(key, value);
        Ok(())
    }

    /// Adds a StateChange::Delete to the specified Context, returning the value, if found, that is
    /// associated with the specified key.
    pub fn delete_state(
        &mut self,
        context_id: &ContextId,
        key: K,
    ) -> Result<Option<V>, ContextManagerError> {
        // Adding a StateChange::Delete to the specified Context, which will occur no matter which
        // Context or State the key and associated value is found in.
        let context_value = self.get_context_mut(context_id)?.delete_state(key.clone());
        if let Some(value) = context_value {
            return Ok(Some(value));
        }

        let current_context = self.get_context(context_id)?;
        let mut containing_context = self.get_context(context_id)?;

        let mut contexts = VecDeque::new();
        contexts.push_front(containing_context);
        // Adding dependent Contexts to search for the Key
        for context_id in containing_context.base_contexts().iter() {
            contexts.push_back(self.get_context(context_id)?);
        }

        while let Some(context) = contexts.pop_front() {
            if context.contains(&key) {
                containing_context = context;
                break;
            } else {
                for context_id in context.base_contexts().iter() {
                    contexts.push_back(self.get_context(context_id)?);
                }
            }
        }
        if containing_context.contains(&key) {
            if let Some(v) = containing_context.get_state(&key) {
                return Ok(Some(v.clone()));
            }
        } else if let Some(value) = self
            .database
            .get(current_context.state_id(), &[key.clone()])?
            .get(&key)
        {
            return Ok(Some(value.clone()));
        }
        Ok(None)
    }

    /// Adds an Event to the specified Context.
    pub fn add_event(
        &mut self,
        context_id: &ContextId,
        event: Event,
    ) -> Result<(), ContextManagerError> {
        let context = self.get_context_mut(&context_id)?;
        context.add_event(event);
        Ok(())
    }

    /// Adds Data to the specified Context.
    pub fn add_data(
        &mut self,
        context_id: &ContextId,
        data: Vec<u8>,
    ) -> Result<(), ContextManagerError> {
        let context = self.get_context_mut(&context_id)?;
        context.add_data(data);
        Ok(())
    }

    /// Creates a Context, and returns the resulting ContextId.
    pub fn create_context(
        &mut self,
        dependent_contexts: Vec<ContextId>,
        state_id: &str,
    ) -> ContextId {
        let new_context = Context::new(state_id, dependent_contexts);
        self.contexts.insert(*new_context.id(), new_context.clone());
        *new_context.id()
    }

    /// Creates a TransactionReceipt based on the information available within the specified Context.
    pub fn get_transaction_receipt(
        &self,
        context_id: &ContextId,
        transaction_id: &str,
    ) -> Result<TransactionReceipt<K, V>, ContextManagerError> {
        let context = self.get_context(context_id)?;
        let new_transaction_receipt = TransactionReceiptBuilder::new()
            .with_state_changes(context.state_changes().to_vec())
            .with_events(context.events().to_vec())
            .with_data(context.data().to_vec())
            .with_transaction_id(transaction_id.to_string())
            .build()?;
        Ok(new_transaction_receipt)
    }

    pub fn drop_context(&self, _context_id: ContextId) {
        unimplemented!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::receipts::EventBuilder;
    use crate::state;
    use crate::state::hashmap::HashMapState;
    use crate::state::Write;

    static KEY1: &str = "111111111111111111111111111111111111111111111111111111111111111111";
    static KEY2: &str = "222222222222222222222222222222222222222222222222222222222222222222";
    static KEY3: &str = "333333333333333333333333333333333333333333333333333333333333333333";
    static KEY4: &str = "444444444444444444444444444444444444444444444444444444444444444444";
    static KEY5: &str = "555555555555555555555555555555555555555555555555555555555555555555";
    static VALUE1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    static VALUE2: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    static VALUE3: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    static VALUE4: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    static BYTES1: [u8; 4] = [0x01, 0x02, 0x03, 0x04];
    static BYTES2: [u8; 4] = [0x05, 0x06, 0x07, 0x08];

    static EVENT_TYPE1: &str = "sawtooth/block-commit";
    static ATTR1: (&str, &str) = (
        "block_id",
        "f40b90d06b4a9074af2ab09e0187223da7466be75ec0f472 \
         f2edd5f22960d76e402e6c07c90b7816374891d698310dd25d9b88dce7dbcba8219d9f7c9cae1861",
    );
    static ATTR2: (&str, &str) = ("block_num", "3");

    fn make_manager(
        state_changes: Option<Vec<state::StateChange<String, String>>>,
    ) -> (ContextManager<String, String, HashMapState>, String) {
        let state = HashMapState::new();
        let mut state_id = HashMapState::state_id(&HashMap::new());
        if let Some(changes) = state_changes {
            state_id = state.commit(&state_id, changes.as_slice()).unwrap();
        }

        (ContextManager::new(state), state_id)
    }

    fn check_state_change(state_change: StateChange<String, String>) {
        match state_change {
            StateChange::Set { key, value } => {
                assert_eq!(KEY1, key);
                assert_eq!(VALUE1, value);
            }
            StateChange::Delete { key } => {
                assert_eq!(KEY1, key);
            }
        }
    }

    fn check_transaction_receipt(
        transaction_receipt: TransactionReceipt<String, String>,
        event: Event,
    ) {
        for state_change in transaction_receipt.state_changes {
            check_state_change(state_change)
        }
        assert_eq!(vec!(event), transaction_receipt.events);
        assert_eq!(vec!(BYTES2.to_vec()), transaction_receipt.data);
    }

    #[test]
    fn create_contexts() {
        let (mut manager, state_id) = make_manager(None);
        let first_context_id = manager.create_context(Vec::new(), &state_id);
        assert!(!manager.contexts.is_empty());
        assert!(manager.contexts.get(&first_context_id).is_some());

        let second_context_id = manager.create_context(Vec::new(), &state_id);
        let second_context = manager.get_context(&second_context_id).unwrap();
        assert_eq!(&second_context_id, second_context.id());
        assert_eq!(manager.contexts.len(), 2);
    }

    #[test]
    fn add_context_event() {
        let (mut manager, state_id) = make_manager(None);
        let context_id = manager.create_context(Vec::new(), &state_id);
        let event = EventBuilder::new()
            .with_event_type(EVENT_TYPE1.to_string())
            .with_attributes(vec![
                (ATTR1.0.to_string(), ATTR1.1.to_string()),
                (ATTR2.0.to_string(), ATTR2.1.to_string()),
            ])
            .with_data(BYTES1.to_vec())
            .build()
            .unwrap();
        let event_add_result = manager.add_event(&context_id, event.clone());
        assert!(event_add_result.is_ok());
        let context = manager.get_context(&context_id).unwrap();
        assert_eq!(context.events()[0], event.clone());
    }

    #[test]
    fn add_context_data() {
        let (mut manager, state_id) = make_manager(None);
        let context_id = manager.create_context(Vec::new(), &state_id);

        let data_add_result = manager.add_data(&context_id, BYTES2.to_vec());
        let context = manager.get_context(&context_id).unwrap();
        assert!(data_add_result.is_ok());
        assert_eq!(context.data()[0], BYTES2);
    }

    #[test]
    fn create_transaction_receipt() {
        let (mut manager, state_id) = make_manager(None);

        let context_id = manager.create_context(Vec::new(), &state_id);
        let mut context = manager.get_context(&context_id).unwrap();
        assert_eq!(&context_id, context.id());

        let set_result = manager.set_state(&context_id, KEY1.to_string(), VALUE1.to_string());
        assert!(set_result.is_ok());
        let delete_result = manager.delete_state(&context_id, KEY1.to_string()).unwrap();
        assert!(delete_result.is_some());

        // Adding an Event to the Context, to be used to build the TransactionReceipt
        let event = EventBuilder::new()
            .with_event_type(EVENT_TYPE1.to_string())
            .with_attributes(vec![
                (ATTR1.0.to_string(), ATTR1.1.to_string()),
                (ATTR2.0.to_string(), ATTR2.1.to_string()),
            ])
            .with_data(BYTES1.to_vec())
            .build()
            .unwrap();
        let event_add_result = manager.add_event(&context_id, event.clone());
        assert!(event_add_result.is_ok());
        context = manager.get_context(&context_id).unwrap();
        assert_eq!(context.events()[0], event.clone());

        // Adding Data to the Context, to be used to build the TransactionReceipt
        let data_add_result = manager.add_data(&context_id, BYTES2.to_vec());
        context = manager.get_context(&context_id).unwrap();
        assert!(data_add_result.is_ok());
        assert_eq!(context.data()[0], BYTES2);

        // Building the TransactionReceipt from the objects within the specified Context
        let transaction_receipt = manager.get_transaction_receipt(&context_id, KEY2).unwrap();
        check_transaction_receipt(transaction_receipt, event)
    }

    #[test]
    fn add_set_state_change() {
        let (mut manager, state_id) = make_manager(None);

        let context_id = manager.create_context(Vec::new(), &state_id);

        let set_result = manager.set_state(&context_id, KEY1.to_string(), VALUE1.to_string());
        assert!(set_result.is_ok());

        let get_value = manager
            .get_context(&context_id)
            .unwrap()
            .get_state(&KEY1.to_string());
        assert_eq!(get_value, Some(&VALUE1.to_string()));
    }

    #[test]
    fn add_delete_state_change() {
        // Creating a ContextManager with a single Context.
        let state_changes = vec![state::StateChange::Set {
            key: KEY1.to_string(),
            value: VALUE1.to_string(),
        }];
        let (mut manager, state_id) = make_manager(Some(state_changes));
        let ancestor_context = manager.create_context(Vec::new(), &state_id);

        assert!(manager
            .set_state(&ancestor_context, KEY2.to_string(), VALUE2.to_string())
            .is_ok());

        let current_context_id = manager.create_context(vec![ancestor_context], &state_id);
        assert!(manager
            .set_state(&current_context_id, KEY3.to_string(), VALUE3.to_string())
            .is_ok());
        assert!(manager
            .set_state(&current_context_id, KEY4.to_string(), VALUE4.to_string())
            .is_ok());

        let deleted_state_value = manager
            .delete_state(&current_context_id, KEY1.to_string())
            .unwrap();
        assert!(deleted_state_value.is_some());
        assert_eq!(deleted_state_value, Some(VALUE1.to_string()));

        let deleted_ancestor_value = manager
            .delete_state(&current_context_id, KEY2.to_string())
            .unwrap();
        assert!(deleted_ancestor_value.is_some());
        assert_eq!(deleted_ancestor_value, Some(VALUE2.to_string()));

        let deleted_current_value = manager
            .delete_state(&current_context_id, KEY3.to_string())
            .unwrap();
        assert!(deleted_current_value.is_some());
        assert_eq!(deleted_current_value, Some(VALUE3.to_string()));

        assert!(manager
            .delete_state(&current_context_id, KEY5.to_string())
            .unwrap()
            .is_none());
    }

    #[test]
    fn get_values() {
        // Creating a ContextManager with a single Context, with a HashMapState backing it
        let state_changes = vec![state::StateChange::Set {
            key: KEY1.to_string(),
            value: VALUE1.to_string(),
        }];
        let (mut manager, state_id) = make_manager(Some(state_changes));
        let ancestor_context = manager.create_context(Vec::new(), &state_id);
        let add_result = manager.set_state(&ancestor_context, KEY2.to_string(), VALUE2.to_string());
        assert!(add_result.is_ok());

        let context_id = manager.create_context(vec![ancestor_context], &state_id);

        // Validates the result from adding the state change to the Context within the ContextManager.
        assert!(manager
            .set_state(&context_id, KEY3.to_string(), VALUE3.to_string())
            .is_ok());
        assert!(manager
            .set_state(&context_id, KEY4.to_string(), VALUE4.to_string())
            .is_ok());
        assert!(manager
            .delete_state(&context_id, KEY4.to_string())
            .unwrap()
            .is_some());

        // Creating a collection of keys to retrieve the values saved in Context or State.
        let keys = vec![
            KEY1.to_string(),
            KEY2.to_string(),
            KEY4.to_string(),
            KEY5.to_string(),
        ];
        let mut key_values = manager.get(&context_id, &keys).unwrap();
        // Two Values are found from the Keys list as KEY4 was deleted and KEY5 does not exist
        assert_eq!(key_values.len(), 2);
        assert_eq!(
            key_values.pop().unwrap(),
            (KEY1.to_string(), Some(VALUE1.to_string()))
        );
        assert_eq!(
            key_values.pop().unwrap(),
            (KEY2.to_string(), Some(VALUE2.to_string()))
        );
    }
}