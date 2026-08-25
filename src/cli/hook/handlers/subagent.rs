use crate::cli::{set_attention, set_status};
use crate::desktop_notification;
use crate::tmux;

use super::super::context::{
    append_subagent, clear_run_state, drain_pending_teardowns, remove_subagent,
};
use super::super::notifications::{NotifyLabels, notify_stop};

pub(in crate::cli::hook) fn on_subagent_start(
    pane: &str,
    agent_type: &str,
    agent_id: Option<&str>,
) -> i32 {
    // Claude Code always sends agent_id per the hooks spec; drop the
    // event silently if it's missing so the tree never gains an
    // untrackable entry.
    let Some(id) = agent_id.filter(|s| !s.is_empty()) else {
        return 0;
    };
    let current = tmux::get_pane_option_value(pane, tmux::PANE_SUBAGENTS);
    let new_val = append_subagent(&current, agent_type, id);
    tmux::set_pane_option(pane, tmux::PANE_SUBAGENTS, &new_val);
    0
}

pub(in crate::cli::hook) fn on_subagent_stop(
    pane: &str,
    agent_id: Option<&str>,
    children_may_outlive_turn: bool,
    notifications: &desktop_notification::DesktopNotificationSettings,
) -> i32 {
    let Some(id) = agent_id.filter(|s| !s.is_empty()) else {
        return 0;
    };
    let current = tmux::get_pane_option_value(pane, tmux::PANE_SUBAGENTS);
    let drained_to_empty = match remove_subagent(&current, id) {
        None => false,
        Some(new_val) if new_val.is_empty() => {
            tmux::unset_pane_option(pane, tmux::PANE_SUBAGENTS);
            true
        }
        Some(new_val) => {
            tmux::set_pane_option(pane, tmux::PANE_SUBAGENTS, &new_val);
            false
        }
    };
    // Once the last subagent stops, replay any teardown that was deferred
    // because subagents were active when SessionEnd / WorktreeRemove fired.
    if drained_to_empty {
        // Child hooks can temporarily replace `background` with `waiting` or
        // `running`; adapters declare whether the turn marker is a durable
        // parent-settlement signal for this child lifecycle.
        let agent = tmux::get_pane_option_value(pane, tmux::PANE_AGENT);
        let parent_turn_settled = children_may_outlive_turn
            && tmux::get_pane_option_value(pane, tmux::PANE_TURN_ACTIVE).is_empty();
        let deferred_body =
            tmux::get_pane_option_value(pane, tmux::PANE_PENDING_STOP_NOTIFICATION_BODY);
        tmux::unset_pane_option(pane, tmux::PANE_PENDING_STOP_NOTIFICATION_BODY);
        drain_pending_teardowns(pane);

        let status = tmux::get_pane_option_value(pane, tmux::PANE_STATUS);
        let bg_shell_live = !tmux::get_pane_option_value(pane, tmux::PANE_BG_CMD).is_empty();
        if parent_turn_settled && !status.is_empty() && status != "error" {
            set_attention(pane, "clear");
            if bg_shell_live {
                tmux::unset_pane_option(pane, tmux::PANE_WAIT_REASON);
                set_status(pane, "background");
            } else {
                clear_run_state(pane);
                set_status(pane, "idle");
                if !deferred_body.is_empty() {
                    let _ = notify_stop(
                        pane,
                        NotifyLabels::FromPane { agent: &agent },
                        notifications,
                        &deferred_body,
                    );
                }
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::super::session::on_session_end;
    use super::super::worktree::on_worktree_remove;
    use super::*;
    use crate::cli::hook::context::{PENDING_SESSION_END, PENDING_WORKTREE_REMOVE};
    use crate::desktop_notification;
    use std::fs;

    fn default_notifications() -> desktop_notification::DesktopNotificationSettings {
        desktop_notification::DesktopNotificationSettings {
            enabled: false,
            events: Default::default(),
        }
    }

    #[test]
    fn on_subagent_start_appends_to_list() {
        let _guard = tmux::test_mock::install();
        let pane = "%SUB_START";
        on_subagent_start(pane, "Explore", Some("sub-1"));
        assert_eq!(
            tmux::test_mock::get(pane, tmux::PANE_SUBAGENTS).as_deref(),
            Some("Explore:sub-1")
        );
        on_subagent_start(pane, "Plan", Some("sub-2"));
        assert_eq!(
            tmux::test_mock::get(pane, tmux::PANE_SUBAGENTS).as_deref(),
            Some("Explore:sub-1,Plan:sub-2")
        );
    }

    #[test]
    fn on_subagent_start_drops_event_without_id() {
        let _guard = tmux::test_mock::install();
        let pane = "%SUB_NO_ID";
        on_subagent_start(pane, "Explore", None);
        assert!(!tmux::test_mock::contains(pane, tmux::PANE_SUBAGENTS));
        on_subagent_start(pane, "Explore", Some(""));
        assert!(!tmux::test_mock::contains(pane, tmux::PANE_SUBAGENTS));
    }

    #[test]
    fn last_grok_subagent_stop_settles_background_without_shell() {
        let _guard = tmux::test_mock::install();
        let pane = "%SUB_LAST_BACKGROUND";
        tmux::test_mock::set(pane, tmux::PANE_AGENT, tmux::GROK_AGENT);
        tmux::test_mock::set(pane, tmux::PANE_SUBAGENTS, "Explore:sub-1");
        tmux::test_mock::set(pane, tmux::PANE_STATUS, "background");
        tmux::test_mock::set(pane, tmux::PANE_STARTED_AT, "1700");

        on_subagent_stop(pane, Some("sub-1"), true, &default_notifications());

        assert_eq!(
            tmux::test_mock::get(pane, tmux::PANE_STATUS).as_deref(),
            Some("idle")
        );
        assert!(!tmux::test_mock::contains(pane, tmux::PANE_STARTED_AT));
        assert!(!tmux::test_mock::contains(pane, tmux::PANE_SUBAGENTS));
    }

    #[test]
    fn last_grok_subagent_stop_consumes_deferred_completion_notification() {
        let _guard = tmux::test_mock::install();
        let pane = "%SUB_LAST_DEFERRED_NOTIFICATION";
        tmux::test_mock::set(pane, tmux::PANE_AGENT, tmux::GROK_AGENT);
        tmux::test_mock::set(pane, tmux::PANE_SUBAGENTS, "Explore:sub-1");
        tmux::test_mock::set(pane, tmux::PANE_STATUS, "background");
        tmux::test_mock::set(
            pane,
            tmux::PANE_PENDING_STOP_NOTIFICATION_BODY,
            "parent response",
        );

        on_subagent_stop(pane, Some("sub-1"), true, &default_notifications());

        assert!(
            !tmux::test_mock::contains(pane, tmux::PANE_PENDING_STOP_NOTIFICATION_BODY),
            "the final child must consume the pending Stop notification"
        );
    }

    #[test]
    fn last_grok_subagent_stop_settles_inactive_parent_after_child_state_transitions() {
        let _guard = tmux::test_mock::install();

        for status in ["waiting", "running"] {
            let pane = format!("%SUB_LAST_{}", status.to_uppercase());
            tmux::test_mock::set(&pane, tmux::PANE_AGENT, tmux::GROK_AGENT);
            tmux::test_mock::set(&pane, tmux::PANE_SUBAGENTS, "Explore:sub-1");
            tmux::test_mock::set(&pane, tmux::PANE_STATUS, status);
            tmux::test_mock::set(&pane, tmux::PANE_STARTED_AT, "1700");
            tmux::test_mock::set(&pane, tmux::PANE_ATTENTION, "notification");
            tmux::test_mock::set(
                &pane,
                tmux::PANE_PENDING_STOP_NOTIFICATION_BODY,
                "parent response",
            );

            on_subagent_stop(&pane, Some("sub-1"), true, &default_notifications());

            assert_eq!(
                tmux::test_mock::get(&pane, tmux::PANE_STATUS).as_deref(),
                Some("idle"),
                "an inactive parent must settle after its final child stops from {status}"
            );
            assert!(!tmux::test_mock::contains(&pane, tmux::PANE_STARTED_AT));
            assert!(!tmux::test_mock::contains(
                &pane,
                tmux::PANE_PENDING_STOP_NOTIFICATION_BODY
            ));
            assert!(!tmux::test_mock::contains(&pane, tmux::PANE_ATTENTION));
        }
    }

    #[test]
    fn last_grok_subagent_stop_keeps_background_shell_running() {
        let _guard = tmux::test_mock::install();
        let pane = "%SUB_LAST_WITH_SHELL";
        tmux::test_mock::set(pane, tmux::PANE_AGENT, tmux::GROK_AGENT);
        tmux::test_mock::set(pane, tmux::PANE_SUBAGENTS, "Explore:sub-1");
        tmux::test_mock::set(pane, tmux::PANE_BG_CMD, "sleep 300");
        tmux::test_mock::set(pane, tmux::PANE_STATUS, "background");
        tmux::test_mock::set(pane, tmux::PANE_STARTED_AT, "1700");
        tmux::test_mock::set(
            pane,
            tmux::PANE_PENDING_STOP_NOTIFICATION_BODY,
            "parent response",
        );

        on_subagent_stop(pane, Some("sub-1"), true, &default_notifications());

        assert_eq!(
            tmux::test_mock::get(pane, tmux::PANE_STATUS).as_deref(),
            Some("background")
        );
        assert_eq!(
            tmux::test_mock::get(pane, tmux::PANE_STARTED_AT).as_deref(),
            Some("1700")
        );
        assert!(
            !tmux::test_mock::contains(pane, tmux::PANE_PENDING_STOP_NOTIFICATION_BODY),
            "shell-backed background work keeps the existing no-notification behavior"
        );
    }

    #[test]
    fn last_grok_subagent_stop_restores_background_after_child_transition_with_live_shell() {
        let _guard = tmux::test_mock::install();

        for status in ["waiting", "running"] {
            let pane = format!("%SUB_LAST_SHELL_{}", status.to_uppercase());
            tmux::test_mock::set(&pane, tmux::PANE_AGENT, tmux::GROK_AGENT);
            tmux::test_mock::set(&pane, tmux::PANE_SUBAGENTS, "Explore:sub-1");
            tmux::test_mock::set(&pane, tmux::PANE_BG_CMD, "sleep 300");
            tmux::test_mock::set(&pane, tmux::PANE_STATUS, status);
            tmux::test_mock::set(&pane, tmux::PANE_STARTED_AT, "1700");
            tmux::test_mock::set(&pane, tmux::PANE_WAIT_REASON, "permission");
            tmux::test_mock::set(&pane, tmux::PANE_ATTENTION, "notification");

            on_subagent_stop(&pane, Some("sub-1"), true, &default_notifications());

            assert_eq!(
                tmux::test_mock::get(&pane, tmux::PANE_STATUS).as_deref(),
                Some("background"),
                "a settled parent with a live shell must return to background from {status}"
            );
            assert_eq!(
                tmux::test_mock::get(&pane, tmux::PANE_STARTED_AT).as_deref(),
                Some("1700")
            );
            assert!(!tmux::test_mock::contains(&pane, tmux::PANE_WAIT_REASON));
            assert!(!tmux::test_mock::contains(&pane, tmux::PANE_ATTENTION));
        }
    }

    #[test]
    fn last_grok_subagent_stop_does_not_settle_running_parent() {
        let _guard = tmux::test_mock::install();
        let pane = "%SUB_LAST_RUNNING";
        tmux::test_mock::set(pane, tmux::PANE_AGENT, tmux::GROK_AGENT);
        tmux::test_mock::set(pane, tmux::PANE_SUBAGENTS, "Explore:sub-1");
        tmux::test_mock::set(pane, tmux::PANE_STATUS, "running");
        tmux::test_mock::set(pane, tmux::PANE_STARTED_AT, "1700");
        tmux::test_mock::set(pane, tmux::PANE_TURN_ACTIVE, "1");

        on_subagent_stop(pane, Some("sub-1"), true, &default_notifications());

        assert_eq!(
            tmux::test_mock::get(pane, tmux::PANE_STATUS).as_deref(),
            Some("running")
        );
        assert_eq!(
            tmux::test_mock::get(pane, tmux::PANE_STARTED_AT).as_deref(),
            Some("1700")
        );
    }

    #[test]
    fn last_subagent_stop_does_not_infer_settlement_for_claude_without_turn_marker() {
        let _guard = tmux::test_mock::install();
        let pane = "%SUB_LAST_CLAUDE_UNINITIALIZED_TURN_MARKER";
        tmux::test_mock::set(pane, tmux::PANE_AGENT, "claude");
        tmux::test_mock::set(pane, tmux::PANE_SUBAGENTS, "Explore:sub-1");
        tmux::test_mock::set(pane, tmux::PANE_STATUS, "running");
        tmux::test_mock::set(pane, tmux::PANE_STARTED_AT, "1700");

        on_subagent_stop(pane, Some("sub-1"), false, &default_notifications());

        assert_eq!(
            tmux::test_mock::get(pane, tmux::PANE_STATUS).as_deref(),
            Some("running")
        );
        assert_eq!(
            tmux::test_mock::get(pane, tmux::PANE_STARTED_AT).as_deref(),
            Some("1700")
        );
        assert!(!tmux::test_mock::contains(pane, tmux::PANE_SUBAGENTS));
    }

    // ─── deferred teardown regression tests ─────────────────────────
    //
    // These pin the invariant that WorktreeRemove fired while subagents
    // are active must not be lost forever — it is recorded as a pending
    // marker and replayed by `on_subagent_stop` once the subagent list
    // drains to empty.
    //
    // SessionEnd does NOT participate in the deferred-drain dance: we
    // can't tell a parent SessionEnd from a child's, and letting the
    // drain replay one on the wrong side risks wiping a live parent.

    #[test]
    fn session_end_while_subagents_active_is_a_no_op() {
        // Regression: previously `on_session_end` set PENDING_SESSION_END
        // whenever `@pane_subagents` was non-empty, and the next
        // `on_subagent_stop` would turn that marker into
        // `run_session_end_teardown`. Because subagents share the
        // parent's `$TMUX_PANE`, there is no way to guarantee the
        // SessionEnd came from the parent — so the safer default is to
        // skip the event entirely and leave the parent's state alone.
        let _guard = tmux::test_mock::install();
        let pane = "%CHILD_SESSIONEND";
        tmux::test_mock::set(pane, tmux::PANE_SUBAGENTS, "Explore:sub-1");
        tmux::test_mock::set(pane, tmux::PANE_AGENT, "claude");
        tmux::test_mock::set(pane, tmux::PANE_CWD, "/repo/parent");
        tmux::test_mock::set(pane, tmux::PANE_STATUS, "running");
        let log_path = crate::activity::log_file_path(pane);
        let _ = fs::create_dir_all(log_path.parent().unwrap());
        fs::write(&log_path, "1234567890|Read|main.rs\n").unwrap();

        on_session_end(pane, "claude", "", false, &default_notifications());
        assert!(
            !tmux::test_mock::contains(pane, PENDING_SESSION_END),
            "child SessionEnd must not record a pending teardown"
        );
        // Every parent field must survive.
        assert!(tmux::test_mock::contains(pane, tmux::PANE_AGENT));
        assert!(tmux::test_mock::contains(pane, tmux::PANE_CWD));
        assert!(tmux::test_mock::contains(pane, tmux::PANE_STATUS));
        assert!(log_path.exists());

        // Subsequent subagent stop must not trigger a teardown either.
        on_subagent_stop(pane, Some("sub-1"), false, &default_notifications());
        assert!(
            tmux::test_mock::contains(pane, tmux::PANE_AGENT),
            "SubagentStop draining an empty list must not tear down a live parent"
        );
        assert!(log_path.exists());

        fs::remove_file(&log_path).ok();
    }

    #[test]
    fn pending_worktree_remove_drains_when_last_subagent_stops() {
        let _guard = tmux::test_mock::install();
        let pane = "%PARENT_WT_DEFER";
        tmux::test_mock::set(pane, tmux::PANE_SUBAGENTS, "Explore:sub-1");
        tmux::test_mock::set(pane, tmux::PANE_WORKTREE_NAME, "feat");
        tmux::test_mock::set(pane, tmux::PANE_WORKTREE_BRANCH, "feat");
        tmux::test_mock::set(pane, tmux::PANE_CWD, "/wt/feat");

        on_worktree_remove(pane);
        assert!(
            tmux::test_mock::contains(pane, PENDING_WORKTREE_REMOVE),
            "WorktreeRemove must be deferred via the pending marker"
        );
        assert!(tmux::test_mock::contains(pane, tmux::PANE_WORKTREE_NAME));

        on_subagent_stop(pane, Some("sub-1"), false, &default_notifications());

        assert!(!tmux::test_mock::contains(pane, tmux::PANE_WORKTREE_NAME));
        assert!(!tmux::test_mock::contains(pane, tmux::PANE_WORKTREE_BRANCH));
        assert!(!tmux::test_mock::contains(pane, tmux::PANE_CWD));
        assert!(
            !tmux::test_mock::contains(pane, PENDING_WORKTREE_REMOVE),
            "pending marker must be cleared once teardown runs"
        );
    }

    #[test]
    fn pending_worktree_remove_waits_for_last_subagent() {
        // Equivalent of the old `pending_teardown_does_not_fire_until_subagents_empty`
        // but anchored on WorktreeRemove, which still uses the deferred
        // drain (SessionEnd dropped it intentionally — see the comment
        // above `session_end_while_subagents_active_is_a_no_op`).
        let _guard = tmux::test_mock::install();
        let pane = "%PARENT_WT_PARTIAL";
        tmux::test_mock::set(pane, tmux::PANE_SUBAGENTS, "Explore:sub-1,Plan:sub-2");
        tmux::test_mock::set(pane, tmux::PANE_WORKTREE_NAME, "feat");
        tmux::test_mock::set(pane, tmux::PANE_WORKTREE_BRANCH, "feat");
        tmux::test_mock::set(pane, tmux::PANE_CWD, "/wt/feat");

        on_worktree_remove(pane);
        assert!(tmux::test_mock::contains(pane, PENDING_WORKTREE_REMOVE));

        // First child stops — list still has sub-2, teardown must NOT fire.
        on_subagent_stop(pane, Some("sub-1"), false, &default_notifications());
        assert!(
            tmux::test_mock::contains(pane, tmux::PANE_WORKTREE_NAME),
            "teardown must wait for the LAST subagent"
        );
        assert!(tmux::test_mock::contains(pane, PENDING_WORKTREE_REMOVE));

        // Last child stops — now teardown fires.
        on_subagent_stop(pane, Some("sub-2"), false, &default_notifications());
        assert!(!tmux::test_mock::contains(pane, tmux::PANE_WORKTREE_NAME));
        assert!(!tmux::test_mock::contains(pane, PENDING_WORKTREE_REMOVE));
    }
}
