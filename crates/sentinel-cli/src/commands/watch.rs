use std::time::Duration;

use sentinel_core::AlertMinLevel;

use crate::{
    adapters::{alert_mattermost::MattermostAlertDispatcher, alert_teams::TeamsAlertDispatcher},
    alerts::{
        datadog::{DatadogAdapter, DatadogSite},
        incident::{IncidentDispatcher, Provider},
    },
    args::WatchArgs,
    commands::{scan::prepare_scan, EXIT_INVALID_ARGS},
    formatters::{render_json, render_text_with_options},
};

/// Builds a configured [`MattermostAlertDispatcher`] when `--alert-mattermost` is supplied.
pub fn build_mattermost_dispatcher(
    args: &WatchArgs,
) -> Result<Option<MattermostAlertDispatcher>, String> {
    let Some(webhook_url) = args.alert_mattermost.as_deref() else {
        return Ok(None);
    };

    let min_level = match args.scan.alert_min_level.as_str() {
        "warn" => AlertMinLevel::Warn,
        "critical" => AlertMinLevel::Critical,
        other => return Err(format!("invalid --alert-min-level value: {other}")),
    };

    let mut dispatcher = MattermostAlertDispatcher::new(webhook_url)
        .map_err(|error| error.to_string())?
        .with_min_level(min_level);

    if let Some(channel) = args.alert_mattermost_channel.as_deref() {
        dispatcher = dispatcher.with_channel(channel);
    }
    if let Some(username) = args.alert_mattermost_username.as_deref() {
        dispatcher = dispatcher.with_username(username);
    }

    Ok(Some(dispatcher))
}

pub async fn run_watch(args: WatchArgs) -> i32 {
    if args.scan.baseline.is_some() || args.scan.fail_on_regression {
        eprintln!("Error: baseline comparison is available with sentinel scan");
        return EXIT_INVALID_ARGS;
    }
    let format = args.scan.format.to_ascii_lowercase();
    if !matches!(format.as_str(), "json" | "text") {
        eprintln!("Error: --format must be text or json");
        return EXIT_INVALID_ARGS;
    }
    if let Err(error) = sentinel_core::parse_contract_id(&args.scan.contract_id) {
        eprintln!("Error: {error}");
        return EXIT_INVALID_ARGS;
    }
    let (usecase, request) = match prepare_scan(&args.scan) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("Error: {error}");
            return EXIT_INVALID_ARGS;
        }
    };
    let mattermost_dispatcher = match build_mattermost_dispatcher(&args) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("Error: {error}");
            return EXIT_INVALID_ARGS;
        }
    };

    let mut teams = if let Some(webhook_url) = args.alert_teams.as_deref() {
        match TeamsAlertDispatcher::new(webhook_url) {
            Ok(dispatcher) => Some(dispatcher),
            Err(error) => {
                eprintln!("Error: {error}");
                return EXIT_INVALID_ARGS;
            }
        }
    } else {
        None
    };

    let providers = [
        args.pagerduty_routing_key
            .map(|routing_key| Provider::PagerDuty {
                endpoint: "https://events.pagerduty.com/v2/enqueue"
                    .parse()
                    .expect("valid PagerDuty URL"),
                routing_key,
            }),
        args.opsgenie_api_key.map(|api_key| Provider::Opsgenie {
            endpoint: "https://api.opsgenie.com/v2/alerts"
                .parse()
                .expect("valid Opsgenie URL"),
            api_key,
        }),
    ];
    let mut dispatchers: Vec<_> = match providers
        .into_iter()
        .flatten()
        .map(IncidentDispatcher::new)
        .collect()
    {
        Ok(value) => value,
        Err(_) => {
            eprintln!("Error: could not initialize incident HTTP client");
            return EXIT_INVALID_ARGS;
        }
    };

    // Build the optional Datadog adapter when credentials are provided.
    let mut datadog: Option<DatadogAdapter> = if let Some(api_key) = args.datadog_api_key {
        let site = match DatadogSite::parse(&args.datadog_site) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error: {e}");
                return EXIT_INVALID_ARGS;
            }
        };
        match DatadogAdapter::new(api_key, &site) {
            Ok(adapter) => Some(adapter),
            Err(_) => {
                eprintln!("Error: could not initialize Datadog HTTP client");
                return EXIT_INVALID_ARGS;
            }
        }
    } else {
        None
    };

    loop {
        match usecase.execute_with_keys(request.clone()).await {
            Ok((report, keys)) => {
                let output = if format == "json" {
                    render_json(&report)
                } else {
                    Ok(render_text_with_options(&report, args.scan.no_color))
                };
                match output {
                    Ok(output) => println!("{output}"),
                    Err(error) => eprintln!("Watch report failed: {error}"),
                }
                for dispatcher in &mut dispatchers {
                    if let Err(error) = dispatcher.observe(&report, &keys).await {
                        eprintln!("Watch notification failed: {error}");
                    }
                }
                if let Some(dd) = datadog.as_mut() {
                    if let Err(error) = dd.observe(&report).await {
                        eprintln!("Datadog notification failed: {error}");
                    }
                }
                if let Some(dispatcher) = mattermost_dispatcher.as_ref() {
                    if let Err(error) = dispatcher.dispatch(&report).await {
                        eprintln!("Watch Mattermost notification failed: {error}");
                    }
                }
                if let Some(teams) = teams.as_mut() {
                    if let Err(error) = teams.observe(&report).await {
                        eprintln!("Teams notification failed: {error}");
                    }
                }
            }
            Err(
                error @ (sentinel_core::SentinelError::InvalidXdrKey(_)
                | sentinel_core::SentinelError::InvalidContractAddress(_)),
            ) => {
                eprintln!("Error: {error}");
                return EXIT_INVALID_ARGS;
            }
            Err(error) => eprintln!("Watch scan failed: {error}"),
        }
        tokio::time::sleep(Duration::from_secs(args.interval)).await;
    }
}
