//! What the tray shows and when quitting needs a confirmation. Pure functions so the rules are
//! testable without a window.

pub const CONFIRM_TITLE: &str = "Stop MochiPrep's backend?";
pub const CONFIRM_TEXT: &str = "Running agents will stop and real accounts will be disarmed. Open positions and their stop orders stay on OKX.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendState {
    Starting,
    Running,
    Stopped,
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendSnapshot {
    pub agents_running: usize,
    pub armed_live_accounts: Vec<String>,
}

/// An unreadable snapshot with a live process counts as "maybe armed": better a needless prompt
/// than silently disarming real accounts. The state is not consulted: a live process is what
/// matters, whatever the launcher believes about it.
pub fn needs_confirmation(
    _state: &BackendState,
    process_alive: bool,
    snapshot: Option<&BackendSnapshot>,
) -> bool {
    if !process_alive {
        return false;
    }
    match snapshot {
        Some(s) => s.agents_running > 0 || !s.armed_live_accounts.is_empty(),
        None => true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuModel {
    pub status_label: String,
    pub armed_label: Option<String>,
    pub can_open_interface: bool,
    pub can_disarm_all: bool,
}

pub fn menu_model(state: &BackendState, snapshot: Option<&BackendSnapshot>) -> MenuModel {
    let status_label = match state {
        BackendState::Starting => "Backend: Starting".to_string(),
        BackendState::Running => "Backend: Running".to_string(),
        BackendState::Stopped => "Backend: Stopped".to_string(),
        BackendState::Error(msg) => format!("Backend: Error: {msg}"),
    };
    let armed = match (state, snapshot) {
        (BackendState::Running, Some(s)) => s.armed_live_accounts.len(),
        _ => 0,
    };
    MenuModel {
        status_label,
        armed_label: (armed > 0).then(|| format!("Real account armed ({armed})")),
        can_open_interface: *state == BackendState::Running,
        can_disarm_all: armed > 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(agents: usize, armed: &[&str]) -> BackendSnapshot {
        BackendSnapshot {
            agents_running: agents,
            armed_live_accounts: armed.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn quit_needs_confirmation_when_an_agent_runs() {
        assert!(needs_confirmation(
            &BackendState::Running,
            true,
            Some(&snap(1, &[]))
        ));
    }

    #[test]
    fn quit_needs_confirmation_when_a_live_account_is_armed() {
        assert!(needs_confirmation(
            &BackendState::Running,
            true,
            Some(&snap(0, &["a"]))
        ));
    }

    #[test]
    fn no_confirmation_when_idle() {
        assert!(!needs_confirmation(
            &BackendState::Running,
            true,
            Some(&snap(0, &[]))
        ));
    }

    #[test]
    fn confirms_when_state_is_unknown_but_process_alive() {
        assert!(needs_confirmation(&BackendState::Running, true, None));
    }

    #[test]
    fn no_confirmation_when_backend_stopped() {
        assert!(!needs_confirmation(&BackendState::Stopped, false, None));
        assert!(!needs_confirmation(
            &BackendState::Error("x".into()),
            false,
            None
        ));
    }

    #[test]
    fn running_menu_with_armed_accounts() {
        let m = menu_model(&BackendState::Running, Some(&snap(0, &["a", "b"])));
        assert_eq!(m.status_label, "Backend: Running");
        assert_eq!(m.armed_label, Some("Real account armed (2)".to_string()));
        assert!(m.can_open_interface);
        assert!(m.can_disarm_all);
    }

    #[test]
    fn running_menu_without_armed_accounts() {
        let m = menu_model(&BackendState::Running, Some(&snap(0, &[])));
        assert_eq!(m.armed_label, None);
        assert!(!m.can_disarm_all);
    }

    #[test]
    fn error_menu_names_the_problem() {
        let m = menu_model(&BackendState::Error("port 8787 is in use".into()), None);
        assert_eq!(m.status_label, "Backend: Error: port 8787 is in use");
        assert!(!m.can_open_interface);
        assert!(!m.can_disarm_all);
    }

    #[test]
    fn starting_and_stopped_labels() {
        assert_eq!(
            menu_model(&BackendState::Starting, None).status_label,
            "Backend: Starting"
        );
        assert_eq!(
            menu_model(&BackendState::Stopped, None).status_label,
            "Backend: Stopped"
        );
    }
}
