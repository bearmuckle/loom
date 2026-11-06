//! Provider verbs: list the worker's providers, and store an API key.
//!
//! `providers list` is a read. It prints whether each provider has a credential
//! and whether it accepts an API key, and it never prints a secret or the
//! opaque credential reference. `providers set-key` sends
//! `Provider/ConfigureApiKeyProvider`, the same request the settings screen
//! sends, and reads the key from an environment variable or a file so the key
//! never appears on the command line. The worker stores the secret in its own
//! credential store; this client does not write one.

use std::{env, fs, io::Write, path::Path};

use loom_model::ProviderSummary;
use loom_protocol::{ClientRequest, ProviderRequest, ProviderResponse, ServerResponse};
use serde_json::{Value, json};

use crate::cli::args::{Invocation, ProviderKeySource, ProviderSetKeyArgs};
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
        "{} {} credential {} api-key {}",
        provider.id.as_str(),
        kind_name(provider),
        flag(provider.credential_id.is_some()),
        flag(provider.api_key_configurable),
    )
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
