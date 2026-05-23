use std::net::IpAddr;

use anyhow::{Context, Result, bail};
use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(
    name = "tmonks",
    version,
    about = "Web UI for tmux sessions",
    long_about = "Launches a local web server exposing a browser-based UI for your tmux sessions. \
                  Binds to 127.0.0.1 by default; print a single URL containing a one-time token \
                  on stdout. Open the URL in any browser."
)]
pub struct Cli {
    /// IP address to bind. Must be a loopback address. Use SSH tunneling for remote access.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: IpAddr,

    /// TCP port. 0 picks an ephemeral port.
    #[arg(long, default_value_t = 0)]
    pub port: u16,

    /// tmux socket name (`-L <socket>`). When omitted, the default socket is used.
    #[arg(long)]
    pub socket: Option<String>,

    /// Disable authentication. DANGEROUS — requires --i-understand-no-auth.
    #[arg(long)]
    pub no_auth: bool,

    /// Required confirmation when passing --no-auth.
    #[arg(long)]
    pub i_understand_no_auth: bool,

    /// Verbose logging (debug level).
    #[arg(long, short = 'v')]
    pub verbose: bool,
}

impl Cli {
    /// Validate inputs that aren't expressible in clap's type system.
    pub fn validate(&self) -> Result<()> {
        if !self.bind.is_loopback() {
            bail!(
                "--bind {} is not a loopback address. tmonks MVP does not support non-loopback \
                 binds (no TLS, no multi-user auth). For remote access use SSH tunneling:\n\
                 \n    ssh -L 8080:127.0.0.1:<port> remote\n\n\
                 Then open http://127.0.0.1:8080/?t=<token> in your local browser.",
                self.bind
            );
        }

        if self.no_auth && !self.i_understand_no_auth {
            bail!(
                "--no-auth is dangerous: anyone on this host (any user, any process able to \
                 connect to {}:{}) can drive your tmux sessions. \n\
                 Re-run with --no-auth --i-understand-no-auth if you accept the risk.",
                self.bind, self.port
            );
        }

        if let Some(sock) = &self.socket {
            validate_socket_name(sock).context("--socket value is invalid")?;
        }

        Ok(())
    }
}

/// tmux socket names are passed as `-L <name>`. We restrict to a safe character
/// class so an attacker-controlled value cannot start with `-` (turning into a
/// flag) or contain path separators.
pub fn validate_socket_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("must not be empty");
    }
    if name.len() > 32 {
        bail!("must be 32 chars or fewer (got {})", name.len());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        bail!("must match [A-Za-z0-9_-]+ (got {name:?})");
    }
    if name.starts_with('-') {
        bail!("must not start with '-' (got {name:?})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_loopback_bind() {
        let cli = Cli::parse_from(["tmonks", "--bind", "0.0.0.0"]);
        let err = cli.validate().unwrap_err();
        assert!(err.to_string().contains("loopback"));
        assert!(err.to_string().contains("SSH tunneling"));
    }

    #[test]
    fn rejects_no_auth_without_confirm() {
        let cli = Cli::parse_from(["tmonks", "--no-auth"]);
        let err = cli.validate().unwrap_err();
        assert!(err.to_string().contains("--no-auth is dangerous"));
        assert!(err.to_string().contains("--i-understand-no-auth"));
    }

    #[test]
    fn accepts_no_auth_with_confirm() {
        let cli = Cli::parse_from(["tmonks", "--no-auth", "--i-understand-no-auth"]);
        cli.validate().unwrap();
    }

    #[test]
    fn accepts_loopback_bind() {
        let cli = Cli::parse_from(["tmonks", "--bind", "127.0.0.1"]);
        cli.validate().unwrap();
    }

    #[test]
    fn accepts_ipv6_loopback() {
        let cli = Cli::parse_from(["tmonks", "--bind", "::1"]);
        cli.validate().unwrap();
    }

    #[test]
    fn socket_name_accepts_safe_chars() {
        validate_socket_name("dev").unwrap();
        validate_socket_name("dev-1").unwrap();
        validate_socket_name("a_b").unwrap();
    }

    #[test]
    fn socket_name_rejects_leading_dash() {
        validate_socket_name("-hack").unwrap_err();
    }

    #[test]
    fn socket_name_rejects_special_chars() {
        validate_socket_name("a/b").unwrap_err();
        validate_socket_name("a b").unwrap_err();
        validate_socket_name("a.b").unwrap_err();
    }

    #[test]
    fn socket_name_rejects_empty_or_too_long() {
        validate_socket_name("").unwrap_err();
        validate_socket_name(&"a".repeat(33)).unwrap_err();
    }
}
