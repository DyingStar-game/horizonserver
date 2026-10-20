use tracing_subscriber::EnvFilter;

/// Horizon modules whose INFO lines are per player × per object GORC chatter (zone
/// entered/exited, subscribed, object registered): thousands of lines/s under load,
/// enough to rotate the horizon pod log within seconds. Their warnings still show.
const QUIET_TARGETS: &[&str] = &[
    "horizon_event_system::gorc::instance=warn",
    "horizon_event_system::system::emitters=warn",
];

/// Installs the tracing subscriber of the calling plugin. Every plugin .so carries its
/// own copy of the tracing globals, so each one must call this for its logs to show.
///
/// `log_level` (plugins.toml) is a level ("info") or an `EnvFilter` directive list
/// ("info,dyingstar_generic_props::lod=debug"). The GORC chatter of [`QUIET_TARGETS`]
/// is silenced unless the directives mention `horizon_event_system` themselves, e.g.
/// `log_level = "info,horizon_event_system=info"` brings it back.
pub fn init(log_level: &str) {
    let filter = EnvFilter::try_new(directives(log_level)).unwrap_or_else(|e| {
        eprintln!("invalid log_level {log_level:?} ({e}), falling back to info");
        EnvFilter::new("info")
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init()
        .ok(); // Ignore errors if already initialized
}

fn directives(log_level: &str) -> String {
    let mut directives = log_level.trim().to_string();
    if directives.is_empty() {
        directives.push_str("info");
    }
    if !directives.contains("horizon_event_system") {
        for target in QUIET_TARGETS {
            directives.push(',');
            directives.push_str(target);
        }
    }
    directives
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiets_gorc_by_default() {
        let d = directives("info");
        assert_eq!(d, "info,horizon_event_system::gorc::instance=warn,horizon_event_system::system::emitters=warn");
        assert!(EnvFilter::try_new(&d).is_ok());
        assert!(directives(" ").starts_with("info,"));
    }

    #[test]
    fn explicit_horizon_directive_wins() {
        assert_eq!(directives("info,horizon_event_system=debug"), "info,horizon_event_system=debug");
    }
}
