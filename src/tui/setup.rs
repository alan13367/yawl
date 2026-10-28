//! Screens shown before a session starts: provider setup and the project
//! trust question. Both are ordinary pickers, and setup drives the same
//! connection flow as `/connect`. A terminal draws them full screen under
//! the wordmark; piped input gets numbered prompts.

mod lines;
mod screen;

use std::io::IsTerminal;
use std::path::Path;

use crate::config::Config;
use crate::error::Error;
use crate::onboarding::provider::{ConnectionActivation, ConnectionPlan};

use super::connection::{self, ConnectContext, ConnectFlow, ConnectStep, ConnectView, FlowStep};
use super::picker::{Picker, PickerAction, PickerItem};

/// Setup's stages, shown as a progress line above the panel.
const SETUP_STEPS: [&str; 4] = ["Provider", "Connect", "Model", "Save"];

/// Text above the picker panel.
struct Backdrop {
    tagline: String,
    details: Vec<String>,
    /// Index of the current entry in `SETUP_STEPS`; past the end once
    /// everything is done.
    progress: Option<usize>,
}

/// One screen: the backdrop, the picker, and the latest notice.
struct Page {
    backdrop: Backdrop,
    picker: Picker,
    notice: Option<String>,
    /// Discovery or a login runs in the background, so the page changes
    /// without input.
    busy: bool,
}

impl ConnectView for Page {
    fn show(&mut self, picker: Picker) {
        self.picker = picker;
    }

    fn notice(&mut self, message: String) {
        self.notice = Some(message);
    }
}

/// Presents pages and returns the action the user picks.
trait Surface {
    /// Shows `page` until the user picks an action. `poll` runs while the
    /// surface waits and says whether it changed the page.
    fn choose(
        &mut self,
        page: &mut Page,
        poll: &mut dyn FnMut(&mut Page) -> bool,
    ) -> Result<PickerAction, Error>;
}

/// The full-screen surface when both ends are a terminal, else prompts.
fn surface(config: &Config, end_of_input: Option<PickerAction>) -> Result<Box<dyn Surface>, Error> {
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        Ok(Box::new(screen::Screen::enter(config)?))
    } else {
        Ok(Box::new(lines::Lines::new(end_of_input)))
    }
}

/// Runs provider setup and returns the config as last saved. The model is
/// still unset when the user left without saving one.
///
/// # Errors
///
/// Returns terminal and save errors, and [`Error::Interrupted`] for Ctrl+C.
pub(crate) fn run_setup(config: &Config) -> Result<Config, Error> {
    let mut surface = surface(config, None)?;
    let mut flow = ConnectFlow::new(config, ConnectContext::Setup, None);
    let result = setup(config, &mut flow, surface.as_mut());
    flow.cancel();
    result
}

/// What the screen after a save offers.
const START: usize = 0;
const CONNECT_ANOTHER: usize = 1;

fn setup(
    config: &Config,
    flow: &mut ConnectFlow,
    surface: &mut dyn Surface,
) -> Result<Config, Error> {
    let mut config = config.clone();
    let mut page = Page {
        backdrop: setup_backdrop(&config),
        picker: flow.first_screen(),
        notice: None,
        busy: false,
    };
    // Set while the screen after a save is showing.
    let mut saved = false;
    loop {
        if !saved {
            page.backdrop.progress = Some(progress(flow.step()));
            page.busy = flow.busy();
        }
        let action = surface.choose(&mut page, &mut |page| {
            if saved {
                return false;
            }
            let changed = connection::poll_flow(flow, page);
            page.backdrop.progress = Some(progress(flow.step()));
            page.busy = flow.busy();
            changed
        })?;
        page.notice = None;
        match action {
            PickerAction::Choice(CONNECT_ANOTHER) => {
                saved = false;
                *flow = ConnectFlow::new(&config, ConnectContext::Setup, None);
                page.backdrop = setup_backdrop(&config);
                page.picker = flow.first_screen();
            }
            PickerAction::Choice(_) => return Ok(config),
            action => match connection::handle(flow, &mut page, action) {
                FlowStep::Continue | FlowStep::Pass(_) => {}
                FlowStep::End(Some(PickerAction::ApplyConnectionPlan(plan))) => {
                    config = save(&config, &plan)?;
                    saved = true;
                    page.picker = saved_picker(&config, &plan);
                    page.backdrop.tagline = format!("{} is ready", plan.provider_label);
                    page.backdrop.details[0] = "Start now, or connect another provider.".into();
                    // Past the last step, so every stage shows as done.
                    page.backdrop.progress = Some(SETUP_STEPS.len());
                    page.busy = false;
                }
                FlowStep::End(_) => return Ok(config),
            },
        }
    }
}

fn setup_backdrop(config: &Config) -> Backdrop {
    let path = super::tool_view::home_relative(
        &config.global_config_path(),
        std::env::home_dir().as_deref(),
    );
    let first_run = config.model.is_none();
    Backdrop {
        tagline: if first_run {
            "Welcome to Yawl".into()
        } else {
            "Provider setup".into()
        },
        details: vec![
            if first_run {
                "Connect a model provider to get started.".into()
            } else {
                "Add a provider or change the default model.".into()
            },
            crate::text::sanitize_line(
                &format!("Changes are saved to {path}"),
                crate::text::Tabs::Space,
            ),
        ],
        progress: Some(0),
    }
}

fn progress(step: ConnectStep) -> usize {
    match step {
        ConnectStep::Provider | ConnectStep::ProviderName => 0,
        ConnectStep::Endpoint
        | ConnectStep::Authentication
        | ConnectStep::Login
        | ConnectStep::Discovery
        | ConnectStep::Recovery => 1,
        ConnectStep::Models | ConnectStep::Reasoning => 2,
        ConnectStep::Review => 3,
    }
}

/// Saves `plan` globally. A default must survive the project config, or Yawl
/// would start without the model the user just chose.
fn save(config: &Config, plan: &ConnectionPlan) -> Result<Config, Error> {
    let mut changes = plan.changes_for_save();
    if config.setup_skipped {
        changes.push(crate::config::ConfigChange::SetupSkipped(false));
    }
    let updated = config.change_global_batch(changes)?.config;
    if plan.activation == ConnectionActivation::Default && updated.model.is_none() {
        return Err(Error::Config(
            "the model was saved globally but a project config overrides it; remove the empty project model setting"
                .into(),
        ));
    }
    Ok(updated)
}

fn saved_picker(config: &Config, plan: &ConnectionPlan) -> Picker {
    let model = config.model.clone().unwrap_or_default();
    let saved = if plan.activation == ConnectionActivation::Default {
        format!("Default model: {model}")
    } else {
        format!("{} saved · default stays {model}", plan.model)
    };
    Picker {
        title: "Connection saved".into(),
        hint: "↑/↓ move  Enter select".into(),
        items: vec![
            PickerItem {
                label: "Finish setup".into(),
                description: saved,
                action: PickerAction::Choice(START),
            },
            PickerItem {
                label: "Connect another provider".into(),
                description: "Switch between models later with /model".into(),
                action: PickerAction::Choice(CONNECT_ANOTHER),
            },
        ],
        selected: 0,
        editing: None,
        parent: Some(PickerAction::Choice(START)),
    }
}

/// How the user answered the project trust question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrustAnswer {
    Permanent,
    Session,
    Deny,
}

const TRUST_ANSWERS: [TrustAnswer; 3] = [
    TrustAnswer::Permanent,
    TrustAnswer::Session,
    TrustAnswer::Deny,
];
const DENY: usize = 2;

/// Asks whether to trust the project at `root`, whose settings include
/// `found` (such as "skills" or "tools"). Esc, and the end of piped input,
/// answer no.
///
/// # Errors
///
/// Returns terminal errors and [`Error::Interrupted`] for Ctrl+C.
pub(crate) fn ask_project_trust(
    config: &Config,
    root: &Path,
    found: &[&str],
) -> Result<TrustAnswer, Error> {
    let mut surface = surface(config, Some(PickerAction::Choice(DENY)))?;
    let mut page = trust_page(root, found);
    match surface.choose(&mut page, &mut |_| false)? {
        PickerAction::Choice(index) => Ok(TRUST_ANSWERS
            .get(index)
            .copied()
            .unwrap_or(TrustAnswer::Deny)),
        _ => Ok(TrustAnswer::Deny),
    }
}

fn trust_page(root: &Path, found: &[&str]) -> Page {
    let path = crate::text::sanitize_line(
        &super::tool_view::home_relative(root, std::env::home_dir().as_deref()),
        crate::text::Tabs::Space,
    );
    let mut details = vec![path.clone()];
    if !found.is_empty() {
        details.push(format!("This project defines {}.", list(found)));
    }
    details.push("Trusting lets them run with your credentials and network access.".into());
    let item = |label: &str, description: String, index: usize| PickerItem {
        label: label.into(),
        description,
        action: PickerAction::Choice(index),
    };
    Page {
        backdrop: Backdrop {
            tagline: "Do you trust this project?".into(),
            details,
            progress: None,
        },
        picker: Picker {
            title: "Project trust".into(),
            hint: "↑/↓ move  Enter select  Esc don't trust".into(),
            items: vec![
                item(
                    "Trust this project",
                    "Remembered in ~/.yawl/trust.json".into(),
                    0,
                ),
                item("Trust for this session", "Ask again next time".into(), 1),
                item(
                    "Don't trust",
                    "Ignore its project settings; remembered".into(),
                    DENY,
                ),
            ],
            // The safe answer is the default, as Enter alone always meant.
            selected: DENY,
            editing: None,
            parent: Some(PickerAction::Choice(DENY)),
        },
        notice: None,
        busy: false,
    }
}

/// "skills, tools, and provider settings".
fn list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [only] => (*only).to_string(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers with a fixed list of actions, recording each page it sees.
    struct Script {
        actions: Vec<PickerAction>,
        titles: Vec<String>,
    }

    impl Surface for Script {
        fn choose(
            &mut self,
            page: &mut Page,
            _poll: &mut dyn FnMut(&mut Page) -> bool,
        ) -> Result<PickerAction, Error> {
            self.titles.push(page.picker.title.clone());
            if self.actions.is_empty() {
                return Err(Error::Interrupted);
            }
            Ok(self.actions.remove(0))
        }
    }

    fn temp_config(name: &str) -> (std::path::PathBuf, Config) {
        let root = crate::test_support::unique_temp_dir(name);
        let config =
            Config::load_from(root.join("home/.yawl"), root.join("project/.yawl")).expect("config");
        (root, config)
    }

    #[test]
    fn leaving_the_provider_list_returns_the_unchanged_config() {
        let (root, config) = temp_config("setup-leave");
        let mut flow = ConnectFlow::new(&config, ConnectContext::Setup, None);
        let first = flow.first_screen();
        let exit = first.items.last().expect("exit row");
        assert_eq!(exit.label, "Skip for now");
        let mut script = Script {
            actions: vec![exit.action.clone()],
            titles: Vec::new(),
        };
        let result = setup(&config, &mut flow, &mut script).expect("setup");
        assert_eq!(result.model, None);
        assert_eq!(script.titles, ["Choose a provider"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn setup_walks_the_connect_flow_and_backs_out() {
        let (root, config) = temp_config("setup-save");
        let mut flow = ConnectFlow::new(&config, ConnectContext::Setup, None);
        let mut script = Script {
            actions: vec![
                PickerAction::ConnectChooseProvider(
                    crate::onboarding::provider::ProviderId::Compatible("ollama".into()),
                ),
                PickerAction::ApplyConnect {
                    field: connection::ConnectEditField::Endpoint,
                    value: "http://127.0.0.1:9/v1".into(),
                },
                PickerAction::ConnectCancelJob,
                PickerAction::ConnectBack(ConnectStep::Provider),
                PickerAction::CloseConnect,
            ],
            titles: Vec::new(),
        };
        let result = setup(&config, &mut flow, &mut script).expect("setup");
        flow.cancel();
        assert_eq!(result.model, None);
        assert_eq!(
            script.titles,
            [
                "Choose a provider",
                "Ollama endpoint",
                "Ollama authentication",
                "Choose a provider",
                "Choose a provider",
            ]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn saving_sets_the_default_and_rejects_a_project_override() {
        let (root, config) = temp_config("setup-default");
        let plan = ConnectionPlan {
            changes: Vec::new(),
            model: "openai-codex:gpt-test".into(),
            activation: ConnectionActivation::Default,
            provider_label: "OpenAI Codex".into(),
        };
        let saved = save(&config, &plan).expect("save");
        assert_eq!(saved.model.as_deref(), Some("openai-codex:gpt-test"));
        let picker = saved_picker(&saved, &plan);
        assert_eq!(
            picker.items[0].description,
            "Default model: openai-codex:gpt-test"
        );
        assert!(matches!(picker.parent, Some(PickerAction::Choice(START))));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn progress_follows_the_connection_steps() {
        assert_eq!(progress(ConnectStep::Provider), 0);
        assert_eq!(progress(ConnectStep::Authentication), 1);
        assert_eq!(progress(ConnectStep::Login), 1);
        assert_eq!(progress(ConnectStep::Reasoning), 2);
        assert_eq!(progress(ConnectStep::Review), 3);
    }

    #[test]
    fn trust_defaults_to_no_and_names_what_the_project_defines() {
        let mut script = Script {
            actions: vec![PickerAction::Choice(1)],
            titles: Vec::new(),
        };
        let page = trust_page(Path::new("/srv/app\x1b[2J"), &["skills", "tools"]);
        assert_eq!(page.picker.selected, 2);
        assert!(!page.backdrop.details[0].contains('\x1b'));
        assert_eq!(
            page.backdrop.details[1],
            "This project defines skills and tools."
        );
        let mut page = page;
        assert!(matches!(
            script.choose(&mut page, &mut |_| false).expect("answer"),
            PickerAction::Choice(1)
        ));
        assert_eq!(list(&["a", "b", "c"]), "a, b, and c");
    }
}
