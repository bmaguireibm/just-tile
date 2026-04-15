use aws_config::{BehaviorVersion, SdkConfig};
use aws_credential_types::provider::ProvideCredentials;
use aws_sigv4::http_request::{sign, SignableBody, SignableRequest, SigningSettings};
use aws_sigv4::sign::v4;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::RwLock;
use url::Url;

#[derive(Clone)]
pub struct S3AuthManager {
    default_config: SdkConfig,
    /// Maps endpoint domains (e.g., "my-minio.com") to an AWS profile name
    endpoint_mapping: HashMap<String, String>,
    /// Cache of profile configs to avoid loading from disk on every request
    profile_configs: Arc<RwLock<HashMap<String, SdkConfig>>>,
}

impl S3AuthManager {
    pub async fn new(mapping_file: Option<&str>) -> Self {
        // Load default config (reads AWS_ACCESS_KEY_ID from env, or default profile from ~/.aws/credentials)
        let default_config = aws_config::load_defaults(BehaviorVersion::latest()).await;

        let mut endpoint_mapping = HashMap::new();
        if let Some(path) = mapping_file {
            if let Ok(data) = std::fs::read_to_string(path) {
                if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&data) {
                    endpoint_mapping = map;
                    println!("Loaded {} custom endpoint mappings from {}", endpoint_mapping.len(), path);
                } else {
                    eprintln!("Failed to parse endpoint mapping JSON at {}", path);
                }
            } else {
                eprintln!("Failed to read endpoint mapping file at {}", path);
            }
        }

        Self {
            default_config,
            endpoint_mapping,
            profile_configs: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn get_config<'a>(&'a self, override_profile: Option<&'a String>, url: &'a str) -> SdkConfig {
        let profile_name = if let Some(p) = override_profile {
            Some(p.clone())
        } else if let Ok(parsed_url) = Url::parse(url) {
            if let Some(host) = parsed_url.host_str() {
                self.endpoint_mapping.get(host).cloned()
            } else {
                None
            }
        } else {
            None
        };

        let profile_name = match profile_name {
            Some(p) => p,
            None => return self.default_config.clone(),
        };

        {
            let configs = self.profile_configs.read().await;
            if let Some(cfg) = configs.get(&profile_name) {
                return cfg.clone();
            }
        }

        // Lazy load the profile config
        let profile_config = aws_config::defaults(BehaviorVersion::latest()).profile_name(&profile_name).load().await;
        let mut configs = self.profile_configs.write().await;
        configs.insert(profile_name.clone(), profile_config.clone());
        profile_config
    }

    pub async fn sign(
        &self,
        mut builder: reqwest::RequestBuilder,
        url_str: &str,
        aws_profile: Option<&String>,
    ) -> Result<reqwest::RequestBuilder, String> {
        let config = self.get_config(aws_profile, url_str).await;

        let credentials_provider = config.credentials_provider();
        if credentials_provider.is_none() {
            // No credentials configured, just return the unauthenticated request builder
            return Ok(builder);
        }

        let credentials = match credentials_provider.unwrap().provide_credentials().await {
            Ok(c) => c,
            Err(_) => return Ok(builder),
        };

        let region = config
            .region()
            .map(|r| r.as_ref().to_string())
            .unwrap_or_else(|| "us-east-1".to_string());

        let built_req = builder
            .try_clone()
            .ok_or_else(|| "Cannot clone request builder".to_string())?
            .build()
            .map_err(|e| e.to_string())?;

        let method = built_req.method().as_str();

        let mut headers_vec = vec![];
        for (k, v) in built_req.headers().iter() {
            let key = k.as_str();
            let val = v.to_str().unwrap_or("");
            headers_vec.push((key, val));
        }

        let signable = SignableRequest::new(
            method,
            url_str,
            headers_vec.iter().map(|(k, v)| (*k, *v)), // Convert to exactly (&str, &str)
            SignableBody::Bytes(&[]),                  // No payload for GET requests
        )
        .map_err(|e| format!("Failed to create signable request: {}", e))?;

        let credentials_identity = credentials.into();
        let params = v4::SigningParams::builder()
            .identity(&credentials_identity)
            .region(&region)
            .name("s3")
            .time(SystemTime::now())
            .settings(SigningSettings::default())
            .build()
            .map_err(|e| format!("Failed to build signing params: {}", e))?;

        let (instructions, _) = sign(signable, &params.into())
            .map_err(|e| format!("Sign error: {}", e))?
            .into_parts();

        // Standard http::Request to apply instructions
        let mut dummy_req = http::Request::builder()
            .method(method)
            .uri(url_str)
            .body(())
            .unwrap();
            
        instructions.apply_to_request_http0x(&mut dummy_req);

        // Inject the newly generated authorization headers back into the original builder
        for (name, value) in dummy_req.headers() {
            builder = builder.header(name.as_str(), value.as_bytes());
        }

        Ok(builder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_sign_without_credentials_skips_signing() {
        // Clear any AWS environment variables that might interfere with tests
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        // By unsetting these, we simulate an unauthenticated environment.
        
        // This will build a SdkConfig natively trying to find credentials
        let auth_manager = S3AuthManager::new(None).await;
        
        let client = reqwest::Client::new();
        let builder = client.get("https://s3.amazonaws.com/test/data.tif");

        // Attempt to sign
        let signed_builder = auth_manager.sign(builder, "https://s3.amazonaws.com/test/data.tif", None)
            .await
            .expect("Signing should not fail, just return the unauthenticated builder");
            
        let request = signed_builder.build().unwrap();
        
        // The Authorization header should NOT be present
        assert!(!request.headers().contains_key("Authorization"));
        assert!(!request.headers().contains_key("x-amz-date"));
    }

    #[tokio::test]
    async fn test_sign_with_credentials_adds_auth() {
        // Set fake AWS environment variables to simulate an authenticated environment
        std::env::set_var("AWS_ACCESS_KEY_ID", "fake_key");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "fake_secret");
        std::env::set_var("AWS_REGION", "us-west-2");
        
        // The SdkConfig will pick up these variables natively
        let auth_manager = S3AuthManager::new(None).await;
        
        let client = reqwest::Client::new();
        let builder = client.get("https://s3.amazonaws.com/test/data.tif");

        // Attempt to sign
        let signed_builder = auth_manager.sign(builder, "https://s3.amazonaws.com/test/data.tif", None)
            .await
            .expect("Signing should succeed");
            
        let request = signed_builder.build().unwrap();
        
        // The Authorization and Date headers should now be present due to sigv4 instructions
        assert!(request.headers().contains_key("Authorization"));
        assert!(request.headers().contains_key("x-amz-date"));
        
        // Cleanup env
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        std::env::remove_var("AWS_REGION");
    }
}

