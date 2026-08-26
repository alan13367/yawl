//! Interactive provider setup shared by idle and active-response TUI loops.

use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::cancellation::CancellationToken;
use crate::config::Config;
use crate::error::Error;
use crate::onboarding::provider::{
    self, ConnectionActivation, ConnectionPlan, CredentialChoice, ProviderDefinition, ProviderId,
};
use crate::provider::codex::{CodexLoginStatus, DeviceLoginPrompt};

use super::picker::{Picker, PickerAction, PickerItem};
use super::state::ViewState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConnectEditField {
    ProviderName,
    Endpoint,
    Environment,
    Secret,
    Model,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConnectStep {
    Provider,
    ProviderName,
    Endpoint,
    Authentication,
    Discovery,
    Login,
    Models,
    Recovery,
    Review,
}

pub(super) struct ConnectFlow {
    config: Config,
    step: ConnectStep,
    provider: Option<ProviderId>,
    provider_label: String,
    endpoint: String,
    credential: CredentialChoice,
    model: String,
    models: Vec<String>,
    job: Option<ConnectJob>,
    root_parent: Option<PickerAction>,
}

enum ConnectJob {
    Discovery {
        receiver: Receiver<Result<Vec<String>, Error>>,
        cancellation: CancellationToken,
    },
    Login {
        receiver: Receiver<LoginEvent>,
        cancellation: CancellationToken,
    },
}

enum LoginEvent {
    Prompt(DeviceLoginPrompt),
    Done(Result<(), Error>),
}

pub(super) fn open(state: &mut ViewState, config: &Config, from_settings: bool) {
    let root_parent = from_settings.then_some(PickerAction::OpenSettingsCategory {
        category: super::picker::SettingsCategory::Providers,
        selected: 0,
    });
    state.connection = Some(ConnectFlow {
        config: config.clone(),
        step: ConnectStep::Provider,
        provider: None,
        provider_label: String::new(),
        endpoint: String::new(),
        credential: CredentialChoice::Keep,
        model: String::new(),
        models: Vec::new(),
        job: None,
        root_parent: root_parent.clone(),
    });
    state.picker = Some(provider_picker(config, root_parent));
}

pub(super) fn handle_action(state: &mut ViewState, action: PickerAction) -> Option<PickerAction> {
    let Some(mut flow) = state.connection.take() else {
        return Some(action);
    };
    let mut keep_flow = true;
    let result = match action {
        PickerAction::ConnectChooseProvider(provider) => {
            choose_provider(&mut flow, provider, state);
            None
        }
        PickerAction::ApplyConnect { field, value } => {
            apply_edit(&mut flow, field, value, state);
            None
        }
        PickerAction::ConnectCredential(credential) => {
            flow.credential = credential;
            start_discovery(&mut flow, state);
            None
        }
        PickerAction::ConnectChooseModel(model) => {
            flow.model = model;
            flow.step = ConnectStep::Review;
            state.picker = Some(review_picker(&flow));
            None
        }
        PickerAction::ConnectRetry => {
            if flow.provider == Some(ProviderId::Codex) {
                start_login(&mut flow, state);
            } else {
                start_discovery(&mut flow, state);
            }
            None
        }
        PickerAction::ConnectCancelJob => {
            cancel_job(&mut flow);
            flow.step = ConnectStep::Provider;
            state.picker = Some(provider_picker(&flow.config, flow.root_parent.clone()));
            None
        }
        PickerAction::CloseConnect => {
            cancel_job(&mut flow);
            keep_flow = false;
            state.picker = None;
            flow.root_parent.clone()
        }
        PickerAction::ConnectBack(step) => {
            go_back(&mut flow, step, state);
            None
        }
        PickerAction::ConnectActivation(activation) => match build_plan(&flow, activation) {
            Ok(plan) => {
                keep_flow = false;
                state.picker = None;
                Some(PickerAction::ApplyConnectionPlan(plan))
            }
            Err(error) => {
                state.notice(format!("Could not prepare connection: {error}"));
                state.picker = Some(review_picker(&flow));
                None
            }
        },
        PickerAction::OpenSettingsCategory { .. } | PickerAction::OpenSettingsRoot { .. } => {
            cancel_job(&mut flow);
            keep_flow = false;
            Some(action)
        }
        action => Some(action),
    };
    if keep_flow {
        state.connection = Some(flow);
    }
    result
}

fn provider_picker(config: &Config, parent: Option<PickerAction>) -> Picker {
    Picker {
        title: "Connect provider".into(),
        hint: "↑/↓ move  Enter configure  Esc close".into(),
        items: provider::provider_catalog(config)
            .into_iter()
            .map(|definition| {
                let description = provider_description(&definition);
                PickerItem {
                    label: definition.label,
                    description,
                    action: PickerAction::ConnectChooseProvider(definition.id),
                }
            })
            .collect(),
        selected: 0,
        editing: None,
        parent: Some(parent.unwrap_or(PickerAction::CloseConnect)),
    }
}

fn provider_description(provider: &ProviderDefinition) -> String {
    if provider.configured {
        format!("Configured · {}", provider.description)
    } else {
        provider.description.clone()
    }
}

fn choose_provider(flow: &mut ConnectFlow, provider: ProviderId, state: &mut ViewState) {
    if provider == ProviderId::Other {
        flow.step = ConnectStep::ProviderName;
        state.picker = Some(edit_picker(
            "Provider name",
            "Name",
            "letters, numbers, '-' or '_'",
            ConnectEditField::ProviderName,
            String::new(),
            false,
            PickerAction::ConnectBack(ConnectStep::Provider),
        ));
        return;
    }
    flow.provider_label = provider_label(&provider);
    flow.provider = Some(provider.clone());
    if provider == ProviderId::Codex {
        if crate::provider::codex::credential_status(&flow.config) == CodexLoginStatus::LoggedIn {
            show_models(flow, codex_models(&flow.config), state);
        } else {
            start_login(flow, state);
        }
        return;
    }
    flow.endpoint = provider::default_endpoint(&provider, &flow.config);
    flow.step = ConnectStep::Endpoint;
    state.picker = Some(edit_picker(
        &format!("{} endpoint", flow.provider_label),
        "API base URL",
        "usually ends in /v1",
        ConnectEditField::Endpoint,
        flow.endpoint.clone(),
        false,
        PickerAction::ConnectBack(ConnectStep::Provider),
    ));
}

fn provider_label(provider: &ProviderId) -> String {
    match provider {
        ProviderId::Codex => "OpenAI Codex".into(),
        ProviderId::Anthropic => "Anthropic".into(),
        ProviderId::OpenAi => "OpenAI".into(),
        ProviderId::Compatible(name) => match name.as_str() {
            "ollama" => "Ollama".into(),
            "lmstudio" => "LM Studio".into(),
            "omlx" => "OMLX".into(),
            _ => name.clone(),
        },
        ProviderId::Other => "Other OpenAI-compatible".into(),
    }
}

fn apply_edit(
    flow: &mut ConnectFlow,
    field: ConnectEditField,
    value: String,
    state: &mut ViewState,
) {
    match field {
        ConnectEditField::ProviderName => match provider::validate_provider_name(&value) {
            Ok(()) => choose_provider(flow, ProviderId::Compatible(value), state),
            Err(error) => {
                state.notice(error.to_string());
                state.picker = Some(edit_picker(
                    "Provider name",
                    "Name",
                    "letters, numbers, '-' or '_'",
                    field,
                    value,
                    false,
                    PickerAction::ConnectBack(ConnectStep::Provider),
                ));
            }
        },
        ConnectEditField::Endpoint => {
            if !value.starts_with("http://") && !value.starts_with("https://") {
                state.notice("The endpoint must start with http:// or https://.");
                state.picker = Some(edit_picker(
                    &format!("{} endpoint", flow.provider_label),
                    "API base URL",
                    "usually ends in /v1",
                    field,
                    value,
                    false,
                    PickerAction::ConnectBack(ConnectStep::Endpoint),
                ));
                return;
            }
            flow.endpoint = value;
            flow.step = ConnectStep::Authentication;
            state.picker = Some(authentication_picker(flow));
        }
        ConnectEditField::Environment => {
            let name = value.trim().trim_start_matches('$').to_string();
            if let Err(error) = provider::validate_environment_name(&name) {
                state.notice(error.to_string());
                state.picker = Some(environment_picker(flow, name));
                return;
            }
            flow.credential = CredentialChoice::Environment(name);
            start_discovery(flow, state);
        }
        ConnectEditField::Secret => {
            if value.is_empty() {
                state.notice("The API key must not be empty.");
                state.picker = Some(secret_picker(flow));
                return;
            }
            flow.credential = CredentialChoice::Literal(value);
            start_discovery(flow, state);
        }
        ConnectEditField::Model => {
            if value.is_empty() {
                state.notice("The model ID must not be empty.");
                state.picker = Some(manual_model_picker(flow));
                return;
            }
            flow.model = value;
            flow.step = ConnectStep::Review;
            state.picker = Some(review_picker(flow));
        }
    }
}

fn authentication_picker(flow: &ConnectFlow) -> Picker {
    let mut items = Vec::new();
    if !provider::credential_is_configured(
        &flow.config,
        flow.provider.as_ref().unwrap_or(&ProviderId::Other),
    ) {
        // There is nothing to preserve, so omit a misleading keep option.
    } else {
        items.push(PickerItem {
            label: "Keep current credential".into(),
            description: "Reuse the configured key without displaying it".into(),
            action: PickerAction::ConnectCredential(CredentialChoice::Keep),
        });
    }
    items.extend([
        PickerItem {
            label: "Use environment variable…".into(),
            description: "Store a $VARIABLE reference".into(),
            action: PickerAction::EditConnect {
                field: ConnectEditField::Environment,
                initial: default_environment(flow),
                secret: false,
            },
        },
        PickerItem {
            label: "Enter API key…".into(),
            description: "Masked while typing".into(),
            action: PickerAction::EditConnect {
                field: ConnectEditField::Secret,
                initial: String::new(),
                secret: true,
            },
        },
    ]);
    if flow.provider.as_ref().is_some_and(provider::can_use_no_key) {
        items.push(PickerItem {
            label: "Use no key".into(),
            description: "Clear a saved key and send no authorization".into(),
            action: PickerAction::ConnectCredential(CredentialChoice::None),
        });
    }
    Picker {
        title: format!("{} authentication", flow.provider_label),
        hint: "↑/↓ move  Enter select  Esc back".into(),
        items,
        selected: 0,
        editing: None,
        parent: Some(PickerAction::ConnectBack(ConnectStep::Endpoint)),
    }
}

fn default_environment(flow: &ConnectFlow) -> String {
    flow.provider
        .as_ref()
        .and_then(provider::credential_environment_name)
        .unwrap_or_default()
}

fn environment_picker(_flow: &ConnectFlow, initial: String) -> Picker {
    edit_picker(
        "Environment credential",
        "Variable",
        "stored as a $VARIABLE reference",
        ConnectEditField::Environment,
        initial,
        false,
        PickerAction::ConnectBack(ConnectStep::Authentication),
    )
}

fn secret_picker(flow: &ConnectFlow) -> Picker {
    edit_picker(
        &format!("{} API key", flow.provider_label),
        "API key",
        "secret input is masked",
        ConnectEditField::Secret,
        String::new(),
        true,
        PickerAction::ConnectBack(ConnectStep::Authentication),
    )
}

fn edit_picker(
    title: &str,
    label: &str,
    description: &str,
    field: ConnectEditField,
    initial: String,
    secret: bool,
    parent: PickerAction,
) -> Picker {
    Picker {
        title: title.into(),
        hint: "Enter edit  Esc back".into(),
        items: vec![PickerItem {
            label: label.into(),
            description: description.into(),
            action: PickerAction::EditConnect {
                field,
                initial,
                secret,
            },
        }],
        selected: 0,
        editing: None,
        parent: Some(parent),
    }
}

fn start_discovery(flow: &mut ConnectFlow, state: &mut ViewState) {
    let Some(provider) = flow.provider.clone() else {
        return;
    };
    let endpoint = flow.endpoint.clone();
    let key = provider::request_credential(&flow.config, &provider, &flow.credential);
    let cancellation = CancellationToken::default();
    let worker_cancellation = cancellation.clone();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let result = crate::cancellation::scope(&worker_cancellation, || {
            if worker_cancellation.is_canceled() {
                return Err(Error::Interrupted);
            }
            let result = provider::discover_models(&provider, &endpoint, &key);
            if worker_cancellation.is_canceled() {
                Err(Error::Interrupted)
            } else {
                result
            }
        });
        let _ = sender.send(result);
    });
    flow.step = ConnectStep::Discovery;
    flow.job = Some(ConnectJob::Discovery {
        receiver,
        cancellation,
    });
    state.picker = Some(waiting_picker(
        "Discovering models",
        &format!("Testing {} in the background", flow.provider_label),
        PickerAction::ConnectCancelJob,
    ));
}

fn start_login(flow: &mut ConnectFlow, state: &mut ViewState) {
    let config = flow.config.clone();
    let cancellation = CancellationToken::default();
    let worker_cancellation = cancellation.clone();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let prompt_sender = sender.clone();
        let result = crate::provider::codex::login_with_callback(
            &config,
            &worker_cancellation,
            true,
            move |prompt| {
                let _ = prompt_sender.send(LoginEvent::Prompt(prompt));
            },
        );
        let _ = sender.send(LoginEvent::Done(result));
    });
    flow.step = ConnectStep::Login;
    flow.job = Some(ConnectJob::Login {
        receiver,
        cancellation,
    });
    state.picker = Some(waiting_picker(
        "OpenAI Codex login",
        "Starting device authentication…",
        PickerAction::ConnectCancelJob,
    ));
}

fn waiting_picker(title: &str, description: &str, parent: PickerAction) -> Picker {
    Picker {
        title: title.into(),
        hint: "Esc cancel this job".into(),
        items: vec![PickerItem {
            label: "Working…".into(),
            description: description.into(),
            action: PickerAction::ConnectCancelJob,
        }],
        selected: 0,
        editing: None,
        parent: Some(parent),
    }
}

fn login_prompt_picker(prompt: &DeviceLoginPrompt) -> Picker {
    let browser = prompt
        .browser_error
        .as_ref()
        .map_or("Browser opened when available".to_string(), |error| {
            format!("Browser not opened: {error}")
        });
    Picker {
        title: "OpenAI Codex login".into(),
        hint: "Complete login in the browser  Esc cancel this job".into(),
        items: vec![
            PickerItem {
                label: prompt.url.into(),
                description: "Open this address".into(),
                action: PickerAction::ConnectCancelJob,
            },
            PickerItem {
                label: format!("Code: {}", prompt.code),
                description: browser,
                action: PickerAction::ConnectCancelJob,
            },
        ],
        selected: 0,
        editing: None,
        parent: Some(PickerAction::ConnectCancelJob),
    }
}

pub(super) fn poll(state: &mut ViewState) -> bool {
    let Some(mut flow) = state.connection.take() else {
        return false;
    };
    let Some(job) = flow.job.as_mut() else {
        state.connection = Some(flow);
        return false;
    };
    enum PollEvent {
        None,
        Models(Vec<String>),
        DiscoveryFailure(&'static str),
        LoginPrompt(DeviceLoginPrompt),
        LoginDone,
        LoginFailure,
    }
    let event = match job {
        ConnectJob::Discovery { receiver, .. } => match receiver.try_recv() {
            Ok(Ok(models)) if !models.is_empty() => PollEvent::Models(models),
            Ok(Ok(_)) => PollEvent::DiscoveryFailure("The provider returned no models."),
            Ok(Err(Error::Interrupted)) | Err(TryRecvError::Empty) => PollEvent::None,
            Ok(Err(_)) | Err(TryRecvError::Disconnected) => {
                PollEvent::DiscoveryFailure("Yawl could not list models from this endpoint.")
            }
        },
        ConnectJob::Login { receiver, .. } => match receiver.try_recv() {
            Ok(LoginEvent::Prompt(prompt)) => PollEvent::LoginPrompt(prompt),
            Ok(LoginEvent::Done(Ok(()))) => PollEvent::LoginDone,
            Ok(LoginEvent::Done(Err(Error::Interrupted))) | Err(TryRecvError::Empty) => {
                PollEvent::None
            }
            Ok(LoginEvent::Done(Err(_))) | Err(TryRecvError::Disconnected) => {
                PollEvent::LoginFailure
            }
        },
    };
    let changed = match event {
        PollEvent::None => false,
        PollEvent::Models(models) => {
            flow.job = None;
            show_models(&mut flow, models, state);
            true
        }
        PollEvent::DiscoveryFailure(message) => {
            flow.job = None;
            show_recovery(&mut flow, message, state);
            true
        }
        PollEvent::LoginPrompt(prompt) => {
            state.picker = Some(login_prompt_picker(&prompt));
            true
        }
        PollEvent::LoginDone => {
            flow.job = None;
            let models = codex_models(&flow.config);
            show_models(&mut flow, models, state);
            true
        }
        PollEvent::LoginFailure => {
            flow.job = None;
            show_recovery(&mut flow, "OpenAI Codex login did not complete.", state);
            true
        }
    };
    state.connection = Some(flow);
    changed
}

fn codex_models(config: &Config) -> Vec<String> {
    crate::model::available_models(config)
        .into_iter()
        .filter_map(|(model, _)| model.strip_prefix("openai-codex:").map(str::to_string))
        .collect()
}

fn show_models(flow: &mut ConnectFlow, models: Vec<String>, state: &mut ViewState) {
    flow.step = ConnectStep::Models;
    flow.models = models.clone();
    state.picker = Some(models_picker(flow, &models));
}

fn models_picker(flow: &ConnectFlow, models: &[String]) -> Picker {
    let mut items = models
        .iter()
        .cloned()
        .map(|model| PickerItem {
            label: model.clone(),
            description: "Available from this provider".into(),
            action: PickerAction::ConnectChooseModel(model),
        })
        .collect::<Vec<_>>();
    items.push(PickerItem {
        label: "Enter model manually…".into(),
        description: "Use an exact model ID".into(),
        action: PickerAction::EditConnect {
            field: ConnectEditField::Model,
            initial: String::new(),
            secret: false,
        },
    });
    Picker {
        title: format!("{} models", flow.provider_label),
        hint: "↑/↓ move  Enter select  Esc back".into(),
        items,
        selected: 0,
        editing: None,
        parent: Some(PickerAction::ConnectBack(
            if flow.provider == Some(ProviderId::Codex) {
                ConnectStep::Provider
            } else {
                ConnectStep::Authentication
            },
        )),
    }
}

fn show_recovery(flow: &mut ConnectFlow, message: &str, state: &mut ViewState) {
    flow.step = ConnectStep::Recovery;
    state.picker = Some(recovery_picker(flow, message));
}

fn recovery_picker(flow: &ConnectFlow, message: &str) -> Picker {
    Picker {
        title: "Connection problem".into(),
        hint: "↑/↓ move  Enter select  Esc back".into(),
        items: vec![
            PickerItem {
                label: "Retry".into(),
                description: message.into(),
                action: PickerAction::ConnectRetry,
            },
            PickerItem {
                label: "Enter model manually…".into(),
                description: "Save without discovery".into(),
                action: PickerAction::EditConnect {
                    field: ConnectEditField::Model,
                    initial: String::new(),
                    secret: false,
                },
            },
            PickerItem {
                label: "Back".into(),
                description: "Choose another provider".into(),
                action: PickerAction::ConnectCancelJob,
            },
        ],
        selected: 0,
        editing: None,
        parent: Some(PickerAction::ConnectBack(
            if flow.provider == Some(ProviderId::Codex) {
                ConnectStep::Provider
            } else {
                ConnectStep::Authentication
            },
        )),
    }
}

fn manual_model_picker(flow: &ConnectFlow) -> Picker {
    edit_picker(
        &format!("{} model", flow.provider_label),
        "Model ID",
        "exact ID expected by the provider",
        ConnectEditField::Model,
        String::new(),
        false,
        PickerAction::ConnectBack(ConnectStep::Recovery),
    )
}

fn review_picker(flow: &ConnectFlow) -> Picker {
    let description = format!("{} · {}", flow.provider_label, qualified_model(flow));
    Picker {
        title: "Review connection".into(),
        hint: "↑/↓ move  Enter save  Esc back".into(),
        items: vec![
            PickerItem {
                label: "Save and use as default".into(),
                description: description.clone(),
                action: PickerAction::ConnectActivation(ConnectionActivation::Default),
            },
            PickerItem {
                label: "Save and use this session".into(),
                description: description.clone(),
                action: PickerAction::ConnectActivation(ConnectionActivation::Session),
            },
            PickerItem {
                label: "Save connection only".into(),
                description,
                action: PickerAction::ConnectActivation(ConnectionActivation::ConnectionOnly),
            },
        ],
        selected: 0,
        editing: None,
        parent: Some(PickerAction::ConnectBack(ConnectStep::Models)),
    }
}

fn qualified_model(flow: &ConnectFlow) -> String {
    let prefix = match flow.provider.as_ref() {
        Some(ProviderId::Codex) => "openai-codex",
        Some(ProviderId::Anthropic) => "anthropic",
        Some(ProviderId::OpenAi) => "openai",
        Some(ProviderId::Compatible(name)) => name,
        Some(ProviderId::Other) | None => "",
    };
    format!("{prefix}:{}", flow.model)
}

fn build_plan(
    flow: &ConnectFlow,
    activation: ConnectionActivation,
) -> Result<ConnectionPlan, Error> {
    let provider = flow
        .provider
        .as_ref()
        .ok_or_else(|| Error::Config("provider is not selected".into()))?;
    let mut changes = Vec::new();
    if provider != &ProviderId::Codex {
        changes.push(provider::endpoint_change(
            provider,
            flow.endpoint.clone(),
            &flow.credential,
        )?);
        if let Some(change) = provider::credential_change(provider, &flow.credential) {
            changes.push(change);
        }
    }
    Ok(ConnectionPlan {
        changes,
        model: qualified_model(flow),
        activation,
        provider_label: flow.provider_label.clone(),
    })
}

fn cancel_job(flow: &mut ConnectFlow) {
    if let Some(job) = flow.job.take() {
        match job {
            ConnectJob::Discovery { cancellation, .. } | ConnectJob::Login { cancellation, .. } => {
                cancellation.cancel()
            }
        }
    }
}

fn go_back(flow: &mut ConnectFlow, step: ConnectStep, state: &mut ViewState) {
    flow.step = step;
    state.picker = Some(match step {
        ConnectStep::Provider => provider_picker(&flow.config, flow.root_parent.clone()),
        ConnectStep::Endpoint => edit_picker(
            &format!("{} endpoint", flow.provider_label),
            "API base URL",
            "usually ends in /v1",
            ConnectEditField::Endpoint,
            flow.endpoint.clone(),
            false,
            PickerAction::ConnectBack(ConnectStep::Provider),
        ),
        ConnectStep::Authentication => authentication_picker(flow),
        ConnectStep::Models => models_picker(flow, &flow.models),
        ConnectStep::Recovery => recovery_picker(flow, "Enter a model ID or try discovery again."),
        ConnectStep::ProviderName
        | ConnectStep::Discovery
        | ConnectStep::Login
        | ConnectStep::Review => provider_picker(&flow.config, flow.root_parent.clone()),
    });
}
