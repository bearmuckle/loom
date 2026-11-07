//! Provider verbs: list the worker's providers and their models, and store an
//! API key.
//!
//! `providers list` is a read. It prints each provider as
//! `id=<id> kind=<kind> credential=<true|false> api-key-configurable=<true|false>`
//! and never prints a secret or the opaque credential reference. `models list`
//! is a read as well: it prints `provider=<id> model=<id> default=<true|false>`
//! for every model of every configured provider, or of one `--provider`, and
//! marks exactly one default per provider. `providers set-key` sends
//! `Provider/ConfigureApiKeyProvider`, the same request the settings screen
//! sends, and reads the key from an environment variable or a file so the key
//! never appears on the command line. The worker stores the secret in its own
//! credential store; this client does not write one.

use std::{env, fs, io::Write, path::Path};

use loom_model::{
    DEEPSEEK_DEFAULT_MODEL, GITHUB_COPILOT_DEFAULT_MODEL, ModelDescriptor, ModelId,
    OPENAI_DEFAULT_MODEL, ProviderKind, ProviderSummary,
};
use loom_protocol::{ClientRequest, ProviderRequest, ProviderResponse, ServerResponse};
use serde_json::{Value, json};

use crate::cli::args::{Invocation, ProviderAddArgs, ProviderKeySource, ProviderSetKeyArgs};
use crate::cli::client::Client;

use super::{Caller, Failure, Outcome, emit, handshake};

/// `providers list`: the providers the worker has registered.
pub(super) async fn list<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Provider(ProviderRequest::ListProviders))
        .await?;
    let mut providers = match response {
        ServerResponse::Provider(ProviderResponse::Providers { providers }) => providers,
        _ => return Err(Failure::unexpected(request_id, "provider list")),
    };
    providers.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
    if !invocation.globals.json {
        for provider in &providers {
            emit(out, &format!("{}\n", provider_line(provider)))?;
        }
    }
    Ok(Outcome::new(
        json!({ "providers": providers.iter().map(provider_value).collect::<Vec<_>>() }),
        caller.primary,
    ))
}

/// `models list`: the models of the configured providers, or of one exact
/// provider when `--provider` is given.
///
/// A provider id the worker does not have is not an error: the answer is an
/// empty `models` array and exit `0`. Exactly one model per provider is marked
/// `default`; the declared default of the provider kind wins when that model is
/// present, otherwise the first model in sorted order is marked.
pub(super) async fn models<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    filter: Option<&str>,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Provider(ProviderRequest::ListProviders))
        .await?;
    let providers = match response {
        ServerResponse::Provider(ProviderResponse::Providers { providers }) => providers,
        _ => return Err(Failure::unexpected(request_id, "provider list")),
    };
    let mut listed = providers
        .iter()
        .filter(|provider| filter.is_none_or(|id| provider.id.as_str() == id))
        .collect::<Vec<_>>();
    listed.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
    let mut payload = Vec::new();
    for provider in listed {
        let mut models = provider.models.iter().collect::<Vec<_>>();
        models.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        // The default is read from the provider's own model order, which is the
        // order the worker stores, so a model `providers add --default-model`
        // put first stays the default regardless of how it sorts.
        let default = default_model(provider, &provider.models);
        for model in models {
            let is_default = default == Some(model.id.as_str());
            if !invocation.globals.json {
                emit(out, &model_line(provider, model, is_default))?;
            }
            payload.push(model_value(provider, model, is_default));
        }
    }
    Ok(Outcome::new(
        json!({ "provider": filter, "models": payload }),
        caller.primary,
    ))
}

/// `providers add`: register an OpenAI-compatible provider, such as OpenCode
/// Go or OpenCode Zen, and print the models it now serves.
///
/// The key is not part of this request: it is stored afterwards with
/// `providers set-key`, through the same server path every other API-key
/// provider uses.
pub(super) async fn add<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    args: &ProviderAddArgs,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let models = args.ordered_models();
    let (request_id, response) = caller
        .call(ClientRequest::Provider(
            ProviderRequest::ConfigureOpenAiCompatibleProvider {
                provider_id: loom_model::ProviderId::new(args.provider.clone()),
                endpoint: args.endpoint.clone(),
                models: models.iter().map(ModelId::new).collect(),
            },
        ))
        .await?;
    match response {
        ServerResponse::Provider(ProviderResponse::ProviderConfigured) => {}
        _ => return Err(Failure::unexpected(request_id, "provider configuration")),
    }
    if !invocation.globals.json {
        emit(
            out,
            &format!("provider {} kind=openai-compatible\n", args.provider),
        )?;
        for model in &models {
            emit(
                out,
                &format!(
                    "model={} default={}\n",
                    model,
                    flag(models.first().is_some_and(|first| first == model))
                ),
            )?;
        }
    }
    Ok(Outcome::new(
        json!({
            "provider": args.provider,
            "kind": "openai_compatible",
            "endpoint": args.endpoint,
            "models": models
                .iter()
                .map(|model| json!({
                    "id": model,
                    "default": models.first().is_some_and(|first| first == model),
                }))
                .collect::<Vec<_>>(),
        }),
        caller.primary,
    ))
}

/// `providers remove`: drop an OpenAI-compatible provider and the key stored
/// for it. A built-in provider is refused by the worker.
pub(super) async fn remove<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    provider: &str,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Provider(ProviderRequest::RemoveProvider {
            provider_id: loom_model::ProviderId::new(provider.to_owned()),
        }))
        .await?;
    match response {
        ServerResponse::Provider(ProviderResponse::ProviderConfigured) => {}
        _ => return Err(Failure::unexpected(request_id, "provider removal")),
    }
    if !invocation.globals.json {
        emit(out, &format!("provider {provider} removed\n"))?;
    }
    Ok(Outcome::new(
        json!({ "provider": provider, "removed": true }),
        caller.primary,
    ))
}

/// `providers discover`: replace one provider's model catalog with the one its
/// endpoint's `/models` answers with.
///
pub(super) async fn discover<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    provider: &str,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let (request_id, response) = caller
        .call(ClientRequest::Provider(
            ProviderRequest::DiscoverProviderModels {
                provider_id: loom_model::ProviderId::new(provider.to_owned()),
            },
        ))
        .await?;
    let models = match response {
        ServerResponse::Provider(ProviderResponse::Models { models }) => models,
        _ => return Err(Failure::unexpected(request_id, "provider model discovery")),
    };
    if !invocation.globals.json {
        for model in &models {
            emit(out, &format!("model={}\n", model.id.as_str()))?;
        }
    }
    Ok(Outcome::new(
        json!({
            "provider": provider,
            "models": models
                .iter()
                .map(|model| json!({
                    "id": model.id.as_str(),
                    "display_name": model.display_name,
                    "context_window": model.context_window,
                    "max_output_tokens": model.max_output_tokens,
                }))
                .collect::<Vec<_>>(),
        }),
        caller.primary,
    ))
}

/// `providers set-key`: store one API key through the worker.
///
/// The key is read here, after parsing, and is sent only inside
/// `ConfigureApiKeyProvider`. Nothing in the success or failure text includes
/// it. An empty key is refused locally, before a request is sent.
pub(super) async fn set_key<C: Client>(
    client: &mut C,
    invocation: &Invocation,
    args: &ProviderSetKeyArgs,
    out: &mut dyn Write,
) -> Result<Outcome, Failure> {
    let api_key = read_key(&args.source)?;
    let mut caller = Caller::new(client);
    handshake(&mut caller, &invocation.verb).await?;
    let provider_id = loom_model::ProviderId::new(args.provider.clone());
    let (request_id, response) = caller
        .call(ClientRequest::Provider(
            ProviderRequest::ConfigureApiKeyProvider {
                provider_id,
                api_key,
            },
        ))
        .await?;
    match response {
        ServerResponse::Provider(ProviderResponse::ProviderConfigured) => {}
        _ => return Err(Failure::unexpected(request_id, "provider configuration")),
    }
    if !invocation.globals.json {
        emit(out, &format!("provider {} credential set\n", args.provider))?;
    }
    Ok(Outcome::new(
        json!({
            "provider": args.provider,
            "has_credential": true,
        }),
        caller.primary,
    ))
}

fn provider_line(provider: &ProviderSummary) -> String {
    format!(
        "id={} kind={} credential={} api-key-configurable={}",
        provider.id.as_str(),
        kind_name(provider),
        flag(provider.credential_id.is_some()),
        flag(provider.api_key_configurable),
    )
}

fn model_line(provider: &ProviderSummary, model: &ModelDescriptor, default: bool) -> String {
    format!(
        "provider={} model={} default={}\n",
        provider.id.as_str(),
        model.id.as_str(),
        flag(default),
    )
}

fn model_value(provider: &ProviderSummary, model: &ModelDescriptor, default: bool) -> Value {
    json!({
        "provider": provider.id.as_str(),
        "id": model.id.as_str(),
        "display_name": model.display_name,
        "default": default,
        "context_window": model.context_window,
        "max_output_tokens": model.max_output_tokens,
    })
}

/// The model `run` starts when `--model` is absent: the first provider, in the
/// order the worker returned them, that has a credential and a known default
/// model, encoded as `loom:model:v1:["<provider-id>","<model>"]`.
///
/// The default is the one `models list` marks for that provider, so the two
/// verbs never disagree about which model a provider prefers. `None` when no
/// provider has both a credential and a model, which is what `run` reports with
/// its deterministic fallback.
pub(super) fn default_selection(providers: &[ProviderSummary]) -> Option<ModelId> {
    providers.iter().find_map(|provider| {
        provider.credential_id.as_ref()?;
        let model = default_model(provider, &provider.models)?;
        Some(ModelId::for_provider(&provider.id, &ModelId::new(model)))
    })
}

/// The model id a provider marks as its default: the kind's declared default
/// when that model is present, otherwise the first model in the order the
/// worker stores it. `None` when the provider lists no model, so no default is
/// invented.
fn default_model<'a>(provider: &ProviderSummary, models: &'a [ModelDescriptor]) -> Option<&'a str> {
    if let Some(declared) = declared_default(provider.kind)
        && models.iter().any(|model| model.id.as_str() == declared)
    {
        return Some(declared);
    }
    models.first().map(|model| model.id.as_str())
}

/// The default model a provider kind declares. The local runtimes and the
/// deterministic demo declare none, so their first model is marked instead.
fn declared_default(kind: ProviderKind) -> Option<&'static str> {
    match kind {
        ProviderKind::DeepSeek => Some(DEEPSEEK_DEFAULT_MODEL),
        ProviderKind::OpenAi | ProviderKind::OpenAiCompatible => Some(OPENAI_DEFAULT_MODEL),
        ProviderKind::GitHubCopilot => Some(GITHUB_COPILOT_DEFAULT_MODEL),
        ProviderKind::Deterministic | ProviderKind::Ollama => None,
    }
}

fn provider_value(provider: &ProviderSummary) -> Value {
    json!({
        "id": provider.id.as_str(),
        "kind": kind_name(provider),
        "display_name": provider.display_name,
        "has_credential": provider.credential_id.is_some(),
        "api_key_configurable": provider.api_key_configurable,
    })
}

fn kind_name(provider: &ProviderSummary) -> String {
    serde_json::to_value(provider.kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

fn flag(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

/// The key from `--from-env` or `--key-file`, trimmed. A missing variable, an
/// unreadable file, and a blank key are usage errors, and none of those
/// messages repeats the key.
fn read_key(source: &ProviderKeySource) -> Result<String, Failure> {
    let raw = match source {
        ProviderKeySource::Env(name) => env::var(name).map_err(|_| {
            Failure::Usage(format!(
                "environment variable '{name}' is not set; --from-env names a variable that \
                 holds the key"
            ))
        })?,
        ProviderKeySource::File(path) => read_key_file(path)?,
    };
    let key = raw.trim();
    if key.is_empty() {
        return Err(Failure::Usage(
            "provider API key must not be empty".to_owned(),
        ));
    }
    Ok(key.to_owned())
}

fn read_key_file(path: &Path) -> Result<String, Failure> {
    let text = fs::read_to_string(path).map_err(|error| {
        Failure::Usage(format!(
            "could not read the key file '{}': {error}",
            path.display()
        ))
    })?;
    Ok(match text.strip_suffix('\n') {
        Some(trimmed) => trimmed.strip_suffix('\r').unwrap_or(trimmed).to_owned(),
        None => text,
    })
}

#[cfg(test)]
mod tests {
    use loom_model::{
        DEEPSEEK_DEFAULT_MODEL, GITHUB_COPILOT_DEFAULT_MODEL, OPENAI_DEFAULT_MODEL, ProviderKind,
    };

    use super::declared_default;

    #[test]
    fn each_provider_kind_declares_its_default_model_or_none() {
        for (kind, expected) in [
            (ProviderKind::DeepSeek, Some(DEEPSEEK_DEFAULT_MODEL)),
            (ProviderKind::OpenAi, Some(OPENAI_DEFAULT_MODEL)),
            (ProviderKind::OpenAiCompatible, Some(OPENAI_DEFAULT_MODEL)),
            (
                ProviderKind::GitHubCopilot,
                Some(GITHUB_COPILOT_DEFAULT_MODEL),
            ),
            (ProviderKind::Deterministic, None),
            (ProviderKind::Ollama, None),
        ] {
            assert_eq!(declared_default(kind), expected, "{kind:?}");
        }
    }
}
