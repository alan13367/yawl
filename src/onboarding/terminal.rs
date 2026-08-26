use std::io::{self, Write};

use crate::error::Error;

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
            let name = loop {
                let name = prompt("Environment variable name, without '$'")?;
                if validate_environment_name(&name).is_ok() {
                    break name;
                }
                println!("Use letters, numbers, and '_', starting with a letter or '_'.");
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
        Some(index) if index == offset + 1 => {
            let key = loop {
                let key = prompt_secret("API key")?;
                if !key.is_empty() {
                    break key;
                }
                println!("The API key must not be empty.");
            };
            Ok(Some(Authentication::Literal(key)))
        }
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

pub(super) fn prompt_secret(label: &str) -> Result<String, Error> {
    print!("{label}, input hidden: ");
    io::stdout().flush()?;

    // SAFETY: `termios` is initialized by `tcgetattr` before use. stdin is a
    // TTY during onboarding, and the guard restores the original flags.
    let original = unsafe {
        let mut original: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut original) != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        let mut hidden = original;
        hidden.c_lflag &= !libc::ECHO;
        if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &hidden) != 0 {
            return Err(Error::Io(io::Error::last_os_error()));
        }
        original
    };
    let guard = EchoGuard(original);
    let value = read_line();
    drop(guard);
    println!();
    value
}

struct EchoGuard(libc::termios);

impl Drop for EchoGuard {
    fn drop(&mut self) {
        // SAFETY: The value came from a successful `tcgetattr` call for
        // stdin and remains initialized until this guard is dropped.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0);
        }
    }
}

fn read_line() -> Result<String, Error> {
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Err(Error::Config("onboarding canceled".into()));
    }
    Ok(line.trim().to_string())
}

pub(super) fn validate_environment_name(name: &str) -> Result<(), Error> {
    super::provider::validate_environment_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_environment_variable_names() {
        assert!(validate_environment_name("OMLX_API_KEY").is_ok());
        assert!(validate_environment_name("2BAD").is_err());
        assert!(validate_environment_name("BAD-NAME").is_err());
    }
}
