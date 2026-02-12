use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub id: Uuid,
    pub ts: DateTime<Utc>,
    pub session_id: String,
    pub speaker: String,
    pub text: String,
    pub tags: Vec<String>,
    pub importance: f32,
    pub context_hash: Option<String>,
    pub source_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ConceptType {
    Fact,
    Preference,
    Rule,
    Entity,
    TaskState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ConceptStatus {
    Active,
    Stale,
    Conflicted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Concept {
    pub id: Uuid,
    pub concept_type: ConceptType,
    pub key: String,
    pub value: String,
    pub confidence: f32,
    pub evidence: Vec<String>,
    pub first_seen: DateTime<Utc>,
    pub last_verified: DateTime<Utc>,
    pub status: ConceptStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LinkRelation {
    Supports,
    Contradicts,
    Updates,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub id: Uuid,
    pub episode_id: Uuid,
    pub concept_id: Uuid,
    pub relation: LinkRelation,
    pub created_at: DateTime<Utc>,
}
