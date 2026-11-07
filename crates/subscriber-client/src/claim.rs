//! Redeem one cabinet claim and return the artefact JSON.
//!
//! The artefact string is the only success value. Callers write it to the
//! device file. This module does not log the claim or the artefact.

use std::time::Duration;

use serde_json::Value;

const POLL_BUDGET: Duration = Duration::from_secs(90);

/// Redeem `claim` at `panel_origin` (`https://host`, no trailing slash required)
/// and return the artefact document as JSON.
///
/// Errors are short tokens: `empty`, `transport`, `unknown`, `spent`,
/// `refused`, `pending`, `empty-artefact`.
pub async fn redeem_artefact_json(panel_origin: &str, claim: &str) -> Result<String, String> {
    let claim = claim.trim();
    if claim.is_empty() || claim.len() > 512 {
        return Err("empty".to_owned());
    }
    let origin = panel_origin.trim().trim_end_matches('/');
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| "transport".to_owned())?;

    let (status, body) = post_json(
        &client,
        &format!("{origin}/public/claims/redeem"),
        serde_json::json!({ "claim": claim }),
    )
    .await?;
    if let Some(token) = http_token(status) {
        return Err(token);
    }
    let poll = body
        .get("poll")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "refused".to_owned())?
        .to_owned();

    let started = std::time::Instant::now();
    loop {
        if started.elapsed() >= POLL_BUDGET {
            return Err("pending".to_owned());
        }
        let (status, body) = post_json(
            &client,
            &format!("{origin}/public/claims/poll"),
            serde_json::json!({ "poll": poll }),
        )
        .await?;
        if status == 429 {
            let rest = POLL_BUDGET.saturating_sub(started.elapsed());
            if rest.is_zero() {
                return Err("pending".to_owned());
            }
            tokio::time::sleep(Duration::from_secs(2).min(rest)).await;
            continue;
        }
        if let Some(token) = http_token(status) {
            return Err(token);
        }
        let state = body.get("status").and_then(|value| value.as_str()).unwrap_or("");
        if state == "ready" {
            let document = body
                .get("artefact")
                .cloned()
                .filter(|value| !value.is_null())
                .or_else(|| body.get("packet").cloned().filter(|value| !value.is_null()))
                .ok_or_else(|| "empty-artefact".to_owned())?;
            return serde_json::to_string(&document).map_err(|_| "refused".to_owned());
        }
        if state == "pending" {
            let rest = POLL_BUDGET.saturating_sub(started.elapsed());
            if rest.is_zero() {
                return Err("pending".to_owned());
            }
            tokio::time::sleep(Duration::from_secs(1).min(rest)).await;
            continue;
        }
        return Err("refused".to_owned());
    }
}

fn http_token(status: u16) -> Option<String> {
    match status {
        200 | 202 => None,
        404 => Some("unknown".to_owned()),
        409 => Some("spent".to_owned()),
        _ => Some("refused".to_owned()),
    }
}

async fn post_json(client: &reqwest::Client, url: &str, body: Value) -> Result<(u16, Value), String> {
    let response = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|_| "transport".to_owned())?;
    let status = response.status().as_u16();
    let text = response.text().await.map_err(|_| "transport".to_owned())?;
    let value = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    Ok((status, value))
}
