use thiserror::Error;
use tonic::{
    metadata::{errors::InvalidMetadataValue, Ascii, MetadataKey, MetadataValue},
    service::Interceptor,
    transport::{Channel, Endpoint},
    Request, Status,
};

pub const DEFAULT_API_KEY_HEADER: &str = "x-localshred-auth";

#[derive(Debug, Error)]
pub enum LocalShredConnectionError {
    #[error("transport error: {0}")]
    Transport(#[from] tonic::transport::Error),

    #[error("client error: {0}")]
    Client(#[from] Status),

    #[error("invalid api key header name: {0}")]
    InvalidApiKeyHeader(String),

    #[error("invalid api key value: {0}")]
    InvalidApiKey(String),
}

pub type LocalShredConnectionResult<T> = Result<T, LocalShredConnectionError>;

/// Adds the api key to each request's headers.
#[derive(Clone)]
pub struct ApiKeyInterceptor {
    header: MetadataKey<Ascii>,
    api_key: MetadataValue<Ascii>,
}

impl ApiKeyInterceptor {
    pub fn new(header: &str, api_key: &str) -> LocalShredConnectionResult<Self> {
        if api_key.trim().is_empty() {
            return Err(LocalShredConnectionError::InvalidApiKey(
                "api key is empty".to_string(),
            ));
        }

        let header = MetadataKey::<Ascii>::from_bytes(header.to_lowercase().as_bytes())
            .map_err(|e| LocalShredConnectionError::InvalidApiKeyHeader(e.to_string()))?;
        let api_key: MetadataValue<Ascii> =
            api_key.parse().map_err(|e: InvalidMetadataValue| {
                LocalShredConnectionError::InvalidApiKey(e.to_string())
            })?;

        Ok(Self { header, api_key })
    }
}

impl Interceptor for ApiKeyInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert(self.header.clone(), self.api_key.clone());

        Ok(request)
    }
}

pub async fn create_grpc_channel(url: String) -> LocalShredConnectionResult<Channel> {
    let endpoint = match url.starts_with("https") {
        true => Endpoint::from_shared(url)
            .map_err(LocalShredConnectionError::Transport)?
            .tls_config(tonic::transport::ClientTlsConfig::new().with_enabled_roots())?,
        false => Endpoint::from_shared(url).map_err(LocalShredConnectionError::Transport)?,
    };
    Ok(endpoint.connect().await?)
}
