use std::net::{IpAddr, SocketAddr};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

pub const DEFAULT_HTTP_HOST: &str = "127.0.0.1";
pub const DEFAULT_HTTP_PORT: u16 = 3032;

#[derive(Debug, Parser)]
#[command(name = "discord_mcp", about = "Local Discord tools for MCP")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open a dedicated browser profile and wait for a normal Discord login.
    Login,
    /// Read and print a small authenticated Discord overview.
    Status,
    /// Print sanitized visible page data while developing the browser adapter.
    Inspect,
    /// Run the MCP server over stdin/stdout.
    Mcp,
    /// Run the MCP server over local Streamable HTTP.
    Http(HttpOptions),
    /// Remove the local Discord browser profile after an explicit confirmation.
    Logout,
}

#[derive(Debug, Clone, Args)]
pub struct HttpOptions {
    /// Interface/IP address to bind. Defaults to loopback.
    #[arg(long, default_value = DEFAULT_HTTP_HOST)]
    pub host: String,
    /// TCP port for the MCP endpoint.
    #[arg(long, default_value_t = DEFAULT_HTTP_PORT)]
    pub port: u16,
}

impl HttpOptions {
    pub fn bind_addr(&self) -> Result<SocketAddr> {
        let host = self.host.parse::<IpAddr>().with_context(|| {
            format!("invalid HTTP bind host {:?}; use an IP address", self.host)
        })?;
        Ok(SocketAddr::new(host, self.port))
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command, DEFAULT_HTTP_HOST, DEFAULT_HTTP_PORT, HttpOptions};
    use clap::Parser;

    #[test]
    fn http_defaults_to_loopback() {
        let cli = Cli::try_parse_from(["discord_mcp", "http"]).expect("valid CLI");
        let Command::Http(options) = cli.command else {
            panic!("expected HTTP command");
        };
        assert_eq!(options.host, DEFAULT_HTTP_HOST);
        assert_eq!(options.port, DEFAULT_HTTP_PORT);
        assert_eq!(options.bind_addr().unwrap().to_string(), "127.0.0.1:3032");
    }

    #[test]
    fn rejects_non_ip_host() {
        let options = HttpOptions {
            host: "localhost".to_string(),
            port: DEFAULT_HTTP_PORT,
        };
        assert!(options.bind_addr().is_err());
    }
}
