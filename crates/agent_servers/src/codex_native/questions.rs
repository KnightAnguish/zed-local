use super::events::text;
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result, ensure};
use serde_json::{Value, json};

pub(super) fn is_blocking(params: &Value) -> bool {
    // Older app-server versions did not send this field. Preserve their blocking behavior.
    params["isBlocking"].as_bool().unwrap_or(true)
}

pub(super) fn request(params: &Value) -> Result<acp::CreateElicitationRequest> {
    let questions = params["questions"]
        .as_array()
        .context("Missing Codex questions")?;
    ensure!(
        !questions.is_empty() && questions.len() <= 16,
        "Invalid Codex question count"
    );
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for question in questions {
        ensure!(
            question["isSecret"].as_bool() != Some(true),
            "Secret answers require a protected input UI; nothing submitted"
        );
        let id = text(question, "id")?;
        ensure!(!properties.contains_key(id), "Duplicate Codex question id");
        let mut field = json!({"type":"string","minLength":1,"title":text(question,"header")?,"description":text(question,"question")?});
        if let Some(options) = question["options"]
            .as_array()
            .filter(|options| !options.is_empty())
        {
            let choices = options
                .iter()
                .map(|option| {
                    let label = text(option, "label")?;
                    Ok(json!({"const":label,"title":label,"description":text(option,"description")?}))
                })
                .collect::<Result<Vec<_>>>()?;
            if question["isOther"].as_bool() == Some(true) {
                // Suggestions are not a JSON Schema enum: a custom answer remains valid.
                field["_meta"] = json!({acp_thread::ELICITATION_SUGGESTIONS_META_KEY:choices});
                field["default"] = json!(text(&options[0], "label")?);
            } else {
                field["oneOf"] = json!(choices);
            }
        }
        properties.insert(id.to_owned(), field);
        required.push(id);
    }
    let schema = serde_json::from_value(
        json!({"type":"object","properties":properties,"required":required}),
    )?;
    Ok(acp::CreateElicitationRequest::new(
        acp::ElicitationFormMode::new(
            acp::ElicitationSessionScope::new(acp::SessionId::new(text(params, "threadId")?)),
            schema,
        ),
        "Codex needs your input",
    )
    .meta(acp::Meta::from_iter([(
        acp_thread::CODEX_QUESTION_META_KEY.to_owned(),
        json!({"isBlocking":is_blocking(params)}),
    )])))
}

pub(super) fn mcp_request(params: &Value) -> Result<acp::CreateElicitationRequest> {
    let scope = acp::ElicitationSessionScope::new(acp::SessionId::new(text(params, "threadId")?));
    let message = format!(
        "{}: {}",
        text(params, "serverName")?,
        text(params, "message")?
    );
    match text(params, "mode")? {
        "form" => {
            let schema = &params["requestedSchema"];
            ensure!(schema["type"] == "object", "MCP form must be an object");
            let properties = schema["properties"]
                .as_object()
                .context("Missing MCP form properties")?;
            ensure!(
                properties.values().all(|field| matches!(
                    field["type"].as_str(),
                    Some("string" | "number" | "integer" | "boolean" | "array")
                )),
                "Unsupported MCP form field type"
            );
            Ok(acp::CreateElicitationRequest::new(
                acp::ElicitationFormMode::new(scope, serde_json::from_value(schema.clone())?),
                message,
            ))
        }
        "url" => Ok(acp::CreateElicitationRequest::new(
            acp::ElicitationUrlMode::new(
                scope,
                acp::ElicitationId::new(text(params, "elicitationId")?),
                text(params, "url")?,
            ),
            message,
        )),
        mode => anyhow::bail!("Unsupported MCP elicitation mode {mode}; nothing submitted"),
    }
}

pub(super) fn mcp_response(response: acp::CreateElicitationResponse) -> Result<Value> {
    Ok(match response.action {
        acp::ElicitationAction::Accept(action) => {
            json!({"action":"accept","content":action.content,"_meta":null})
        }
        acp::ElicitationAction::Decline => json!({"action":"decline","content":null,"_meta":null}),
        acp::ElicitationAction::Cancel => json!({"action":"cancel","content":null,"_meta":null}),
        _ => anyhow::bail!("Unsupported MCP elicitation response"),
    })
}

pub(super) fn response(params: &Value, response: acp::CreateElicitationResponse) -> Result<Value> {
    let mut answers = serde_json::Map::new();
    if let acp::ElicitationAction::Accept(action) = response.action {
        let content = action.content.context("Missing question answers")?;
        for question in params["questions"]
            .as_array()
            .context("Missing questions")?
        {
            let id = text(question, "id")?;
            let Some(acp::ElicitationContentValue::String(answer)) = content.get(id) else {
                anyhow::bail!("Missing or invalid answer to {id}");
            };
            ensure!(!answer.trim().is_empty(), "Empty answer to {id}");
            if question["isOther"].as_bool() != Some(true) {
                if let Some(options) = question["options"]
                    .as_array()
                    .filter(|options| !options.is_empty())
                {
                    ensure!(
                        options
                            .iter()
                            .any(|option| option["label"].as_str() == Some(answer)),
                        "Invalid choice for {id}"
                    );
                }
            }
            answers.insert(id.to_owned(), json!({"answers":[answer]}));
        }
    }
    Ok(json!({"answers":answers}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn form_and_answer_roundtrip_preserves_question_ids() {
        let params = json!({"threadId":"thread","questions":[{"id":"choice","header":"Choose","question":"Which?","isOther":false,"isSecret":false,"options":[{"label":"One","description":"First"}]}]});
        let form = serde_json::to_value(request(&params).unwrap()).unwrap();
        assert_eq!(
            form["requestedSchema"]["properties"]["choice"]["oneOf"],
            json!([{"const":"One","title":"One","description":"First"}])
        );
        assert_eq!(
            form["_meta"][acp_thread::CODEX_QUESTION_META_KEY]["isBlocking"],
            true
        );
        let accepted = acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
            acp::ElicitationAcceptAction::new().content(std::collections::BTreeMap::from([(
                "choice".to_owned(),
                "One".into(),
            )])),
        ));
        assert_eq!(
            response(&params, accepted).unwrap(),
            json!({"answers":{"choice":{"answers":["One"]}}})
        );
        assert_eq!(
            response(
                &params,
                acp::CreateElicitationResponse::new(acp::ElicitationAction::Cancel)
            )
            .unwrap(),
            json!({"answers":{}})
        );
    }
    #[test]
    fn background_questions_keep_suggestions_and_accept_custom_answers() {
        let params = json!({"threadId":"thread","isBlocking":false,"questions":[{"id":"choice","header":"Choose","question":"Which?","isOther":true,"options":[{"label":"One","description":"First"},{"label":"Two","description":"Second"}]}]});
        let form = serde_json::to_value(request(&params).unwrap()).unwrap();
        let field = &form["requestedSchema"]["properties"]["choice"];
        assert_eq!(
            form["_meta"][acp_thread::CODEX_QUESTION_META_KEY]["isBlocking"],
            false
        );
        assert_eq!(field["default"], "One");
        assert_eq!(
            field["_meta"][acp_thread::ELICITATION_SUGGESTIONS_META_KEY][1]["const"],
            "Two"
        );
        assert!(field.get("enum").is_none());
        assert!(field.get("oneOf").is_none());
        for answer in ["Two", "Something else"] {
            let accepted = acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
                acp::ElicitationAcceptAction::new().content(std::collections::BTreeMap::from([(
                    "choice".to_owned(),
                    answer.into(),
                )])),
            ));
            assert_eq!(
                response(&params, accepted).unwrap()["answers"]["choice"]["answers"],
                json!([answer])
            );
        }
    }

    #[test]
    fn restricted_choices_reject_custom_and_empty_answers() {
        let params = json!({"threadId":"thread","questions":[{"id":"choice","isOther":false,"options":[{"label":"One"}]}]});
        for answer in ["Other", " "] {
            let accepted = acp::CreateElicitationResponse::new(acp::ElicitationAction::Accept(
                acp::ElicitationAcceptAction::new().content(std::collections::BTreeMap::from([(
                    "choice".to_owned(),
                    answer.into(),
                )])),
            ));
            assert!(response(&params, accepted).is_err());
        }
    }
    #[test]
    fn secret_input_is_not_rendered_as_plain_text() {
        assert!(
            request(&json!({"threadId":"thread","questions":[{"id":"password","isSecret":true}]}))
                .is_err()
        );
    }
}
