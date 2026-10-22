use super::types::*;
use std::{collections::BTreeMap, future::Future, time::Duration};

#[derive(Clone, Debug)]
pub struct TransportConfig {
    /// None explicitly disables environment/system proxy discovery.
    pub proxy: Option<String>,
    pub max_response_bytes: usize,
    /// At most two additional attempts, restricted to idempotent requests.
    pub retries: u8,
}
impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            proxy: None,
            max_response_bytes: 8 * 1024 * 1024,
            retries: 2,
        }
    }
}

pub struct ReqwestTransport {
    client: reqwest::Client,
    redirect_client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
    config: TransportConfig,
}
impl ReqwestTransport {
    pub fn new(config: TransportConfig) -> Result<Self, ProviderError> {
        if config.max_response_bytes == 0
            || config.max_response_bytes > 64 * 1024 * 1024
            || config.retries > 2
        {
            return Err(ProviderError::invalid(
                "response bound must be 1..=64 MiB and retries <= 2",
            ));
        }
        let build_client = |redirects| {
            let mut builder = reqwest::Client::builder()
                .no_proxy()
                .user_agent("LX-Native/0.3")
                .connect_timeout(Duration::from_secs(8))
                .redirect(if redirects {
                    reqwest::redirect::Policy::limited(5)
                } else {
                    reqwest::redirect::Policy::none()
                });
            if let Some(proxy) = &config.proxy {
                builder = builder.proxy(
                    reqwest::Proxy::all(proxy)
                        .map_err(|e| ProviderError::invalid(e.to_string()))?,
                );
            }
            builder.build().map_err(|e| ProviderError::Network {
                message: e.to_string(),
            })
        };
        let client = build_client(true)?;
        let redirect_client = build_client(false)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| ProviderError::Network {
                message: e.to_string(),
            })?;
        Ok(Self {
            client,
            redirect_client,
            runtime,
            config,
        })
    }

    async fn checked<T, F>(&self, future: F, context: &ProviderContext) -> Result<T, ProviderError>
    where
        F: Future<Output = Result<T, reqwest::Error>>,
    {
        let mut future = Box::pin(future);
        loop {
            context.check()?;
            let tick = context.remaining()?.min(Duration::from_millis(25));
            match tokio::time::timeout(tick, future.as_mut()).await {
                Ok(result) => {
                    context.check()?;
                    return result.map_err(|e| ProviderError::Network {
                        message: e.to_string(),
                    });
                }
                Err(_) => continue,
            }
        }
    }

    async fn once(
        &self,
        request: &HttpRequest,
        context: &ProviderContext,
    ) -> Result<HttpResponse, ProviderError> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|e| ProviderError::invalid(e.to_string()))?;
        let url =
            reqwest::Url::parse(&request.url).map_err(|e| ProviderError::invalid(e.to_string()))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(ProviderError::invalid(
                "platform URL must be HTTP(S) without embedded credentials",
            ));
        }
        let client = if request.follow_redirects {
            &self.client
        } else {
            &self.redirect_client
        };
        let mut builder = client.request(method, url).timeout(context.remaining()?);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if !request.body.is_empty() {
            builder = builder.body(request.body.clone());
        }
        let mut response = self.checked(builder.send(), context).await?;
        if response
            .content_length()
            .is_some_and(|size| size > self.config.max_response_bytes as u64)
        {
            return Err(ProviderError::ResponseTooLarge {
                limit: self.config.max_response_bytes,
            });
        }
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.to_string(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut bytes = Vec::new();
        while let Some(chunk) = self.checked(response.chunk(), context).await? {
            if chunk.len() > self.config.max_response_bytes.saturating_sub(bytes.len()) {
                return Err(ProviderError::ResponseTooLarge {
                    limit: self.config.max_response_bytes,
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        context.check()?;
        Ok(HttpResponse {
            status,
            headers,
            bytes,
        })
    }
}
impl HttpTransport for ReqwestTransport {
    fn execute(
        &self,
        request: HttpRequest,
        context: &ProviderContext,
    ) -> Result<HttpResponse, ProviderError> {
        context.check()?;
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(ProviderError::invalid(
                "synchronous platform transport must run on a blocking worker",
            ));
        }
        let retryable = matches!(request.method.as_str(), "GET" | "HEAD");
        let attempts = if retryable {
            self.config.retries + 1
        } else {
            1
        };
        for attempt in 0..attempts {
            context.check()?;
            let result = self.runtime.block_on(self.once(&request, context));
            let retry = matches!(&result, Err(ProviderError::Network { .. }))
                || matches!(&result,Ok(resp) if resp.status==408 || resp.status==429 || (500..600).contains(&resp.status));
            if !retry || attempt + 1 == attempts {
                return result;
            }
            let wait = Duration::from_millis(100 * (attempt as u64 + 1));
            let until = std::time::Instant::now() + wait;
            while std::time::Instant::now() < until {
                context.check()?;
                std::thread::sleep(context.remaining()?.min(Duration::from_millis(25)));
            }
        }
        unreachable!("positive bounded attempt count")
    }
}
