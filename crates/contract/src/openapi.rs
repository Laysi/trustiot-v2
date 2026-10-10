//! The OpenAPI side of the contract: the schemas every service shares.

use utoipa::OpenApi;

use crate::{ErrorBody, ErrorCode, FieldError, MessageKey};

/// The M1 error format as OpenAPI schemas. `tools/openapi` merges it into every generated
/// `api/openapi.json`, so a service's own spec can refer to `#/components/schemas/ErrorBody`
/// for its error responses without declaring the schema itself.
#[derive(OpenApi)]
#[openapi(components(schemas(ErrorBody, ErrorCode, FieldError, MessageKey)))]
pub struct SharedSchemas;
