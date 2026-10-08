use agent_client_protocol::schema::v1 as acp;
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(super) struct Choices {
    pub options: Vec<acp::PermissionOption>,
    replies: Vec<Value>,
    cancelled: Value,
}

impl Choices {
    pub fn new(method: &str, params: &Value) -> Result<Self> {
        let profile = method == "item/permissions/requestApproval";
        ensure!(
            profile
                || matches!(
                    method,
                    "item/commandExecution/requestApproval" | "item/fileChange/requestApproval"
                ),
            "Unsupported Codex server request {method}; nothing was approved"
        );
        let denied = if profile {
            json!({"permissions":{},"scope":"turn"})
        } else {
            json!({"decision":"decline"})
        };
        let mut choices = Self {
            options: Vec::new(),
            replies: Vec::new(),
            cancelled: if profile {
                denied.clone()
            } else {
                json!({"decision":"cancel"})
            },
        };
        if profile {
            let requested = params["permissions"]
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("Missing permission profile"))?;
            ensure!(
                requested
                    .keys()
                    .all(|key| matches!(key.as_str(), "network" | "fileSystem")),
                "Unknown permission profile field"
            );
            let permissions: serde_json::Map<String, Value> = requested
                .iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            choices.add(
                "Allow for this turn",
                acp::PermissionOptionKind::AllowOnce,
                json!({"permissions":permissions,"scope":"turn"}),
            );
            choices.add(
                "Allow for this session",
                acp::PermissionOptionKind::AllowAlways,
                json!({"permissions":permissions,"scope":"session"}),
            );
        } else {
            choices.add(
                "Allow once",
                acp::PermissionOptionKind::AllowOnce,
                json!({"decision":"accept"}),
            );
            choices.add(
                "Allow for this session",
                acp::PermissionOptionKind::AllowAlways,
                json!({"decision":"acceptForSession"}),
            );
            if method == "item/commandExecution/requestApproval" {
                if let Some(amendment) = params["proposedExecpolicyAmendment"]
                    .as_array()
                    .filter(|value| !value.is_empty())
                {
                    choices.add(&format!("Always allow command prefix: {}", serde_json::to_string(amendment)?), acp::PermissionOptionKind::AllowAlways,
                        json!({"decision":{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":amendment}}}));
                }
                if let Some(amendments) = params["proposedNetworkPolicyAmendments"].as_array() {
                    ensure!(
                        amendments.len() <= 32,
                        "Too many proposed network permission changes"
                    );
                    for amendment in amendments {
                        let kind = match amendment["action"].as_str() {
                            Some("allow") => acp::PermissionOptionKind::AllowAlways,
                            Some("deny") => acp::PermissionOptionKind::RejectAlways,
                            _ => anyhow::bail!("Unknown network permission action"),
                        };
                        choices.add(&format!("Remember network policy: {amendment}"), kind,
                            json!({"decision":{"applyNetworkPolicyAmendment":{"network_policy_amendment":amendment}}}));
                    }
                }
            }
        }
        choices.add("Reject", acp::PermissionOptionKind::RejectOnce, denied);
        Ok(choices)
    }

    fn add(&mut self, label: &str, kind: acp::PermissionOptionKind, reply: Value) {
        self.options.push(acp::PermissionOption::new(
            self.replies.len().to_string(),
            label.to_owned(),
            kind,
        ));
        self.replies.push(reply);
    }

    pub fn reply(&self, selected: Option<&str>) -> Result<Value> {
        match selected {
            None => Ok(self.cancelled.clone()),
            Some(id) => {
                let index: usize = id.parse()?;
                self.replies.get(index).cloned().ok_or_else(|| {
                    anyhow::anyhow!("Unknown Codex permission choice; nothing approved")
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_and_unknown_options_never_grant() {
        let choices = Choices::new("item/commandExecution/requestApproval", &json!({})).unwrap();
        assert_eq!(choices.reply(None).unwrap(), json!({"decision":"cancel"}));
        assert!(choices.reply(Some("999")).is_err());
        assert_eq!(
            choices.reply(Some("0")).unwrap(),
            json!({"decision":"accept"})
        );
        assert_eq!(
            choices.reply(Some("1")).unwrap(),
            json!({"decision":"acceptForSession"})
        );
        assert_eq!(
            choices.reply(Some("2")).unwrap(),
            json!({"decision":"decline"})
        );
    }

    #[test]
    fn profile_grants_only_requested_permissions_after_selection() {
        let params = json!({"permissions":{"network":{"enabled":true},"fileSystem":null}});
        let choices = Choices::new("item/permissions/requestApproval", &params).unwrap();
        assert_eq!(
            choices.reply(None).unwrap(),
            json!({"permissions":{},"scope":"turn"})
        );
        assert_eq!(
            choices.reply(Some("0")).unwrap(),
            json!({"permissions":{"network":{"enabled":true}},"scope":"turn"})
        );
        assert_eq!(choices.reply(Some("1")).unwrap()["scope"], "session");
    }

    #[test]
    fn amendments_are_explicit_and_preserve_the_server_proposal() {
        let choices = Choices::new(
            "item/commandExecution/requestApproval",
            &json!({"proposedExecpolicyAmendment":["git","status"]}),
        )
        .unwrap();
        assert_eq!(
            choices.reply(Some("2")).unwrap(),
            json!({"decision":{"acceptWithExecpolicyAmendment":{"execpolicy_amendment":["git","status"]}}})
        );
    }
}
