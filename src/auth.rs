use azalea::account::{Account, AccountTrait};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::Deserialize;
use std::fmt::Debug;
use uuid::Uuid;

/// An account authenticated via a pre-existing Minecraft access token (e.g., from Xbox/Mojang login).
#[derive(Clone)]
pub struct CustomTokenAccount {
    pub username: String,
    pub uuid: Uuid,
    pub access_token: String,
}

impl Debug for CustomTokenAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomTokenAccount")
            .field("username", &self.username)
            .field("uuid", &self.uuid)
            .field("access_token", &"[redacted]")
            .finish()
    }
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
                Err(_) => {
                    tracing::error!("Mojang sessionserver join failed.");
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
                anyhow::bail!("MC_TOKEN has expired; generate a fresh token or configure MICROSOFT_EMAIL");
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
#[derive(Clone, PartialEq, Eq)]
pub enum AccountConfig {
    Microsoft(String),
    Token(String),
    Offline(String),
}

impl Debug for AccountConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Microsoft(email) => f.debug_tuple("Microsoft").field(email).finish(),
            Self::Token(_) => f.debug_tuple("Token").field(&"[redacted]").finish(),
            Self::Offline(name) => f.debug_tuple("Offline").field(name).finish(),
        }
    }
}

// Pass the parent's exact account without putting token credentials in process arguments.
pub const CHILD_ACCOUNT_KIND_ENV: &str = "RELIABILITY_CHILD_ACCOUNT_KIND";
pub const CHILD_ACCOUNT_VALUE_ENV: &str = "RELIABILITY_CHILD_ACCOUNT_VALUE";

impl AccountConfig {
    pub fn description(&self) -> String {
        match self {
            AccountConfig::Microsoft(email) => format!("Microsoft ({email})"),
            AccountConfig::Token(_) => "Token (configured)".to_string(),
            AccountConfig::Offline(name) => format!("Offline ({name})"),
        }
    }

    pub fn child_env_parts(&self) -> (&'static str, &str) {
        match self {
            AccountConfig::Microsoft(value) => ("microsoft", value),
            AccountConfig::Token(value) => ("token", value),
            AccountConfig::Offline(value) => ("offline", value),
        }
    }

    fn from_child_env_parts(kind: &str, value: String) -> anyhow::Result<Self> {
        if value.trim().is_empty() {
            anyhow::bail!("Child account credential is empty");
        }
        match kind {
            "microsoft" => Ok(Self::Microsoft(value)),
            "token" => Ok(Self::Token(value)),
            "offline" => Ok(Self::Offline(value)),
            _ => anyhow::bail!("Unknown child account kind"),
        }
    }
}

pub fn child_account_from_env() -> anyhow::Result<Option<AccountConfig>> {
    use std::env::VarError;
    let kind = std::env::var(CHILD_ACCOUNT_KIND_ENV);
    let value = std::env::var(CHILD_ACCOUNT_VALUE_ENV);
    match (kind, value) {
        (Err(VarError::NotPresent), Err(VarError::NotPresent)) => Ok(None),
        (Ok(kind), Ok(value)) => AccountConfig::from_child_env_parts(&kind, value).map(Some),
        _ => anyhow::bail!("Incomplete or invalid child account configuration"),
    }
}

/// Discover all configured accounts from CLI arguments and environment variables (.env).
pub fn discover_accounts(
    cli_token: Option<&str>,
    cli_microsoft: Option<&str>,
    cli_offline: Option<&str>,
) -> Vec<AccountConfig> {
    let mut list = Vec::new();

    // Explicit CLI credentials precede environment accounts, with offline override first.
    if let Some(name) = cli_offline {
        if !name.trim().is_empty() {
            list.push(AccountConfig::Offline(name.trim().to_string()));
        }
    }

    // Explicit CLI Microsoft email
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

/// Select a configured account by zero-based index or email/name.
pub fn select_account(accounts: &[AccountConfig], selector: Option<&str>) -> anyhow::Result<AccountConfig> {
    if accounts.is_empty() {
        anyhow::bail!("No accounts are configured");
    }

    if let Some(sel) = selector {
        let trimmed = sel.trim();
        // Indices shown in startup logs are zero-based.
        if let Ok(idx) = trimmed.parse::<usize>() {
            if idx < accounts.len() {
                return Ok(accounts[idx].clone());
            }
            anyhow::bail!("Account index {idx} is out of range (0..{})", accounts.len());
        }

        // Check if matches email or description
        for acc in accounts {
            match acc {
                AccountConfig::Microsoft(email) if email.eq_ignore_ascii_case(trimmed) => {
                    return Ok(acc.clone());
                }
                AccountConfig::Offline(name) if name.eq_ignore_ascii_case(trimmed) => {
                    return Ok(acc.clone());
                }
                _ => {}
            }
        }

        anyhow::bail!("Account selector does not match a configured account");
    }

    // Default to first account
    Ok(accounts[0].clone())
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
                    // Malformed JWT-like tokens must not silently become opaque accounts.
                    if token.split('.').count() >= 3 || token.starts_with("eyJ") {
                        return Err(anyhow::anyhow!("Invalid or expired JWT access token: {e}"));
                    }
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
        let expected_uuid = Uuid::parse_str("293d1420-0afe-4295-8296-8960107ea31c").unwrap();
        let payload = serde_json::json!({
            "pfd": [{"name": "TestUser", "id": expected_uuid.to_string()}]
        });
        let token = format!(
            "e30.{}.unused-signature",
            URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes())
        );
        let account = CustomTokenAccount::from_jwt(&token).expect("Failed to parse JWT");
        assert_eq!(account.username, "TestUser");
        assert_eq!(account.uuid, expected_uuid);
        let azalea_acc = account.into_azalea_account();
        assert!(azalea_acc.access_token().is_some());
    }

    #[tokio::test]
    async fn expired_jwt_is_rejected() {
        let payload = serde_json::json!({"exp": 1u64});
        let token = format!(
            "e30.{}.unused-signature",
            URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes())
        );
        let error = CustomTokenAccount::from_jwt(&token).unwrap_err();
        assert!(error.to_string().contains("expired"));
        assert!(authenticate_account(AccountConfig::Token(token)).await.is_err());
    }

    #[tokio::test]
    async fn malformed_jwt_is_not_used_as_an_opaque_token() {
        assert!(authenticate_account(AccountConfig::Token("e30.invalid.signature".into())).await.is_err());
        assert!(authenticate_account(AccountConfig::Token("opaque-token".into())).await.is_ok());
        assert!(authenticate_account(AccountConfig::Token("opaque.token".into())).await.is_ok());
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
        assert!(select_account(&accounts, Some("3")).is_err());
        assert!(select_account(&accounts, Some("unknown")).is_err());
        assert!(select_account(&accounts, Some("other@outlook.com")).is_err());
        assert!(select_account(&accounts, Some(" ")).is_err());
        assert!(select_account(&[], None).is_err());
    }

    #[test]
    fn explicit_cli_offline_precedes_environment_accounts() {
        let accounts = discover_accounts(None, None, Some("CliOffline"));
        assert_eq!(accounts[0], AccountConfig::Offline("CliOffline".to_string()));
        assert_eq!(select_account(&accounts, None).unwrap(), accounts[0]);
    }

    #[test]
    fn child_account_round_trip_preserves_account_type_and_credential() {
        for account in [
            AccountConfig::Microsoft("user@outlook.com".to_string()),
            AccountConfig::Token("opaque-token-value".to_string()),
            AccountConfig::Offline("OfflineBot".to_string()),
        ] {
            let (kind, value) = account.child_env_parts();
            assert_eq!(AccountConfig::from_child_env_parts(kind, value.to_string()).unwrap(), account);
        }
        assert!(AccountConfig::from_child_env_parts("token", String::new()).is_err());
        assert!(AccountConfig::from_child_env_parts("unknown", "secret".to_string()).is_err());
        let token = AccountConfig::Token("opaque-token-value".to_string());
        assert!(!token.description().contains("opaque-token-value"));
        assert!(!format!("{token:?}").contains("opaque-token-value"));
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
