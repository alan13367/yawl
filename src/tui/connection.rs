//! Interactive provider setup shared by idle and active-response TUI loops.

use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::cancellation::CancellationToken;
use crate::config::Config;
use crate::error::Error;
use crate::onboarding::provider::{
    self, ConnectionActivation, ConnectionPlan, CredentialChoice, DiscoveredModel,
    ProviderDefinition, ProviderId,
};
use crate::provider::codex::{CodexLoginStatus, DeviceLoginPrompt};

use super::picker::{FieldTarget, Picker, PickerAction, PickerField, PickerItem, field_picker};
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
    Reasoning,
    Recovery,
    Review,
}

pub(super) struct ConnectFlow {
    config: Config,
    context: ConnectContext,
    step: ConnectStep,
    provider: Option<ProviderId>,
    provider_label: String,
    endpoint: String,
    credential: CredentialChoice,
    model: String,
    models: Vec<DiscoveredModel>,
    reasoning_efforts: Vec<String>,
    job: Option<ConnectJob>,
    root_parent: Option<PickerAction>,
}

enum ConnectJob {
    Discovery {
        receiver: Receiver<Result<Vec<DiscoveredModel>, Error>>,
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

/// Where the flow runs. Setup runs before Yawl starts, so it has no
/// session to switch and offers a way out of setup in the provider list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConnectContext {
    Session,
    Setup,
}

/// Where the flow shows its next screen: the TUI view, or a setup screen.
pub(super) trait ConnectView {
    fn show(&mut self, picker: Picker);
    fn notice(&mut self, message: String);
}

impl ConnectView for ViewState {
    fn show(&mut self, picker: Picker) {
        self.picker = Some(picker);
    }

    fn notice(&mut self, message: String) {
        ViewState::notice(self, message);
    }
}

/// What an action did to the flow.
pub(super) enum FlowStep {
    /// The flow continues on the screen it showed.
    Continue,
    /// Not a connection action; the flow stays open behind it.
    Pass(PickerAction),
    /// The flow ended, with the action to run next, if any.
    End(Option<PickerAction>),
}

impl ConnectFlow {
    pub(super) fn new(
        config: &Config,
        context: ConnectContext,
        root_parent: Option<PickerAction>,
    ) -> Self {
        Self {
            config: config.clone(),
            context,
            step: ConnectStep::Provider,
            provider: None,
            provider_label: String::new(),
            endpoint: String::new(),
            credential: CredentialChoice::Keep,
            model: String::new(),
            models: Vec::new(),
            reasoning_efforts: Vec::new(),
            job: None,
            root_parent,
        }
    }

    pub(super) fn step(&self) -> ConnectStep {
        self.step
    }

    /// Whether discovery or a login runs in the background.
    pub(super) fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// The first screen: the provider list.
    pub(super) fn first_screen(&self) -> Picker {
        provider_picker(self)
    }

    /// Ends any background discovery or login.
    pub(super) fn cancel(&mut self) {
        cancel_job(self);
    }
}

pub(super) fn open(state: &mut ViewState, config: &Config, from_settings: bool) {
    let root_parent = from_settings.then_some(PickerAction::OpenSettingsCategory {
        category: super::picker::SettingsCategory::Providers,
        selected: 0,
    });
    let flow = ConnectFlow::new(config, ConnectContext::Session, root_parent);
    state.picker = Some(flow.first_screen());
    state.connection = Some(flow);
}

pub(super) fn handle_action(state: &mut ViewState, action: PickerAction) -> Option<PickerAction> {
    let Some(mut flow) = state.connection.take() else {
        return Some(action);
    };
    match handle(&mut flow, state, action) {
        FlowStep::Continue => {
            state.connection = Some(flow);
            None
        }
        FlowStep::Pass(action) => {
            state.connection = Some(flow);
            Some(action)
        }
        FlowStep::End(action) => {
            state.picker = None;
            action
        }
    }
}

/// Advances `flow` by one picker action, showing the next screen in `view`.
pub(super) fn handle(
    flow: &mut ConnectFlow,
    view: &mut dyn ConnectView,
    action: PickerAction,
) -> FlowStep {
    match action {
        PickerAction::ConnectChooseProvider(provider) => choose_provider(flow, provider, view),
        PickerAction::ConnectLogin { fresh } => {
            if fresh {
                start_login(flow, view);
            } else {
                start_codex_discovery(flow, view);
            }
        }
        PickerAction::ApplyConnect { field, value } => apply_edit(flow, field, value, view),
        PickerAction::ConnectCredential(credential) => {
            flow.credential = credential;
            start_discovery(flow, view);
        }
        PickerAction::ConnectChooseModel(model) => {
            flow.model = model;
            configure_model_reasoning(flow, view);
        }
        PickerAction::ConnectToggleReasoning(index) => {
            if let Some(effort) = crate::config::REASONING_EFFORTS.get(index) {
                if flow.reasoning_efforts.iter().any(|value| value == effort) {
                    flow.reasoning_efforts.retain(|value| value != effort);
                } else {
                    flow.reasoning_efforts.push((*effort).into());
                }
            }
            let mut picker = connection_reasoning_picker(flow);
            picker.selected = index.min(picker.items.len().saturating_sub(1));
            view.show(picker);
        }
        PickerAction::ConnectConfirmReasoning => {
            flow.step = ConnectStep::Review;
            view.show(review_picker(flow));
        }
        PickerAction::ConnectRetry => {
            if flow.provider == Some(ProviderId::Codex) {
                if crate::provider::codex::credential_status(&flow.config)
                    == CodexLoginStatus::LoggedIn
                {
                    start_codex_discovery(flow, view);
                } else {
                    start_login(flow, view);
                }
            } else {
                start_discovery(flow, view);
            }
        }
        PickerAction::ConnectCancelJob => {
            cancel_job(flow);
            flow.step = ConnectStep::Provider;
            view.show(provider_picker(flow));
        }
        PickerAction::CloseConnect => {
            cancel_job(flow);
            return FlowStep::End(flow.root_parent.clone());
        }
        PickerAction::ConnectBack(step) => go_back(flow, step, view),
        PickerAction::ConnectActivation(activation) => match build_plan(flow, activation) {
            Ok(plan) => return FlowStep::End(Some(PickerAction::ApplyConnectionPlan(plan))),
            Err(error) => {
                view.notice(format!("Could not prepare connection: {error}"));
                view.show(review_picker(flow));
            }
        },
        PickerAction::OpenSettingsCategory { .. } | PickerAction::OpenSettingsRoot { .. } => {
            cancel_job(flow);
            return FlowStep::End(Some(action));
        }
        action => return FlowStep::Pass(action),
    }
    FlowStep::Continue
}

fn provider_picker(flow: &ConnectFlow) -> Picker {
    let mut items = provider::provider_catalog(&flow.config)
        .into_iter()
        .map(|definition| {
            let description = provider_description(&definition);
            PickerItem {
                label: definition.label,
                description,
                action: PickerAction::ConnectChooseProvider(definition.id),
            }
        })
        .collect::<Vec<_>>();
    let (title, hint) = match flow.context {
        ConnectContext::Session => ("Connect provider", "↑/↓ move  Enter configure  Esc close"),
        ConnectContext::Setup => {
            // Esc does the same, but a first run should not have to guess.
            items.push(match &flow.config.model {
                Some(model) => PickerItem {
                    label: "Keep current setup".into(),
                    description: format!("Start with {model}"),
                    action: PickerAction::CloseConnect,
                },
                None => PickerItem {
                    label: "Skip for now".into(),
                    description: "Run 'yawl --setup' anytime, or pass --model".into(),
                    action: PickerAction::CloseConnect,
                },
            });
            (
                "Choose a provider",
                "↑/↓ move  Enter select  Esc leave setup",
            )
        }
    };
    Picker {
        title: title.into(),
        hint: hint.into(),
        items,
        selected: 0,
        editing: None,
        filter: None,
        parent: Some(
            flow.root_parent
                .clone()
                .unwrap_or(PickerAction::CloseConnect),
        ),
    }
}

fn provider_description(provider: &ProviderDefinition) -> String {
    if provider.configured {
        format!("Configured · {}", provider.description)
    } else {
        provider.description.clone()
    }
}

/// The text field for one connection value, labeled for someone setting up
/// a provider for the first time.
pub(super) fn field(field: ConnectEditField, initial: String) -> PickerField {
    let target = FieldTarget::Connect(field);
    match field {
        ConnectEditField::ProviderName => PickerField::new(
            target,
            "Provider name",
            "Becomes the model prefix, as in my-provider:model-id. Use letters, numbers, '-' and '_'.",
            initial,
        )
        .placeholder("my-provider"),
        ConnectEditField::Endpoint => PickerField::new(
            target,
            "API base URL",
            "The OpenAI-compatible endpoint, usually ending in /v1",
            initial,
        )
        .placeholder("http://localhost:8000/v1"),
        ConnectEditField::Environment => PickerField::new(
            target,
            "Environment variable",
            "Yawl saves a $NAME reference and reads the key from your environment",
            initial,
        )
        .placeholder("OPENAI_API_KEY"),
        ConnectEditField::Secret => PickerField::new(
            target,
            "API key",
            "Saved in ~/.yawl/config.json, readable only by you",
            initial,
        )
        .placeholder("Paste your key")
        .secret(),
        ConnectEditField::Model => PickerField::new(
            target,
            "Model ID",
            "The exact model ID the provider expects",
            initial,
        ),
    }
}

/// A validation error phrased for a field: without the internal error kind
/// and starting with a capital letter.
fn field_error(error: Error) -> String {
    let message = match error {
        Error::Config(message) | Error::Invalid(message) => message,
        other => other.to_string(),
    };
    let mut characters = message.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

fn provider_name_picker(value: String, error: Option<String>) -> Picker {
    let mut name = field(ConnectEditField::ProviderName, value);
    if let Some(error) = error {
        name = name.with_error(error);
    }
    field_picker(
        "Add an OpenAI-compatible provider",
        name,
        PickerAction::ConnectBack(ConnectStep::Provider),
    )
}

fn endpoint_picker(flow: &ConnectFlow, value: String, error: Option<&str>) -> Picker {
    let mut endpoint = field(ConnectEditField::Endpoint, value);
    if let Some(error) = error {
        endpoint = endpoint.with_error(error);
    }
    field_picker(
        format!("{} endpoint", flow.provider_label),
        endpoint,
        PickerAction::ConnectBack(ConnectStep::Provider),
    )
}

fn choose_provider(flow: &mut ConnectFlow, provider: ProviderId, view: &mut dyn ConnectView) {
    if provider == ProviderId::Other {
        flow.step = ConnectStep::ProviderName;
        view.show(provider_name_picker(String::new(), None));
        return;
    }
    flow.provider_label = provider::label(&provider);
    flow.provider = Some(provider.clone());
    if provider == ProviderId::Codex {
        if crate::provider::codex::credential_status(&flow.config) == CodexLoginStatus::LoggedIn {
            flow.step = ConnectStep::Login;
            view.show(codex_login_picker());
        } else {
            start_login(flow, view);
        }
        return;
    }
    flow.endpoint = provider::default_endpoint(&provider, &flow.config);
    flow.step = ConnectStep::Endpoint;
    view.show(endpoint_picker(flow, flow.endpoint.clone(), None));
}

/// Applies a submitted value, or reopens its field with the problem shown
/// under the value the user typed.
fn apply_edit(
    flow: &mut ConnectFlow,
    field: ConnectEditField,
    value: String,
    view: &mut dyn ConnectView,
) {
    match field {
        ConnectEditField::ProviderName => match provider::validate_provider_name(&value) {
            Ok(()) => choose_provider(flow, ProviderId::Compatible(value), view),
            Err(error) => {
                view.show(provider_name_picker(value, Some(field_error(error))));
            }
        },
        ConnectEditField::Endpoint => {
            if !value.starts_with("http://") && !value.starts_with("https://") {
                view.show(endpoint_picker(
                    flow,
                    value,
                    Some("Start the URL with http:// or https://."),
                ));
                return;
            }
            flow.endpoint = value;
            flow.step = ConnectStep::Authentication;
            view.show(authentication_picker(flow));
        }
        ConnectEditField::Environment => {
            let name = value.trim().trim_start_matches('$').to_string();
            if let Err(error) = provider::validate_environment_name(&name) {
                view.show(environment_picker(flow, name, Some(field_error(error))));
                return;
            }
            flow.credential = CredentialChoice::Environment(name);
            start_discovery(flow, view);
        }
        ConnectEditField::Secret => {
            flow.credential = CredentialChoice::Literal(value);
            start_discovery(flow, view);
        }
        ConnectEditField::Model => {
            flow.model = value;
            configure_model_reasoning(flow, view);
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
            label: "Enter API key…".into(),
            description: "Paste or type it; Tab shows it while typing".into(),
            action: PickerAction::EditConnect {
                field: ConnectEditField::Secret,
                initial: String::new(),
                secret: true,
            },
        },
        PickerItem {
            label: "Use environment variable…".into(),
            description: environment_description(&default_environment(flow)),
            action: PickerAction::EditConnect {
                field: ConnectEditField::Environment,
                initial: default_environment(flow),
                secret: false,
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
        filter: None,
        parent: Some(PickerAction::ConnectBack(ConnectStep::Endpoint)),
    }
}

fn default_environment(flow: &ConnectFlow) -> String {
    flow.provider
        .as_ref()
        .and_then(provider::credential_environment_name)
        .unwrap_or_default()
}

/// Says which variable would be used and whether this shell has it, so
/// choosing it is not a guess.
fn environment_description(name: &str) -> String {
    if name.is_empty() {
        return "Read the key from a variable you name".into();
    }
    let set = std::env::var_os(name).is_some_and(|value| !value.is_empty());
    format!(
        "Read ${name}{}",
        if set {
            " · set here"
        } else {
            " · not set here"
        }
    )
}

fn environment_picker(flow: &ConnectFlow, initial: String, error: Option<String>) -> Picker {
    let mut variable = field(ConnectEditField::Environment, initial);
    if let Some(error) = error {
        variable = variable.with_error(error);
    }
    field_picker(
        format!("{} credential", flow.provider_label),
        variable,
        PickerAction::ConnectBack(ConnectStep::Authentication),
    )
}

fn start_discovery(flow: &mut ConnectFlow, view: &mut dyn ConnectView) {
    let Some(provider) = flow.provider.clone() else {
        return;
    };
    let endpoint = flow.endpoint.clone();
    let key = provider::request_credential(&flow.config, &provider, &flow.credential);
    let description = format!("Testing {} in the background", flow.provider_label);
    spawn_discovery(flow, view, "Discovering models", &description, move || {
        provider::discover_models(&provider, &endpoint, &key)
    });
}

fn start_codex_discovery(flow: &mut ConnectFlow, view: &mut dyn ConnectView) {
    let config = flow.config.clone();
    spawn_discovery(
        flow,
        view,
        "Discovering Codex models",
        "Fetching models for this account",
        move || {
            let models = crate::provider::codex::refresh_catalog(&config)?;
            Ok(models
                .into_iter()
                .map(|model| DiscoveredModel::named(model.slug))
                .collect())
        },
    );
}

/// Lists models on a worker thread behind a cancelable waiting picker. A
/// cancel during the request reports an interruption instead of its result.
fn spawn_discovery(
    flow: &mut ConnectFlow,
    view: &mut dyn ConnectView,
    title: &str,
    description: &str,
    discover: impl FnOnce() -> Result<Vec<DiscoveredModel>, Error> + Send + 'static,
) {
    let cancellation = CancellationToken::default();
    let worker_cancellation = cancellation.clone();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let result = crate::cancellation::scope(&worker_cancellation, || {
            if worker_cancellation.is_canceled() {
                return Err(Error::Interrupted);
            }
            let result = discover();
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
    view.show(waiting_picker(
        title,
        description,
        PickerAction::ConnectCancelJob,
    ));
}

fn start_login(flow: &mut ConnectFlow, view: &mut dyn ConnectView) {
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
    view.show(waiting_picker(
        "OpenAI Codex login",
        "Starting device authentication…",
        PickerAction::ConnectCancelJob,
    ));
}

/// Offers a fresh device login when an account is already saved, for
/// switching accounts.
fn codex_login_picker() -> Picker {
    Picker {
        title: "OpenAI Codex login".into(),
        hint: "↑/↓ move  Enter select  Esc back".into(),
        items: vec![
            PickerItem {
                label: "Use the existing login".into(),
                description: "Already saved in ~/.yawl/auth.json".into(),
                action: PickerAction::ConnectLogin { fresh: false },
            },
            PickerItem {
                label: "Sign in again".into(),
                description: "Start a new device login, for another account".into(),
                action: PickerAction::ConnectLogin { fresh: true },
            },
        ],
        selected: 0,
        editing: None,
        filter: None,
        parent: Some(PickerAction::ConnectBack(ConnectStep::Provider)),
    }
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
        filter: None,
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
        filter: None,
        parent: Some(PickerAction::ConnectCancelJob),
    }
}

pub(super) fn poll(state: &mut ViewState) -> bool {
    let Some(mut flow) = state.connection.take() else {
        return false;
    };
    let changed = poll_flow(&mut flow, state);
    state.connection = Some(flow);
    changed
}

/// Shows the next screen once background discovery or login reports.
/// Returns whether anything changed.
pub(super) fn poll_flow(flow: &mut ConnectFlow, view: &mut dyn ConnectView) -> bool {
    let Some(job) = flow.job.as_mut() else {
        return false;
    };
    enum PollEvent {
        None,
        Models(Vec<DiscoveredModel>),
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
    match event {
        PollEvent::None => false,
        PollEvent::Models(models) => {
            flow.job = None;
            show_models(flow, models, view);
            true
        }
        PollEvent::DiscoveryFailure(message) => {
            flow.job = None;
            let cached = if flow.provider == Some(ProviderId::Codex) {
                crate::provider::codex::cached_models(&flow.config)
                    .into_iter()
                    .map(|model| DiscoveredModel::named(model.slug))
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            if cached.is_empty() {
                show_recovery(flow, message, view);
            } else {
                view.notice("Could not refresh Codex models. Showing the saved catalog.".into());
                show_models(flow, cached, view);
            }
            true
        }
        PollEvent::LoginPrompt(prompt) => {
            view.show(login_prompt_picker(&prompt));
            true
        }
        PollEvent::LoginDone => {
            flow.job = None;
            start_codex_discovery(flow, view);
            true
        }
        PollEvent::LoginFailure => {
            flow.job = None;
            show_recovery(flow, "OpenAI Codex login did not complete.", view);
            true
        }
    }
}

fn show_models(flow: &mut ConnectFlow, models: Vec<DiscoveredModel>, view: &mut dyn ConnectView) {
    flow.step = ConnectStep::Models;
    flow.models = models.clone();
    view.show(models_picker(flow, &models));
}

fn models_picker(flow: &ConnectFlow, models: &[DiscoveredModel]) -> Picker {
    let codex = if flow.provider == Some(ProviderId::Codex) {
        crate::provider::codex::cached_models(&flow.config)
    } else {
        Vec::new()
    };
    let mut items = models
        .iter()
        .map(|model| PickerItem {
            label: codex
                .iter()
                .find(|entry| entry.slug == model.id)
                .map(|entry| entry.display_name.clone())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| model.id.clone()),
            description: if !codex.is_empty() {
                model.id.clone()
            } else if let Some(window) = model.context_window {
                format!("{} context", super::status_bar::format_token_count(window))
            } else {
                "Available from this provider".into()
            },
            action: PickerAction::ConnectChooseModel(model.id.clone()),
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
    // Every discovered model is listed; aggregators list hundreds, so the
    // picker searches as you type and keeps the manual entry row.
    Picker {
        title: format!("{} models", flow.provider_label),
        hint: "Type to search  ↑/↓ move  Enter select  Esc back".into(),
        items,
        selected: 0,
        editing: None,
        filter: None,
        parent: Some(PickerAction::ConnectBack(
            if flow.provider == Some(ProviderId::Codex) {
                ConnectStep::Provider
            } else {
                ConnectStep::Authentication
            },
        )),
    }
    .searchable(1)
}

fn show_recovery(flow: &mut ConnectFlow, message: &str, view: &mut dyn ConnectView) {
    flow.step = ConnectStep::Recovery;
    view.show(recovery_picker(flow, message));
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
        filter: None,
        parent: Some(PickerAction::ConnectBack(
            if flow.provider == Some(ProviderId::Codex) {
                ConnectStep::Provider
            } else {
                ConnectStep::Authentication
            },
        )),
    }
}

fn configure_model_reasoning(flow: &mut ConnectFlow, view: &mut dyn ConnectView) {
    if matches!(flow.provider, Some(ProviderId::Compatible(_))) {
        let saved = crate::model::saved_reasoning_efforts(&flow.config, &qualified_model(flow));
        // `None` is an unknown provider or unlisted model and defaults to
        // every level; `Some(vec![])` is the explicit "send no effort" choice
        // from clearing every box, preserved so it survives re-entry.
        flow.reasoning_efforts = match saved {
            None => crate::config::REASONING_EFFORTS
                .iter()
                .map(|effort| (*effort).to_string())
                .collect(),
            Some(efforts) => efforts.into_iter().map(str::to_string).collect(),
        };
        flow.step = ConnectStep::Reasoning;
        view.show(connection_reasoning_picker(flow));
    } else {
        flow.reasoning_efforts.clear();
        flow.step = ConnectStep::Review;
        view.show(review_picker(flow));
    }
}

fn connection_reasoning_picker(flow: &ConnectFlow) -> Picker {
    Picker {
        title: "Supported reasoning efforts".into(),
        hint: "↑/↓ move  Space toggle  Enter confirm  Esc back".into(),
        items: crate::config::REASONING_EFFORTS
            .iter()
            .enumerate()
            .map(|(index, effort)| {
                let checked = flow.reasoning_efforts.iter().any(|value| value == effort);
                PickerItem {
                    label: format!("[{}] {effort}", if checked { "x" } else { " " }),
                    description: "Select levels this model accepts; leave all clear for none"
                        .into(),
                    action: PickerAction::ConnectToggleReasoning(index),
                }
            })
            .collect(),
        selected: 0,
        editing: None,
        filter: None,
        parent: Some(PickerAction::ConnectBack(ConnectStep::Models)),
    }
}

fn review_picker(flow: &ConnectFlow) -> Picker {
    let description = format!("{} · {}", flow.provider_label, qualified_model(flow));
    let item = |label: &str, activation| PickerItem {
        label: label.into(),
        description: description.clone(),
        action: PickerAction::ConnectActivation(activation),
    };
    let mut items = vec![item(
        "Save and use as default",
        ConnectionActivation::Default,
    )];
    let session = flow.context == ConnectContext::Session;
    if session {
        items.push(item(
            "Save and use this session",
            ConnectionActivation::Session,
        ));
    }
    // Setup has no session yet, and its first connection must become the
    // default for Yawl to start.
    if session || flow.config.model.is_some() {
        items.push(item(
            "Save connection only",
            ConnectionActivation::ConnectionOnly,
        ));
    }
    Picker {
        title: "Review connection".into(),
        hint: "↑/↓ move  Enter save  Esc back".into(),
        items,
        selected: 0,
        editing: None,
        filter: None,
        parent: Some(PickerAction::ConnectBack(
            if matches!(flow.provider, Some(ProviderId::Compatible(_))) {
                ConnectStep::Reasoning
            } else {
                ConnectStep::Models
            },
        )),
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
        .ok_or_else(|| Error::Invalid("provider is not selected".into()))?;
    let mut changes = Vec::new();
    if provider != &ProviderId::Codex {
        changes.push(provider::endpoint_change(
            provider,
            flow.endpoint.clone(),
            &flow.credential,
        )?);
        if let ProviderId::Compatible(name) = provider {
            changes.push(crate::config::ConfigChange::ProviderModel {
                name: name.clone(),
                model: flow.model.clone(),
                reasoning_efforts: crate::config::REASONING_EFFORTS
                    .iter()
                    .filter(|effort| flow.reasoning_efforts.iter().any(|value| value == *effort))
                    .map(|effort| (*effort).to_string())
                    .collect(),
                context_window: flow
                    .models
                    .iter()
                    .find(|model| model.id == flow.model)
                    .and_then(|model| model.context_window),
            });
        }
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

fn go_back(flow: &mut ConnectFlow, step: ConnectStep, view: &mut dyn ConnectView) {
    flow.step = step;
    view.show(match step {
        ConnectStep::Provider => provider_picker(flow),
        ConnectStep::Endpoint => endpoint_picker(flow, flow.endpoint.clone(), None),
        ConnectStep::Authentication => authentication_picker(flow),
        ConnectStep::Models => models_picker(flow, &flow.models),
        ConnectStep::Reasoning => connection_reasoning_picker(flow),
        ConnectStep::Recovery => recovery_picker(flow, "Enter a model ID or try discovery again."),
        ConnectStep::ProviderName
        | ConnectStep::Discovery
        | ConnectStep::Login
        | ConnectStep::Review => provider_picker(flow),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_only_persists_the_selected_compatible_model() -> Result<(), Error> {
        let root = crate::test_support::unique_temp_dir("connection-only-model");
        let config = Config::load_from(root.join("home/.yawl"), root.join("project/.yawl"))?;
        let flow = ConnectFlow {
            config: config.clone(),
            context: ConnectContext::Session,
            step: ConnectStep::Review,
            provider: Some(ProviderId::Compatible("local-api".into())),
            provider_label: "Local API".into(),
            endpoint: "http://127.0.0.1:9000/v1".into(),
            credential: CredentialChoice::None,
            model: "chosen-model".into(),
            models: vec![
                DiscoveredModel {
                    id: "chosen-model".into(),
                    context_window: Some(65_536),
                },
                DiscoveredModel {
                    id: "other-model".into(),
                    context_window: Some(131_072),
                },
            ],
            reasoning_efforts: vec!["low".into(), "ultra".into()],
            job: None,
            root_parent: None,
        };

        let plan = build_plan(&flow, ConnectionActivation::ConnectionOnly)?;
        let saved = config.change_global_batch(plan.changes_for_save())?.config;

        assert_eq!(
            saved.providers["local-api"]
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            ["chosen-model"]
        );
        assert!(
            crate::model::available_models(&saved)
                .iter()
                .any(|(model, _)| model == "local-api:chosen-model")
        );
        assert_eq!(saved.model, config.model);
        assert_eq!(
            saved.providers["local-api"].models[0].reasoning_efforts,
            ["low", "ultra"]
        );
        assert_eq!(
            crate::model::context_window(&saved, "local-api:chosen-model"),
            65_536
        );

        let saved_again = saved.change_global_batch(plan.changes_for_save())?.config;
        assert_eq!(saved_again.providers["local-api"].models.len(), 1);
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }

    #[test]
    fn models_picker_lists_every_model_and_searches_them() -> Result<(), Error> {
        let root = crate::test_support::unique_temp_dir("connection-models-picker");
        let config = Config::load_from(root.join("home/.yawl"), root.join("project/.yawl"))?;
        let flow = ConnectFlow {
            provider: Some(ProviderId::Compatible("router".into())),
            provider_label: "Router".into(),
            ..ConnectFlow::new(&config, ConnectContext::Session, None)
        };
        let mut models = (0..300)
            .map(|index| DiscoveredModel::named(format!("vendor{}/model-{index:03}", index % 7)))
            .collect::<Vec<_>>();
        models.push(DiscoveredModel {
            id: "qwen/qwen3-coder".into(),
            context_window: Some(262_144),
        });

        let mut picker = models_picker(&flow, &models);

        assert_eq!(picker.items.len(), 301 + 1, "no cap on discovered models");
        assert!(picker.filter.is_some());
        assert_eq!(
            picker.hint,
            "Type to search  ↑/↓ move  Enter select  Esc back"
        );
        let qwen = picker
            .items
            .iter()
            .find(|item| item.label == "qwen/qwen3-coder")
            .expect("qwen listed");
        assert_eq!(qwen.description, "262.1k context");
        assert_eq!(picker.items[0].description, "Available from this provider");

        picker.set_filter("vendor3 -00");
        let labels = picker
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            ["vendor3/model-003", "Enter model manually…"],
            "the manual row stays while the list narrows"
        );
        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }
}
