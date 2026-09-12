use azalea::account::{Account, AccountTrait};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use std::fmt::Debug;
use uuid::Uuid;

/// An account authenticated via a pre-existing Minecraft access token (e.g., from Xbox/Mojang login).
#[derive(Debug, Clone)]
pub struct CustomTokenAccount {
    pub username: String,
    pub uuid: Uuid,
    pub access_token: String,
}

impl AccountTrait for CustomTokenAccount {
    fn username(&self) -> &str {
        &self.username
    }

    fn uuid(&self) -> Uuid {
        self.uuid
    }

    fn access_token(&self) -> Option<String> {
        Some(self.access_token.clone())
    }

    fn join<'a>(
        &'a self,
        public_key: &'a [u8],
        private_key: &'a [u8; 16],
        server_id: &'a str,
        proxy: Option<reqwest::Proxy>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(), azalea_auth::sessionserver::ClientSessionServerError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            tracing::info!(
                "Sending join request to Mojang sessionserver for player '{}' (uuid: {})...",
                self.username,
                self.uuid
            );
            let res = azalea_auth::sessionserver::join(azalea_auth::sessionserver::SessionServerJoinOpts {
                access_token: &self.access_token,
                public_key,
                private_key,
                uuid: &self.uuid,
                server_id,
                proxy,
            })
            .await;

            match &res {
                Ok(_) => tracing::info!("Mojang sessionserver join successful!"),
                Err(e) => {
                    tracing::error!("Mojang sessionserver join failed: {e:?}");
                    tracing::error!("NOTE: 'ForbiddenOperation' means your MC_TOKEN has expired or is invalid.");
                    tracing::error!("Please generate a fresh access token from your launcher/auth script and update MC_TOKEN in .env!");
                }
            }

            res
        })
    }
}

impl CustomTokenAccount {
    /// Create a new CustomTokenAccount with explicit username, uuid, and token.
    pub fn new(username: impl Into<String>, uuid: Uuid, access_token: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            uuid,
            access_token: access_token.into(),
        }
    }

    /// Automatically construct an account from a Minecraft JWT access token by extracting
    /// the profile name and UUID from the token payload.
    pub fn from_jwt(jwt_token: &str) -> Result<Self, anyhow::Error> {
        let parts: Vec<&str> = jwt_token.split('.').collect();
        if parts.len() < 2 {
            anyhow::bail!("Invalid JWT: expected at least header and payload separated by '.'");
        }

        // Add padding if missing
        let mut payload_b64 = parts[1].to_string();
        while payload_b64.len() % 4 != 0 {
            payload_b64.push('=');
        }

        let payload_bytes = URL_SAFE_NO_PAD
            .decode(parts[1])
            .or_else(|_| base64::engine::general_purpose::STANDARD.decode(&payload_b64))
            .map_err(|e| anyhow::anyhow!("Failed to base64 decode JWT payload: {e}"))?;

        #[derive(Deserialize)]
        struct ProfileData {
            #[serde(default)]
            name: String,
            #[serde(default)]
            id: String,
        }

        #[derive(Deserialize)]
        struct JwtPayload {
            #[serde(default)]
            pfd: Vec<ProfileData>,
            #[serde(default)]
            profiles: Option<serde_json::Value>,
            #[serde(default)]
            exp: Option<u64>,
        }

        let payload: JwtPayload = serde_json::from_slice(&payload_bytes)
            .map_err(|e| anyhow::anyhow!("Failed to parse JWT payload JSON: {e}"))?;

        if let Some(exp) = payload.exp {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if exp <= now {
                let expired_mins_ago = (now - exp) / 60;
                tracing::warn!(
                    "⚠️ WARNING: The provided MC_TOKEN expired ~{} minute(s) ago! (exp: {}, now: {}). Mojang will reject server joins with 'ForbiddenOperation'. Please update MC_TOKEN in .env with a fresh token.",
                    expired_mins_ago, exp, now
                );
            } else {
                let remaining_mins = (exp - now) / 60;
                tracing::info!("MC_TOKEN is valid for ~{} more minute(s).", remaining_mins);
            }
        }

        let mut username = String::new();
        let mut uuid_str = String::new();

        for p in &payload.pfd {
            if !p.name.is_empty() {
                username = p.name.clone();
            }
            if !p.id.is_empty() {
                uuid_str = p.id.clone();
            }
        }

        if uuid_str.is_empty() {
            if let Some(profiles) = payload.profiles {
                if let Some(mc_id) = profiles.get("mc").and_then(|v| v.as_str()) {
                    uuid_str = mc_id.to_string();
                }
            }
        }

        if username.is_empty() {
            username = "AzaleaBot".to_string();
        }

        let uuid = if uuid_str.is_empty() {
            Uuid::new_v4()
        } else {
            Uuid::parse_str(&uuid_str)
                .unwrap_or_else(|_| Uuid::parse_str(&uuid_str.replace('-', "")).unwrap_or_else(|_| Uuid::new_v4()))
        };

        Ok(Self {
            username,
            uuid,
            access_token: jwt_token.to_string(),
        })
    }

    /// Convert this custom account into an Azalea `Account`.
    pub fn into_azalea_account(self) -> Account {
        self.into()
    }
}

/// Configured representation of a Minecraft account before connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountConfig {
    Microsoft(String),
    Token(String),
    Offline(String),
}

impl AccountConfig {
    pub fn description(&self) -> String {
        match self {
            AccountConfig::Microsoft(email) => format!("Microsoft ({email})"),
            AccountConfig::Token(token) => {
                if let Ok(acc) = CustomTokenAccount::from_jwt(token) {
                    format!("Token (Profile: {}, UUID: {})", acc.username, acc.uuid)
                } else {
                    "Token (configured; value hidden)".to_string()
                }
            }
            AccountConfig::Offline(name) => format!("Offline ({name})"),
        }
    }
}

/// Account lists never silently fall back to unauthenticated offline login.
fn parse_account_entry(value: &str) -> Result<AccountConfig, anyhow::Error> {
    let value = value.trim();
    if let Some(username) = value.strip_prefix("offline:") {
        let username = username.trim();
        anyhow::ensure!(!username.is_empty() && username.len() <= 16
            && username.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
            "Offline usernames must contain 1-16 letters, digits, or underscores.");
        return Ok(AccountConfig::Offline(username.to_string()));
    }
    if value.contains('@') {
        let mut parts = value.split('@');
        let local = parts.next().unwrap_or_default();
        let domain = parts.next().unwrap_or_default();
        anyhow::ensure!(!local.is_empty() && !domain.is_empty() && parts.next().is_none()
            && !value.chars().any(char::is_whitespace),
            "Invalid Microsoft email. Enter the complete email, including @, in ACCOUNTS or MICROSOFT_EMAIL.");
        return Ok(AccountConfig::Microsoft(value.to_string()));
    }
    if value.starts_with("ey") || value.len() > 50 {
        return Ok(AccountConfig::Token(value.to_string()));
    }
    anyhow::bail!("Invalid account entry: use a complete Microsoft email including @, or a Minecraft access token. Offline login requires --offline USERNAME or offline:USERNAME and only works on offline-mode servers.")
}

/// Discover all configured accounts from CLI arguments and environment variables (.env).
pub fn discover_accounts(
    cli_token: Option<&str>,
    cli_microsoft: Option<&str>,
    cli_offline: Option<&str>,
) -> Result<Vec<AccountConfig>, anyhow::Error> {
    let mut list = Vec::new();

    // Priority 1: Explicit CLI Microsoft email
    if let Some(email) = cli_microsoft {
        if !email.trim().is_empty() {
            let account = parse_account_entry(email)?;
            anyhow::ensure!(matches!(account, AccountConfig::Microsoft(_)), "--microsoft requires a complete email including @.");
            list.push(account);
        }
    }

    // Priority 2: Explicit CLI Token
    if let Some(tok) = cli_token {
        if !tok.trim().is_empty() {
            list.push(AccountConfig::Token(tok.trim().to_string()));
        }
    }

    // Priority 3: ACCOUNTS in .env (comma-separated accounts: emails or tokens)
    if let Ok(accounts_env) = std::env::var("ACCOUNTS") {
        for item in accounts_env.split(',') {
            let trimmed = item.trim();
            if trimmed.is_empty() {
                continue;
            }
            list.push(parse_account_entry(trimmed)
                .map_err(|err| anyhow::anyhow!("ACCOUNTS: {err}"))?);
        }
    }

    // Priority 4: Numbered accounts in .env (ACCOUNT_1, ACCOUNT_2, etc. and ACCOUNT_TOKEN_1, ...)
    for i in 1..=20 {
        if let Ok(val) = std::env::var(format!("ACCOUNT_{i}")) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                list.push(parse_account_entry(trimmed)
                    .map_err(|err| anyhow::anyhow!("ACCOUNT_{i}: {err}"))?);
            }
        }
        if let Ok(tok) = std::env::var(format!("ACCOUNT_TOKEN_{i}")) {
            let trimmed = tok.trim();
            if !trimmed.is_empty() {
                list.push(AccountConfig::Token(trimmed.to_string()));
            }
        }
    }

    // Priority 5: MICROSOFT_EMAIL in .env
    if let Ok(email) = std::env::var("MICROSOFT_EMAIL") {
        let trimmed = email.trim();
        if !trimmed.is_empty() {
            let acc = parse_account_entry(trimmed)?;
            anyhow::ensure!(matches!(acc, AccountConfig::Microsoft(_)), "MICROSOFT_EMAIL requires a complete email including @.");
            if !list.contains(&acc) {
                list.push(acc);
            }
        }
    }

    // Priority 6: MC_TOKEN / TOKEN in .env
    let env_token = std::env::var("MC_TOKEN").or_else(|_| std::env::var("TOKEN")).ok();
    if let Some(tok) = env_token {
        let trimmed = tok.trim();
        if !trimmed.is_empty() {
            let acc = AccountConfig::Token(trimmed.to_string());
            if !list.contains(&acc) {
                list.push(acc);
            }
        }
    }

    // Priority 7: Explicit CLI Offline username
    if let Some(name) = cli_offline {
        if !name.trim().is_empty() {
            list.push(parse_account_entry(&format!("offline:{}", name.trim()))?);
        }
    }

    // Remove duplicates while preserving order
    let mut unique = Vec::new();
    for acc in list {
        if !unique.contains(&acc) {
            unique.push(acc);
        }
    }

    anyhow::ensure!(!unique.is_empty(),
        "No accounts configured. Edit .env: set ACCOUNTS=your_actual_email@example.com (including @), or set MC_TOKEN to a Minecraft access token. Save the file and restart. Offline login requires explicit --offline USERNAME.");
    Ok(unique)
}

/// Select an account from the discovered list using selector flag or env var (index or email/name).
pub fn select_account(accounts: &[AccountConfig], selector: Option<&str>) -> Result<AccountConfig, anyhow::Error> {
    let first = accounts.first().ok_or_else(|| anyhow::anyhow!("No accounts configured; edit .env before launching."))?;
    let Some(sel) = selector.map(str::trim).filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all")) else {
        return Ok(first.clone());
    };
    if let Ok(index) = sel.parse::<usize>() {
        return accounts.get(index).cloned().ok_or_else(||
            anyhow::anyhow!("ACCOUNT index is out of range. Use a zero-based index from 0 to {}.", accounts.len() - 1));
    }
    for acc in accounts {
        match acc {
            AccountConfig::Microsoft(name) | AccountConfig::Offline(name) if name.eq_ignore_ascii_case(sel) => return Ok(acc.clone()),
            _ => {}
        }
    }
    if sel.contains('@') { return parse_account_entry(sel); }
    anyhow::bail!("ACCOUNT does not match a configured account. Use its zero-based index or complete Microsoft email.")
}

/// Perform authentication for the selected AccountConfig.
pub async fn authenticate_account(config: AccountConfig) -> Result<Account, anyhow::Error> {
    match config {
        AccountConfig::Microsoft(email) => {
            tracing::info!("============================================================");
            tracing::info!("*** MICROSOFT AUTHENTICATION: '{}' ***", email);
            tracing::info!("Authenticating via Microsoft OAuth (browser device login / cached session)...");
            tracing::info!("If this is your first time logging in with this account, check the terminal for the device login code!");
            tracing::info!("============================================================");
            let acc = Account::microsoft(&email).await?;
            tracing::info!("Microsoft authentication successful! Username: '{}', UUID: {}", acc.username(), acc.uuid());
            Ok(acc)
        }
        AccountConfig::Token(token) => {
            tracing::info!("Using token authentication...");
            match CustomTokenAccount::from_jwt(&token) {
                Ok(custom_acc) => {
                    tracing::info!(
                        "Successfully parsed token! Profile Name: '{}', UUID: {}",
                        custom_acc.username, custom_acc.uuid
                    );
                    Ok(custom_acc.into_azalea_account())
                }
                Err(e) => {
                    tracing::error!("Failed to parse token payload: {e}");
                    tracing::info!("Creating account with raw token string...");
                    Ok(CustomTokenAccount::new("AzaleaBot", Uuid::new_v4(), token).into_azalea_account())
                }
            }
        }
        AccountConfig::Offline(username) => {
            tracing::info!("Using offline mode for '{}'...", username);
            Ok(Account::offline(&username))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_jwt_token() {
        let token = "eyJraWQiOiIwNDkxODEiLCJhbGciOiJSUzI1NiJ9.eyJ4dWlkIjoiMjUzNTQwODk0NTU4OTY3NyIsImFnZyI6IkFkdWx0Iiwic3ViIjoiZDk1ZDQ4ZTAtZTk5Ny00ZGMyLTg1ZmItODNjMDg0YTQwNDYzIiwiYXV0aCI6IlhCT1giLCJucyI6ImRlZmF1bHQiLCJyb2xlcyI6W10sImlzcyI6ImF1dGhlbnRpY2F0aW9uIiwiZmxhZ3MiOlsibXVsdGlwbGF5ZXIiXSwicHJvZmlsZXMiOnsibWMiOiIyOTNkMTQyMC0wYWZlLTQyOTUtODI5Ni04OTYwMTA3ZWEzMWMifSwicG1pZCI6ImM2ODdmZWM1LTQzYTYtNTMwMS1iMzgzLWJlZjI1OGM2NzVjZSIsInBsYXRmb3JtIjoiUENfTEFVTkNIRVIiLCJ0aWQiOiJFOTlCMCIsInBmZCI6W3sidHlwZSI6Im1jIiwiaWQiOiIyOTNkMTQyMC0wYWZlLTQyOTUtODI5Ni04OTYwMTA3ZWEzMWMiLCJuYW1lIjoiVG91ZlRvdWZfNjQ3OTUwIn1dLCJ4aWQiOiIyNTM1NDA4OTQ1NTg5Njc3IiwibmJmIjoxNzg4ODA3MDMwLCJleHAiOjE3ODg4OTM0MzAsImlhdCI6MTc4ODgwNzAzMCwiYWlkIjoiMDAwMDAwMDAtMDAwMC0wMDAwLTAwMDAtMDAwMDQwMmI1MzI4In0.CHY2gbpgyLR3j0l8UHWXL7J9JvHYXgOQxupmBnNKfAjgyg73XWRyexy8XDR4iBQ10QcYaXbDamEEFql-5C8r_bsnNlxzjz9q0VJzm7EsjewJyWJZxsk58fH-qpexTdm_42iRYppWP5ye4mUK8smaoJ7vCBLu1HjTNerBpCHUXWhvbSoqrgBoQFtvOdMJeS88FPPGO9OT28ejYyKTaUmmbDCZK4_wcmMo7e9cqAYSLUp578xqMrwIf3mFE3qcQS9gWbFE7u1dLMZBzoARDCO-lnU-JguRyinuQvAfHD_ODd4vCxpLJPnf7Up4es2AeWTAgNPHk_dMdBNhjvvwAVmiUA";
        let account = CustomTokenAccount::from_jwt(token).expect("Failed to parse JWT");
        assert_eq!(account.username, "ToufTouf_647950");
        assert_eq!(account.uuid, Uuid::parse_str("293d1420-0afe-4295-8296-8960107ea31c").unwrap());

        let azalea_acc = account.into_azalea_account();
        println!("azalea_acc: {:?}", azalea_acc);
        println!("azalea_acc.access_token: {:?}", azalea_acc.access_token());
    }

    #[test]
    fn malformed_email_never_becomes_offline_account() {
        for value in ["BilluBackAgain8888Outlook.com", "ExampleBot", "", "user@@outlook.com", "user @outlook.com"] {
            assert!(parse_account_entry(value).is_err());
        }
        assert_eq!(parse_account_entry("user@outlook.com").unwrap(), AccountConfig::Microsoft("user@outlook.com".into()));
    }

    #[test]
    fn offline_login_requires_explicit_valid_username() {
        assert_eq!(parse_account_entry("offline:LocalBot").unwrap(), AccountConfig::Offline("LocalBot".into()));
        assert!(parse_account_entry("offline:bad.name").is_err());
        assert!(parse_account_entry("offline:ThisNameIsFarTooLong").is_err());
    }

    #[test]
    fn missing_or_invalid_selector_cannot_start_another_account() {
        assert!(select_account(&[], None).is_err());
        let accounts = vec![AccountConfig::Microsoft("user@outlook.com".into())];
        assert!(select_account(&accounts, Some("1")).is_err());
        assert!(select_account(&accounts, Some("typo")).is_err());
        assert_eq!(select_account(&accounts, Some("all")).unwrap(), accounts[0]);
    }

    #[test]
    fn invalid_token_description_does_not_expose_token() {
        let token = "not-a-valid-token-with-private-content";
        assert_eq!(AccountConfig::Token(token.into()).description(), "Token (configured; value hidden)");
    }

    #[test]
    fn test_multi_account_discovery_and_selection() {
        let accounts = vec![
            AccountConfig::Microsoft("user1@outlook.com".to_string()),
            AccountConfig::Microsoft("user2@outlook.com".to_string()),
            AccountConfig::Offline("OfflineBot".to_string()),
        ];

        // Test index 0
        let sel0 = select_account(&accounts, Some("0")).unwrap();
        assert_eq!(sel0, AccountConfig::Microsoft("user1@outlook.com".to_string()));

        // Test index 1
        let sel1 = select_account(&accounts, Some("1")).unwrap();
        assert_eq!(sel1, AccountConfig::Microsoft("user2@outlook.com".to_string()));

        // Test by email
        let sel_email = select_account(&accounts, Some("user2@outlook.com")).unwrap();
        assert_eq!(sel_email, AccountConfig::Microsoft("user2@outlook.com".to_string()));

        // Test default
        let sel_default = select_account(&accounts, None).unwrap();
        assert_eq!(sel_default, AccountConfig::Microsoft("user1@outlook.com".to_string()));
    }
}
