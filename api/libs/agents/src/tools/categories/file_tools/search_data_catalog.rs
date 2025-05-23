use std::collections::{HashMap, HashSet};
use std::{env, sync::Arc, time::Instant};
use tokio::sync::Mutex;

use anyhow::{Context, Result};
use async_trait::async_trait;
use braintrust::{get_prompt_system_message, BraintrustClient};
use chrono::{DateTime, Utc};
use cohere_rust::{
    api::rerank::{ReRankModel, ReRankRequest},
    Cohere,
};
use database::{
    enums::DataSourceType,
    pool::get_pg_pool,
    schema::datasets,
    schema::data_sources,
};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use futures::stream::{self, StreamExt};
use litellm::{AgentMessage, ChatCompletionRequest, EmbeddingRequest, LiteLLMClient, Metadata, ResponseFormat};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use dataset_security::{get_permissioned_datasets, PermissionedDataset};
use sqlx::PgPool;
use stored_values;

use crate::{agent::Agent, tools::ToolExecutor};

// NEW: Structure to represent found values with their source information
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
pub struct FoundValueInfo {
    pub value: String,
    pub database_name: String,
    pub schema_name: String,
    pub table_name: String,
    pub column_name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchDataCatalogParams {
    specific_queries: Option<Vec<String>>,
    exploratory_topics: Option<Vec<String>>,
    value_search_terms: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SearchDataCatalogOutput {
    pub message: String,
    pub specific_queries: Option<Vec<String>>,
    pub exploratory_topics: Option<Vec<String>>,
    pub duration: i64,
    pub results: Vec<DatasetSearchResult>,
    pub data_source_id: Option<Uuid>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
pub struct DatasetSearchResult {
    pub id: Uuid,
    pub name: Option<String>,
    pub yml_content: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
struct DatasetResult {
    id: Uuid,
    name: Option<String>,
    yml_content: Option<String>,
}

#[derive(Debug, Clone)]
struct RankedDataset {
    dataset: PermissionedDataset,
}

/// Represents a searchable dimension in a model
#[derive(Debug, Clone)]
struct SearchableDimension {
    model_name: String,
    dimension_name: String,
    dimension_path: Vec<String>, // Path to locate this dimension in the YAML
}

// NEW: Helper function to generate embeddings for search terms
async fn generate_embedding_for_text(text: &str) -> Result<Vec<f32>> {
    let litellm_client = LiteLLMClient::new(None, None);
    
    let embedding_request = EmbeddingRequest {
        model: "text-embedding-3-small".to_string(),
        input: vec![text.to_string()], // Single input as a vector
        dimensions: Some(1536),
        encoding_format: Some("float".to_string()),
        user: None,
    };
    
    let embedding_response = litellm_client
        .generate_embeddings(embedding_request)
        .await?;
    
    if embedding_response.data.is_empty() {
        return Err(anyhow::anyhow!("No embeddings returned from API"));
    }
    
    Ok(embedding_response.data[0].embedding.clone())
}

// Rename and modify the function signature
async fn search_values_for_term_by_embedding(
    data_source_id: &Uuid,
    embedding: Vec<f32>, // Accept pre-computed embedding
    limit: i64,
) -> Result<Vec<stored_values::search::StoredValueResult>> {
    // Skip searching if embedding is invalid (e.g., empty)
    if embedding.is_empty() {
        debug!("Skipping search for empty embedding");
        return Ok(vec![]);
    }

    // Search values using the provided embedding (no table/column filters)
    match stored_values::search::search_values_by_embedding(
        *data_source_id,
        &embedding,
        limit,
    ).await {
        Ok(results) => {
            debug!(count = results.len(), "Successfully found values matching embedding");
            Ok(results)
        }
        Err(e) => {
            error!(data_source_id = %data_source_id, error = %e, "Failed to search values by embedding");
            // Return empty results on error to continue the process
            Ok(vec![])
        }
    }
}

// Helper function to identify time-based terms that might cause issues
fn is_time_period_term(term: &str) -> bool {
    let term_lower = term.to_lowercase();
    
    // List of time periods that might cause embedding search issues
    let time_terms = [
        "today", "yesterday", "tomorrow",
        "last week", "last month", "last year", "last quarter",
        "this week", "this month", "this year", "this quarter",
        "next week", "next month", "next year", "next quarter",
        "q1", "q2", "q3", "q4",
        "january", "february", "march", "april", "may", "june", 
        "july", "august", "september", "october", "november", "december",
        "jan", "feb", "mar", "apr", "jun", "jul", "aug", "sep", "oct", "nov", "dec"
    ];
    
    time_terms.iter().any(|&t| term_lower.contains(t))
}

// NEW: Convert StoredValueResult to FoundValueInfo
fn to_found_value_info(result: stored_values::search::StoredValueResult, _score: f64) -> FoundValueInfo {
    FoundValueInfo {
        value: result.value,
        database_name: result.database_name,
        schema_name: result.schema_name,
        table_name: result.table_name,
        column_name: result.column_name,
    }
}

#[derive(Debug, Deserialize)]
struct LLMFilterResponse {
    results: Vec<String>,
}

const SPECIFIC_LLM_FILTER_PROMPT: &str = r#"
You are a dataset relevance evaluator, focused on specific analytical requirements. Your task is to determine which datasets are **semantically relevant** to the user's query and the anticipated analytical needs based on their structure and metadata. Focus on the core **Business Objects, Properties, Events, Metrics, and Filters** explicitly requested or strongly implied.

USER REQUEST (Context): {user_request}
SPECIFIC SEARCH QUERY: {query} (This query is framed around key semantic concepts and anticipated attributes/joins identified from the user request)

Below is a list of datasets that were identified as potentially relevant by an initial ranking system.
For each dataset, review its description in the YAML format. Evaluate how well the dataset's described contents (columns, metrics, entities, documentation) **semantically align** with the key **Objects, Properties, Events, Metrics, and Filters** required by the SPECIFIC SEARCH QUERY and USER REQUEST context.

IMPORTANT EVIDENCE - ACTUAL DATA VALUES FOUND IN THIS DATASET:
{found_values_json}
These values were found in the actual data that matches your search requirements. Consider these as concrete evidence that this dataset contains data relevant to your query.

**Crucially, anticipate necessary attributes**: Pay close attention to whether the dataset contains specific attributes like **names, IDs, emails, timestamps, or other identifying/linking information** that are likely required to fulfill the analytical goal, even if not explicitly stated in the query but inferable from the user request context and common analytical patterns (e.g., needing 'customer name' when analyzing 'customer revenue').

Include datasets where the YAML description suggests a reasonable semantic match or overlap with the needed concepts and anticipated attributes. Prioritize datasets that appear to contain the core Objects or Events AND the necessary linking/descriptive Properties.

DATASETS:
{datasets_json}

Return a JSON response containing ONLY a list of the UUIDs for the semantically relevant datasets. The response should have the following structure:
```json
{
  "results": [
    "dataset-uuid-here-1",
    "dataset-uuid-here-2"
    // ... semantically relevant dataset UUIDs
  ]
}
```

IMPORTANT GUIDELINES:
1.  **Focus on Semantic Relevance & Anticipation**: Include datasets whose content, as described in the YAML, is semantically related to the required Objects, Properties, Events, Metrics, or Filters, AND contains the anticipated attributes needed for analysis (like names, IDs, relevant dimensions).
2.  **Consider the Core Concepts & Analytical Goal**: Does the dataset seem to be about the primary Business Object(s) or Event(s)? Does it contain relevant Properties or Metrics (including anticipated ones)?
3.  **Prioritize Datasets with Key Attributes**: Give higher importance to datasets containing necessary identifying or descriptive attributes (names, IDs, categories, timestamps) relevant to the query and user request context.
4.  **Evaluate based on Semantic Fit**: Does the dataset's purpose and structure align well with the user's information need and the likely analytical steps?
5.  **Consider Found Values as Evidence**: The actual values found in the dataset provide concrete evidence of relevance. If values matching the user's query (like specific entities, terms, or categories) appear in the dataset, this strongly suggests relevance.
6.  **Contextual Information is Relevant**: Include datasets providing important contextual Properties for the core Objects or Events.
7.  **When in doubt, lean towards inclusion if semantically plausible and potentially useful**: If the dataset seems semantically related, include it.
8.  **CRITICAL:** Each string in the "results" array MUST contain ONLY the dataset's UUID string (e.g., "9711ca55-8329-4fd9-8b20-b6a3289f3d38").
9.  **Use USER REQUEST for context, SPECIFIC SEARCH QUERY for focus**: Understand the underlying need (user request) and the specific concepts/attributes being targeted (search query).
"#;

const EXPLORATORY_LLM_FILTER_PROMPT: &str = r#"
You are a dataset relevance evaluator, focused on exploring potential connections and related concepts. Your task is to determine which datasets might be **thematically relevant** or provide useful **contextual information** related to the user's exploratory topic and broader request.

USER REQUEST (Context): {user_request}
EXPLORATORY TOPIC: {topic} (This topic represents a general area of interest derived from the user request)

Below is a list of datasets identified as potentially relevant by an initial ranking system.
For each dataset, review its description in the YAML format. Evaluate how well the dataset's described contents (columns, metrics, entities, documentation) **thematically relate** to the EXPLORATORY TOPIC and the overall USER REQUEST context.

IMPORTANT EVIDENCE - ACTUAL DATA VALUES FOUND IN THIS DATASET:
{found_values_json}
These values were found in the actual data that matches your exploratory topics. Consider these as concrete evidence that this dataset contains data relevant to your exploration.

Consider datasets that:
- Directly address the EXPLORATORY TOPIC.
- Contain concepts, objects, or events that are often related to the EXPLORATORY TOPIC (e.g., if the topic is 'customer churn', related datasets might involve 'customer support interactions', 'product usage', 'marketing engagement', 'customer demographics').
- Provide valuable contextual dimensions (like time, geography, product categories) that could enrich the analysis of the EXPLORATORY TOPIC.
- Might reveal interesting patterns or correlations when combined with data more central to the topic.

Focus on **potential utility for exploration and discovery**, rather than strict semantic matching to the topic words alone.

DATASETS:
{datasets_json}

Return a JSON response containing ONLY a list of the UUIDs for the potentially relevant datasets for exploration. The response should have the following structure:
```json
{
  "results": [
    "dataset-uuid-here-1",
    "dataset-uuid-here-2"
    // ... potentially relevant dataset UUIDs for exploration
  ]
}
```

IMPORTANT GUIDELINES:
1.  **Focus on Thematic Relevance & Potential Utility**: Include datasets whose content seems related to the EXPLORATORY TOPIC or could provide valuable context/insights for exploration.
2.  **Consider Related Concepts**: Think broadly about what data is often analyzed alongside the given topic.
3.  **Consider Found Values as Evidence**: The actual values found in the dataset provide concrete evidence of relevance. If values matching the user's exploratory topic (like specific entities, terms, or categories) appear in the dataset, this strongly suggests usefulness for exploration.
4.  **Prioritize Breadth**: Lean towards including datasets that might offer different perspectives or dimensions related to the topic.
5.  **Evaluate based on Potential for Discovery**: Does the dataset seem like it could contribute to understanding the topic area, even indirectly?
6.  **Contextual Information is Valuable**: Include datasets providing relevant dimensions or related entities.
7.  **When in doubt, lean towards inclusion if thematically plausible**: If the dataset seems potentially related to the exploration goal, include it.
8.  **CRITICAL:** Each string in the "results" array MUST contain ONLY the dataset's UUID string (e.g., "9711ca55-8329-4fd9-8b20-b6a3289f3d38").
9.  **Use USER REQUEST for context, EXPLORATORY TOPIC for focus**: Understand the underlying need (user request) and the general area being explored (topic).
"#;

// NEW: Helper function to extract data source ID from permissioned datasets
// This is a placeholder - you'll need to adjust based on how data_source_id is actually stored/retrieved
fn extract_data_source_id(datasets: &[PermissionedDataset]) -> Option<Uuid> {
    // Assuming datasets have a data_source_id property or it can be derived from dataset.id
    // As a fallback, we're using the ID of the first dataset
    // Replace this with actual implementation based on your data model
    if datasets.is_empty() {
        return None;
    }
    
    // For this implementation, we're assuming the dataset ID is the data source ID
    // In a real implementation, you would likely have a different way to get the data_source_id
    Some(datasets[0].data_source_id)
}

pub struct SearchDataCatalogTool {
    agent: Arc<Agent>,
}

impl SearchDataCatalogTool {
    pub fn new(agent: Arc<Agent>) -> Self {
        Self { agent }
    }

    #[allow(dead_code)]
    async fn is_enabled(&self) -> bool {
        true
    }

    async fn get_datasets(user_id: &Uuid) -> Result<Vec<PermissionedDataset>> {
        debug!("Fetching permissioned datasets for agent tool for user {}", user_id);
        let datasets_result = get_permissioned_datasets(user_id, 0, 10000).await;

        match datasets_result {
            Ok(datasets) => {
                let filtered_datasets: Vec<PermissionedDataset> = datasets
                    .into_iter()
                    .filter(|d| d.yml_content.is_some())
                    .collect();

                debug!(
                    count = filtered_datasets.len(),
                    user_id = %user_id,
                    "Successfully loaded and filtered permissioned datasets for agent tool"
                );
                Ok(filtered_datasets)
            }
            Err(e) => {
                error!(user_id = %user_id, "Failed to load permissioned datasets for agent tool: {}", e);
                Err(anyhow::anyhow!("Error fetching permissioned datasets: {}", e))
            }
        }
    }
}

#[async_trait]
impl ToolExecutor for SearchDataCatalogTool {
    type Output = SearchDataCatalogOutput;
    type Params = SearchDataCatalogParams;

    async fn execute(&self, params: Self::Params, _tool_call_id: String) -> Result<Self::Output> {
        let start_time = Instant::now();
        let user_id = self.agent.get_user_id();
        let session_id = self.agent.get_session_id();

        let specific_queries = params.specific_queries.clone().unwrap_or_default();
        let exploratory_topics = params.exploratory_topics.clone().unwrap_or_default();
        
        // Get the user prompt for extracting value search terms
        let user_prompt_value = self.agent.get_state_value("user_prompt").await;
        let user_prompt_str = match user_prompt_value {
            Some(Value::String(prompt)) => prompt,
            _ => {
                warn!("User prompt not found in agent state for value extraction.");
                "User query context not available.".to_string()
            }
        };
        
        debug!(
            specific_queries_count = specific_queries.len(),
            exploratory_topics_count = exploratory_topics.len(),
            "Starting request with specific queries and exploratory topics"
        );

        // Start concurrent tasks
        
        // 2. Begin fetching datasets concurrently
        let user_id_for_datasets = user_id.clone();
        let all_datasets_future = tokio::spawn(async move {
            Self::get_datasets(&user_id_for_datasets).await
        });
        
        // Await the datasets future first (we need this to proceed)
        let all_datasets = match all_datasets_future.await? {
            Ok(datasets) => datasets,
            Err(e) => {
                error!(user_id=%user_id, "Failed to retrieve permissioned datasets for tool execution: {}", e);
                return Ok(SearchDataCatalogOutput {
                    message: format!("Error fetching datasets: {}", e),
                    specific_queries: params.specific_queries,
                    exploratory_topics: params.exploratory_topics,
                    duration: start_time.elapsed().as_millis() as i64,
                    results: vec![],
                    data_source_id: None,
                });
            }
        };

        // Check if datasets were fetched and are not empty
        if all_datasets.is_empty() {
            info!("No datasets found for the organization or user.");
            // Optionally cache that no data source was found or handle as needed
            self.agent.set_state_value(String::from("data_source_id"), Value::Null).await;
            return Ok(SearchDataCatalogOutput {
                message: "No datasets available to search. Have you deployed datasets? If you believe this is an error, please contact support.".to_string(),
                specific_queries: params.specific_queries,
                exploratory_topics: params.exploratory_topics,
                duration: start_time.elapsed().as_millis() as i64,
                results: vec![],
                data_source_id: None,
            });
        }

        // Extract and cache the data_source_id from the first dataset
        // Assumes all datasets belong to the same data source for this user context
        let target_data_source_id = all_datasets[0].data_source_id;
        debug!(data_source_id = %target_data_source_id, "Extracted data source ID");
        
        // Cache the data_source_id in agent state
        self.agent.set_state_value(
            "data_source_id".to_string(),
            Value::String(target_data_source_id.to_string())
        ).await;
        debug!(data_source_id = %target_data_source_id, "Cached data source ID in agent state");

        // --- BEGIN: Spawn concurrent task to fetch data source syntax ---
        let agent_clone = self.agent.clone(); // Clone Arc<Agent> for the async block
        let syntax_future = tokio::spawn(async move {
            let result: Result<String> = async {
                let mut conn = get_pg_pool().get().await
                    .context("Failed to get DB connection for data source type lookup")?;

                let source_type = data_sources::table
                    .filter(data_sources::id.eq(target_data_source_id))
                    .select(data_sources::type_) // <-- Use type_ as per user edit
                    .first::<DataSourceType>(&mut conn) // <-- Use corrected enum name
                    .await
                    .context(format!("Failed to find data source type for ID: {}", target_data_source_id))?;

                // Use the enum's to_string() method directly
                let syntax_string = source_type.to_string();
                Ok(syntax_string)
            }.await;

            // Set state inside the spawned task
            match result {
                Ok(syntax) => {
                    debug!(data_source_id = %target_data_source_id, syntax = %syntax, "Determined data source syntax concurrently");
                    agent_clone.set_state_value(
                        "data_source_syntax".to_string(),
                        Value::String(syntax)
                    ).await;
                },
                Err(e) => {
                    warn!(data_source_id = %target_data_source_id, error = %e, "Failed to determine data source syntax concurrently, setting state to null");
                    agent_clone.set_state_value(
                        "data_source_syntax".to_string(),
                        Value::Null
                    ).await;
                }
            }
        });
        // --- END: Spawn concurrent task to fetch data source syntax ---

        // --- BEGIN REORDERED VALUE SEARCH ---

        // Extract value search terms
        let value_search_terms = params.value_search_terms.clone().unwrap_or_default();
        
        // Filter terms before generating embeddings
        let valid_value_search_terms: Vec<String> = value_search_terms
            .into_iter()
            .filter(|term| term.len() >= 2 && !is_time_period_term(term))
            .collect();

        // Generate embeddings for all valid terms concurrently using batching
        let term_embeddings: HashMap<String, Vec<f32>> = if !valid_value_search_terms.is_empty() {
            let embedding_terms = valid_value_search_terms.clone();
            let embedding_batch_future = tokio::spawn(async move {
                generate_embeddings_batch(embedding_terms).await
            });

            // Await the batch embedding generation
            match embedding_batch_future.await? {
                Ok(results) => results.into_iter().collect(),
                Err(e) => {
                    error!(error = %e, "Batch embedding generation failed");
                    HashMap::new() // Return empty map on error
                }
            }
        } else {
            HashMap::new() // No valid terms, no embeddings needed
        };

        debug!(count = term_embeddings.len(), "Generated embeddings for value search terms via batch");

        // Begin value searches concurrently using pre-generated embeddings and schema filter
        let mut value_search_futures = Vec::new();
        if !term_embeddings.is_empty() {
            let schema_name = format!("ds_{}", target_data_source_id.to_string().replace('-', "_"));
            debug!(schema_filter = %schema_name, "Using schema filter for value search");

            for (term, embedding) in term_embeddings.iter() {
                let term_clone = term.clone();
                let embedding_clone = embedding.clone();
                let data_source_id_clone = target_data_source_id;

                let future = tokio::spawn(async move {
                    // Use search_values_by_embedding_with_filters with only the schema filter
                    let results = stored_values::search::search_values_by_embedding(
                        data_source_id_clone,
                        &embedding_clone,
                        20, // Limit to 20 values per term
                    ).await;
                    
                    (term_clone, results)
                });
                
                value_search_futures.push(future);
            }
        }
        
        // Await value searches to complete
        let value_search_results_vec: Vec<(String, Result<Vec<stored_values::search::StoredValueResult>>)> = 
            futures::future::join_all(value_search_futures)
                .await
                .into_iter()
                .filter_map(|r| r.ok()) // Filter out any join errors
                .collect();
        
        // Process the value search results
        let mut found_values_by_term = HashMap::new();
        for (term, result) in value_search_results_vec {
            match result {
                Ok(values) => {
                    let found_values: Vec<FoundValueInfo> = values.into_iter()
                        .map(|val| {
                            to_found_value_info(val, 0.0) // We don't use score in FoundValueInfo
                        })
                        .collect();
                    
                    let term_str = term.clone(); // Clone before moving into HashMap
                    let values_count = found_values.len();
                    found_values_by_term.insert(term, found_values);
                    debug!(term = %term_str, count = values_count, schema = %format!("ds_{}", target_data_source_id.to_string().replace('-', "_")), "Found values for search term");
                }
                Err(e) => {
                    error!(term = %term, error = %e, "Error searching for values");
                    // Store empty vec even on error to avoid issues later
                    found_values_by_term.insert(term, vec![]);
                }
            }
        }
        
        // Flatten all found values into a single list (needed for LLM filter)
        let all_found_values: Vec<FoundValueInfo> = found_values_by_term.values()
            .flat_map(|values| values.clone())
            .collect();
        
        debug!(value_count = all_found_values.len(), "Total found values across all terms after initial search");

        // --- END REORDERED VALUE SEARCH ---

        // Check if we have anything to search for *after* value search and before reranking
        if specific_queries.is_empty() && exploratory_topics.is_empty() && all_found_values.is_empty() && valid_value_search_terms.is_empty() {
            // Adjusted condition to check all_found_values as well
            warn!("SearchDataCatalogTool executed with no specific queries, exploratory topics, or valid value search terms resulting in found values.");
            // We might still want to return an empty list if no queries/topics provided, even if values were searched but none found.
            // Let's return the empty list if no queries/topics AND no values found from terms.
            if specific_queries.is_empty() && exploratory_topics.is_empty() && all_found_values.is_empty() {
                 return Ok(SearchDataCatalogOutput {
                    message: "No search queries, exploratory topics, or found values from provided terms.".to_string(),
                    specific_queries: params.specific_queries,
                    exploratory_topics: params.exploratory_topics,
                    duration: start_time.elapsed().as_millis() as i64,
                    results: vec![],
                    data_source_id: Some(target_data_source_id),
                });
            }
        }

        // Prepare documents from datasets (needed for reranking)
        let documents: Vec<String> = all_datasets
            .iter()
            .filter_map(|dataset| dataset.yml_content.clone())
            .collect();

        if documents.is_empty() {
            warn!("No datasets with YML content found after filtering.");
            return Ok(SearchDataCatalogOutput {
                message: "No searchable dataset content found.".to_string(),
                specific_queries: params.specific_queries,
                exploratory_topics: params.exploratory_topics,
                duration: start_time.elapsed().as_millis() as i64,
                results: vec![],
                data_source_id: Some(target_data_source_id),
            });
        }

        // --- BEGIN MOVED RERANKING ---
        // We'll use the user prompt for the LLM filtering
        let user_prompt_for_task = user_prompt_str.clone();
        
        // Keep track of reranking errors using Arc<Mutex>
        let rerank_errors = Arc::new(Mutex::new(Vec::new()));
        
        // Start specific query reranking
        let specific_rerank_futures = stream::iter(specific_queries.clone())
            .map(|query| {
                let current_query = query.clone();
                let datasets_clone = all_datasets.clone();
                let documents_clone = documents.clone();
                let rerank_errors_clone = Arc::clone(&rerank_errors); // Clone Arc

                async move {
                    let ranked = match rerank_datasets(&current_query, &datasets_clone, &documents_clone).await {
                        Ok(r) => r,
                        Err(e) => {
                            error!(error = %e, query = current_query, "Reranking failed for specific query");
                            // Lock and push error
                            let mut errors = rerank_errors_clone.lock().await;
                            errors.push(format!("Failed to rerank for specific query '{}': {}", current_query, e));
                            Vec::new() // Return empty vec on error to avoid breaking flow
                        }
                    };

                    (current_query, ranked)
                }
            })
            .buffer_unordered(10);

        // Start exploratory topic reranking
        let exploratory_rerank_futures = stream::iter(exploratory_topics.clone())
            .map(|topic| {
                let current_topic = topic.clone();
                let datasets_clone = all_datasets.clone();
                let documents_clone = documents.clone();
                let rerank_errors_clone = Arc::clone(&rerank_errors); // Clone Arc

                async move {
                    let ranked = match rerank_datasets(&current_topic, &datasets_clone, &documents_clone).await {
                        Ok(r) => r,
                        Err(e) => {
                            error!(error = %e, topic = current_topic, "Reranking failed for exploratory topic");
                            // Lock and push error
                            let mut errors = rerank_errors_clone.lock().await;
                            errors.push(format!("Failed to rerank for exploratory topic '{}': {}", current_topic, e));
                            Vec::new() // Return empty vec on error to avoid breaking flow
                        }
                    };

                    (current_topic, ranked)
                }
            })
            .buffer_unordered(10);

        // Collect rerank results in parallel
        let specific_reranked_vec = specific_rerank_futures.collect::<Vec<(String, Vec<RankedDataset>)>>().await;
        let exploratory_reranked_vec = exploratory_rerank_futures.collect::<Vec<(String, Vec<RankedDataset>)>>().await;
        // --- END MOVED RERANKING ---
        
        // 6. Now run LLM filtering with the found values and ranked datasets
        let specific_filter_futures = stream::iter(specific_reranked_vec)
            .map(|(query, ranked)| {
                let user_id_clone = user_id.clone();
                let session_id_clone = session_id.clone();
                let prompt_clone = user_prompt_for_task.clone();
                let values_clone = all_found_values.clone();

                async move {
                    if ranked.is_empty() {
                        return Ok(vec![]);
                    }
                    
                    match filter_specific_datasets_with_llm(&query, &prompt_clone, ranked, &user_id_clone, &session_id_clone, &values_clone).await {
                        Ok(filtered) => Ok(filtered),
                        Err(e) => {
                            error!(error = %e, query = query, "LLM filtering failed for specific query");
                            Ok(vec![])
                        }
                    }
                }
            })
            .buffer_unordered(10);

        let exploratory_filter_futures = stream::iter(exploratory_reranked_vec)
            .map(|(topic, ranked)| {
                let user_id_clone = user_id.clone();
                let session_id_clone = session_id.clone();
                let prompt_clone = user_prompt_for_task.clone();
                let values_clone = all_found_values.clone();

                async move {
                    if ranked.is_empty() {
                        return Ok(vec![]);
                    }
                    
                    match filter_exploratory_datasets_with_llm(&topic, &prompt_clone, ranked, &user_id_clone, &session_id_clone, &values_clone).await {
                        Ok(filtered) => Ok(filtered),
                        Err(e) => {
                            error!(error = %e, topic = topic, "LLM filtering failed for exploratory topic");
                            Ok(vec![])
                        }
                    }
                }
            })
            .buffer_unordered(10);
        
        // Collect filter results
        let specific_results_vec: Vec<Result<Vec<DatasetResult>>> = specific_filter_futures.collect().await;
        let exploratory_results_vec: Vec<Result<Vec<DatasetResult>>> = exploratory_filter_futures.collect().await;

        // Process and combine results
        let mut combined_results = Vec::new();
        let mut unique_ids = HashSet::new();

        for result in specific_results_vec {
            match result {
                Ok(datasets) => {
                    for dataset in datasets {
                        if unique_ids.insert(dataset.id) {
                            combined_results.push(dataset);
                        }
                    }
                }
                Err(e) => {
                    warn!("Error processing a specific query stream: {}", e);
                }
            }
        }

        for result in exploratory_results_vec {
            match result {
                Ok(datasets) => {
                    for dataset in datasets {
                        if unique_ids.insert(dataset.id) {
                            combined_results.push(dataset);
                        }
                    }
                }
                Err(e) => {
                    warn!("Error processing an exploratory topic stream: {}", e);
                }
            }
        }

        let final_search_results: Vec<DatasetSearchResult> = combined_results
            .into_iter()
            .map(|result| DatasetSearchResult {
                id: result.id,
                name: result.name,
                yml_content: result.yml_content,
            })
            .collect();

        // After filtering and before returning results, update YML content with search results
        // For each dataset in the final results, search for searchable dimensions and update YML
        let mut updated_results = Vec::new();
        
        for result in &final_search_results {
            let mut updated_result = result.clone();
            
            if let Some(yml_content) = &result.yml_content {
                // Inject pre-found values into YML
                match inject_prefound_values_into_yml(
                    yml_content,
                    &all_found_values, // Pass the results from the initial value search
                ).await {
                    Ok(updated_yml) => {
                        debug!(
                            dataset_id = %result.id,
                            "Successfully updated YML with relevant values for searchable dimensions"
                        );
                        updated_result.yml_content = Some(updated_yml);
                    },
                    Err(e) => {
                        warn!(
                            dataset_id = %result.id,
                            error = %e,
                            "Failed to update YML with relevant values"
                        );
                    }
                }
            }
            
            updated_results.push(updated_result);
        }

        // --- BEGIN: Wait for syntax future ---
        // Ensure the syntax task completes before finishing.
        if let Err(e) = syntax_future.await {
            // Handle potential join errors (e.g., if the spawned task panicked)
            warn!(error = %e, "Syntax fetching task failed to join");
            // Depending on requirements, you might want to return an error here
            // or ensure the state is explicitly null if it didn't get set.
            // For now, we'll just log the warning, as the task itself handles
            // setting state to null on internal errors.
        }
        // --- END: Wait for syntax future ---

        // Return the updated results
        let mut message = if updated_results.is_empty() {
            "No relevant datasets found after filtering.".to_string()
        } else {
            format!("Found {} relevant datasets with injected values for searchable dimensions.", updated_results.len())
        };

        // Append reranking error information if any occurred
        // Lock the mutex to access the errors safely
        let final_errors = rerank_errors.lock().await;
        if !final_errors.is_empty() {
            message.push_str("
 Warning: Some parts of the search failed due to reranking errors:");
            for error_msg in final_errors.iter() { // Iterate over locked data
                message.push_str(&format!("
 - {}", error_msg));
            }
        }
        // Mutex guard `final_errors` is dropped here

        self.agent
            .set_state_value(
                String::from("data_context"),
                Value::Bool(!updated_results.is_empty()),
            )
            .await;

        self.agent
            .set_state_value(String::from("searched_data_catalog"), Value::Bool(true))
            .await;

        let duration = start_time.elapsed().as_millis();

        Ok(SearchDataCatalogOutput {
            message,
            specific_queries: params.specific_queries,
            exploratory_topics: params.exploratory_topics,
            duration: duration as i64,
            results: updated_results,  // Use updated results instead of final_search_results
            data_source_id: Some(target_data_source_id),
        })
    }

    fn get_name(&self) -> String {
        "search_data_catalog".to_string()
    }

    async fn get_schema(&self) -> Value {
        serde_json::json!({
          "name": "search_data_catalog",
          "description": get_search_data_catalog_description().await,
          "parameters": {
            "type": "object",
            "properties": {
              "specific_queries": {
                "type": "array",
                "description": "Optional list of specific, high-intent search queries targeting data assets based on identified Objects, Properties, Events, Metrics, Filters, and anticipated needs (e.g., required joins, identifiers). Use for focused requests.",
                "items": {
                  "type": "string",
                  "description": "A concise, full-sentence, natural language query describing the specific search intent, e.g., 'Find datasets with Customer orders including Order ID, Order Date, Total Amount, and linked Customer Name and Email.'"
                },
              },
              "exploratory_topics": {
                 "type": "array",
                 "description": "Optional list of broader topics for exploration when the user's request is vague or seeks related concepts. Aims to discover potentially relevant datasets based on thematic connections.",
                 "items": {
                   "type": "string",
                   "description": "A concise topic phrase describing a general area of interest, e.g., 'Customer churn factors', 'Website traffic analysis', 'Product performance metrics'."
                 },
               },
               "value_search_terms": {
                 "type": "array",
                 "description": "Optional list of specific, concrete, meaningful values (e.g., 'Red Bull', 'California', 'John Smith', 'Premium Tier') extracted directly from the user query. These are used for semantic value search within columns. **CRITICAL**: Exclude general concepts ('revenue'), time periods ('last month'), generic identifiers (UUIDs, numerical IDs like 'cust_12345'), and non-semantic composite values (e.g., avoid 'item 987abc', prefer 'item' if meaningful or omit). Focus on distinct proper nouns, categories, or status names.",
                 "items": {
                   "type": "string",
                   "description": "A specific value or entity likely to appear in database columns."
                 },
               },
            },
            "additionalProperties": false
          }
        })
    }
}

async fn get_search_data_catalog_description() -> String {
    if env::var("USE_BRAINTRUST_PROMPTS").is_err() {
        return "Searches the data catalog for relevant data assets (e.g., datasets, models, metrics, filters, properties, documentation) based on high-intent queries derived solely from the user's request and conversation history, with no assumptions about data availability. Queries are concise, full-sentence, natural language expressions of search intent. Specific requests generate a single, focused query, while broad requests produce multiple queries to cover all context-implied assets (datasets, models, metrics, properties, documentation), starting with topics mentioned in the context (e.g., sales, customers, products) and refining with filters, metrics, or relationships. Supports multiple concurrent queries for comprehensive coverage.".to_string();
    }

    let client = BraintrustClient::new(None, "96af8b2b-cf3c-494f-9092-44eb3d5b96ff").unwrap();
    match get_prompt_system_message(&client, "865efb24-4355-4abb-aaf7-260af0f06794").await {
        Ok(message) => message,
        Err(e) => {
            eprintln!(
                "Failed to get prompt system message for tool description: {}",
                e
            );
            "Searches the data catalog for relevant data assets (e.g., datasets, models, metrics, filters, properties, documentation) based on high-intent queries derived solely from the user's request and conversation history, with no assumptions about data availability. Queries are concise, full-sentence, natural language expressions of search intent. Specific requests generate a single, focused query, while broad requests produce multiple queries to cover all context-implied assets (datasets, models, metrics, properties, documentation), starting with topics mentioned in the context (e.g., sales, customers, products) and refining with filters, metrics, or relationships. Supports multiple concurrent queries for comprehensive coverage.".to_string()
        }
    }
}

async fn rerank_datasets(
    query: &str,
    all_datasets: &[PermissionedDataset],
    documents: &[String],
) -> Result<Vec<RankedDataset>, anyhow::Error> {
    if documents.is_empty() || all_datasets.is_empty() {
        return Ok(vec![]);
    }
    let co = Cohere::default();

    let request = ReRankRequest {
        query,
        documents,
        model: ReRankModel::EnglishV3,
        top_n: Some(35),
        ..Default::default()
    };

    let rerank_results = match co.rerank(&request).await {
        Ok(results) => results,
        Err(e) => {
            error!(error = %e, query = query, "Cohere rerank API call failed");
            return Err(anyhow::anyhow!("Cohere rerank failed: {}", e));
        }
    };

    let mut ranked_datasets = Vec::new();
    for result in rerank_results {
        if let Some(dataset) = all_datasets.get(result.index as usize) {
            ranked_datasets.push(RankedDataset {
                dataset: dataset.clone(),
            });
        } else {
            error!(
                "Invalid dataset index {} from Cohere for query '{}'. Max index: {}",
                result.index,
                query,
                all_datasets.len() - 1
            );
        }
    }

    let relevant_datasets = ranked_datasets.into_iter().collect::<Vec<_>>();

    Ok(relevant_datasets)
}

async fn llm_filter_helper(
    prompt_template: &str,
    query_or_topic: &str,
    user_prompt: &str,
    ranked_datasets: Vec<RankedDataset>,
    user_id: &Uuid,
    session_id: &Uuid,
    generation_name_suffix: &str,
    all_found_values: &[FoundValueInfo],
) -> Result<Vec<DatasetResult>, anyhow::Error> {
    if ranked_datasets.is_empty() {
        return Ok(vec![]);
    }

    let datasets_json = ranked_datasets
        .iter()
        .map(|ranked| {
            serde_json::json!({
                "id": ranked.dataset.id.to_string(),
                "name": ranked.dataset.name,
                "yml_content": ranked.dataset.yml_content.clone().unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();

    // NEW: Format found values as JSON for the prompt
    let found_values_json = if all_found_values.is_empty() {
        "No specific values were found in the dataset that match the search terms.".to_string()
    } else {
        // Convert found values to a formatted string that can be inserted in the prompt
        let values_json = all_found_values
            .iter()
            .map(|val| {
                format!(
                    "- '{}' (found in {}.{}.{})",
                    val.value, val.database_name, val.table_name, val.column_name
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        values_json
    };

    let prompt = prompt_template
        .replace("{user_request}", user_prompt)
        .replace("{query}", query_or_topic)
        .replace("{topic}", query_or_topic)
        .replace(
            "{datasets_json}",
            &serde_json::to_string_pretty(&datasets_json)?,
        )
        .replace("{found_values_json}", &found_values_json);

    let llm_client = LiteLLMClient::new(None, None);

    let request = ChatCompletionRequest {
        model: "gemini-2.0-flash-001".to_string(),
        messages: vec![AgentMessage::User {
            id: None,
            content: prompt,
            name: None,
        }],
        stream: Some(false),
        response_format: Some(ResponseFormat {
            type_: "json_object".to_string(),
            json_schema: None,
        }),
        metadata: Some(Metadata {
            generation_name: format!("filter_data_catalog_{}_agent", generation_name_suffix),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            trace_id: Uuid::new_v4().to_string(),
        }),
        max_completion_tokens: Some(8096),
        temperature: Some(0.0),
        ..Default::default()
    };

    let response = llm_client.chat_completion(request).await?;

    let content = match response.choices.get(0).map(|c| &c.message) {
        Some(AgentMessage::Assistant { content: Some(content), .. }) => content,
        _ => {
            error!("LLM filter response missing or invalid content for query/topic: {}", query_or_topic);
            return Err(anyhow::anyhow!("LLM filter response missing or invalid content"));
        }
    };

    let filter_response: LLMFilterResponse = match serde_json::from_str(content) {
        Ok(response) => response,
        Err(e) => {
            error!(
                "Failed to parse LLM filter response for query/topic '{}': {}. Content: {}",
                query_or_topic, e, content
            );
            return Err(anyhow::anyhow!(
                "Failed to parse LLM filter response: {}",
                e
            ));
        }
    };

    let dataset_map: HashMap<Uuid, &PermissionedDataset> = ranked_datasets
        .iter()
        .map(|ranked| (ranked.dataset.id, &ranked.dataset))
        .collect();

    let filtered_datasets: Vec<DatasetResult> = filter_response
        .results
        .into_iter()
        .filter_map(|dataset_id_str| {
            match Uuid::parse_str(&dataset_id_str) {
                Ok(parsed_id) => {
                    if let Some(dataset) = dataset_map.get(&parsed_id) {
                        debug!(dataset_id = %dataset.id, dataset_name = %dataset.name, "Found matching dataset via LLM filter for query/topic: {}", query_or_topic);
                        Some(DatasetResult {
                            id: dataset.id,
                            name: Some(dataset.name.clone()),
                            yml_content: dataset.yml_content.clone(),
                        })
                    } else {
                        warn!(parsed_id = %parsed_id, query_or_topic = query_or_topic, "LLM filter returned UUID not found in ranked list");
                        None
                    }
                }
                Err(e) => {
                    error!(llm_result_id_str = %dataset_id_str, error = %e, query_or_topic = query_or_topic, "Failed to parse UUID from LLM filter result string");
                    None
                }
            }
        })
        .collect();

    debug!(
        "LLM filtering ({}) complete for query/topic '{}', keeping {} relevant datasets",
        generation_name_suffix,
        query_or_topic,
        filtered_datasets.len()
    );
    Ok(filtered_datasets)
}

async fn filter_specific_datasets_with_llm(
    query: &str,
    user_prompt: &str,
    ranked_datasets: Vec<RankedDataset>,
    user_id: &Uuid,
    session_id: &Uuid,
    all_found_values: &[FoundValueInfo],
) -> Result<Vec<DatasetResult>, anyhow::Error> {
    debug!(
        "Filtering {} datasets with SPECIFIC LLM for query: {}",
        ranked_datasets.len(),
        query
    );
    llm_filter_helper(
        SPECIFIC_LLM_FILTER_PROMPT,
        query,
        user_prompt,
        ranked_datasets,
        user_id,
        session_id,
        "specific",
        all_found_values
    ).await
}

async fn filter_exploratory_datasets_with_llm(
    topic: &str,
    user_prompt: &str,
    ranked_datasets: Vec<RankedDataset>,
    user_id: &Uuid,
    session_id: &Uuid,
    all_found_values: &[FoundValueInfo],
) -> Result<Vec<DatasetResult>, anyhow::Error> {
    debug!(
        "Filtering {} datasets with EXPLORATORY LLM for topic: {}",
        ranked_datasets.len(),
        topic
    );
    llm_filter_helper(
        EXPLORATORY_LLM_FILTER_PROMPT,
        topic,
        user_prompt,
        ranked_datasets,
        user_id,
        session_id,
        "exploratory",
        all_found_values
    ).await
}

// NEW: Helper function to generate embeddings for multiple texts in a batch
async fn generate_embeddings_batch(texts: Vec<String>) -> Result<Vec<(String, Vec<f32>)>> {
    if texts.is_empty() {
        return Ok(vec![]);
    }
    
    let litellm_client = LiteLLMClient::new(None, None);
    
    let embedding_request = EmbeddingRequest {
        model: "text-embedding-3-small".to_string(),
        input: texts.clone(), // Pass all texts to the API
        dimensions: Some(1536),
        encoding_format: Some("float".to_string()),
        user: None,
    };
    
    debug!(count = texts.len(), "Generating embeddings in batch");
    
    let embedding_response = litellm_client
        .generate_embeddings(embedding_request)
        .await
        .context("Failed to generate embeddings batch")?;
        
    if embedding_response.data.len() != texts.len() {
        warn!(
            "Mismatch between input text count ({}) and returned embedding count ({})",
            texts.len(),
            embedding_response.data.len()
        );
        // Attempt to match based on index, but this might be inaccurate if the order isn't guaranteed
    }

    let mut results = Vec::with_capacity(texts.len());
    for (index, text) in texts.into_iter().enumerate() {
        if let Some(embedding_data) = embedding_response.data.get(index) {
            results.push((text, embedding_data.embedding.clone()));
        } else {
            error!(term = %text, index = index, "Could not find corresponding embedding in batch response");
        }
    }
    
    Ok(results)
}

/// Parse YAML content to find models with searchable dimensions
fn extract_searchable_dimensions(yml_content: &str) -> Result<Vec<SearchableDimension>> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(yml_content)
        .context("Failed to parse dataset YAML content")?;
    
    let mut searchable_dimensions = Vec::new();
    
    // Check if models field exists
    if let Some(models) = yaml["models"].as_sequence() {
        for model in models {
            let model_name = model["name"].as_str().unwrap_or("unknown_model").to_string();
            
            // Check if dimensions field exists
            if let Some(dimensions) = model["dimensions"].as_sequence() {
                for dimension in dimensions {
                    // Check if dimension has searchable: true
                    if let Some(true) = dimension["searchable"].as_bool() {
                        let dimension_name = dimension["name"].as_str().unwrap_or("unknown_dimension").to_string();
                        
                        // Store this dimension as searchable
                        searchable_dimensions.push(SearchableDimension {
                            model_name: model_name.clone(), // Clone here to avoid move
                            dimension_name: dimension_name.clone(),
                            dimension_path: vec!["models".to_string(), model_name.clone(), "dimensions".to_string(), dimension_name],
                        });
                    }
                }
            }
        }
    }
    
    Ok(searchable_dimensions)
}

/// Extract database structure from YAML content based on actual model structure
fn extract_database_info_from_yaml(yml_content: &str) -> Result<HashMap<String, HashMap<String, HashMap<String, Vec<String>>>>> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(yml_content)
        .context("Failed to parse dataset YAML content")?;
    
    // Structure: database -> schema -> table -> columns
    let mut database_info = HashMap::new();
    
    // Process models
    if let Some(models) = yaml["models"].as_sequence() {
        for model in models {
            // Extract database, schema, and model name (which acts as table name)
            let database_name = model["database"].as_str().unwrap_or("unknown").to_string();
            let schema_name = model["schema"].as_str().unwrap_or("public").to_string();
            let table_name = model["name"].as_str().unwrap_or("unknown_model").to_string();
            
            // Initialize the nested structure if needed
            database_info
                .entry(database_name.clone())
                .or_insert_with(HashMap::new)
                .entry(schema_name.clone())
                .or_insert_with(HashMap::new);
            
            // Collect column names from dimensions, measures, and metrics
            let mut columns = Vec::new();
            
            // Add dimensions
            if let Some(dimensions) = model["dimensions"].as_sequence() {
                for dim in dimensions {
                    if let Some(dim_name) = dim["name"].as_str() {
                        columns.push(dim_name.to_string());
                        
                        // Also add the expression as a potential column to search
                        if let Some(expr) = dim["expr"].as_str() {
                            if expr != dim_name {
                                columns.push(expr.to_string());
                            }
                        }
                    }
                }
            }
            
            // Add measures
            if let Some(measures) = model["measures"].as_sequence() {
                for measure in measures {
                    if let Some(measure_name) = measure["name"].as_str() {
                        columns.push(measure_name.to_string());
                        
                        // Also add the expression as a potential column to search
                        if let Some(expr) = measure["expr"].as_str() {
                            if expr != measure_name {
                                columns.push(expr.to_string());
                            }
                        }
                    }
                }
            }
            
            // Add metrics
            if let Some(metrics) = model["metrics"].as_sequence() {
                for metric in metrics {
                    if let Some(metric_name) = metric["name"].as_str() {
                        columns.push(metric_name.to_string());
                    }
                }
            }
            
            // Store columns for this model
            database_info
                .get_mut(&database_name)
                .unwrap()
                .get_mut(&schema_name)
                .unwrap()
                .insert(table_name, columns);
        }
    }
    
    Ok(database_info)
}

/// Injects relevant values from a pre-compiled list into the YML of a dataset.
/// Matches values based on the database/schema/table/column defined in the YML.
async fn inject_prefound_values_into_yml(
    yml_content: &str,
    all_found_values: &[FoundValueInfo], // Use the pre-found values
) -> Result<String> {
    // Parse YAML for dimension definitions and modification
    let mut yaml: serde_yaml::Value = serde_yaml::from_str(yml_content)
        .context("Failed to parse dataset YAML for injecting values")?;

    // Extract database structure from YAML (which defines the source for dimensions)
    let database_info = match extract_database_info_from_yaml(yml_content) {
        Ok(info) => info,
        Err(e) => {
            warn!(error = %e, "Failed to extract database info from YAML, skipping value injection");
            return Ok(yml_content.to_string()); // Return original YML if parsing fails
        }
    };

    // Get searchable dimensions from the YML
    let searchable_dimensions = match extract_searchable_dimensions(yml_content) {
        Ok(dims) => dims,
        Err(e) => {
             warn!(error = %e, "Failed to extract searchable dimensions from YAML, skipping value injection");
            return Ok(yml_content.to_string());
        }
    };

    if searchable_dimensions.is_empty() {
        debug!("No searchable dimensions found in YAML content for value injection");
        return Ok(yml_content.to_string());
    }

    // Inject values into the mutable YAML structure
    if let Some(models) = yaml["models"].as_sequence_mut() {
        for model_yaml in models {
            // --- Extract immutable info before mutable borrow ---
            let model_name_opt = model_yaml["name"].as_str();
            if model_name_opt.is_none() { continue; }
            let model_name = model_name_opt.unwrap().to_string(); // Clone name to avoid borrow issue

            // Find the database and schema for this model from extracted info
            let mut model_db_info: Option<(&String, &String)> = None;
            for (db_name, schemas) in &database_info {
                for (schema_name, tables) in schemas {
                    if tables.contains_key(&model_name) {
                        model_db_info = Some((db_name, schema_name));
                        break;
                    }
                }
                if model_db_info.is_some() { break; }
            }

            let (model_database_name, model_schema_name) = if let Some(info) = model_db_info {
                info
            } else {
                 warn!(model=%model_name, "Could not find database/schema info for model in YAML, skipping value injection for its dimensions");
                 continue;
            };
            // --- End immutable info extraction ---

            if let Some(dimensions_yaml) = model_yaml["dimensions"].as_sequence_mut() {
                for dim_yaml in dimensions_yaml {
                    let dim_name_opt = dim_yaml["name"].as_str();
                    if dim_name_opt.is_none() { continue; }
                    let dim_name = dim_name_opt.unwrap();

                    // Check if this dimension is marked as searchable
                    let is_searchable = searchable_dimensions.iter().any(|sd| sd.model_name == model_name && sd.dimension_name == dim_name);
                    if !is_searchable {
                        continue; // Only inject into searchable dimensions
                    }

                    // Find values from the pre-found list that match this dimension's source
                    let relevant_values_for_dim: Vec<String> = all_found_values
                        .iter()
                        .filter(|found_val| {
                            // Match based on db, schema, table (model name), and column (dimension name)
                            found_val.database_name == *model_database_name
                                && found_val.schema_name == *model_schema_name
                                && found_val.table_name == model_name
                                && found_val.column_name == dim_name
                        })
                        .map(|found_val| found_val.value.clone())
                        .collect::<std::collections::HashSet<_>>() // Deduplicate
                        .into_iter()
                        .take(20) // Limit to max 20 unique values
                        .collect();

                    if !relevant_values_for_dim.is_empty() {
                        debug!(
                            model = %model_name,
                            dimension = %dim_name,
                            values_count = relevant_values_for_dim.len(),
                            "Injecting relevant values into dimension from pre-found list"
                        );
                        // Add/update relevant_values field in the YAML dimension map
                        dim_yaml["relevant_values"] = serde_yaml::Value::Sequence(
                            relevant_values_for_dim.iter()
                                .map(|v| serde_yaml::Value::String(v.clone()))
                                .collect()
                        );
                    }
                }
            }
        }
    }

    // Convert back to YAML string
    let updated_yml = serde_yaml::to_string(&yaml)
        .context("Failed to convert updated YAML with injected values back to string")?;

    Ok(updated_yml)
}