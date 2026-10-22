use super::{
    kg::KgProvider, kw::KwProvider, mg::MgProvider, tx::TxProvider, types::*, wy::WyProvider,
};
use std::{collections::BTreeMap, sync::Arc};

pub struct ProviderRegistry {
    providers: BTreeMap<Platform, Arc<dyn Provider>>,
}
impl ProviderRegistry {
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(KwProvider::new(transport.clone())),
            Arc::new(TxProvider::new(transport.clone())),
            Arc::new(KgProvider::new(transport.clone())),
            Arc::new(WyProvider::new(transport.clone())),
            Arc::new(MgProvider::new(transport)),
        ];
        Self {
            providers: providers
                .into_iter()
                .map(|provider| (provider.platform(), provider))
                .collect(),
        }
    }
    pub fn provider(&self, platform: Platform) -> Arc<dyn Provider> {
        self.providers[&platform].clone()
    }
    pub fn execute(
        &self,
        platform: Platform,
        request: ProviderRequest,
        context: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        context.check()?;
        self.providers[&platform].execute(request, context)
    }
    pub fn platforms(&self) -> Vec<Platform> {
        self.providers.keys().copied().collect()
    }
}
