//! Brain service: NATS handlers that let plugins interact with ArangoDB
//! via request/reply.
//!
//! Subscribes to `brain.*` subjects, executes operations against ArangoDB,
//! and publishes response events.

use std::time::Instant;

use anyhow::{Context, Result};
use chrono::Utc;
use futures::StreamExt;
use seidrum_common::events::{
    BrainQueryRequest, BrainQueryResponse, ContentStoreRequest, ContentStored,
    ConversationAppendRequest, ConversationAppendResponse, ConversationCreateRequest,
    ConversationCreateResponse, ConversationFindRequest, ConversationGetRequest,
    ConversationListRequest, ConversationListResponse, ConversationSummary, EntityUpsertRequest,
    EntityUpserted, EventEnvelope, FactUpsertRequest, FactUpserted, PreferencesQueryRequest,
    PreferencesQueryResponse, ScopeAssignRequest, ScopeAssigned, SkillGetRequest,
    SkillListRequest, SkillListResponse, SkillSaveRequest, SkillSaveResponse, SkillSearchRequest,
    SkillSearchResponse, SkillSearchResult, UserPreference,
};
use serde_json::Value;
use tracing::{debug, error, info, warn};

use super::client::ArangoClient;
use crate::embedding::service::EmbeddingService;
use crate::scope::service::ScopeService;

/// Long-lived service that handles brain NATS subjects.
pub struct BrainService {
    arango: ArangoClient,
    nats: async_nats::Client,
    scope_service: ScopeService,
    embedding: EmbeddingService,
}

impl BrainService {
    /// Create a new brain service.
    pub fn new(
        arango: ArangoClient,
        nats: async_nats::Client,
        embedding: EmbeddingService,
    ) -> Self {
        let scope_service = ScopeService::new(arango.clone());
        Self {
            arango,
            nats,
            scope_service,
            embedding,
        }
    }

    /// Start listening on all brain subjects. This runs forever.
    pub async fn run(self) -> Result<()> {
        // Subscribe to all brain subjects concurrently.
        let mut content_store = self
            .nats
            .subscribe("brain.content.store")
            .await
            .context("subscribe brain.content.store")?;
        let mut entity_upsert = self
            .nats
            .subscribe("brain.entity.upsert")
            .await
            .context("subscribe brain.entity.upsert")?;
        let mut fact_upsert = self
            .nats
            .subscribe("brain.fact.upsert")
            .await
            .context("subscribe brain.fact.upsert")?;
        let mut scope_assign = self
            .nats
            .subscribe("brain.scope.assign")
            .await
            .context("subscribe brain.scope.assign")?;
        let mut task_upsert = self
            .nats
            .subscribe("brain.task.upsert")
            .await
            .context("subscribe brain.task.upsert")?;
        let mut query_request = self
            .nats
            .subscribe("brain.query.request")
            .await
            .context("subscribe brain.query.request")?;
        let mut conv_create = self
            .nats
            .subscribe("brain.conversation.create")
            .await
            .context("subscribe brain.conversation.create")?;
        let mut conv_append = self
            .nats
            .subscribe("brain.conversation.append")
            .await
            .context("subscribe brain.conversation.append")?;
        let mut conv_get = self
            .nats
            .subscribe("brain.conversation.get")
            .await
            .context("subscribe brain.conversation.get")?;
        let mut conv_find = self
            .nats
            .subscribe("brain.conversation.find")
            .await
            .context("subscribe brain.conversation.find")?;
        let mut conv_list = self
            .nats
            .subscribe("brain.conversation.list")
            .await
            .context("subscribe brain.conversation.list")?;
        let mut skill_search = self
            .nats
            .subscribe("brain.skill.search")
            .await
            .context("subscribe brain.skill.search")?;
        let mut skill_save = self
            .nats
            .subscribe("brain.skill.save")
            .await
            .context("subscribe brain.skill.save")?;
        let mut skill_get = self
            .nats
            .subscribe("brain.skill.get")
            .await
            .context("subscribe brain.skill.get")?;
        let mut skill_list = self
            .nats
            .subscribe("brain.skill.list")
            .await
            .context("subscribe brain.skill.list")?;
        let mut skill_delete = self
            .nats
            .subscribe("brain.skill.delete")
            .await
            .context("subscribe brain.skill.delete")?;
        let mut preferences_query = self
            .nats
            .subscribe("agent.preferences.query")
            .await
            .context("subscribe agent.preferences.query")?;

        info!("Brain service started — listening on brain.* and agent.preferences.query subjects");

        loop {
            tokio::select! {
                Some(msg) = content_store.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    let embedding = self.embedding.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_content_store(&arango, &nats, &embedding, msg).await {
                            error!(error = %e, "brain.content.store handler failed");
                        }
                    });
                }
                Some(msg) = entity_upsert.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    let embedding = self.embedding.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_entity_upsert(&arango, &nats, &embedding, msg).await {
                            error!(error = %e, "brain.entity.upsert handler failed");
                        }
                    });
                }
                Some(msg) = fact_upsert.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_fact_upsert(&arango, &nats, msg).await {
                            error!(error = %e, "brain.fact.upsert handler failed");
                        }
                    });
                }
                Some(msg) = scope_assign.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_scope_assign(&arango, &nats, msg).await {
                            error!(error = %e, "brain.scope.assign handler failed");
                        }
                    });
                }
                Some(msg) = task_upsert.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_task_upsert(&arango, &nats, msg).await {
                            error!(error = %e, "brain.task.upsert handler failed");
                        }
                    });
                }
                Some(msg) = query_request.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    let scope_svc = self.scope_service.clone();
                    let embedding = self.embedding.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_query_request(&arango, &nats, &scope_svc, &embedding, msg).await {
                            error!(error = %e, "brain.query.request handler failed");
                        }
                    });
                }
                Some(msg) = conv_create.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conversation_create(&arango, &nats, msg).await {
                            error!(error = %e, "brain.conversation.create handler failed");
                        }
                    });
                }
                Some(msg) = conv_append.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conversation_append(&arango, &nats, msg).await {
                            error!(error = %e, "brain.conversation.append handler failed");
                        }
                    });
                }
                Some(msg) = conv_get.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conversation_get(&arango, &nats, msg).await {
                            error!(error = %e, "brain.conversation.get handler failed");
                        }
                    });
                }
                Some(msg) = conv_find.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conversation_find(&arango, &nats, msg).await {
                            error!(error = %e, "brain.conversation.find handler failed");
                        }
                    });
                }
                Some(msg) = conv_list.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_conversation_list(&arango, &nats, msg).await {
                            error!(error = %e, "brain.conversation.list handler failed");
                        }
                    });
                }
                Some(msg) = skill_search.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_skill_search(&arango, &nats, msg).await {
                            error!(error = %e, "brain.skill.search handler failed");
                        }
                    });
                }
                Some(msg) = skill_save.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_skill_save(&arango, &nats, msg).await {
                            error!(error = %e, "brain.skill.save handler failed");
                        }
                    });
                }
                Some(msg) = skill_get.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_skill_get(&arango, &nats, msg).await {
                            error!(error = %e, "brain.skill.get handler failed");
                        }
                    });
                }
                Some(msg) = skill_list.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_skill_list(&arango, &nats, msg).await {
                            error!(error = %e, "brain.skill.list handler failed");
                        }
                    });
                }
                Some(msg) = skill_delete.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_skill_delete(&arango, &nats, msg).await {
                            error!(error = %e, "brain.skill.delete handler failed");
                        }
                    });
                }
                Some(msg) = preferences_query.next() => {
                    let arango = self.arango.clone();
                    let nats = self.nats.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_preferences_query(&arango, &nats, msg).await {
                            error!(error = %e, "agent.preferences.query handler failed");
                        }
                    });
                }
                else => {
                    warn!("All brain subscriptions closed — exiting brain service");
                    break;
                }
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Parse the incoming message as an EventEnvelope, then deserialize the payload.
fn parse_envelope<T: serde::de::DeserializeOwned>(
    msg: &async_nats::Message,
) -> Result<(EventEnvelope, T)> {
    let envelope: EventEnvelope =
        serde_json::from_slice(&msg.payload).context("failed to parse EventEnvelope")?;
    let payload: T =
        serde_json::from_value(envelope.payload.clone()).context("failed to parse payload")?;
    Ok((envelope, payload))
}

/// Build a response envelope and publish it.
async fn publish_response<T: serde::Serialize>(
    nats: &async_nats::Client,
    subject: &str,
    correlation_id: Option<String>,
    scope: Option<String>,
    payload: &T,
) -> Result<()> {
    let envelope = EventEnvelope::new(subject, "kernel", correlation_id, scope, payload)
        .context("failed to build response envelope")?;
    let bytes = serde_json::to_vec(&envelope).context("failed to serialize response")?;
    nats.publish(subject.to_string(), bytes.into())
        .await
        .with_context(|| format!("failed to publish to {}", subject))?;
    debug!(subject, "published response event");
    Ok(())
}

/// Reply to a NATS request/reply message.
async fn reply_with<T: serde::Serialize>(
    nats: &async_nats::Client,
    reply_subject: &str,
    correlation_id: Option<String>,
    scope: Option<String>,
    payload: &T,
) -> Result<()> {
    let envelope = EventEnvelope::new(
        "brain.query.response",
        "kernel",
        correlation_id,
        scope,
        payload,
    )
    .context("failed to build reply envelope")?;
    let bytes = serde_json::to_vec(&envelope).context("failed to serialize reply")?;
    nats.publish(reply_subject.to_string(), bytes.into())
        .await
        .with_context(|| format!("failed to reply to {}", reply_subject))?;
    debug!(reply_subject, "sent reply");
    Ok(())
}

/// Generate a content key from timestamp and a ULID suffix.
fn generate_content_key(_content_type: &str) -> String {
    let now = Utc::now().format("%Y%m%d");
    let suffix = ulid::Ulid::new().to_string().to_lowercase();
    format!("content_{}_{}", now, &suffix[..8])
}

/// Generate an entity key from name.
fn generate_entity_key(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    format!("entity_{}", slug)
}

/// Generate a fact key.
fn generate_fact_key() -> String {
    let id = ulid::Ulid::new().to_string().to_lowercase();
    format!("fact_{}", &id[..12])
}

/// Generate a task key.
fn generate_task_key() -> String {
    let id = ulid::Ulid::new().to_string().to_lowercase();
    format!("task_{}", &id[..12])
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Handle `brain.content.store` — store content in ArangoDB.
async fn handle_content_store(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    embedding: &EmbeddingService,
    msg: async_nats::Message,
) -> Result<()> {
    let (envelope, req): (EventEnvelope, ContentStoreRequest) = parse_envelope(&msg)?;
    debug!(
        content_type = %req.content_type,
        channel = %req.channel,
        "handling brain.content.store"
    );

    let content_key = generate_content_key(&req.content_type);

    let doc = serde_json::json!({
        "_key": content_key,
        "type": req.content_type,
        "channel": req.channel,
        "channel_id": req.channel_id,
        "raw_text": req.raw_text,
        "timestamp": req.timestamp,
        "ingested_at": Utc::now(),
        "metadata": req.metadata,
    });

    arango
        .insert_document("content", &doc)
        .await
        .context("failed to insert content document")?;

    // Generate embedding if requested
    let mut embedding_generated = false;
    if req.generate_embedding {
        match embedding.embed(&req.raw_text).await {
            Ok(vector) => {
                // Update the document with the embedding
                let update = serde_json::json!({ "embedding": vector });
                match arango
                    .execute_aql(
                        "UPDATE { _key: @key } WITH @update IN content RETURN NEW",
                        &serde_json::json!({
                            "key": content_key,
                            "update": update,
                        }),
                    )
                    .await
                {
                    Ok(_) => {
                        embedding_generated = true;
                        debug!(content_key = %content_key, "Embedding generated and stored");
                    }
                    Err(e) => {
                        warn!(error = %e, "Failed to store embedding for content {}", content_key);
                    }
                }
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "Embedding generation failed for content {}, continuing without", content_key
                );
            }
        }
    }

    let response = ContentStored {
        content_key: content_key.clone(),
        content_type: req.content_type,
        channel: req.channel,
        embedding_generated,
        timestamp: req.timestamp,
    };

    publish_response(
        nats,
        "brain.content.stored",
        envelope.correlation_id,
        envelope.scope,
        &response,
    )
    .await?;

    info!(content_key, "content stored");
    Ok(())
}

/// Handle `brain.entity.upsert` — create or update an entity.
async fn handle_entity_upsert(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    embedding: &EmbeddingService,
    msg: async_nats::Message,
) -> Result<()> {
    let (envelope, req): (EventEnvelope, EntityUpsertRequest) = parse_envelope(&msg)?;
    debug!(name = %req.name, entity_type = %req.entity_type, "handling brain.entity.upsert");

    let entity_key = req
        .entity_key
        .clone()
        .unwrap_or_else(|| generate_entity_key(&req.name));

    let now = Utc::now();
    let doc = serde_json::json!({
        "type": req.entity_type,
        "name": req.name,
        "aliases": req.aliases,
        "properties": req.properties,
        "source": req.source_content.as_deref().unwrap_or("system"),
        "updated_at": now,
    });

    let (_result_doc, is_new) = arango
        .upsert_document("entities", &entity_key, &doc)
        .await
        .context("failed to upsert entity")?;

    // If the entity is new, also set created_at
    if is_new {
        let update_query = r#"
            UPDATE { _key: @key } WITH { created_at: @now } IN entities
        "#;
        let _ = arango
            .execute_aql(
                update_query,
                &serde_json::json!({ "key": entity_key, "now": now }),
            )
            .await;
    }

    // Generate embedding from entity name and properties
    let embedding_text = format!(
        "{} {}",
        req.name,
        if req.properties.is_empty() {
            String::new()
        } else {
            format!("{:?}", req.properties)
        }
    );

    match embedding.embed(&embedding_text).await {
        Ok(vector) => {
            // Update the entity with the embedding
            let update = serde_json::json!({ "embedding": vector });
            match arango
                .execute_aql(
                    "UPDATE { _key: @key } WITH @update IN entities RETURN NEW",
                    &serde_json::json!({
                        "key": entity_key,
                        "update": update,
                    }),
                )
                .await
            {
                Ok(_) => {
                    debug!(entity_key = %entity_key, "Embedding generated and stored for entity");
                }
                Err(e) => {
                    warn!(error = %e, "Failed to store embedding for entity {}", entity_key);
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "Embedding generation failed for entity {}", entity_key);
        }
    }

    // Create mentions edge if requested
    if let Some(content_key) = &req.mentions_content {
        let mention_type = req.mention_type.as_deref().unwrap_or("direct");
        let edge_data = serde_json::json!({
            "mention_type": mention_type,
        });
        let from = format!("content/{}", content_key);
        let to = format!("entities/{}", entity_key);
        if let Err(e) = arango.insert_edge("mentions", &from, &to, &edge_data).await {
            warn!(error = %e, "failed to create mentions edge (may already exist)");
        }
    }

    let response = EntityUpserted {
        entity_key: entity_key.clone(),
        entity_type: req.entity_type,
        name: req.name,
        is_new,
        source_content: req.source_content,
    };

    publish_response(
        nats,
        "brain.entity.upserted",
        envelope.correlation_id,
        envelope.scope,
        &response,
    )
    .await?;

    info!(entity_key, is_new, "entity upserted");
    Ok(())
}

/// Handle `brain.fact.upsert` — create or update a fact.
async fn handle_fact_upsert(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    msg: async_nats::Message,
) -> Result<()> {
    let (envelope, req): (EventEnvelope, FactUpsertRequest) = parse_envelope(&msg)?;
    debug!(
        subject = %req.subject,
        predicate = %req.predicate,
        "handling brain.fact.upsert"
    );

    let now = Utc::now();

    // Check for existing fact with same subject+predicate that should be superseded.
    let existing_query = r#"
        FOR fact IN facts
            FILTER fact.subject == @subject
               AND fact.predicate == @predicate
               AND fact.valid_to == null
               AND fact.superseded_by == null
            LIMIT 1
            RETURN fact
    "#;
    let existing_result = arango
        .execute_aql(
            existing_query,
            &serde_json::json!({
                "subject": &req.subject,
                "predicate": &req.predicate,
            }),
        )
        .await
        .context("failed to query existing facts")?;

    let existing_fact = existing_result
        .get("result")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .cloned();

    let mut superseded_fact_key: Option<String> = None;
    let is_new: bool;
    let fact_key: String;

    if let Some(old_fact) = existing_fact {
        let old_key = old_fact.get("_key").and_then(|v| v.as_str()).unwrap_or("");

        // Check if the value/object is different — if same, just reinforce
        let old_object = old_fact.get("object").and_then(|v| v.as_str());
        let old_value = old_fact.get("value").and_then(|v| v.as_str());

        let same_content = old_object == req.object.as_deref() && old_value == req.value.as_deref();

        if same_content {
            // Reinforce existing fact
            let reinforce_query = r#"
                UPDATE { _key: @key } WITH {
                    last_reinforced: @now,
                    reinforcement_count: (
                        FOR f IN facts FILTER f._key == @key RETURN f.reinforcement_count
                    )[0] + 1,
                    confidence: @confidence
                } IN facts
                RETURN NEW
            "#;
            let _ = arango
                .execute_aql(
                    reinforce_query,
                    &serde_json::json!({
                        "key": old_key,
                        "now": now,
                        "confidence": req.confidence,
                    }),
                )
                .await;

            fact_key = old_key.to_string();
            is_new = false;
        } else {
            // Supersede old fact, create new one
            fact_key = generate_fact_key();
            is_new = true;
            superseded_fact_key = Some(old_key.to_string());

            // Mark old fact as superseded
            let supersede_query = r#"
                UPDATE { _key: @old_key } WITH {
                    superseded_by: @new_key,
                    valid_to: @now
                } IN facts
            "#;
            let _ = arango
                .execute_aql(
                    supersede_query,
                    &serde_json::json!({
                        "old_key": old_key,
                        "new_key": &fact_key,
                        "now": now,
                    }),
                )
                .await;

            // Create supersedes edge
            let edge_data = serde_json::json!({
                "reason": "updated via brain.fact.upsert",
                "superseded_at": now,
            });
            let from = format!("facts/{}", fact_key);
            let to = format!("facts/{}", old_key);
            let _ = arango
                .insert_edge("supersedes", &from, &to, &edge_data)
                .await;

            // Insert new fact
            let fact_doc = serde_json::json!({
                "_key": fact_key,
                "subject": req.subject,
                "predicate": req.predicate,
                "object": req.object,
                "value": req.value,
                "confidence": req.confidence,
                "source_content": req.source_content,
                "valid_from": req.valid_from.unwrap_or(now),
                "valid_to": null,
                "superseded_by": null,
                "last_reinforced": now,
                "reinforcement_count": 0,
                "extraction_method": "plugin",
                "created_at": now,
            });
            arango
                .insert_document("facts", &fact_doc)
                .await
                .context("failed to insert new fact")?;

            // Create derived_from edge
            let derived_edge = serde_json::json!({
                "extraction_method": "plugin",
                "extraction_confidence": req.confidence,
                "extracted_at": now,
            });
            let from = format!("facts/{}", fact_key);
            let to = format!("content/{}", req.source_content);
            let _ = arango
                .insert_edge("derived_from", &from, &to, &derived_edge)
                .await;
        }
    } else {
        // Brand new fact
        fact_key = generate_fact_key();
        is_new = true;

        let fact_doc = serde_json::json!({
            "_key": fact_key,
            "subject": req.subject,
            "predicate": req.predicate,
            "object": req.object,
            "value": req.value,
            "confidence": req.confidence,
            "source_content": req.source_content,
            "valid_from": req.valid_from.unwrap_or(now),
            "valid_to": null,
            "superseded_by": null,
            "last_reinforced": now,
            "reinforcement_count": 0,
            "extraction_method": "plugin",
            "created_at": now,
        });
        arango
            .insert_document("facts", &fact_doc)
            .await
            .context("failed to insert fact")?;

        // Create derived_from edge
        let derived_edge = serde_json::json!({
            "extraction_method": "plugin",
            "extraction_confidence": req.confidence,
            "extracted_at": now,
        });
        let from = format!("facts/{}", fact_key);
        let to = format!("content/{}", req.source_content);
        let _ = arango
            .insert_edge("derived_from", &from, &to, &derived_edge)
            .await;
    }

    let response = FactUpserted {
        fact_key: fact_key.clone(),
        subject: req.subject,
        predicate: req.predicate,
        is_new,
        superseded_fact: superseded_fact_key,
    };

    publish_response(
        nats,
        "brain.fact.upserted",
        envelope.correlation_id,
        envelope.scope,
        &response,
    )
    .await?;

    info!(fact_key, is_new, "fact upserted");
    Ok(())
}

/// Handle `brain.scope.assign` — create a scoped_to edge.
async fn handle_scope_assign(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    msg: async_nats::Message,
) -> Result<()> {
    let (envelope, req): (EventEnvelope, ScopeAssignRequest) = parse_envelope(&msg)?;
    debug!(
        target_key = %req.target_key,
        scope_key = %req.scope_key,
        "handling brain.scope.assign"
    );

    let edge_data = serde_json::json!({
        "relevance": req.relevance,
        "added_at": Utc::now(),
        "added_by": envelope.source,
    });

    // target_key should be a full _id like "entities/entity_foo" or just a key.
    // Normalize: if it doesn't contain '/', try to detect collection.
    let from = if req.target_key.contains('/') {
        req.target_key.clone()
    } else {
        // Attempt to find which collection the key belongs to
        detect_collection_id(arango, &req.target_key).await?
    };

    let to = if req.scope_key.contains('/') {
        req.scope_key.clone()
    } else {
        format!("scopes/{}", req.scope_key)
    };

    arango
        .insert_edge("scoped_to", &from, &to, &edge_data)
        .await
        .context("failed to create scoped_to edge")?;

    let response = ScopeAssigned {
        target_key: req.target_key,
        scope_key: req.scope_key,
    };

    publish_response(
        nats,
        "brain.scope.assigned",
        envelope.correlation_id,
        envelope.scope,
        &response,
    )
    .await?;

    info!("scope assigned");
    Ok(())
}

/// Try to detect which collection a bare key belongs to.
async fn detect_collection_id(arango: &ArangoClient, key: &str) -> Result<String> {
    // Try common collections in order
    for collection in &["entities", "content", "facts", "tasks"] {
        if let Ok(Some(_)) = arango.get_document(collection, key).await {
            return Ok(format!("{}/{}", collection, key));
        }
    }
    // Default to entities if not found (the insert might fail, but that is
    // the caller's problem).
    Ok(format!("entities/{}", key))
}

/// Handle `brain.task.upsert` — create or update a task.
///
/// Expects a JSON payload with at minimum: title, status, priority.
/// Optional: task_key (for update), assigned_agent, due_date, etc.
async fn handle_task_upsert(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    msg: async_nats::Message,
) -> Result<()> {
    let (envelope, payload): (EventEnvelope, Value) = parse_envelope(&msg)?;
    debug!("handling brain.task.upsert");

    let now = Utc::now();
    let task_key = payload
        .get("task_key")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(generate_task_key);

    let title = payload
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("Untitled task");
    let status = payload
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("open");
    let priority = payload
        .get("priority")
        .and_then(|v| v.as_str())
        .unwrap_or("medium");

    let task_doc = serde_json::json!({
        "title": title,
        "status": status,
        "priority": priority,
        "assigned_agent": payload.get("assigned_agent"),
        "due_date": payload.get("due_date"),
        "callback_channel": payload.get("callback_channel"),
        "context": payload.get("context"),
        "updated_at": now,
    });

    let (_, is_new) = arango
        .upsert_document("tasks", &task_key, &task_doc)
        .await
        .context("failed to upsert task")?;

    if is_new {
        let update_query = r#"
            UPDATE { _key: @key } WITH { created_at: @now } IN tasks
        "#;
        let _ = arango
            .execute_aql(
                update_query,
                &serde_json::json!({ "key": &task_key, "now": now }),
            )
            .await;
    }

    // Publish appropriate event
    let response_subject = if is_new {
        "brain.task.created"
    } else {
        "brain.task.upserted"
    };

    let response = serde_json::json!({
        "task_key": task_key,
        "title": title,
        "status": status,
        "is_new": is_new,
    });

    publish_response(
        nats,
        response_subject,
        envelope.correlation_id,
        envelope.scope,
        &response,
    )
    .await?;

    info!(task_key, is_new, "task upserted");
    Ok(())
}

/// Handle `brain.query.request` — execute a query and reply.
///
/// Uses NATS request/reply: the response is sent to the message's reply
/// subject (if present), otherwise published to `brain.query.response`.
async fn handle_query_request(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    scope_svc: &ScopeService,
    embedding: &EmbeddingService,
    msg: async_nats::Message,
) -> Result<()> {
    let (envelope, req): (EventEnvelope, BrainQueryRequest) = parse_envelope(&msg)?;
    debug!(query_type = %req.query_type, "handling brain.query.request");

    let start = Instant::now();

    // Resolve the accessible scopes from the envelope's scope field.
    let resolved_scopes = if let Some(scope) = envelope.scope.as_deref() {
        match scope_svc.resolve_scopes(scope, &[]).await {
            Ok(resolved) => {
                debug!(
                    primary = scope,
                    count = resolved.allowed.len(),
                    "scope enforcement: resolved accessible scopes"
                );
                Some(resolved)
            }
            Err(e) => {
                warn!(error = %e, "failed to resolve scopes, proceeding without enforcement");
                None
            }
        }
    } else {
        None
    };

    let scopes_applied: Vec<String> = resolved_scopes
        .as_ref()
        .map(|r| r.allowed.iter().cloned().collect())
        .unwrap_or_default();

    let result = match req.query_type.as_str() {
        "aql" => handle_aql_query(arango, &req, &envelope, resolved_scopes.as_ref()).await,
        "vector_search" => handle_vector_search(arango, embedding, &req, scope_svc, &envelope).await,
        "hybrid_search" => handle_hybrid_search(arango, embedding, &req, scope_svc, &envelope).await,
        "graph_traverse" => handle_graph_traverse(arango, &req, &envelope).await,
        "get_facts" => handle_get_facts(arango, &req, &envelope).await,
        "get_context" => handle_get_context(arango, &req, &envelope).await,
        other => {
            warn!(query_type = other, "unknown query_type");
            Ok(serde_json::json!({
                "error": format!("unknown query_type: {}", other),
            }))
        }
    };

    let duration_ms = start.elapsed().as_millis() as u64;

    let (results, count) = match result {
        Ok(val) => {
            let count = if let Some(arr) = val.as_array() {
                arr.len() as u32
            } else {
                1
            };
            (val, count)
        }
        Err(e) => {
            error!(error = %e, "query execution failed");
            (serde_json::json!({ "error": e.to_string() }), 0)
        }
    };

    let response = BrainQueryResponse {
        results,
        count,
        scopes_applied,
        duration_ms,
    };

    // Use reply subject if available (request/reply pattern), otherwise publish
    if let Some(reply_subject) = msg.reply {
        reply_with(
            nats,
            reply_subject.as_str(),
            envelope.correlation_id,
            envelope.scope,
            &response,
        )
        .await?;
    } else {
        publish_response(
            nats,
            "brain.query.response",
            envelope.correlation_id,
            envelope.scope,
            &response,
        )
        .await?;
    }

    debug!(duration_ms, "query completed");
    Ok(())
}

// ---------------------------------------------------------------------------
// Query type handlers
// ---------------------------------------------------------------------------

/// Execute a raw AQL query with bind vars, applying scope enforcement.
async fn handle_aql_query(
    arango: &ArangoClient,
    req: &BrainQueryRequest,
    _envelope: &EventEnvelope,
    resolved_scopes: Option<&crate::scope::service::ResolvedScopes>,
) -> Result<Value> {
    let aql = req
        .aql
        .as_deref()
        .context("aql field is required for query_type 'aql'")?;

    let bind_vars = req
        .bind_vars
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .context("failed to serialize bind_vars")?
        .unwrap_or(serde_json::json!({}));

    // Inject scope filtering if scopes were resolved.
    let (final_aql, final_vars) = if let Some(scopes) = resolved_scopes {
        // Use a temporary ScopeService just for the filter injection (it is
        // a pure function that does not hit the database).
        let scope_svc = ScopeService::new(arango.clone());
        let (wrapped, merged) = scope_svc.inject_scope_filter(aql, &bind_vars, &scopes.allowed);
        debug!("scope enforcement: AQL query wrapped with scope filter");
        (wrapped, merged)
    } else {
        debug!("scope enforcement: no scope specified, query runs unfiltered");
        (aql.to_string(), bind_vars)
    };

    let resp = arango
        .execute_aql(&final_aql, &final_vars)
        .await
        .context("AQL execution failed")?;

    Ok(resp.get("result").cloned().unwrap_or(serde_json::json!([])))
}

/// Perform vector similarity search in ArangoDB with scope enforcement.
async fn handle_vector_search(
    arango: &ArangoClient,
    embedding: &EmbeddingService,
    req: &BrainQueryRequest,
    scope_svc: &ScopeService,
    envelope: &EventEnvelope,
) -> Result<Value> {
    let collection = req.collection.as_deref().unwrap_or("content");
    let limit = req.limit.unwrap_or(10);

    debug!(collection = %collection, limit = limit, "handling vector_search");

    // Get or generate the query embedding
    let query_embedding = if let Some(vec) = &req.embedding {
        vec.clone()
    } else if let Some(query_text) = &req.query_text {
        match embedding.embed(query_text).await {
            Ok(vec) => vec,
            Err(e) => {
                warn!(error = %e, "failed to generate embedding for query text");
                return Ok(serde_json::json!([]));
            }
        }
    } else {
        warn!("vector_search requires either 'embedding' or 'query_text' field");
        return Ok(serde_json::json!([]));
    };

    // Resolve scope for filtering
    let scope_filter = if let Some(scope) = envelope.scope.as_deref() {
        match scope_svc.resolve_scopes(scope, &[]).await {
            Ok(resolved) => {
                debug!(
                    primary = scope,
                    count = resolved.allowed.len(),
                    "vector_search scope enforcement: resolved scopes"
                );
                Some(resolved.allowed)
            }
            Err(e) => {
                warn!(error = %e, "failed to resolve scopes for vector_search");
                None
            }
        }
    } else {
        None
    };

    // Build AQL with optional scope filtering via scoped_to edges
    let aql = if scope_filter.is_some() {
        format!(
            r#"FOR doc IN {collection}
                LET distance = COSINE_SIMILARITY(doc.embedding, @query_embedding)
                FILTER distance != null
                LET doc_scopes = (
                    FOR s IN 1..1 OUTBOUND doc._id scoped_to
                        RETURN s._key
                )
                FILTER LENGTH(doc_scopes) == 0 OR LENGTH(INTERSECTION(doc_scopes, @allowed_scopes)) > 0
                SORT distance DESC
                LIMIT @limit
                RETURN MERGE(doc, {{ _similarity: distance }})"#,
            collection = collection
        )
    } else {
        format!(
            r#"FOR doc IN {collection}
                LET distance = COSINE_SIMILARITY(doc.embedding, @query_embedding)
                FILTER distance != null
                SORT distance DESC
                LIMIT @limit
                RETURN MERGE(doc, {{ _similarity: distance }})"#,
            collection = collection
        )
    };

    let bind_vars = if let Some(ref scopes) = scope_filter {
        serde_json::json!({
            "query_embedding": query_embedding,
            "limit": limit,
            "allowed_scopes": scopes,
        })
    } else {
        serde_json::json!({
            "query_embedding": query_embedding,
            "limit": limit,
        })
    };

    match arango.execute_aql(&aql, &bind_vars).await {
        Ok(resp) => {
            let results = resp.get("result").cloned().unwrap_or(serde_json::json!([]));
            debug!(
                count = if let Some(arr) = results.as_array() {
                    arr.len()
                } else {
                    0
                },
                "vector search completed"
            );
            Ok(results)
        }
        Err(e) => {
            warn!(error = %e, "vector search query failed");
            Ok(serde_json::json!([]))
        }
    }
}

/// Hybrid search: combines vector similarity search with graph traversal.
///
/// This performs:
/// 1. Vector search to find semantically similar content/entities (scope-filtered)
/// 2. For top results, traverse the graph to find connected entities and facts
/// 3. Merge and deduplicate results, returning both direct matches and graph neighbors
async fn handle_hybrid_search(
    arango: &ArangoClient,
    embedding: &EmbeddingService,
    req: &BrainQueryRequest,
    scope_svc: &ScopeService,
    envelope: &EventEnvelope,
) -> Result<Value> {
    let collection = req.collection.as_deref().unwrap_or("content");
    let vector_limit = req.limit.unwrap_or(5);
    let graph_depth = req.depth.unwrap_or(2);

    debug!(collection = %collection, vector_limit = vector_limit, "handling hybrid_search");

    // Get or generate the query embedding
    let query_embedding = if let Some(vec) = &req.embedding {
        // Use provided embedding directly
        vec.clone()
    } else if let Some(query_text) = &req.query_text {
        // Generate embedding from query text
        match embedding.embed(query_text).await {
            Ok(vec) => vec,
            Err(e) => {
                warn!(error = %e, "failed to generate embedding for query text");
                return Ok(serde_json::json!([]));
            }
        }
    } else {
        warn!("hybrid_search requires either 'embedding' or 'query_text' field");
        return Ok(serde_json::json!([]));
    };

    // Resolve scope for filtering
    let scope_filter = if let Some(scope) = envelope.scope.as_deref() {
        match scope_svc.resolve_scopes(scope, &[]).await {
            Ok(resolved) => {
                debug!(
                    primary = scope,
                    count = resolved.allowed.len(),
                    "hybrid_search scope enforcement: resolved scopes"
                );
                Some(resolved.allowed)
            }
            Err(e) => {
                warn!(error = %e, "failed to resolve scopes for hybrid_search");
                None
            }
        }
    } else {
        None
    };

    // Step 1: Vector search with scope enforcement
    // Collection name is validated against an allowlist to prevent injection
    let valid_collections = ["content", "entities", "facts", "tasks", "skills"];
    let safe_collection = if valid_collections.contains(&collection) {
        collection
    } else {
        warn!(collection = %collection, "invalid collection for hybrid_search, defaulting to 'content'");
        "content"
    };

    let vector_aql = if scope_filter.is_some() {
        format!(
            r#"FOR doc IN {safe_collection}
                LET distance = COSINE_SIMILARITY(doc.embedding, @query_embedding)
                FILTER distance != null
                LET doc_scopes = (
                    FOR s IN 1..1 OUTBOUND doc._id scoped_to
                        RETURN s._key
                )
                FILTER LENGTH(doc_scopes) == 0 OR LENGTH(INTERSECTION(doc_scopes, @allowed_scopes)) > 0
                SORT distance DESC
                LIMIT @limit
                RETURN {{ doc, distance, source: "vector_match" }}"#,
            safe_collection = safe_collection
        )
    } else {
        format!(
            r#"FOR doc IN {safe_collection}
                LET distance = COSINE_SIMILARITY(doc.embedding, @query_embedding)
                FILTER distance != null
                SORT distance DESC
                LIMIT @limit
                RETURN {{ doc, distance, source: "vector_match" }}"#,
            safe_collection = safe_collection
        )
    };

    let vector_vars = if let Some(ref scopes) = scope_filter {
        serde_json::json!({
            "query_embedding": query_embedding,
            "limit": vector_limit,
            "allowed_scopes": scopes,
        })
    } else {
        serde_json::json!({
            "query_embedding": query_embedding,
            "limit": vector_limit,
        })
    };

    let mut all_results: Vec<Value> = Vec::new();
    let mut seen_keys: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Execute vector search
    if let Ok(resp) = arango.execute_aql(&vector_aql, &vector_vars).await {
        if let Some(vector_results) = resp.get("result").and_then(|v| v.as_array()) {
            for item in vector_results {
                if let Some(doc) = item.get("doc") {
                    if let Some(key) = doc.get("_key").and_then(|k| k.as_str()) {
                        seen_keys.insert(key.to_string());
                        all_results.push(item.clone());

                        // Step 2: Graph traversal from this document (parameterized)
                        let traverse_aql = r#"FOR v, e, p IN 1..@depth ANY @start_vertex
                                GRAPH "brain"
                                RETURN { vertex: v, edge: e, depth: LENGTH(p.vertices) - 1, source: "graph_neighbor" }"#;

                        let traverse_vars = serde_json::json!({
                            "start_vertex": format!("{}/{}", safe_collection, key),
                            "depth": graph_depth,
                        });

                        if let Ok(traverse_resp) = arango.execute_aql(traverse_aql, &traverse_vars).await {
                            if let Some(neighbors) = traverse_resp.get("result").and_then(|v| v.as_array()) {
                                for neighbor in neighbors {
                                    if let Some(vertex) = neighbor.get("vertex") {
                                        if let Some(neighbor_key) = vertex.get("_key").and_then(|k| k.as_str()) {
                                            if !seen_keys.contains(neighbor_key) {
                                                seen_keys.insert(neighbor_key.to_string());
                                                all_results.push(neighbor.clone());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    } else {
        warn!("hybrid_search vector phase failed");
    }

    debug!(
        total_results = all_results.len(),
        vector_matches = vector_limit as usize,
        "hybrid search completed"
    );

    Ok(serde_json::json!(all_results))
}

/// Execute a graph traversal query.
async fn handle_graph_traverse(
    arango: &ArangoClient,
    req: &BrainQueryRequest,
    _envelope: &EventEnvelope,
) -> Result<Value> {
    let start_vertex = req
        .start_vertex
        .as_deref()
        .context("start_vertex is required for query_type 'graph_traverse'")?;
    let direction = req.direction.as_deref().unwrap_or("any");
    let depth = req.depth.unwrap_or(2);

    let direction_keyword = match direction {
        "outbound" => "OUTBOUND",
        "inbound" => "INBOUND",
        _ => "ANY",
    };

    let aql = format!(
        r#"FOR v, e, p IN 1..@depth {direction} @start
             GRAPH "brain"
             RETURN {{ vertex: v, edge: e }}"#,
        direction = direction_keyword,
    );

    let bind_vars = serde_json::json!({
        "start": start_vertex,
        "depth": depth,
    });

    let resp = arango
        .execute_aql(&aql, &bind_vars)
        .await
        .context("graph traversal failed")?;

    Ok(resp.get("result").cloned().unwrap_or(serde_json::json!([])))
}

/// Get current facts for an entity.
async fn handle_get_facts(
    arango: &ArangoClient,
    req: &BrainQueryRequest,
    _envelope: &EventEnvelope,
) -> Result<Value> {
    let start_vertex = req
        .start_vertex
        .as_deref()
        .context("start_vertex is required for query_type 'get_facts'")?;
    let min_confidence = req.min_confidence.unwrap_or(0.5);
    let limit = req.limit.unwrap_or(50);

    // start_vertex can be an entity _id ("entities/entity_luis") or _key
    let entity_id = if start_vertex.contains('/') {
        start_vertex.to_string()
    } else {
        format!("entities/{}", start_vertex)
    };

    let aql = r#"
        FOR fact IN facts
            FILTER fact.subject == @entity_id
               AND fact.valid_to == null
               AND fact.superseded_by == null
               AND fact.confidence >= @min_confidence
            SORT fact.confidence DESC
            LIMIT @limit
            RETURN fact
    "#;

    let bind_vars = serde_json::json!({
        "entity_id": entity_id,
        "min_confidence": min_confidence,
        "limit": limit,
    });

    let resp = arango
        .execute_aql(aql, &bind_vars)
        .await
        .context("get_facts query failed")?;

    Ok(resp.get("result").cloned().unwrap_or(serde_json::json!([])))
}

/// High-level convenience query: get entities, facts, and related content
/// for a scope. This is used by the graph context loader plugin.
async fn handle_get_context(
    arango: &ArangoClient,
    req: &BrainQueryRequest,
    envelope: &EventEnvelope,
) -> Result<Value> {
    let max_facts = req.max_facts.unwrap_or(50);
    let _graph_depth = req.graph_depth.unwrap_or(2);
    let min_confidence = req.min_confidence.unwrap_or(0.5);

    let scope = envelope.scope.as_deref().unwrap_or("scope_root");

    // Get entities in scope with their current facts
    let aql = r#"
        LET scope_entities = (
            FOR v, e IN 1..1 INBOUND @scope_key scoped_to
                FILTER IS_SAME_COLLECTION("entities", v)
                RETURN v
        )
        FOR entity IN scope_entities
            LET current_facts = (
                FOR fact IN facts
                    FILTER fact.subject == entity._id
                       AND fact.valid_to == null
                       AND fact.superseded_by == null
                       AND fact.confidence >= @min_confidence
                    SORT fact.confidence DESC
                    LIMIT @max_facts_per_entity
                    RETURN fact
            )
            RETURN { entity: entity, facts: current_facts }
    "#;

    let scope_id = if scope.contains('/') {
        scope.to_string()
    } else {
        format!("scopes/{}", scope)
    };

    let bind_vars = serde_json::json!({
        "scope_key": scope_id,
        "min_confidence": min_confidence,
        "max_facts_per_entity": max_facts,
    });

    let resp = arango
        .execute_aql(aql, &bind_vars)
        .await
        .context("get_context query failed")?;

    Ok(resp.get("result").cloned().unwrap_or(serde_json::json!([])))
}

// ---------------------------------------------------------------------------
// Conversation handlers
// ---------------------------------------------------------------------------

/// Create a new conversation document.
async fn handle_conversation_create(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    msg: async_nats::Message,
) -> Result<()> {
    let req: ConversationCreateRequest =
        serde_json::from_slice(&msg.payload).context("parse conversation.create")?;

    let conv_id = ulid::Ulid::new().to_string();
    let now = Utc::now();

    let doc = serde_json::json!({
        "_key": &conv_id,
        "platform": &req.platform,
        "participants": &req.participants,
        "agent_id": &req.agent_id,
        "scope": &req.scope,
        "messages": [],
        "metadata": &req.metadata,
        "state": "active",
        "created_at": now.to_rfc3339(),
        "updated_at": now.to_rfc3339(),
    });

    arango
        .insert_document("conversations", &doc)
        .await
        .context("insert conversation")?;

    debug!(conversation_id = %conv_id, "Conversation created");

    if let Some(reply) = msg.reply {
        let resp = ConversationCreateResponse {
            conversation_id: conv_id,
        };
        let _ = nats.publish(reply, serde_json::to_vec(&resp)?.into()).await;
    }

    Ok(())
}

/// Append a message to an existing conversation.
async fn handle_conversation_append(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    msg: async_nats::Message,
) -> Result<()> {
    let req: ConversationAppendRequest =
        serde_json::from_slice(&msg.payload).context("parse conversation.append")?;

    let now = Utc::now();

    let query = r#"
        LET conv = DOCUMENT(CONCAT("conversations/", @conv_id))
        FILTER conv != null
        UPDATE conv WITH {
            messages: APPEND(conv.messages, [@message]),
            updated_at: @now
        } IN conversations
        RETURN { message_count: LENGTH(NEW.messages) }
    "#;

    let message_json = serde_json::to_value(&req.message)?;
    let bind_vars = serde_json::json!({
        "conv_id": &req.conversation_id,
        "message": message_json,
        "now": now.to_rfc3339(),
    });

    let resp = arango.execute_aql(query, &bind_vars).await?;
    let count = resp
        .get("result")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.get("message_count"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    if let Some(reply) = msg.reply {
        let resp = ConversationAppendResponse {
            success: count > 0,
            message_count: count,
        };
        let _ = nats.publish(reply, serde_json::to_vec(&resp)?.into()).await;
    }

    Ok(())
}

/// Get a conversation by ID.
async fn handle_conversation_get(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    msg: async_nats::Message,
) -> Result<()> {
    let req: ConversationGetRequest =
        serde_json::from_slice(&msg.payload).context("parse conversation.get")?;

    let doc = arango
        .get_document("conversations", &req.conversation_id)
        .await?;

    if let Some(reply) = msg.reply {
        let resp = match doc {
            Some(mut d) => {
                // Trim messages if max_messages is set
                if req.max_messages > 0 {
                    if let Some(messages) = d.get("messages").and_then(|v| v.as_array()) {
                        let len = messages.len();
                        let skip = len.saturating_sub(req.max_messages as usize);
                        let trimmed: Vec<_> = messages.iter().skip(skip).cloned().collect();
                        d["messages"] = serde_json::Value::Array(trimmed);
                    }
                }
                // Map _key to id
                if let Some(key) = d.get("_key").and_then(|v| v.as_str()) {
                    d["id"] = serde_json::Value::String(key.to_string());
                }
                d
            }
            None => serde_json::json!(null),
        };
        let _ = nats.publish(reply, serde_json::to_vec(&resp)?.into()).await;
    }

    Ok(())
}

/// Find a conversation by platform metadata.
async fn handle_conversation_find(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    msg: async_nats::Message,
) -> Result<()> {
    let req: ConversationFindRequest =
        serde_json::from_slice(&msg.payload).context("parse conversation.find")?;

    let query = r#"
        FOR conv IN conversations
            FILTER conv.agent_id == @agent_id
            FILTER conv.platform == @platform
            FILTER conv.metadata[@meta_key] == @meta_value
            FILTER conv.state == "active"
            SORT conv.updated_at DESC
            LIMIT 1
            RETURN MERGE(conv, { id: conv._key })
    "#;

    let bind_vars = serde_json::json!({
        "agent_id": &req.agent_id,
        "platform": &req.platform,
        "meta_key": &req.metadata_key,
        "meta_value": &req.metadata_value,
    });

    let resp = arango.execute_aql(query, &bind_vars).await?;
    let result = resp
        .get("result")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(serde_json::json!(null));

    if let Some(reply) = msg.reply {
        let _ = nats
            .publish(reply, serde_json::to_vec(&result)?.into())
            .await;
    }

    Ok(())
}

/// List conversations for an agent.
async fn handle_conversation_list(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    msg: async_nats::Message,
) -> Result<()> {
    let req: ConversationListRequest =
        serde_json::from_slice(&msg.payload).context("parse conversation.list")?;

    let limit = if req.limit == 0 { 20 } else { req.limit };

    let agent_id_empty = req.agent_id.is_empty();

    let (query, bind_vars) = if let Some(ref platform) = req.platform {
        if agent_id_empty {
            (
                r#"
                FOR conv IN conversations
                    FILTER conv.platform == @platform
                    SORT conv.updated_at DESC
                    LIMIT @limit
                    RETURN {
                        id: conv._key,
                        platform: conv.platform,
                        participants: conv.participants,
                        message_count: LENGTH(conv.messages),
                        state: conv.state,
                        updated_at: conv.updated_at
                    }
            "#,
                serde_json::json!({
                    "platform": platform,
                    "limit": limit,
                }),
            )
        } else {
            (
                r#"
                FOR conv IN conversations
                    FILTER conv.agent_id == @agent_id
                    FILTER conv.platform == @platform
                    SORT conv.updated_at DESC
                    LIMIT @limit
                    RETURN {
                        id: conv._key,
                        platform: conv.platform,
                        participants: conv.participants,
                        message_count: LENGTH(conv.messages),
                        state: conv.state,
                        updated_at: conv.updated_at
                    }
            "#,
                serde_json::json!({
                    "agent_id": &req.agent_id,
                    "platform": platform,
                    "limit": limit,
                }),
            )
        }
    } else if agent_id_empty {
        (
            r#"
                FOR conv IN conversations
                    SORT conv.updated_at DESC
                    LIMIT @limit
                    RETURN {
                        id: conv._key,
                        platform: conv.platform,
                        participants: conv.participants,
                        message_count: LENGTH(conv.messages),
                        state: conv.state,
                        updated_at: conv.updated_at
                    }
            "#,
            serde_json::json!({
                "limit": limit,
            }),
        )
    } else {
        (
            r#"
                FOR conv IN conversations
                    FILTER conv.agent_id == @agent_id
                    SORT conv.updated_at DESC
                    LIMIT @limit
                    RETURN {
                        id: conv._key,
                        platform: conv.platform,
                        participants: conv.participants,
                        message_count: LENGTH(conv.messages),
                        state: conv.state,
                        updated_at: conv.updated_at
                    }
            "#,
            serde_json::json!({
                "agent_id": &req.agent_id,
                "limit": limit,
            }),
        )
    };

    let resp = arango.execute_aql(query, &bind_vars).await?;
    let conversations: Vec<ConversationSummary> = resp
        .get("result")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default();

    if let Some(reply) = msg.reply {
        let list_resp = ConversationListResponse { conversations };
        let _ = nats
            .publish(reply, serde_json::to_vec(&list_resp)?.into())
            .await;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Skill handlers
// ---------------------------------------------------------------------------

/// Search skills by semantic similarity (brute-force cosine for now).
async fn handle_skill_search(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    msg: async_nats::Message,
) -> Result<()> {
    let req: SkillSearchRequest =
        serde_json::from_slice(&msg.payload).context("parse skill.search")?;

    let limit = req.limit.unwrap_or(5);

    // Text-based search with score capped at 1.0.
    // Vector search (ANN) can be added when the vector index is created.
    let (query, bind_vars) = if req.query.is_empty() {
        // Empty query returns all skills
        (
            r#"
                FOR doc IN skills
                    SORT doc.created_at DESC
                    LIMIT @limit
                    RETURN {
                        id: doc.id, description: doc.description, snippet: doc.snippet,
                        score: 1.0, source: doc.source, tags: doc.tags || []
                    }
            "#,
            serde_json::json!({ "limit": limit }),
        )
    } else if let Some(ref scope) = req.scope {
        (
            r#"
                FOR doc IN skills
                    LET desc_match = CONTAINS(LOWER(doc.description), LOWER(@query)) ? 0.6 : 0.0
                    LET snip_match = CONTAINS(LOWER(doc.snippet), LOWER(@query)) ? 0.4 : 0.0
                    LET score = MIN([desc_match + snip_match, 1.0])
                    FILTER score > 0
                    FILTER doc.scope == null OR doc.scope == @scope
                    SORT score DESC
                    LIMIT @limit
                    RETURN {
                        id: doc.id, description: doc.description, snippet: doc.snippet,
                        score, source: doc.source, tags: doc.tags || []
                    }
            "#,
            serde_json::json!({ "query": &req.query, "limit": limit, "scope": scope }),
        )
    } else {
        (
            r#"
                FOR doc IN skills
                    LET desc_match = CONTAINS(LOWER(doc.description), LOWER(@query)) ? 0.6 : 0.0
                    LET snip_match = CONTAINS(LOWER(doc.snippet), LOWER(@query)) ? 0.4 : 0.0
                    LET score = MIN([desc_match + snip_match, 1.0])
                    FILTER score > 0
                    SORT score DESC
                    LIMIT @limit
                    RETURN {
                        id: doc.id, description: doc.description, snippet: doc.snippet,
                        score, source: doc.source, tags: doc.tags || []
                    }
            "#,
            serde_json::json!({ "query": &req.query, "limit": limit }),
        )
    };

    let resp = arango.execute_aql(query, &bind_vars).await?;
    let skills: Vec<SkillSearchResult> = resp
        .get("result")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default();

    if let Some(reply) = msg.reply {
        let search_resp = SkillSearchResponse { skills };
        let _ = nats
            .publish(reply, serde_json::to_vec(&search_resp)?.into())
            .await;
    }

    Ok(())
}

/// Save a skill (upsert by ID).
async fn handle_skill_save(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    msg: async_nats::Message,
) -> Result<()> {
    let req: SkillSaveRequest = serde_json::from_slice(&msg.payload).context("parse skill.save")?;

    // Validate required fields
    if req.description.trim().is_empty() || req.snippet.trim().is_empty() {
        if let Some(reply) = msg.reply {
            let resp = serde_json::json!({"error": "description and snippet are required"});
            let _ = nats.publish(reply, serde_json::to_vec(&resp)?.into()).await;
        }
        return Ok(());
    }

    let skill_id = req.id.unwrap_or_else(|| ulid::Ulid::new().to_string());

    // Validate skill ID format
    if skill_id.is_empty()
        || skill_id.len() > 254
        || !skill_id
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        if let Some(reply) = msg.reply {
            let resp = serde_json::json!({"error": "Invalid skill ID: must be 1-254 alphanumeric, dash, or underscore"});
            let _ = nats.publish(reply, serde_json::to_vec(&resp)?.into()).await;
        }
        return Ok(());
    }

    let now = chrono::Utc::now().to_rfc3339();
    let doc = serde_json::json!({
        "id": &skill_id,
        "description": &req.description,
        "snippet": &req.snippet,
        "source": &req.source,
        "scope": req.scope,
        "tags": req.tags,
        "updated_at": &now,
        "learned_from": req.learned_from,
        "embedding": if req.embedding.is_empty() { serde_json::json!(null) } else { serde_json::json!(req.embedding) },
    });

    // Use AQL UPSERT with separate INSERT/UPDATE to preserve created_at
    let query = r#"
        UPSERT { _key: @key }
        INSERT MERGE(@doc, { _key: @key, created_at: @now })
        UPDATE MERGE(OLD, @doc)
        IN skills
        RETURN { is_new: IS_NULL(OLD) }
    "#;
    let bind_vars = serde_json::json!({
        "key": &skill_id,
        "doc": &doc,
        "now": &now,
    });

    let result = arango.execute_aql(query, &bind_vars).await?;
    let is_new = result
        .get("result")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|v| v.get("is_new"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    info!(skill_id = %skill_id, %is_new, "Skill saved");

    if let Some(reply) = msg.reply {
        let resp = SkillSaveResponse { skill_id, is_new };
        let _ = nats.publish(reply, serde_json::to_vec(&resp)?.into()).await;
    }

    Ok(())
}

/// Get a skill by ID.
async fn handle_skill_get(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    msg: async_nats::Message,
) -> Result<()> {
    let req: SkillGetRequest = serde_json::from_slice(&msg.payload).context("parse skill.get")?;

    let doc = arango.get_document("skills", &req.skill_id).await?;

    if let Some(reply) = msg.reply {
        let resp = doc.unwrap_or(serde_json::json!(null));
        let _ = nats.publish(reply, serde_json::to_vec(&resp)?.into()).await;
    }

    Ok(())
}

/// List skills with optional source filter.
async fn handle_skill_list(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    msg: async_nats::Message,
) -> Result<()> {
    let req: SkillListRequest = serde_json::from_slice(&msg.payload).context("parse skill.list")?;

    let limit = req.limit.unwrap_or(50);

    let (query, bind_vars) = if let Some(ref source) = req.source_filter {
        (
            r#"
                FOR doc IN skills
                    FILTER doc.source == @source
                    SORT doc.created_at DESC
                    LIMIT @limit
                    RETURN {
                        id: doc.id,
                        description: doc.description,
                        snippet: doc.snippet,
                        score: 1.0,
                        source: doc.source,
                        tags: doc.tags || []
                    }
            "#,
            serde_json::json!({ "source": source, "limit": limit }),
        )
    } else {
        (
            r#"
                FOR doc IN skills
                    SORT doc.created_at DESC
                    LIMIT @limit
                    RETURN {
                        id: doc.id,
                        description: doc.description,
                        snippet: doc.snippet,
                        score: 1.0,
                        source: doc.source,
                        tags: doc.tags || []
                    }
            "#,
            serde_json::json!({ "limit": limit }),
        )
    };

    let resp = arango.execute_aql(query, &bind_vars).await?;
    let skills: Vec<SkillSearchResult> = resp
        .get("result")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default();

    if let Some(reply) = msg.reply {
        let list_resp = SkillListResponse { skills };
        let _ = nats
            .publish(reply, serde_json::to_vec(&list_resp)?.into())
            .await;
    }

    Ok(())
}

/// Delete a skill by ID.
async fn handle_skill_delete(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    msg: async_nats::Message,
) -> Result<()> {
    let req: SkillGetRequest =
        serde_json::from_slice(&msg.payload).context("parse skill.delete")?;

    debug!(skill_id = %req.skill_id, "handling brain.skill.delete");

    let query = "REMOVE { _key: @key } IN skills";
    let bind_vars = serde_json::json!({ "key": req.skill_id });

    let result = arango.execute_aql(query, &bind_vars).await;

    if let Some(reply) = msg.reply {
        let resp = match result {
            Ok(_) => serde_json::json!({ "success": true, "skill_id": req.skill_id }),
            Err(e) => {
                warn!(error = %e, skill_id = %req.skill_id, "failed to delete skill");
                serde_json::json!({ "success": false, "error": e.to_string() })
            }
        };
        let _ = nats.publish(reply, serde_json::to_vec(&resp)?.into()).await;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Preferences query handler
// ---------------------------------------------------------------------------

/// Handle `agent.preferences.query` — query preference facts for an agent.
///
/// Preferences are stored as facts with predicates prefixed by `user_prefers_`.
/// This handler queries the facts collection and returns matching preferences
/// as `PreferencesQueryResponse` via NATS request/reply.
async fn handle_preferences_query(
    arango: &ArangoClient,
    nats: &async_nats::Client,
    msg: async_nats::Message,
) -> Result<()> {
    // Parse the inner payload from the EventEnvelope
    let envelope: serde_json::Value =
        serde_json::from_slice(&msg.payload).context("parse preferences query envelope")?;
    let payload = envelope
        .get("payload")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let req: PreferencesQueryRequest =
        serde_json::from_value(payload).context("parse PreferencesQueryRequest")?;

    debug!(agent_id = %req.agent_id, "handling agent.preferences.query");

    // Query facts where subject matches the agent_id and predicate starts with "user_prefers_"
    let aql = r#"
        FOR fact IN facts
            FILTER fact.subject == @agent_id
               AND STARTS_WITH(fact.predicate, "user_prefers_")
               AND fact.valid_to == null
               AND fact.superseded_by == null
               AND fact.confidence >= @min_confidence
            SORT fact.confidence DESC
            LIMIT 50
            RETURN fact
    "#;

    let min_confidence = 0.3; // Low threshold to include weak preferences
    let bind_vars = serde_json::json!({
        "agent_id": req.agent_id,
        "min_confidence": min_confidence,
    });

    let preferences = match arango.execute_aql(aql, &bind_vars).await {
        Ok(resp) => {
            let facts = resp
                .get("result")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            facts
                .iter()
                .filter_map(|fact| {
                    let predicate = fact.get("predicate")?.as_str()?;
                    let key = predicate.strip_prefix("user_prefers_")?.to_string();
                    let value = fact
                        .get("value")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let confidence = fact
                        .get("confidence")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.5);
                    let reinforcement_count = fact
                        .get("reinforcement_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let last_updated = fact
                        .get("last_reinforced")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<chrono::DateTime<Utc>>().ok())
                        .unwrap_or_else(Utc::now);

                    // Apply category filter if provided
                    if let Some(ref category) = req.category {
                        if !key.starts_with(category.as_str()) {
                            return None;
                        }
                    }

                    Some(UserPreference {
                        key,
                        value,
                        confidence,
                        source_feedback_count: reinforcement_count,
                        last_updated,
                    })
                })
                .collect::<Vec<_>>()
        }
        Err(e) => {
            warn!(error = %e, agent_id = %req.agent_id, "failed to query preferences");
            Vec::new()
        }
    };

    debug!(
        agent_id = %req.agent_id,
        count = preferences.len(),
        "preferences query completed"
    );

    let response = PreferencesQueryResponse { preferences };

    if let Some(reply) = msg.reply {
        let resp_bytes = serde_json::to_vec(&response)?;
        let _ = nats.publish(reply, resp_bytes.into()).await;
    }

    Ok(())
}
