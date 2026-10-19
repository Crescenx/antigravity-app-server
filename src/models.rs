use crate::*;

pub fn effort(params: &Value) -> Option<&Value> {
    params
        .get("reasoningEffort")
        .or(params.get("effort"))
        .or(params["collaborationMode"]["settings"].get("reasoning_effort"))
}

/// Decode advertised ID suffixes; invocation still uses the exact account ID.
pub fn split(model: &str) -> (&str, Option<&str>) {
    for (suffix, effort) in [
        ("-extra-low", "minimal"),
        ("-extra-high", "xhigh"),
        ("-thinking", "high"),
        ("-minimal", "minimal"),
        ("-low", "low"),
        ("-medium", "medium"),
        ("-high", "high"),
        ("-xhigh", "xhigh"),
    ] {
        if let Some(base) = model.strip_suffix(suffix) {
            return (base, Some(effort));
        }
    }
    (model, None)
}

pub fn select(current: &str, params: &Value) -> Result<String> {
    let settings = &params["collaborationMode"]["settings"];
    let requested = params["model"].as_str().or(settings["model"].as_str());
    for value in [params.get("model"), settings.get("model")]
        .into_iter()
        .flatten()
    {
        ensure!(
            value.is_null()
                || value
                    .as_str()
                    .is_some_and(|s| !s.is_empty() && s.len() <= 512),
            "model must be nonempty text or null"
        );
    }
    let (current_base, current_effort) = split(current);
    let model = requested.unwrap_or(current);
    let (base, encoded) = split(model);
    let effort = effort(params);
    let Some(effort) = effort.filter(|v| !v.is_null()) else {
        // UI round trips use the canonical name, while storage keeps the native ID.
        return Ok(
            if base == current_base && encoded.is_none() && current_effort.is_some() {
                current.to_owned()
            } else {
                model.to_owned()
            },
        );
    };
    let effort = effort
        .as_str()
        .context("reasoningEffort must be text or null")?;
    ensure!(
        ["none", "minimal", "low", "medium", "high", "xhigh"].contains(&effort),
        "unsupported reasoning effort"
    );
    ensure!(
        requested.is_none() || encoded.is_none_or(|e| e == effort),
        "model suffix conflicts with reasoningEffort"
    );
    if encoded == Some(effort) {
        return Ok(model.into());
    }
    if effort == "none" {
        return Ok(base.into());
    }
    bail!("reasoning variant requires the account model catalog")
}

#[derive(Clone)]
pub struct Catalog {
    pub entries: Vec<Value>,
    pub variants: Vec<String>,
}

impl Catalog {
    pub fn resolve(&self, current: &str, params: &Value) -> Result<String> {
        let settings = &params["collaborationMode"]["settings"];
        for value in [params.get("model"), settings.get("model")]
            .into_iter()
            .flatten()
        {
            ensure!(
                value.is_null()
                    || value
                        .as_str()
                        .is_some_and(|s| !s.is_empty() && s.len() <= 512),
                "model must be nonempty text or null"
            );
        }
        let model_param = params["model"].as_str().or(settings["model"].as_str());
        let requested = model_param.unwrap_or(current);
        let (base, encoded) = split(requested);
        let encoded = model_param.and(encoded);
        let entry = self.entries.iter().find(|entry| entry["model"] == base);
        let explicit = effort(params).filter(|e| !e.is_null());
        if let Some(entry) = entry {
            let effort = explicit
                .map(|e| e.as_str().context("reasoningEffort must be text"))
                .transpose()?
                .or(encoded)
                .or_else(|| {
                    if split(current).0 == base && self.variants.iter().any(|v| v == current) {
                        Some(split(current).1.unwrap_or("none"))
                    } else {
                        None
                    }
                })
                .unwrap_or(
                    entry["defaultReasoningEffort"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .unwrap_or("none"),
                );
            ensure!(
                encoded.is_none_or(|e| e == effort),
                "model suffix conflicts with reasoningEffort"
            );
            for preferred in [requested, current] {
                let (b, e) = split(preferred);
                if b == base
                    && e.unwrap_or("none") == effort
                    && self.variants.iter().any(|v| v == preferred)
                {
                    return Ok(preferred.into());
                }
            }
            return self
                .variants
                .iter()
                .find(|raw| {
                    let (b, e) = split(raw);
                    b == base && e.unwrap_or("none") == effort
                })
                .cloned()
                .context("account does not advertise this reasoning variant");
        }
        select(current, params)
    }
    pub fn parse(raw: &Value) -> Result<Self> {
        let mut variants = Vec::new();
        let mut details = BTreeMap::new();
        let mut recommended = std::collections::BTreeSet::new();
        let default = raw["defaultAgentModelId"].as_str();
        if let Some(models) = raw["models"].as_object() {
            for sort in raw["agentModelSorts"].as_array().into_iter().flatten() {
                for group in sort["groups"].as_array().into_iter().flatten() {
                    for id in group["modelIds"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        if models.contains_key(id) && !variants.iter().any(|v| v == id) {
                            variants.push(id.to_owned());
                        }
                    }
                }
            }
            recommended.extend(variants.iter().cloned());
            for (id, info) in models {
                if !variants.contains(id) {
                    variants.push(id.clone());
                }
                details.insert(id.clone(), info.clone());
            }
        } else if let Some(models) = raw["models"].as_array() {
            for info in models {
                if !info["supportedGenerationMethods"]
                    .as_array()
                    .is_some_and(|methods| methods.iter().any(|m| m == "generateContent"))
                {
                    continue;
                }
                let id = info["name"]
                    .as_str()
                    .context("model entry missing name")?
                    .trim_start_matches("models/")
                    .to_owned();
                if !variants.contains(&id) {
                    variants.push(id.clone());
                }
                details.insert(id, info.clone());
            }
        } else {
            bail!("model catalog missing models");
        }
        ensure!(
            !variants.is_empty(),
            "account advertised no generation models"
        );
        let default = default
            .filter(|d| variants.iter().any(|v| v == d))
            .unwrap_or(&variants[0]);
        let mut entries: Vec<Value> = Vec::new();
        for native in &variants {
            let (base, effort) = split(native);
            let info = &details[native];
            let index = entries.iter().position(|m| m["model"] == base).unwrap_or_else(|| {
                let display = if effort.is_some() { base } else { info["displayName"].as_str().unwrap_or(base) };
                entries.push(json!({"id":base,"model":base,"displayName":display,"description":info["description"].as_str().unwrap_or("Account-advertised model; inference availability is not tested"),"hidden":!recommended.is_empty(),"isDefault":false,"supportedReasoningEfforts":[],"defaultReasoningEffort":effort.unwrap_or("none"),"upgrade":null,"upgradeInfo":null,"availabilityNux":null,"modelSpecialty":null,"inputModalities":["text"],"supportsPersonality":false,"multiAgentVersion":null,"additionalSpeedTiers":[],"serviceTiers":[],"defaultServiceTier":null,"availableAccessPrograms":null}));
                entries.len()-1
            });
            let entry = &mut entries[index];
            if recommended.contains(native) || native == default {
                entry["hidden"] = json!(false);
            }
            if native == default {
                entry["isDefault"] = json!(true);
                entry["defaultReasoningEffort"] = json!(effort.unwrap_or("none"));
            }
            // An unsuffixed sibling represents the explicit non-thinking choice.
            let e = effort.unwrap_or("none");
            let efforts = entry["supportedReasoningEfforts"].as_array_mut().unwrap();
            if !efforts.iter().any(|v| v["reasoningEffort"] == e) {
                efforts.push(json!({"reasoningEffort":e,"description":if native.ends_with("-thinking") {"Native thinking variant (high is the protocol alias, not a calibrated token budget)"}else{e}}));
            }
        }
        for entry in &mut entries {
            if entry["supportedReasoningEfforts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["reasoningEffort"] == "none")
            {
                entry["supportedReasoningEfforts"] = json!([]);
                entry["defaultReasoningEffort"] = json!("none");
            }
        }
        Ok(Self { entries, variants })
    }
}

fn empty_limit(id: &str, name: &str) -> Value {
    json!({"limitId":id,"limitName":name,"normalModelSlug":null,"primary":null,"secondary":null,"credits":null,"individualLimit":null,"spendControlReached":null,"planType":null,"rateLimitReachedType":null})
}

pub fn quota(raw: &Value) -> Value {
    let mut limits = json!({"antigravity-google":empty_limit("antigravity-google","Google models"),"antigravity-external":empty_limit("antigravity-external","External models")});
    for group in raw["groups"].as_array().into_iter().flatten() {
        let label = group["displayName"]
            .as_str()
            .or(group["display_name"].as_str())
            .unwrap_or("")
            .to_lowercase();
        let id = if label.contains("gemini") || label.contains("google") {
            "antigravity-google"
        } else if label.contains("third")
            || label.contains("external")
            || label.contains("claude")
            || label.contains("other")
        {
            "antigravity-external"
        } else {
            continue;
        };
        for bucket in group["buckets"].as_array().into_iter().flatten() {
            if bucket["disabled"] == true {
                continue;
            }
            let label = bucket["window"]
                .as_str()
                .or(bucket["displayName"]
                    .as_str()
                    .or(bucket["display_name"].as_str()))
                .unwrap_or("")
                .to_lowercase();
            let minutes = if label.contains("week") {
                10080
            } else if label == "5h"
                || label.contains("five hour")
                || label.contains('5') && (label.contains("hour") || label.contains("hr"))
            {
                300
            } else {
                continue;
            };
            let Some(remaining) = bucket["remainingFraction"]
                .as_f64()
                .or(bucket["remaining_fraction"].as_f64())
                .filter(|v| (0.0..=1.0).contains(v))
            else {
                continue;
            };
            let reset = bucket["resetTime"]
                .as_str()
                .or(bucket["reset_time"].as_str())
                .and_then(|s| {
                    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                        .ok()
                })
                .map(|t| t.unix_timestamp());
            limits[id][if minutes == 300 {
                "primary"
            } else {
                "secondary"
            }] = json!({"usedPercent":((1.0-remaining)*100.0).round() as i32,"windowDurationMins":minutes,"resetsAt":reset});
        }
    }
    json!({"rateLimits":limits["antigravity-google"],"rateLimitsByLimitId":limits})
}

pub async fn rate_limits(endpoint: &Value) -> Result<Value> {
    if endpoint["httpHeaders"]["X-ACP-Proxy-Token"].is_null() {
        return Ok(quota(&Value::Null));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(25))
        .build()?;
    let base = endpoint["baseUrl"]
        .as_str()
        .context("account endpoint missing")?
        .trim_end_matches('/');
    let response = client
        .get(format!("{base}/v1beta/account/rateLimits"))
        .header(
            "X-ACP-Proxy-Token",
            endpoint["httpHeaders"]["X-ACP-Proxy-Token"]
                .as_str()
                .context("invalid metadata token")?,
        )
        .send()
        .await
        .context("quota discovery request failed")?;
    ensure!(
        response.status().is_success(),
        "quota discovery rejected: HTTP {}",
        response.status()
    );
    Ok(quota(&response.json::<Value>().await?))
}

pub async fn fetch(endpoint: &Value) -> Result<Catalog> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let base = endpoint["baseUrl"]
        .as_str()
        .context("model endpoint missing")?
        .trim_end_matches('/');
    let mut combined = Vec::new();
    let mut cursor = String::new();
    let mut seen = std::collections::BTreeSet::new();
    loop {
        let mut request = client
            .get(format!("{base}/v1beta/models"))
            .header("x-goog-api-key", endpoint["apiKey"].as_str().unwrap_or(""));
        for (name, value) in endpoint["httpHeaders"].as_object().into_iter().flatten() {
            request = request.header(
                name,
                value.as_str().context("invalid model request header")?,
            );
        }
        if !cursor.is_empty() {
            request = request.query(&[("pageToken", &cursor)]);
        }
        let response = request
            .send()
            .await
            .context("model discovery request failed")?;
        ensure!(
            response.status().is_success(),
            "model discovery rejected: HTTP {}",
            response.status()
        );
        let raw: Value = response
            .json()
            .await
            .context("invalid model discovery response")?;
        if raw["models"].is_object() {
            return Catalog::parse(&raw);
        }
        combined.extend(
            raw["models"]
                .as_array()
                .context("model catalog missing models")?
                .iter()
                .cloned(),
        );
        cursor = raw["nextPageToken"].as_str().unwrap_or("").into();
        if cursor.is_empty() {
            return Catalog::parse(&json!({"models":combined}));
        }
        ensure!(
            seen.insert(cursor.clone()) && seen.len() <= 100,
            "invalid model catalog pagination"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_catalog_preserves_other_providers_and_round_trips_efforts() {
        let catalog = Catalog::parse(&json!({"defaultAgentModelId":"claude-opus-4-6-thinking","agentModelSorts":[{"groups":[{"modelIds":["claude-opus-4-6-thinking","gpt-oss-120b-medium"]}]}],"models":{"gemini-3-pro-high":{},"claude-opus-4-6-thinking":{},"claude-opus-4-6":{},"gpt-oss-120b-medium":{},"gpt-oss-120b-high":{}}})).unwrap();
        assert_eq!(catalog.entries.len(), 3);
        assert_eq!(catalog.entries[0]["model"], "claude-opus-4-6");
        assert_eq!(catalog.entries[0]["defaultReasoningEffort"], "high");
        assert!(catalog.entries[0]["isDefault"].as_bool().unwrap());
        for raw in &catalog.variants {
            let (base, effort) = split(raw);
            assert_eq!(
                catalog
                    .resolve(raw, &json!({"model":base,"reasoningEffort":effort}))
                    .unwrap(),
                *raw
            );
        }
        assert!(select("test", &json!({"reasoningEffort":"high"})).is_err());
        assert!(select("gpt-oss-120b-high", &json!({"reasoningEffort":"low"})).is_err());
        assert_eq!(
            select(
                "claude-opus-4-6-thinking",
                &json!({"model":"claude-opus-4-6","reasoningEffort":null})
            )
            .unwrap(),
            "claude-opus-4-6-thinking"
        );
    }

    #[test]
    fn quotas_keep_two_groups_and_two_windows_without_inventing_missing_data() {
        let variants = Catalog::parse(
            &json!({"models":{"claude-sonnet-5-5-low":{},"claude-sonnet-5-5-high":{}}}),
        )
        .unwrap();
        assert_eq!(
            variants
                .resolve("claude-sonnet-5-5-high", &json!({"effort":"low"}))
                .unwrap(),
            "claude-sonnet-5-5-low"
        );
        let raw = json!({"groups":[
            {"displayName":"Gemini Models","buckets":[{"window":"weekly","remainingFraction":0.4,"resetTime":"1970-01-01T00:00:10Z"},{"window":"5h","displayName":"Five Hour Limit Remaining","remainingFraction":0.75}]},
            {"displayName":"Claude and GPT models","buckets":[{"window":"weekly","remainingFraction":0.9},{"window":"5h","remainingFraction":0.5}]}
        ]});
        let result = quota(&raw);
        assert_eq!(
            result["rateLimitsByLimitId"]["antigravity-google"]["primary"]["usedPercent"],
            25
        );
        assert_eq!(
            result["rateLimitsByLimitId"]["antigravity-google"]["secondary"]["resetsAt"],
            10
        );
        assert_eq!(
            result["rateLimitsByLimitId"]["antigravity-external"]["primary"]["windowDurationMins"],
            300
        );
        assert_eq!(
            result["rateLimitsByLimitId"]["antigravity-external"]["secondary"]["usedPercent"],
            10
        );
        assert!(quota(&Value::Null)["rateLimits"]["primary"].is_null());
        let catalog=Catalog::parse(&json!({"models":{"claude-sonnet-5-5-low":{},"claude-sonnet-5-5-high":{},"gemini-4-argon-medium":{}}})).unwrap();
        assert_eq!(
            catalog
                .resolve(
                    "old",
                    &json!({"model":"claude-sonnet-5-5","reasoningEffort":"high"})
                )
                .unwrap(),
            "claude-sonnet-5-5-high"
        );
        assert!(
            catalog
                .resolve(
                    "old",
                    &json!({"model":"claude-sonnet-5-5","reasoningEffort":"medium"})
                )
                .is_err()
        );
        assert_eq!(catalog.resolve("old",&json!({"collaborationMode":{"settings":{"model":"gemini-4-argon","reasoning_effort":"medium"}}})).unwrap(),"gemini-4-argon-medium");
    }
}
