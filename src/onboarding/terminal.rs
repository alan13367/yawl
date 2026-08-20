use std::io::{self, Write};

use crate::error::Error;

pub(super) enum Authentication {
    None,
    Environment { reference: String, value: String },
    Literal(String),
}

impl Authentication {
    pub(super) fn config_value(&self) -> &str {
        match self {
            Authentication::None => "-",
            Authentication::Environment { reference, .. } => reference,
            Authentication::Literal(value) => value,
        }
    }

    pub(super) fn request_key(&self) -> &str {
        match self {
            Authentication::None => "",
            Authentication::Environment { value, .. } | Authentication::Literal(value) => value,
        }
    }
}

pub(super) fn choose_authentication() -> Result<Authentication, Error> {
    println!(
        "\nAuthentication:\n  1. No API key\n  2. Read the key from an environment variable\n  3. Enter an API key now"
    );
    match prompt("Authentication number")?.as_str() {
        "1" => Ok(Authentication::None),
        "2" => {
            let name = prompt("Environment variable name, without '$'")?;
            validate_environment_name(&name)?;
            let value = std::env::var(&name).unwrap_or_default();
            Ok(Authentication::Environment {
                reference: format!("${name}"),
                value,
            })
        }
        "3" => {
            let key = prompt_secret("API key")?;
            if key.is_empty() {
                Err(Error::Config("API key must not be empty".into()))
            } else {
                Ok(Authentication::Literal(key))
            }
        }
        _ => Err(Error::Config(
            "authentication must be a number from 1 through 3".into(),
        )),
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

fn prompt_secret(label: &str) -> Result<String, Error> {
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

pub(super) fn nonempty(value: String, name: &str) -> Result<String, Error> {
    if value.is_empty() {
        Err(Error::Config(format!("{name} must not be empty")))
    } else {
        Ok(value)
    }
}

pub(super) fn validate_environment_name(name: &str) -> Result<(), Error> {
    let mut bytes = name.bytes();
    let valid_start = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    if valid_start && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
        Ok(())
    } else {
        Err(Error::Config("invalid environment variable name".into()))
    }
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
