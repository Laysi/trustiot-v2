//! OpenAI Decisions (`POST /v1/decisions`, public beta since 2026-10-06): the router and the
//! reranker (decision 15). Only predicate and score questions, text and base64 images, no
//! streaming. Request and response shapes follow the API reference
//! (<https://developers.openai.com/api/reference/resources/decisions/methods/create>) and the
//! guide (<https://developers.openai.com/api/docs/guides/decisions>).

use std::collections::HashSet;
use std::sync::Arc;

use base64::Engine as _;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::time::Instant;

use crate::Reply;
use crate::error::{ApiErrorBody, Error, ErrorClass, TimeoutKind};
use crate::transport::{
    ApiKey, EndpointConfig, Transport, error_from_response, header_text, read_body,
};
use crate::usage::TokenUsage;

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com";
/// The only model the endpoint accepts on 2026-10-21.
pub const DEFAULT_MODEL: &str = "gpt-6-luna";
const DECISIONS_PATH: &str = "/v1/decisions";
const REQUEST_ID_HEADER: &str = "x-request-id";
/// The API accepts 1 to 400 questions per request.
pub const MAX_QUESTIONS: usize = 400;
/// The API accepts at most 128 images per request.
pub const MAX_IMAGES: usize = 128;
/// Image types sent as data URLs.
pub const IMAGE_MEDIA_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
/// Slack for rounding when checking that probabilities lie in [0, 1].
const EPSILON: f64 = 1e-6;
/// Slack for checking a score answer's distribution (sum 1, mean equal to `score`): two-decimal
/// rounding of three to five probabilities stays well inside it.
const SCORE_TOLERANCE: f64 = 0.02;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRequest {
    pub model: String,
    pub input: DecisionInput,
    pub questions: Vec<Question>,
    /// Other request fields (`prompt_cache_key`, `safety_identifier`, ...).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// The shared evidence: a string, or messages of text and image parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DecisionInput {
    Text(String),
    Messages(Vec<InputMessage>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InputRole {
    User,
    Developer,
    System,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputMessage {
    pub role: InputRole,
    pub content: Vec<InputPart>,
}

/// A part of a message. Images go only as base64 data URLs: our images are not public, and
/// the API takes no file IDs, audio or video (docs/architecture/ai-assistant.md, 「分流」).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum InputPart {
    #[serde(rename = "input_text")]
    Text { text: String },
    #[serde(rename = "input_image")]
    Image {
        image_url: String,
        /// `low`, `high`, `auto` (the default) or `original`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

impl InputPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// An image as a base64 data URL; `media_type` must be one of [`IMAGE_MEDIA_TYPES`].
    pub fn image_base64(
        media_type: &str,
        bytes: &[u8],
        detail: Option<&str>,
    ) -> Result<Self, Error> {
        if !IMAGE_MEDIA_TYPES.contains(&media_type) {
            return Err(Error::InvalidRequest(format!(
                "Decisions images must be one of {IMAGE_MEDIA_TYPES:?}, not {media_type}"
            )));
        }
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        Ok(Self::Image {
            image_url: format!("data:{media_type};base64,{data}"),
            detail: detail.map(str::to_owned),
        })
    }
}

/// A question. Every question needs a unique `name`, so each answer can be matched to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    /// How likely a statement about the input is true.
    Predicate { name: String, instructions: String },
    /// Rate the input against ordered levels, lowest first; the answer's `score` is the
    /// probability-weighted mean of the level indices (0-based).
    Score {
        name: String,
        instructions: String,
        levels: Vec<ScoreLevel>,
    },
}

impl Question {
    pub fn name(&self) -> &str {
        match self {
            Self::Predicate { name, .. } | Self::Score { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreLevel {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl DecisionRequest {
    /// Checks the request before sending: 1 to 400 questions with unique non-empty names,
    /// score questions with at least two levels, at most 128 images, all base64 data URLs.
    pub fn validate(&self) -> Result<(), Error> {
        let invalid = |why: String| Err(Error::InvalidRequest(why));
        if self.questions.is_empty() || self.questions.len() > MAX_QUESTIONS {
            return invalid(format!(
                "a decision needs 1 to {MAX_QUESTIONS} questions, not {}",
                self.questions.len()
            ));
        }
        let mut names = HashSet::new();
        for question in &self.questions {
            let name = question.name();
            if name.is_empty() || !names.insert(name) {
                return invalid(format!(
                    "question names must be unique and non-empty: {name:?}"
                ));
            }
            if let Question::Score { levels, .. } = question
                && levels.len() < 2
            {
                return invalid(format!("score question {name:?} needs at least two levels"));
            }
        }
        if let DecisionInput::Messages(messages) = &self.input {
            let mut images = 0usize;
            for part in messages.iter().flat_map(|message| &message.content) {
                if let InputPart::Image { image_url, .. } = part {
                    images += 1;
                    if !is_base64_image_url(image_url) {
                        return invalid("Decisions images must be base64 data URLs".into());
                    }
                }
            }
            if images > MAX_IMAGES {
                return invalid(format!("at most {MAX_IMAGES} images, not {images}"));
            }
        }
        Ok(())
    }
}

fn is_base64_image_url(url: &str) -> bool {
    url.strip_prefix("data:")
        .and_then(|rest| rest.split_once(";base64,"))
        .is_some_and(|(media_type, _)| IMAGE_MEDIA_TYPES.contains(&media_type))
}

/// An answer, in question order. Any question may come back as a refusal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Predicate {
        name: Option<String>,
        probability: f64,
    },
    Score {
        name: Option<String>,
        score: f64,
        probabilities: Vec<LevelProbability>,
        confidence: f64,
    },
    Refusal {
        name: Option<String>,
    },
}

impl Answer {
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Predicate { name, .. } | Self::Score { name, .. } | Self::Refusal { name } => {
                name.as_deref()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LevelProbability {
    /// The level index.
    pub value: i64,
    pub label: String,
    pub probability: f64,
}

/// `usage` as Decisions reports it. Decisions bills input tokens only, at one rate, with no
/// cache or output charges (guide, 「Pricing and availability」), so the ledger gets
/// `input = input_tokens`; the cached and cache-write counts are kept for monitoring.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DecisionUsage {
    pub input_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens_details: Option<InputTokensDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct InputTokensDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

impl DecisionUsage {
    pub fn tokens(&self) -> TokenUsage {
        TokenUsage {
            input: self.input_tokens,
            output: self.output_tokens.unwrap_or(0),
            ..TokenUsage::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionResponse {
    pub model: String,
    pub answers: Vec<Answer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<DecisionUsage>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl DecisionResponse {
    /// Parses a response body. A missing field, a wrong type or an answer type other than
    /// predicate, score and refusal is a protocol error.
    pub fn parse(body: &[u8]) -> Result<Self, Error> {
        serde_json::from_slice(body)
            .map_err(|error| Error::Protocol(format!("decision response: {error}")))
    }

    /// Checks that the answers fit the questions: one per question, in order, with the
    /// question's name and type (or a refusal), probabilities and confidence in [0, 1], and
    /// score distributions over exactly the question's levels.
    pub fn validate_against(&self, request: &DecisionRequest) -> Result<(), Error> {
        let mismatch = |why: String| Err(Error::Protocol(format!("decision answers: {why}")));
        if self.answers.len() != request.questions.len() {
            return mismatch(format!(
                "{} answers for {} questions",
                self.answers.len(),
                request.questions.len()
            ));
        }
        for (index, (question, answer)) in request.questions.iter().zip(&self.answers).enumerate() {
            let name = question.name();
            if answer.name() != Some(name) {
                return mismatch(format!(
                    "answer {index} is named {:?}, the question {name:?}",
                    answer.name()
                ));
            }
            match (question, answer) {
                (_, Answer::Refusal { .. }) => {}
                (Question::Predicate { .. }, Answer::Predicate { probability, .. }) => {
                    if !is_probability(*probability) {
                        return mismatch(format!("{name:?} has probability {probability}"));
                    }
                }
                (
                    Question::Score { levels, .. },
                    Answer::Score {
                        score,
                        probabilities,
                        confidence,
                        ..
                    },
                ) => validate_score(name, levels, *score, probabilities, *confidence)
                    .or_else(mismatch)?,
                _ => {
                    return mismatch(format!(
                        "{name:?} is a {} question with a different answer type",
                        match question {
                            Question::Predicate { .. } => "predicate",
                            Question::Score { .. } => "score",
                        }
                    ));
                }
            }
        }
        Ok(())
    }

    /// The answer to the question named `name`.
    pub fn answer(&self, name: &str) -> Option<&Answer> {
        self.answers
            .iter()
            .find(|answer| answer.name() == Some(name))
    }
}

fn is_probability(value: f64) -> bool {
    value.is_finite() && (-EPSILON..=1.0 + EPSILON).contains(&value)
}

fn validate_score(
    name: &str,
    levels: &[ScoreLevel],
    score: f64,
    probabilities: &[LevelProbability],
    confidence: f64,
) -> Result<(), String> {
    let top = levels.len().saturating_sub(1);
    let top_f = u32::try_from(top).map(f64::from).unwrap_or(f64::MAX);
    if !score.is_finite() || score < -EPSILON || score > top_f + EPSILON {
        return Err(format!("{name:?} has score {score} outside 0..={top}"));
    }
    if !is_probability(confidence) {
        return Err(format!("{name:?} has confidence {confidence}"));
    }
    if probabilities.len() != levels.len() {
        return Err(format!(
            "{name:?} has {} level probabilities for {} levels",
            probabilities.len(),
            levels.len()
        ));
    }
    let mut seen = vec![false; levels.len()];
    let mut sum = 0.0;
    let mut mean = 0.0;
    for entry in probabilities {
        let level = usize::try_from(entry.value)
            .ok()
            .and_then(|index| levels.get(index).map(|level| (index, level)));
        let Some((index, level)) = level else {
            return Err(format!(
                "{name:?} has a probability for level {}",
                entry.value
            ));
        };
        if level.label != entry.label {
            return Err(format!(
                "{name:?} level {index} is labelled {:?}, the question says {:?}",
                entry.label, level.label
            ));
        }
        if std::mem::replace(&mut seen[index], true) {
            return Err(format!("{name:?} has level {index} twice"));
        }
        if !is_probability(entry.probability) {
            return Err(format!(
                "{name:?} level {index} has probability {}",
                entry.probability
            ));
        }
        sum += entry.probability;
        mean += f64::from(u32::try_from(index).unwrap_or(u32::MAX)) * entry.probability;
    }
    // The distribution sums to 1 and `score` is its mean level index (guide, "Score"); the
    // tolerance allows probabilities rounded to two decimals.
    if (sum - 1.0).abs() > SCORE_TOLERANCE {
        return Err(format!("{name:?} has level probabilities summing to {sum}"));
    }
    if (score - mean).abs() > SCORE_TOLERANCE * top_f.max(1.0) {
        return Err(format!(
            "{name:?} has score {score}, but its level probabilities give {mean}"
        ));
    }
    Ok(())
}

/// Classifies an OpenAI error by status: 429 (rate limit, or the `insufficient_quota` code,
/// which keeps failing), 503 (overloaded), 504 (timed out).
pub fn classify_error(status: u16) -> ErrorClass {
    match status {
        429 => ErrorClass::RateLimited { retry_after: None },
        503 => ErrorClass::Overloaded,
        504 => ErrorClass::Timeout(TimeoutKind::Provider),
        _ => ErrorClass::Other,
    }
}

fn classify_response(status: StatusCode, _body: &ApiErrorBody) -> ErrorClass {
    classify_error(status.as_u16())
}

/// A client for one Decisions endpoint. Clones share the concurrency limit.
#[derive(Clone)]
pub struct DecisionsClient {
    transport: Arc<Transport>,
    api_key: Arc<ApiKey>,
}

impl std::fmt::Debug for DecisionsClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionsClient")
            .field("endpoint", &self.transport.config.name)
            .finish_non_exhaustive()
    }
}

impl DecisionsClient {
    /// `http` brings the TLS backend and trust anchors (see [`crate::anthropic::AnthropicClient::new`]).
    pub fn new(
        config: EndpointConfig,
        api_key: ApiKey,
        http: reqwest::ClientBuilder,
    ) -> Result<Self, Error> {
        Ok(Self {
            transport: Arc::new(Transport::new(config, http)?),
            api_key: Arc::new(api_key),
        })
    }

    /// Asks the questions. The request is validated first, and the answers are checked
    /// against the questions; headers and body must arrive within `timeouts.response`.
    pub async fn decide(
        &self,
        request: &DecisionRequest,
    ) -> Result<Reply<DecisionResponse>, Error> {
        request.validate()?;
        let body = serde_json::to_vec(request)
            .map_err(|error| Error::InvalidRequest(format!("request body: {error}")))?;
        let permit = self.transport.acquire().await?;
        let deadline = Instant::now() + self.transport.config.timeouts.response;
        let builder = self
            .transport
            .http
            .post(self.transport.url(DECISIONS_PATH)?)
            .header(AUTHORIZATION, self.api_key.header_value("Bearer ")?)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .body(body);
        let mut response = self
            .transport
            .send(builder, deadline, TimeoutKind::Response)
            .await?;
        if !response.status().is_success() {
            return Err(error_from_response(
                response,
                REQUEST_ID_HEADER,
                deadline,
                classify_response,
            )
            .await);
        }
        let request_id = header_text(&response, REQUEST_ID_HEADER);
        let body = read_body(
            &mut response,
            self.transport.config.max_response_bytes,
            deadline,
            TimeoutKind::Response,
        )
        .await?;
        drop(permit);
        let decision = DecisionResponse::parse(&body)?;
        decision.validate_against(request)?;
        Ok(Reply {
            body: decision,
            request_id,
        })
    }
}
