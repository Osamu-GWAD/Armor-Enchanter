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
                Err(e) => tracing::error!("Mojang sessionserver join failed: {e:?}"),
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
        }

        let payload: JwtPayload = serde_json::from_slice(&payload_bytes)
            .map_err(|e| anyhow::anyhow!("Failed to parse JWT payload JSON: {e}"))?;

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
}
