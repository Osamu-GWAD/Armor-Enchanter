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
                tracing::error!(
                    "❌ ERROR: The provided MC_TOKEN expired ~{} minute(s) ago! (exp: {}, now: {}).",
                    expired_mins_ago, exp, now
                );
                tracing::error!(
                    "❌ Mojang sessionserver will reject connection with 'ForbiddenOperation'."
                );
                tracing::error!(
                    "❌ Please generate a fresh access token or configure MICROSOFT_EMAIL in .env."
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
                    let preview = if token.len() > 16 {
                        format!("{}...", &token[..16])
                    } else {
                        token.clone()
                    };
                    format!("Token ({preview})")
                }
            }
            AccountConfig::Offline(name) => format!("Offline ({name})"),
        }
    }
}

/// Discover all configured accounts from CLI arguments and environment variables (.env).
pub fn discover_accounts(
    cli_token: Option<&str>,
    cli_microsoft: Option<&str>,
    cli_offline: Option<&str>,
) -> Vec<AccountConfig> {
    let mut list = Vec::new();

    // Priority 1: Explicit CLI Microsoft email
    if let Some(email) = cli_microsoft {
        if !email.trim().is_empty() {
            list.push(AccountConfig::Microsoft(email.trim().to_string()));
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
            if trimmed.contains('@') {
                list.push(AccountConfig::Microsoft(trimmed.to_string()));
            } else if trimmed.starts_with("ey") || trimmed.len() > 50 {
                list.push(AccountConfig::Token(trimmed.to_string()));
            } else {
                list.push(AccountConfig::Offline(trimmed.to_string()));
            }
        }
    }

    // Priority 4: Numbered accounts in .env (ACCOUNT_1, ACCOUNT_2, etc. and ACCOUNT_TOKEN_1, ...)
    for i in 1..=20 {
        if let Ok(val) = std::env::var(format!("ACCOUNT_{i}")) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                if trimmed.contains('@') {
                    list.push(AccountConfig::Microsoft(trimmed.to_string()));
                } else if trimmed.starts_with("ey") || trimmed.len() > 50 {
                    list.push(AccountConfig::Token(trimmed.to_string()));
                } else {
                    list.push(AccountConfig::Offline(trimmed.to_string()));
                }
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
            let acc = AccountConfig::Microsoft(trimmed.to_string());
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
            list.push(AccountConfig::Offline(name.trim().to_string()));
        }
    }

    // Remove duplicates while preserving order
    let mut unique = Vec::new();
    for acc in list {
        if !unique.contains(&acc) {
            unique.push(acc);
        }
    }

    if unique.is_empty() {
        unique.push(AccountConfig::Offline("EnchanterBot".to_string()));
    }

    unique
}

/// Select an account from the discovered list using selector flag or env var (index or email/name).
pub fn select_account(accounts: &[AccountConfig], selector: Option<&str>) -> AccountConfig {
    if accounts.is_empty() {
        return AccountConfig::Offline("EnchanterBot".to_string());
    }

    if let Some(sel) = selector {
        let trimmed = sel.trim();
        // Check if numeric index (e.g. 0, 1, 2)
        if let Ok(idx) = trimmed.parse::<usize>() {
            if idx < accounts.len() {
                return accounts[idx].clone();
            } else if idx > 0 && (idx - 1) < accounts.len() {
                return accounts[idx - 1].clone();
            }
        }

        // Check if matches email or description
        for acc in accounts {
            match acc {
                AccountConfig::Microsoft(email) if email.eq_ignore_ascii_case(trimmed) => {
                    return acc.clone();
                }
                AccountConfig::Offline(name) if name.eq_ignore_ascii_case(trimmed) => {
                    return acc.clone();
                }
                _ => {}
            }
        }

        // Direct email provided
        if trimmed.contains('@') {
            return AccountConfig::Microsoft(trimmed.to_string());
        }
    }

    // Default to first account
    accounts[0].clone()
}

fn get_minecraft_cache_file() -> std::path::PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        std::path::PathBuf::from(appdata).join(".minecraft").join("azalea-auth.json")
    } else if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        std::path::PathBuf::from(home).join(".minecraft").join("azalea-auth.json")
    } else {
        std::path::PathBuf::from("azalea-auth.json")
    }
}

/// Authenticate a Microsoft account with a prominent login banner, automatic browser opening, and caching.
pub async fn authenticate_microsoft(email: &str) -> Result<Account, anyhow::Error> {
    let cache_file = get_minecraft_cache_file();

    // 1. Check if a valid, unexpired Minecraft auth session already exists in cache
    if let Some(cached) = azalea_auth::cache::get_account_in_cache(&cache_file, email).await {
        if !cached.mca.is_expired() {
            tracing::info!(
                "Found valid cached Minecraft session for '{}' (Player: '{}', UUID: {})! Reusing session...",
                email, cached.profile.name, cached.profile.id
            );
            return Account::microsoft(email).await.map_err(|e| anyhow::anyhow!(e));
        }
        if !cached.msa.is_expired() {
            tracing::info!("Cached Minecraft token expired, but Microsoft MSA token is still valid. Refreshing session for '{}'...", email);
            if let Ok(acc) = Account::microsoft(email).await {
                return Ok(acc);
            }
        }
    }

    // 2. Interactive device login required: Request fresh device code
    let client = reqwest::Client::new();
    let res = azalea_auth::get_ms_link_code(&client, None, None)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to request Microsoft device login code: {e}"))?;

    let link = format!("{}?otc={}", res.verification_uri, res.user_code);

    eprintln!();
    eprintln!("╔══════════════════════════════════════════════════════════════════════════════╗");
    eprintln!("║                    🔑 MICROSOFT LOGIN REQUIRED 🔑                            ║");
    eprintln!("╠══════════════════════════════════════════════════════════════════════════════╣");
    eprintln!("║                                                                              ║");
    eprintln!("║  Please authorize the bot by opening this link in your web browser:         ║");
    eprintln!("║                                                                              ║");
    eprintln!("║  👉 {:<72} ║", link);
    eprintln!("║                                                                              ║");
    eprintln!("║  Account : {:<65} ║", email);
    eprintln!("║  Code    : {:<65} ║", res.user_code);
    eprintln!("║                                                                              ║");
    eprintln!("║  (Opening link in your default browser... If it does not open automatically, ║");
    eprintln!("║   copy and paste the link above into your browser)                           ║");
    eprintln!("║                                                                              ║");
    eprintln!("║  Waiting for you to log in and approve access in your browser...             ║");
    eprintln!("╚══════════════════════════════════════════════════════════════════════════════╝");
    eprintln!();

    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/c", "start", "", &link])
            .spawn();
    }

    let msa = azalea_auth::get_ms_auth_token(&client, res, None)
        .await
        .map_err(|e| anyhow::anyhow!("Microsoft authorization timed out or failed: {e}"))?;

    let acc = Account::with_microsoft_access_token(msa.clone())
        .await
        .map_err(|e| anyhow::anyhow!("Failed to initialize Minecraft account: {e}"))?;

    // Cache the session so subsequent runs don't require browser login
    let msa_token = &msa.data.access_token;
    if let Ok(mc_token_res) = azalea_auth::get_minecraft_token(&client, msa_token).await {
        if let Ok(profile) = azalea_auth::get_profile(&client, &mc_token_res.minecraft_access_token).await {
            let _ = azalea_auth::cache::set_account_in_cache(
                &cache_file,
                email,
                azalea_auth::cache::CachedAccount {
                    cache_key: email.to_string(),
                    mca: mc_token_res.mca,
                    msa,
                    xbl: mc_token_res.xbl,
                    profile,
                },
            )
            .await;
        }
    }

    Ok(acc)
}

/// Perform authentication for the selected AccountConfig.
pub async fn authenticate_account(config: AccountConfig) -> Result<Account, anyhow::Error> {
    match config {
        AccountConfig::Microsoft(email) => {
            let acc = authenticate_microsoft(&email).await?;
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
    fn test_multi_account_discovery_and_selection() {
        let accounts = vec![
            AccountConfig::Microsoft("user1@outlook.com".to_string()),
            AccountConfig::Microsoft("user2@outlook.com".to_string()),
            AccountConfig::Offline("OfflineBot".to_string()),
        ];

        // Test index 0
        let sel0 = select_account(&accounts, Some("0"));
        assert_eq!(sel0, AccountConfig::Microsoft("user1@outlook.com".to_string()));

        // Test index 1
        let sel1 = select_account(&accounts, Some("1"));
        assert_eq!(sel1, AccountConfig::Microsoft("user2@outlook.com".to_string()));

        // Test by email
        let sel_email = select_account(&accounts, Some("user2@outlook.com"));
        assert_eq!(sel_email, AccountConfig::Microsoft("user2@outlook.com".to_string()));

        // Test default
        let sel_default = select_account(&accounts, None);
        assert_eq!(sel_default, AccountConfig::Microsoft("user1@outlook.com".to_string()));
    }

    #[test]
    fn test_discover_accounts_from_env() {
        let _ = dotenvy::dotenv();
        let accounts = discover_accounts(None, None, None);
        println!("Discovered accounts count: {}", accounts.len());
        for (i, acc) in accounts.iter().enumerate() {
            println!("  [{i}] {}", acc.description());
        }
    }
}
