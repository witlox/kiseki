//! Admin CLI command parsing.
//!
//! Provides lightweight argument parsing for admin subcommands.
//! No external CLI framework dependency — the server binary is small
//! enough that manual parsing suffices.

/// Admin CLI subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminCommand {
    /// Show cluster status.
    Status,
    /// List storage pools.
    PoolList,
    /// List devices.
    DeviceList,
    /// List shards.
    ShardList,
    /// Enable maintenance mode.
    MaintenanceOn,
    /// Disable maintenance mode.
    MaintenanceOff,
}

/// Parse an admin command from CLI arguments.
///
/// Expects `args[1]` to be the primary verb and `args[2]` to be the
/// sub-verb where applicable (e.g., `pool list`).
///
/// Returns `None` if the arguments do not match any known command.
#[must_use]
pub fn parse_admin_args(args: &[String]) -> Option<AdminCommand> {
    match args.get(1)?.as_str() {
        "status" => Some(AdminCommand::Status),
        "pool" if args.get(2).map(String::as_str) == Some("list") => Some(AdminCommand::PoolList),
        "device" if args.get(2).map(String::as_str) == Some("list") => {
            Some(AdminCommand::DeviceList)
        }
        "shard" if args.get(2).map(String::as_str) == Some("list") => Some(AdminCommand::ShardList),
        "maintenance" if args.get(2).map(String::as_str) == Some("on") => {
            Some(AdminCommand::MaintenanceOn)
        }
        "maintenance" if args.get(2).map(String::as_str) == Some("off") => {
            Some(AdminCommand::MaintenanceOff)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parse_status() {
        let result = parse_admin_args(&args(&["kiseki-admin", "status"]));
        assert_eq!(result, Some(AdminCommand::Status));
    }

    #[test]
    fn parse_maintenance_on() {
        let result = parse_admin_args(&args(&["kiseki-admin", "maintenance", "on"]));
        assert_eq!(result, Some(AdminCommand::MaintenanceOn));
    }

    #[test]
    fn unknown_returns_none() {
        let result = parse_admin_args(&args(&["kiseki-admin", "frobnicate"]));
        assert_eq!(result, None);
    }
}
