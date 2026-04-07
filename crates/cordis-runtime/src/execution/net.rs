//! Colored Petri Net model and validation.
//! This module defines the runtime net schema used by the execution engine.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

use crate::core::models::NodeOutcome;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CorrelationKey(pub String);

impl CorrelationKey {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn derive(execution_id: &str, transition_id: &str, logical_group: &str) -> Self {
        Self(format!("{execution_id}:{transition_id}:{logical_group}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenMeta {
    pub execution_id: String,
    pub transition_id: String,
    pub logical_group: String,
    pub sequence: u64,
    pub outcome: NodeOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Token {
    pub key: CorrelationKey,
    pub payload: Value,
    pub meta: TokenMeta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceSpec {
    pub place_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinPolicy {
    AllOf,
    AnyOf,
    Quorum(usize),
    FirstSuccess,
    FirstCompleted,
    KeyedPair,
    KeyedGroup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionSpec {
    pub transition_id: String,
    pub priority: i32,
    pub join_policy: JoinPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArcDirection {
    PlaceToTransition,
    TransitionToPlace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArcSpec {
    pub arc_id: String,
    pub place_id: String,
    pub transition_id: String,
    pub direction: ArcDirection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PetriNetSpec {
    pub places: Vec<PlaceSpec>,
    pub transitions: Vec<TransitionSpec>,
    pub arcs: Vec<ArcSpec>,
}

#[derive(Debug, Clone)]
pub struct PetriNetGraph {
    pub places: BTreeMap<String, PlaceSpec>,
    pub transitions: BTreeMap<String, TransitionSpec>,
    pub input_arcs_by_transition: BTreeMap<String, Vec<ArcSpec>>,
    pub output_arcs_by_transition: BTreeMap<String, Vec<ArcSpec>>,
    pub consumer_by_place: BTreeMap<String, String>,
    pub producer_transitions_by_place: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PetriNetBuildError {
    #[error("duplicate place id: {place_id}")]
    DuplicatePlaceId { place_id: String },

    #[error("duplicate transition id: {transition_id}")]
    DuplicateTransitionId { transition_id: String },

    #[error("duplicate arc id: {arc_id}")]
    DuplicateArcId { arc_id: String },

    #[error("arc references unknown place: arc={arc_id}, place={place_id}")]
    ArcPlaceNotFound { arc_id: String, place_id: String },

    #[error("arc references unknown transition: arc={arc_id}, transition={transition_id}")]
    ArcTransitionNotFound {
        arc_id: String,
        transition_id: String,
    },

    #[error("place has multiple consumer transitions: place={place_id}, consumers={consumers:?}")]
    PlaceMultipleConsumers {
        place_id: String,
        consumers: Vec<String>,
    },
}

pub fn build_petri_net(spec: PetriNetSpec) -> Result<PetriNetGraph, PetriNetBuildError> {
    let mut places = BTreeMap::<String, PlaceSpec>::new();
    for place in spec.places {
        if places.contains_key(&place.place_id) {
            return Err(PetriNetBuildError::DuplicatePlaceId {
                place_id: place.place_id,
            });
        }
        places.insert(place.place_id.clone(), place);
    }

    let mut transitions = BTreeMap::<String, TransitionSpec>::new();
    for transition in spec.transitions {
        if transitions.contains_key(&transition.transition_id) {
            return Err(PetriNetBuildError::DuplicateTransitionId {
                transition_id: transition.transition_id,
            });
        }
        transitions.insert(transition.transition_id.clone(), transition);
    }

    let mut arc_ids = BTreeSet::<String>::new();
    let mut input_arcs_by_transition = BTreeMap::<String, Vec<ArcSpec>>::new();
    let mut output_arcs_by_transition = BTreeMap::<String, Vec<ArcSpec>>::new();
    let mut consumer_set_by_place = BTreeMap::<String, BTreeSet<String>>::new();
    let mut producer_transitions_by_place = BTreeMap::<String, BTreeSet<String>>::new();

    for arc in spec.arcs {
        if !arc_ids.insert(arc.arc_id.clone()) {
            return Err(PetriNetBuildError::DuplicateArcId { arc_id: arc.arc_id });
        }
        if !places.contains_key(&arc.place_id) {
            return Err(PetriNetBuildError::ArcPlaceNotFound {
                arc_id: arc.arc_id,
                place_id: arc.place_id,
            });
        }
        if !transitions.contains_key(&arc.transition_id) {
            return Err(PetriNetBuildError::ArcTransitionNotFound {
                arc_id: arc.arc_id,
                transition_id: arc.transition_id,
            });
        }

        match arc.direction {
            ArcDirection::PlaceToTransition => {
                consumer_set_by_place
                    .entry(arc.place_id.clone())
                    .or_default()
                    .insert(arc.transition_id.clone());
                input_arcs_by_transition
                    .entry(arc.transition_id.clone())
                    .or_default()
                    .push(arc);
            }
            ArcDirection::TransitionToPlace => {
                producer_transitions_by_place
                    .entry(arc.place_id.clone())
                    .or_default()
                    .insert(arc.transition_id.clone());
                output_arcs_by_transition
                    .entry(arc.transition_id.clone())
                    .or_default()
                    .push(arc);
            }
        }
    }

    let mut consumer_by_place = BTreeMap::<String, String>::new();
    for (place_id, consumers) in consumer_set_by_place {
        if consumers.len() > 1 {
            return Err(PetriNetBuildError::PlaceMultipleConsumers {
                place_id,
                consumers: consumers.into_iter().collect(),
            });
        }
        if let Some(consumer) = consumers.into_iter().next() {
            consumer_by_place.insert(place_id, consumer);
        }
    }

    Ok(PetriNetGraph {
        places,
        transitions,
        input_arcs_by_transition,
        output_arcs_by_transition,
        consumer_by_place,
        producer_transitions_by_place,
    })
}
