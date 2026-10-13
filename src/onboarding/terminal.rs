use std::io::{self, IsTerminal, Write};

use crate::config::validate_environment_name;
use crate::error::Error;

use super::field::Field;
use super::select::{self, Choice};

pub(super) enum Authentication {
    Keep(String),
    None,
    Environment { reference: String, value: String },
    Literal(String),
}

impl Authentication {
    pub(super) fn config_value(&self) -> Option<&str> {
        match self {
            Authentication::Keep(_) => None,
            Authentication::None => Some("-"),
            Authentication::Environment { reference, .. } => Some(reference),
            Authentication::Literal(value) => Some(value),
        }
    }

    pub(super) fn request_key(&self) -> &str {
        match self {
            Authentication::None => "",
            Authentication::Keep(value)
            | Authentication::Environment { value, .. }
            | Authentication::Literal(value) => value,
        }
    }
}

/// Asks how the provider authenticates. `None` means the user canceled and
/// wants to go back.
pub(super) fn choose_authentication(
    existing: Option<String>,
    allow_no_key: bool,
) -> Result<Option<Authentication>, Error> {
    let mut choices = Vec::new();
    if existing.is_some() {
        choices.push(Choice::new(
            "Keep current credential",
            "reuse it without displaying or changing it",
        ));
    }
    choices.extend([
        Choice::new("Environment variable", "stores a $NAME reference"),
        Choice::new("Enter the key now", "stored in config.json"),
    ]);
    if allow_no_key {
        choices.push(Choice::new("No API key", "clears any saved credential"));
    }
    let choice = select::select("Authentication", &choices)?;
    let offset = usize::from(existing.is_some());
    match choice {
        None => Ok(None),
        Some(0) if existing.is_some() => Ok(existing.map(Authentication::Keep)),
        Some(index) if index == offset => {
            let Some(name) = ask_environment_name()? else {
                return Ok(None);
            };
            let value = std::env::var(&name).unwrap_or_default();
            if value.is_empty() {
                println!("Note: {name} is not set right now. Requests will fail until it is.");
            }
            Ok(Some(Authentication::Environment {
                reference: format!("${name}"),
                value,
            }))
        }
        Some(index) if index == offset + 1 => Ok(ask_api_key()?.map(Authentication::Literal)),
        Some(index) if allow_no_key && index == offset + 2 => Ok(Some(Authentication::None)),
        Some(_) => Ok(None),
    }
}

/// Asks a yes or no question until it gets a valid answer. An empty answer
/// picks `default`.
pub(super) fn confirm(question: &str, default: bool) -> Result<bool, Error> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    loop {
        let answer = prompt(&format!("{question} {hint}"))?;
        match answer.to_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("Answer y or n."),
        }
    }
}

pub(super) fn prompt(label: &str) -> Result<String, Error> {
    print!("{label}: ");
    io::stdout().flush()?;
    read_line()
}

/// Asks for one value. Terminals get an inline field with an editable
/// default; piped input falls back to line prompts that repeat until
/// `validate` passes. `None` means the user pressed Esc to go back.
pub(super) fn ask_text(
    field: Field<'_>,
    validate: &dyn Fn(&str) -> Result<(), String>,
) -> Result<Option<String>, Error> {
    if io::stdin().is_terminal() {
        return super::field::ask(&field, validate);
    }
    loop {
        let value = if field.initial.is_empty() {
            prompt(field.label)?
        } else {
            prompt_with_default(field.label, field.initial)?
        };
        match validate(&value) {
            Ok(()) => return Ok(Some(value)),
            Err(error) => println!("{error}"),
        }
    }
}

pub(super) fn ask_api_key() -> Result<Option<String>, Error> {
    ask_text(
        Field {
            label: "API key",
            initial: "",
            placeholder: "Paste your key",
            help: "Saved in ~/.yawl/config.json, readable only by you",
            secret: true,
        },
        &|value| {
            if value.is_empty() {
                Err("Paste or type the API key, or press Esc to go back.".into())
            } else {
                Ok(())
            }
        },
    )
}

fn ask_environment_name() -> Result<Option<String>, Error> {
    let name = ask_text(
        Field {
            label: "Environment variable",
            initial: "",
            placeholder: "OPENAI_API_KEY",
            help: "Yawl saves a $NAME reference and reads the key at run time",
            secret: false,
        },
        &|value| {
            validate_environment_name(value.trim_start_matches('$')).map_err(|_| {
                "Use letters, numbers, and '_', starting with a letter or '_'.".to_string()
            })
        },
    )?;
    Ok(name.map(|name| name.trim_start_matches('$').to_string()))
}

pub(super) fn prompt_with_default(label: &str, default: &str) -> Result<String, Error> {
    print!("{label} [{default}]: ");
    io::stdout().flush()?;
    let value = read_line()?;
    if value.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(value)
    }
}

fn read_line() -> Result<String, Error> {
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Err(Error::Config("onboarding canceled".into()));
    }
    Ok(line.trim().to_string())
}

/// Reads one byte of raw-mode terminal input; `None` at end of input.
pub(super) fn read_byte() -> io::Result<Option<u8>> {
    use std::io::Read;
    let mut buffer = [0u8; 1];
    Ok((io::stdin().read(&mut buffer)? == 1).then_some(buffer[0]))
}
