// Adapted from openai/codex 8c076942, Apache-2.0; see NOTICE.md.
//! The production synchronous reviewer's output schema and tolerant JSON parser.
//! Parsing defaults and error text are shared unchanged by all reviewer callers.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// Structured output contract that the guardian reviewer must satisfy.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct GuardianAssessment {
    pub risk_level: GuardianRiskLevel,
    pub user_authorization: GuardianUserAuthorization,
    pub outcome: GuardianAssessmentOutcome,
    pub rationale: String,
}

/// The model is asked for strict JSON, but we still accept a surrounding prose
/// wrapper so transient formatting drift fails less noisily during dogfooding.
/// Non-JSON output is still a review failure; this is only a thin recovery path
/// for cases where the model wrapped the JSON in extra prose.
pub fn parse_guardian_assessment(text: &str) -> anyhow::Result<GuardianAssessment> {
    let parsed_payload =
        if let Ok(payload) = serde_json::from_str::<GuardianAssessmentPayload>(text) {
            payload
        } else if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
            && start < end
            && let Some(slice) = text.get(start..=end)
        {
            serde_json::from_str::<GuardianAssessmentPayload>(slice)?
        } else {
            anyhow::bail!("guardian assessment was not valid JSON");
        };

    let outcome = parsed_payload.outcome;
    let risk_level = parsed_payload.risk_level.unwrap_or(match outcome {
        GuardianAssessmentOutcome::Allow => GuardianRiskLevel::Low,
        GuardianAssessmentOutcome::Deny => GuardianRiskLevel::High,
    });
    let rationale = parsed_payload
        .rationale
        .filter(|rationale| !rationale.trim().is_empty())
        .unwrap_or_else(|| match outcome {
            GuardianAssessmentOutcome::Allow => {
                "Auto-review returned a low-risk allow decision.".to_string()
            }
            GuardianAssessmentOutcome::Deny => {
                "Auto-review returned a deny decision without a rationale.".to_string()
            }
        });

    Ok(GuardianAssessment {
        risk_level,
        user_authorization: parsed_payload
            .user_authorization
            .unwrap_or(GuardianUserAuthorization::Unknown),
        outcome,
        rationale,
    })
}

#[derive(Deserialize)]
struct GuardianAssessmentPayload {
    risk_level: Option<GuardianRiskLevel>,
    user_authorization: Option<GuardianUserAuthorization>,
    outcome: GuardianAssessmentOutcome,
    rationale: Option<String>,
}

/// JSON schema supplied as `final_output_json_schema` to guide a structured
/// final answer from the guardian review session.
///
/// Keep this next to `guardian_output_contract_prompt()` so the prompt text and
/// output schema stay aligned.
pub fn guardian_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "risk_level": {
                "type": "string",
                "enum": ["low", "medium", "high", "critical"]
            },
            "user_authorization": {
                "type": "string",
                "enum": ["unknown", "low", "medium", "high"]
            },
            "outcome": {
                "type": "string",
                "enum": ["allow", "deny"]
            },
            "rationale": {
                "type": "string"
            }
        },
        "required": ["outcome"]
    })
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GuardianAssessmentOutcome {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GuardianRiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GuardianUserAuthorization {
    Unknown,
    Low,
    Medium,
    High,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upstream_contract() {
        let a = parse_guardian_assessment(r#"{"outcome":"allow"}"#).unwrap();
        assert_eq!(a.risk_level, GuardianRiskLevel::Low);
        assert_eq!(a.user_authorization, GuardianUserAuthorization::Unknown);
        assert_eq!(
            parse_guardian_assessment("```json\n{\"outcome\":\"deny\"}\n```")
                .unwrap()
                .risk_level,
            GuardianRiskLevel::High
        );
        for s in [
            "allow",
            "{}",
            r#"{"outcome":"maybe"}"#,
            r#"{"outcome":"allow","risk_level":"invalid"}"#,
        ] {
            assert!(parse_guardian_assessment(s).is_err());
        }
    }
}
