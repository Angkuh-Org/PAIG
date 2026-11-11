mod backend;
mod config;
mod runtime;
mod support;
mod transport;

pub use backend::{bob, cc, kilo, mistral, opencode};
pub use runtime::{daemon, router};
pub use support::{crypto, json, rules, util};
pub use transport::{client, http, proxy, sse, tls};

use std::net::TcpListener;
use std::sync::Arc;

use util::{loge, logi, logw};

unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

const SIGPIPE: i32 = 13;
const SIG_DFL: usize = 0;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        run_server();
        return;
    }
    if args[0].starts_with("--") {
        // CLI output is commonly piped to head/less; terminate on EPIPE like a
        // normal Unix tool instead of panicking inside println!.
        unsafe {
            signal(SIGPIPE, SIG_DFL);
        }
        if let Some(code) = daemon::dispatch(&args) {
            std::process::exit(code);
        }
        std::process::exit(config::run_cli(&args));
    }
    eprintln!("unknown argument: {}", args[0]);
    eprintln!("try: paig --help");
    std::process::exit(2);
}

fn run_server() {
    let loaded = match config::load_config() {
        Ok(loaded) => loaded,
        Err(error) => {
            loge!("{error}");
            std::process::exit(1);
        }
    };
    let config = loaded.config.clone();
    let (errors, warnings) = config::validate_config(&config);
    if !errors.is_empty() {
        for error in &errors {
            loge!("config error: {error}");
        }
        std::process::exit(1);
    }
    for warning in loaded.warnings.iter().chain(warnings.iter()) {
        logw!("warning: {warning}");
    }

    let sources = if loaded.sources.is_empty() {
        "defaults".to_string()
    } else {
        loaded
            .sources
            .iter()
            .map(|source| source.path.clone())
            .collect::<Vec<_>>()
            .join(" + ")
    };
    let project = if config.command_code.project_slug.is_empty() {
        "paig"
    } else {
        &config.command_code.project_slug
    };
    logi!(
        "listening on http://{}:{}/v1 (project: {})",
        config.server.host,
        config.server.port,
        project
    );
    logi!("config: {sources}");
    logi!(
        "command code: {} (key: {})",
        config.command_code.base_url,
        if config.command_code.api_key.is_empty() {
            "MISSING".to_string()
        } else {
            format!("configured via {}", loaded.key_source)
        }
    );
    logi!(
        "bob: {} (key: {})",
        config.bob.base_url,
        if config.bob.api_key.is_empty() && config.bob.api_key_file.is_empty() {
            "MISSING"
        } else {
            "configured"
        }
    );
    logi!(
        "mistral: {} (key: {})",
        config.mistral.base_url,
        if config.mistral.api_key.is_empty() && config.mistral.api_key_file.is_empty() {
            "MISSING"
        } else {
            "configured"
        }
    );

    let listener = match TcpListener::bind((config.server.host.as_str(), config.server.port)) {
        Ok(listener) => listener,
        Err(error) => {
            loge!(
                "cannot bind {}:{}: {error}",
                config.server.host,
                config.server.port
            );
            std::process::exit(1);
        }
    };
    let app = Arc::new(router::App::new(config));
    let handler = Arc::new(router::handler(app));
    if let Err(error) = http::serve(listener, handler) {
        loge!("server failed: {error}");
        std::process::exit(1);
    }
}
