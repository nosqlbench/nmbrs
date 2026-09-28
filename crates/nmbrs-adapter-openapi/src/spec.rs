// Copyright 2024-2026 Jonathan Shook
// SPDX-License-Identifier: Apache-2.0

//! OpenAPI 3.x specification parser using the `openapiv3` crate.
//!
//! Extracts operation metadata: method, path, operationId, parameters,
//! request body schema, and tags. Produces `ApiOperation` structs that
//! the workload generator converts into `ParsedOp`s.

use openapiv3::{
    ObjectType, OpenAPI, Operation, Parameter, ParameterSchemaOrContent, ReferenceOr, Schema,
    SchemaKind, Type,
};
use serde::de::Error as _;

/// A discovered API operation from the OpenAPI spec.
#[derive(Debug, Clone)]
pub struct ApiOperation {
    /// HTTP method (GET, POST, PUT, DELETE, PATCH).
    pub method: String,
    /// URL path with parameter placeholders (e.g., "/pets/{petId}").
    pub path: String,
    /// OpenAPI operationId (unique identifier).
    pub operation_id: String,
    /// Human-readable summary.
    pub summary: String,
    /// Path parameters (name → schema type).
    pub path_params: Vec<ParamInfo>,
    /// Query parameters.
    pub query_params: Vec<ParamInfo>,
    /// Request body content type and schema fields (if any).
    pub request_body: Option<BodyInfo>,
    /// Tags for filtering.
    pub tags: Vec<String>,
}

/// A parameter (path or query).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ParamInfo {
    pub name: String,
    pub schema_type: String,
    pub required: bool,
}

/// Request body info.
#[derive(Debug, Clone)]
pub struct BodyInfo {
    pub content_type: String,
    /// Flattened field name → type for the body schema.
    pub fields: Vec<FieldInfo>,
}

/// A field in a request body schema.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FieldInfo {
    pub name: String,
    pub schema_type: String,
    pub required: bool,
}

/// Parse an OpenAPI 3.x spec from YAML or JSON source.
pub fn parse_spec(source: &str) -> Result<(OpenAPI, Vec<ApiOperation>), String> {
    let spec: OpenAPI = serde_json::from_str(source)
        .or_else(|_| {
            // Parse YAML → serde_json::Value → OpenAPI
            let value: serde_json::Value = serde_yaml::from_str(source)
                .map_err(|e| serde_json::Error::custom(format!("YAML parse: {e}")))?;
            serde_json::from_value(value)
        })
        .map_err(|e| format!("failed to parse OpenAPI spec: {e}"))?;

    let mut operations = Vec::new();

    for (path, path_ref) in &spec.paths.paths {
        let path_item = match path_ref {
            ReferenceOr::Item(item) => item,
            ReferenceOr::Reference { .. } => continue,
        };

        let method_ops: Vec<(&str, Option<&Operation>)> = vec![
            ("GET", path_item.get.as_ref()),
            ("POST", path_item.post.as_ref()),
            ("PUT", path_item.put.as_ref()),
            ("DELETE", path_item.delete.as_ref()),
            ("PATCH", path_item.patch.as_ref()),
            ("HEAD", path_item.head.as_ref()),
        ];

        for (method, maybe_op) in method_ops {
            let Some(op) = maybe_op else { continue };

            let operation_id = op.operation_id.clone().unwrap_or_else(|| {
                let clean = path.replace('/', "_").replace(['{', '}'], "");
                format!("{}{clean}", method.to_lowercase())
            });

            let summary = op
                .summary
                .clone()
                .or_else(|| op.description.clone())
                .unwrap_or_default();

            // Collect parameters from path-level and operation-level
            let all_params: Vec<&Parameter> = path_item
                .parameters
                .iter()
                .chain(op.parameters.iter())
                .filter_map(|p| match p {
                    ReferenceOr::Item(param) => Some(param),
                    _ => None,
                })
                .collect();

            let mut path_params = Vec::new();
            let mut query_params = Vec::new();

            for param in &all_params {
                let info = ParamInfo {
                    name: param.parameter_data_ref().name.clone(),
                    schema_type: extract_param_type(param),
                    required: param.parameter_data_ref().required,
                };
                match param {
                    Parameter::Path { .. } => path_params.push(info),
                    Parameter::Query { .. } => query_params.push(info),
                    _ => query_params.push(info),
                }
            }

            // Request body
            let request_body = op.request_body.as_ref().and_then(|rb| match rb {
                ReferenceOr::Item(body) => extract_body_info(body, &spec),
                _ => None,
            });

            operations.push(ApiOperation {
                method: method.to_string(),
                path: path.clone(),
                operation_id,
                summary,
                path_params,
                query_params,
                request_body,
                tags: op.tags.clone(),
            });
        }
    }

    Ok((spec, operations))
}

/// Extract the schema type string from a parameter.
fn extract_param_type(param: &Parameter) -> String {
    let data = param.parameter_data_ref();
    match &data.format {
        ParameterSchemaOrContent::Schema(ReferenceOr::Item(schema)) => {
            schema_type_name(&schema.schema_kind)
        }
        _ => "string".into(),
    }
}

/// Extract body field info from a request body.
fn extract_body_info(body: &openapiv3::RequestBody, spec: &OpenAPI) -> Option<BodyInfo> {
    // Prefer application/json
    let (content_type, media) = body
        .content
        .iter()
        .find(|(k, _)| k.contains("json"))
        .or_else(|| body.content.iter().next())?;

    let schema = media.schema.as_ref()?;
    let fields = flatten_schema_ref(schema, spec, "", &body.required);

    Some(BodyInfo {
        content_type: content_type.clone(),
        fields,
    })
}

/// Flatten a schema reference into field info.
// `parent_required` is part of the recursive contract: the `$ref`
// branch forwards it to the nested `flatten_schema_ref` call so a
// resolved component inherits its referencing site's required flag.
#[allow(clippy::only_used_in_recursion)]
fn flatten_schema_ref(
    schema_ref: &ReferenceOr<Schema>,
    spec: &OpenAPI,
    prefix: &str,
    parent_required: &bool,
) -> Vec<FieldInfo> {
    match schema_ref {
        ReferenceOr::Item(schema) => flatten_schema(&schema.schema_kind, spec, prefix, &[]),
        ReferenceOr::Reference { reference } => {
            // Resolve $ref by name from components/schemas
            if let Some(name) = reference.strip_prefix("#/components/schemas/")
                && let Some(schema_ref) = spec.components.as_ref().and_then(|c| c.schemas.get(name))
            {
                return flatten_schema_ref(
                    &match schema_ref {
                        ReferenceOr::Item(s) => ReferenceOr::Item(s.clone()),
                        r @ ReferenceOr::Reference { .. } => r.clone(),
                    },
                    spec,
                    prefix,
                    parent_required,
                );
            }
            vec![]
        }
    }
}

/// Flatten a schema kind into field info.
fn flatten_schema(
    kind: &SchemaKind,
    spec: &OpenAPI,
    prefix: &str,
    required_fields: &[String],
) -> Vec<FieldInfo> {
    match kind {
        SchemaKind::Type(Type::Object(obj)) => flatten_object(obj, spec, prefix, required_fields),
        SchemaKind::Type(typ) => {
            if !prefix.is_empty() {
                vec![FieldInfo {
                    name: prefix.to_string(),
                    schema_type: type_name(typ),
                    required: required_fields.iter().any(|r| r == prefix),
                }]
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

/// Flatten an object schema's properties.
fn flatten_object(
    obj: &ObjectType,
    spec: &OpenAPI,
    prefix: &str,
    _parent_required: &[String],
) -> Vec<FieldInfo> {
    let required = &obj.required;
    let mut fields = Vec::new();

    for (name, prop_ref) in &obj.properties {
        let full_name = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };

        let is_required = required.contains(name);

        match prop_ref {
            ReferenceOr::Item(boxed_schema) => match &boxed_schema.schema_kind {
                SchemaKind::Type(Type::Object(nested_obj)) => {
                    fields.extend(flatten_object(nested_obj, spec, &full_name, &[]));
                }
                SchemaKind::Type(typ) => {
                    fields.push(FieldInfo {
                        name: full_name,
                        schema_type: type_name(typ),
                        required: is_required,
                    });
                }
                _ => {
                    fields.push(FieldInfo {
                        name: full_name,
                        schema_type: "string".into(),
                        required: is_required,
                    });
                }
            },
            ReferenceOr::Reference { reference } => {
                if let Some(schema_name) = reference.strip_prefix("#/components/schemas/")
                    && let Some(schema_ref) = spec
                        .components
                        .as_ref()
                        .and_then(|c| c.schemas.get(schema_name))
                {
                    let resolved = flatten_schema_ref(
                        &match schema_ref {
                            ReferenceOr::Item(s) => ReferenceOr::Item(s.clone()),
                            r @ ReferenceOr::Reference { .. } => r.clone(),
                        },
                        spec,
                        &full_name,
                        &is_required,
                    );
                    fields.extend(resolved);
                }
            }
        }
    }

    fields
}

/// Get a simple type name from a schema type.
fn type_name(typ: &Type) -> String {
    match typ {
        Type::String(_) => "string".into(),
        Type::Number(_) => "number".into(),
        Type::Integer(_) => "integer".into(),
        Type::Boolean(_) => "boolean".into(),
        Type::Array(_) => "array".into(),
        Type::Object(_) => "object".into(),
    }
}

/// Get a type name from a schema kind.
fn schema_type_name(kind: &SchemaKind) -> String {
    match kind {
        SchemaKind::Type(t) => type_name(t),
        _ => "string".into(),
    }
}

/// Display operations in a human-readable table.
pub fn describe_operations(ops: &[ApiOperation]) {
    if ops.is_empty() {
        println!("  (no operations found)");
        return;
    }

    for op in ops {
        let params_str = if !op.path_params.is_empty() || !op.query_params.is_empty() {
            let pp: Vec<String> = op
                .path_params
                .iter()
                .map(|p| format!("{{{}}}", p.name))
                .collect();
            let qp: Vec<String> = op
                .query_params
                .iter()
                .map(|p| format!("{}?", p.name))
                .collect();
            let all: Vec<String> = pp.into_iter().chain(qp).collect();
            format!(" params=[{}]", all.join(", "))
        } else {
            String::new()
        };

        let body_str = op
            .request_body
            .as_ref()
            .map(|b| format!(" body=[{} fields]", b.fields.len()))
            .unwrap_or_default();

        println!(
            "  {:7} {:<40} {:<30}{}{}",
            op.method, op.path, op.operation_id, params_str, body_str
        );
        if !op.summary.is_empty() {
            println!("          {}", op.summary);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PETSTORE: &str = r#"
openapi: "3.0.0"
info:
  title: Petstore
  version: 1.0.0
paths:
  /pets:
    get:
      operationId: listPets
      summary: List all pets
      tags: [pets]
      parameters:
        - name: limit
          in: query
          schema:
            type: integer
      responses:
        "200":
          description: A list of pets
    post:
      operationId: createPet
      summary: Create a pet
      tags: [pets]
      requestBody:
        required: true
        content:
          application/json:
            schema:
              type: object
              required: [name]
              properties:
                name:
                  type: string
                tag:
                  type: string
      responses:
        "201":
          description: Created
  /pets/{petId}:
    get:
      operationId: showPetById
      summary: Info for a specific pet
      tags: [pets]
      parameters:
        - name: petId
          in: path
          required: true
          schema:
            type: integer
      responses:
        "200":
          description: A pet
"#;

    #[test]
    fn parse_petstore() {
        let (_, ops) = parse_spec(PETSTORE).unwrap();
        assert_eq!(ops.len(), 3);

        let list = ops.iter().find(|o| o.operation_id == "listPets").unwrap();
        assert_eq!(list.method, "GET");
        assert_eq!(list.path, "/pets");
        assert_eq!(list.query_params.len(), 1);
        assert_eq!(list.query_params[0].name, "limit");

        let create = ops.iter().find(|o| o.operation_id == "createPet").unwrap();
        assert_eq!(create.method, "POST");
        assert!(create.request_body.is_some());
        let body = create.request_body.as_ref().unwrap();
        assert_eq!(body.fields.len(), 2); // name, tag
        assert!(body.fields.iter().any(|f| f.name == "name" && f.required));

        let show = ops
            .iter()
            .find(|o| o.operation_id == "showPetById")
            .unwrap();
        assert_eq!(show.method, "GET");
        assert_eq!(show.path_params.len(), 1);
        assert_eq!(show.path_params[0].name, "petId");
    }
}
