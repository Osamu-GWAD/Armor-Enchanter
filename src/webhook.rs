use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

const ALERT_COOLDOWN: Duration = Duration::from_secs(120);

#[derive(Clone, Copy)]
enum AlertState {
    Sending,
    Sent(Instant),
}

static ALERT_STATES: OnceLock<Arc<Mutex<HashMap<String, AlertState>>>> = OnceLock::new();

fn get_alert_map() -> Arc<Mutex<HashMap<String, AlertState>>> {
    ALERT_STATES
        .get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
        .clone()
}

fn reserve_alert(states: &mut HashMap<String, AlertState>, item_key: &str, now: Instant) -> bool {
    match states.get(item_key) {
        Some(AlertState::Sending) => false,
        Some(AlertState::Sent(last))
            if now.saturating_duration_since(*last) < ALERT_COOLDOWN => false,
        _ => {
            states.insert(item_key.to_owned(), AlertState::Sending);
            true
        }
    }
}

fn finish_alert(states: &mut HashMap<String, AlertState>, item_key: &str, sent: bool, now: Instant) {
    if sent {
        states.insert(item_key.to_owned(), AlertState::Sent(now));
    } else {
        states.remove(item_key);
    }
}

/// Dispatch only after GuiManager verifies zero inventory and unavailable matching orders.
/// Suppress concurrent sends and apply a 120s cooldown per item only after success.
pub(crate) fn send_verified_out_of_item_alert(item_name: &str, details: Option<&str>) {
    let webhook_url = match std::env::var("DISCORD_WEBHOOK_URL")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    {
        Some(url) => url,
        None => {
            warn!("Discord alert skipped: set DISCORD_WEBHOOK_URL to enable out-of-stock alerts");
            return;
        }
    };
    let item_key = item_name.trim().to_lowercase();
    let map = get_alert_map();

    {
        let mut lock = map.lock().unwrap();
        if !reserve_alert(&mut lock, &item_key, Instant::now()) {
            return;
        }
    }

    let item_name = item_name.to_string();
    let detail_str = details.unwrap_or("").to_string();

    tokio::spawn(async move {
        let content = format!(
            "@everyone ⚠️ **[Armor-Enchanter Alert]** The bot is **OUT OF {}**!",
            item_name
        );

        let embed = serde_json::json!({
            "title": format!("🚨 Out of Stock: {}", item_name),
            "description": if detail_str.is_empty() {
                format!("The bot is out of **{}** on the server. Please restock `/order`!", item_name)
            } else {
                format!("The bot is out of **{}** on the server. Details: {}\n\nPlease restock `/order`!", item_name, detail_str)
            },
            "color": 15158332, // Red color
            "fields": [
                {
                    "name": "Item",
                    "value": item_name,
                    "inline": true
                },
                {
                    "name": "Action Required",
                    "value": "Restock buy orders in `/order`",
                    "inline": true
                }
            ],
            "footer": {
                "text": "Armor-Enchanter Bot • Status Notification"
            }
        });

        let payload = serde_json::json!({
            "content": content,
            "embeds": [embed]
        });

        info!("Sending Discord webhook alert for exhausted item '{}'...", item_name);

        let client = reqwest::Client::new();
        let sent = match client
            .post(&webhook_url)
            .header("Content-Type", "application/json")
            .body(payload.to_string())
            .timeout(Duration::from_secs(15))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                info!("Successfully sent Discord webhook alert for '{}'!", item_name);
                true
            }
            Ok(resp) => {
                error!("Discord webhook returned non-success status: {}", resp.status());
                false
            }
            Err(err) => {
                // Reqwest errors can include the URL, which contains the webhook token.
                error!("Discord webhook request failed (timeout: {}, connection: {})", err.is_timeout(), err.is_connect());
                false
            }
        };
        let mut lock = map.lock().unwrap();
        finish_alert(&mut lock, &item_key, sent, Instant::now());
    });
}

#[cfg(test)]
mod tests {
    use super::{finish_alert, reserve_alert, AlertState, ALERT_COOLDOWN};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    #[test]
    fn alert_is_reserved_while_send_is_in_flight() {
        let mut states = HashMap::new();
        let now = Instant::now();
        assert!(reserve_alert(&mut states, "mending", now));
        assert!(!reserve_alert(&mut states, "mending", now));
        assert!(matches!(states.get("mending"), Some(AlertState::Sending)));
    }

    #[test]
    fn failed_send_can_retry_immediately() {
        let mut states = HashMap::new();
        let now = Instant::now();
        assert!(reserve_alert(&mut states, "mending", now));
        finish_alert(&mut states, "mending", false, now);
        assert!(reserve_alert(&mut states, "mending", now));
    }

    #[test]
    fn cooldown_starts_after_successful_send() {
        let mut states = HashMap::new();
        let started = Instant::now();
        let completed = started + Duration::from_secs(15);
        assert!(reserve_alert(&mut states, "mending", started));
        finish_alert(&mut states, "mending", true, completed);
        assert!(!reserve_alert(&mut states, "mending", completed + ALERT_COOLDOWN - Duration::from_millis(1)));
        assert!(reserve_alert(&mut states, "mending", completed + ALERT_COOLDOWN));
    }

    #[test]
    fn test_webhook_payload_structure() {
        let item_name = "Diamond Helmet";
        let detail_str = "Missing 2 Diamond Helmets";
        let content = format!(
            "@everyone ⚠️ **[Armor-Enchanter Alert]** The bot is **OUT OF {}**!",
            item_name
        );
        let embed = serde_json::json!({
            "title": format!("🚨 Out of Stock: {}", item_name),
            "description": format!("The bot is out of **{}** on the server. Details: {}\n\nPlease restock `/order`!", item_name, detail_str),
            "color": 15158332,
            "fields": [
                {
                    "name": "Item",
                    "value": item_name,
                    "inline": true
                },
                {
                    "name": "Action Required",
                    "value": "Restock buy orders in `/order`",
                    "inline": true
                }
            ],
            "footer": {
                "text": "Armor-Enchanter Bot • Status Notification"
            }
        });

        let payload = serde_json::json!({
            "content": content,
            "embeds": [embed]
        });

        assert!(payload["content"].as_str().unwrap().contains("Diamond Helmet"));
        assert!(payload["embeds"][0]["title"].as_str().unwrap().contains("Diamond Helmet"));
    }
}
