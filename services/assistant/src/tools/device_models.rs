//! The device model tool (Laysi/trustiot-v2#591): the tenant's device models, read from the
//! assets service's `GET /api/assets/v1/device-models` (services/assets/README.md, "設備型號") as
//! the question's user. Row-level security limits the list to the user's tenant.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::callee::{Callee, Get};
use crate::model::BoxFuture;
use crate::tools::{Tool, ToolContext, ToolError, ToolOutput, ToolSpec};

/// The route, as services/assets serves it (`device_models::MODELS_PATH`).
pub const MODELS_PATH: &str = "/api/assets/v1/device-models";

/// The most models handed to the model; the rest are counted.
pub const MAX_MODEL_ROWS: usize = 200;
/// The longest name filter: a model name has at most 40 characters.
const MAX_NAME_FILTER: usize = 40;

const MODELS_REJECTED: &str = "The assets service rejected the request; call the tool again \
without input.";

/// The tenant's device models.
pub struct DeviceModels {
    assets: Callee,
}

impl DeviceModels {
    pub const NAME: &'static str = "device_models";

    /// The tool, calling the assets service through `assets`.
    pub fn new(assets: Callee) -> Arc<Self> {
        Arc::new(Self { assets })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    #[serde(default)]
    name_contains: Option<String>,
}

const FIELDS: &str = "The input has only the optional `name_contains`. The organisation and user \
are always the asking user's.";

impl Tool for DeviceModels {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: format!(
                "Lists the device models of the asking user's organisation: the register maps \
                 that devices of one type share. Returns JSON with `rows` ordered by name, each \
                 [model ID, name, revision, number of data points, last updated (UTC)], and \
                 `total`, the number of models that match. At most {MAX_MODEL_ROWS} rows; when \
                 `total` is larger, pass `name_contains` to narrow the list."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name_contains": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_NAME_FILTER,
                        "description": "Only models whose name contains this text, ignoring case, such as `meter`."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    fn narrowing_hint(&self) -> &str {
        "Pass `name_contains` to list fewer models."
    }

    fn call(
        self: Arc<Self>,
        context: ToolContext,
        input: Value,
    ) -> BoxFuture<'static, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: Input =
                serde_json::from_value(input).map_err(|_| ToolError::Input(FIELDS.into()))?;
            let filter = match input.name_contains {
                Some(text) if text.is_empty() || text.chars().count() > MAX_NAME_FILTER => {
                    return Err(ToolError::Input(
                        "`name_contains` must be 1 to 40 characters, or be left out.".into(),
                    ));
                }
                Some(text) => Some(text.to_lowercase()),
                None => None,
            };
            let list: ModelList = self
                .assets
                .get(
                    &context,
                    Get {
                        tool: Self::NAME,
                        path: MODELS_PATH,
                        query: &[],
                        rejected_input: MODELS_REJECTED,
                    },
                )
                .await?;
            let mut models: Vec<&ModelSummary> = list
                .models
                .iter()
                .filter(|model| {
                    filter
                        .as_deref()
                        .is_none_or(|text| model.name.to_lowercase().contains(text))
                })
                .collect();
            models.sort_by(|a, b| a.name.cmp(&b.name));
            let rows: Vec<Value> = models
                .iter()
                .take(MAX_MODEL_ROWS)
                .map(|model| {
                    json!([
                        model.model_id,
                        model.name,
                        model.revision,
                        model.point_count,
                        model.updated_at
                    ])
                })
                .collect();
            let mut result = json!({
                "columns": ["model_id", "name", "revision", "point_count", "updated_at"],
                "rows": rows,
                "total": models.len(),
            });
            if models.len() > MAX_MODEL_ROWS {
                result["note"] = json!(format!(
                    "Only the first {MAX_MODEL_ROWS} of {} models are shown; pass \
                     `name_contains` to narrow the list.",
                    models.len()
                ));
            }
            Ok(ToolOutput::text(result.to_string()))
        })
    }
}

/// The fields of the service's `DeviceModelList` the tool hands on; unknown fields are ignored.
#[derive(Debug, Deserialize)]
struct ModelList {
    models: Vec<ModelSummary>,
}

#[derive(Debug, Deserialize)]
struct ModelSummary {
    model_id: Uuid,
    name: String,
    revision: u64,
    point_count: u64,
    updated_at: String,
}
