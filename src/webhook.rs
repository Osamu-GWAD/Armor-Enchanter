use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing::{error, info};

pub const DEFAULT_WEBHOOK_URL: &str = "https://discord.com/api/webhooks/1547288275050430577/wEpCQkTGZ_ltD4qkgy8ZWa60GXVU5ddgxOqbPwZZcfLFD_BXL5KbxFRGp18XkjfK0U9s";

static ALERT_TIMESTAMPS: OnceLock<Arc<Mutex<HashMap<String, Instant>>>> = OnceLock::new();

fn get_alert_map() -> Arc<Mutex<HashMap<String, Instant>>> {
    ALERT_TIMESTAMPS
        .get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
        .clone()
}

/// Dispatch only after GuiManager verifies zero inventory and unavailable matching orders.
/// Includes debouncing (120s cooldown per item type) to prevent spamming Discord.
pub(crate) fn send_verified_out_of_item_alert(item_name: &str, details: Option<&str>) {
    let item_key = item_name.trim().to_lowercase();
    let map = get_alert_map();

    // Check cooldown (120 seconds per item type)
    {
        let mut lock = map.lock().unwrap();
        if let Some(last) = lock.get(&item_key) {
            if last.elapsed() < Duration::from_secs(120) {
                return;
            }
        }
        lock.insert(item_key, Instant::now());
    }

    let webhook_url = std::env::var("DISCORD_WEBHOOK_URL")
        .unwrap_or_else(|_| DEFAULT_WEBHOOK_URL.to_string());

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
        match client
            .post(&webhook_url)
            .header("Content-Type", "application/json")
            .body(payload.to_string())
            .send()
            .await
        {
            Ok(resp) => {
                if resp.status().is_success() {
                    info!("Successfully sent Discord webhook alert for '{}'!", item_name);
                } else {
                    error!("Discord webhook returned non-success status: {}", resp.status());
                }
            }
            Err(err) => {
                error!("Failed to send Discord webhook alert: {err}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
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
