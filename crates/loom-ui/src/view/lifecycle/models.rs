use super::*;

impl LoomView {
    /// Refreshes the model list. The synchronous variant is only used during
    /// the startup bootstrap, before the window exists.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn refresh_models(&mut self) {
        let provider_ids = match list_provider_ids(&self.connection) {
            Ok(provider_ids) => provider_ids,
            Err(error) => {
                self.record_status(format!(
                    "Could not list providers for model refresh: {error}"
                ));
                return;
            }
        };
        let catalog = match list_models(&self.connection) {
            Ok(models) => models,
            Err(error) => {
                self.record_status(format!("Could not load available models: {error}"));
                return;
            }
        };
        let mut provider_names = catalog.provider_names;
        let mut models = catalog.models;
        for provider_id in provider_ids {
            let response = self
                .connection
                .request(RequestEnvelope::new(ClientRequest::Provider(
                    ProviderRequest::DiscoverProviderModels {
                        provider_id: provider_id.clone(),
                    },
                )));
            match response.result {
                Ok(ServerResponse::Provider(ProviderResponse::Models { models: discovered })) => {
                    for model in discovered {
                        provider_names.insert(
                            model.id.clone(),
                            crate::connection::provider_name_for_id(provider_id.as_str()),
                        );
                        models.push(model.id);
                    }
                }
                Err(error) => self.record_status(format!(
                    "Model discovery unavailable for {}: {}",
                    provider_id.as_str(),
                    error.message
                )),
                Ok(response) => self.record_backend_error(
                    "model discovery",
                    unexpected_response("model list", response),
                ),
            }
        }
        models.sort();
        models.dedup();
        self.node_model_provider_names
            .insert(self.default_backend_node_id.clone(), provider_names);
        self.apply_models(models);
    }

    pub(crate) fn refresh_models_for_node_async(
        &mut self,
        node_id: String,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = self.node_backends.get(&node_id).cloned() else {
            self.record_backend_error(
                "model refresh",
                LoomError::new(
                    ErrorCode::NotFound,
                    format!("worker node {node_id} is not connected"),
                    false,
                ),
            );
            cx.notify();
            return;
        };
        if !self.model_refreshes_in_flight.insert(node_id.clone()) {
            return;
        }
        let active_node_id = self
            .session_node_ids
            .get(&self.active_session.id)
            .map(String::as_str);
        if active_node_id == Some(node_id.as_str()) {
            self.models.clear();
            self.model_catalog_node_id = None;
            cx.notify();
        }
        cx.spawn(async move |view, cx| {
            let result = list_models_from_backend(&backend).await;
            let provider_response = backend
                .submit(RequestEnvelope::new(ClientRequest::Provider(
                    ProviderRequest::ListProviders,
                )))
                .wait()
                .await;
            view.update(cx, |view, cx| {
                view.model_refreshes_in_flight.remove(&node_id);
                if view.providers_node_id.as_deref() == Some(node_id.as_str()) {
                    match provider_response.result {
                        Ok(ServerResponse::Provider(ProviderResponse::Providers { providers })) => {
                            view.providers = providers;
                        }
                        Err(error) => view.record_backend_error("list providers", error),
                        Ok(response) => view.record_backend_error(
                            "list providers",
                            unexpected_response("provider list", response),
                        ),
                    }
                }
                match result {
                    Ok(catalog) => {
                        view.record_model_discovery_errors(catalog.discovery_errors);
                        view.node_model_provider_names
                            .insert(node_id.clone(), catalog.provider_names);
                        let models = catalog.models;
                        view.node_model_catalogs
                            .insert(node_id.clone(), models.clone());
                        if view.default_backend_node_id == node_id {
                            view.default_models = models.clone();
                            #[cfg(target_family = "wasm")]
                            let preferred_model = view
                                .browser_model
                                .as_ref()
                                .filter(|model| models.contains(model))
                                .cloned();
                            #[cfg(not(target_family = "wasm"))]
                            let preferred_model = None;
                            if let Some(model) = preferred_model.or_else(|| {
                                if models.contains(&view.default_model) {
                                    None
                                } else {
                                    models.first().cloned()
                                }
                            }) {
                                view.default_model = model;
                            }
                        }
                        let active_node_id = view
                            .session_node_ids
                            .get(&view.active_session.id)
                            .map(String::as_str);
                        if active_node_id == Some(node_id.as_str()) {
                            view.models = models;
                            view.model_catalog_node_id = Some(node_id.clone());
                            view.record_status(format!(
                                "Loaded available models for {}",
                                view.node_names
                                    .get(&node_id)
                                    .map(String::as_str)
                                    .unwrap_or(node_id.as_str())
                            ));
                        }
                    }
                    Err(error) => {
                        view.record_backend_error("model refresh", error);
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn record_model_discovery_errors(
        &mut self,
        errors: Vec<crate::connection::ModelDiscoveryError>,
    ) {
        for discovery_error in errors {
            self.record_status(format!(
                "Model discovery unavailable for {}: {}",
                discovery_error.provider_id, discovery_error.error.message
            ));
        }
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn apply_models(&mut self, models: Vec<ModelId>) {
        let node_id = self.default_backend_node_id.clone();
        self.node_model_catalogs
            .insert(node_id.clone(), models.clone());
        self.default_models = models.clone();
        let active_node_id = self
            .session_node_ids
            .get(&self.active_session.id)
            .map(String::as_str);
        if active_node_id == Some(node_id.as_str()) {
            self.models = models;
            self.model_catalog_node_id = Some(node_id);
        }
        self.record_status(format!(
            "Loaded {} available model{}",
            self.default_models.len(),
            if self.default_models.len() == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
}
