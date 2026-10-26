// ZMY SELF-ADDED CODE START
//! Subscription tier checks, provider usage notices, and auto-topup handling.
// ZMY SELF-ADDED CODE END

use super::queue::{maybe_drain_queue, note_peek_page_flip};
use crate::app::actions::Effect;
use crate::app::agent::AgentId;
use crate::app::agent_view::AgentView;
use crate::app::app_view::AppView;
use crate::scrollback::block::RenderBlock;
use std::time::Duration;

// ZMY SELF-ADDED CODE START
// Official plan and billing destinations and their promotional telemetry are intentionally omitted.
// ZMY SELF-ADDED CODE END
/// How long the pager auto-checks subscription status before stopping.
/// After this, the user can still manually check via the [Refresh] button.
pub(super) const PAYWALL_AUTO_CHECK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Whether the user is at the highest subscription tier (SuperGrok Heavy).
/// Returns `true` only when `subscription_tier` positively matches a known max-tier identifier.
/// An unknown (`None`) or unrecognized tier returns `false`, so lower-tier users always get the Q&A modal with the upgrade option.
pub(super) fn is_max_tier(subscription_tier: Option<&str>) -> bool {
    let Some(t) = subscription_tier else {
        return false; // Unknown: default to Q&A.
    };
    // Lowercase and replace spaces with underscores to match both JWT-derived keys ("supergrok_heavy") and CCP display names ("SuperGrok Heavy")
    t.to_ascii_lowercase().replace(' ', "_") == "supergrok_heavy"
}

// ZMY SELF-ADDED CODE START
// Removed production plan-upgrade and credit-purchase URL constants; these test-only sentinels cannot open external links.
#[cfg(test)]
pub(crate) const UPSELL_URL_UPGRADE: &str = "https://example.invalid/disabled-upgrade";
#[cfg(test)]
pub(crate) const UPSELL_URL_PAYG: &str = "https://example.invalid/disabled-billing";
// ZMY SELF-ADDED CODE END

/// Billing mode for credit-limit upsell copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CreditLimitUpsellMode {
    /// Unified usage pool: suggest purchasing prepaid credits.
    UnifiedCredits,
    /// Legacy on-demand PAYG (`enabled` means the on-demand cap is already active).
    LegacyPayg { enabled: bool },
}

/// Resolve upsell copy mode from credits config.
/// A positive `pay_as_you_go` (an on-demand cap over 0) only selects legacy when the unified flag is absent.
/// Unknown defaults to unified (buy credits) so pool users are never told to enable on-demand.
pub(super) fn credit_limit_upsell_mode(
    balance: Option<&crate::views::credit_bar::CreditBalance>,
) -> CreditLimitUpsellMode {
    match balance {
        Some(b) if b.is_unified_billing_user == Some(true) => CreditLimitUpsellMode::UnifiedCredits,
        Some(b) if b.is_unified_billing_user == Some(false) => CreditLimitUpsellMode::LegacyPayg {
            enabled: b.pay_as_you_go,
        },
        // Flag absent: only treat as legacy PAYG on a positive on-demand cap (`pay_as_you_go` derives from that cap)
        Some(b) if b.pay_as_you_go => CreditLimitUpsellMode::LegacyPayg { enabled: true },
        _ => CreditLimitUpsellMode::UnifiedCredits,
    }
}

/// Whether an API or retry error is a credit-limit or spend-block denial.
/// 402 Payment Required always means a credit or spend block here (Build pool and IC spend blocks); no message filter.
/// 403 counts only when the body contains "run out of credits" (legacy IC spend wording); other 403s (content-safety, ZDR, …) are excluded.
pub(crate) fn is_credit_limit_error(http_status: Option<u16>, message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    let legacy = m.contains("run out of credits");
    match http_status {
        Some(402) => true,
        Some(403) if legacy => true,
        // Retry notifications embed "status 402" / "status 403" in the body without a separate status field
        None | Some(_) => m.contains("status 402") || (m.contains("status 403") && legacy),
    }
}

/// Option id for Try Again. Submit routes on this sentinel, not on position in the telemetry `choices` vec.
/// position in the telemetry `choices` vec.
pub(crate) const CREDIT_LIMIT_RETRY_OPTION_ID: &str = "retry-last-prompt";

// ZMY SELF-ADDED CODE START
/// Show an actionable, provider-neutral credit-limit notice on the given agent.
// ZMY SELF-ADDED CODE END
pub(super) fn open_credit_limit_upsell(
    agent: &mut AgentView,
    mode: CreditLimitUpsellMode,
    // ZMY SELF-ADDED CODE START
    _max_tier: bool,
    // ZMY SELF-ADDED CODE END
) {
    // ZMY SELF-ADDED CODE START
    // Replaces the official purchase modal with a neutral provider usage notice.
    let message = match mode {
        CreditLimitUpsellMode::UnifiedCredits => {
            "The configured provider reports that no credits remain. Check its usage settings or configure another provider."
        }
        CreditLimitUpsellMode::LegacyPayg { .. } => {
            "The configured provider reports that its spending limit was reached. Check its usage settings or configure another provider."
        }
    };
    if agent.credit_limit_stashed_prompt.is_some() && agent.question_view.is_none() {
        let question = crate::views::question_view::Question {
            question: message.into(),
            id: None,
            options: vec![crate::views::question_view::QuestionOption {
                label: "Try Again".into(),
                description: "Retry the previous prompt".into(),
                preview: None,
                id: Some(CREDIT_LIMIT_RETRY_OPTION_ID.into()),
            }],
            multi_select: Some(false),
        };
        let state = crate::views::question_view::QuestionViewState::new(
            format!("credit-limit-{}", uuid::Uuid::new_v4()),
            vec![question],
            agent.prompt.stash(),
        )
        .with_local_kind(
            crate::views::question_view::LocalQuestionKind::CreditLimitUpsell { choices: vec![] },
        )
        .with_no_freeform();
        agent.install_local_question(state);
        agent.prompt.set_text("");
    } else {
        agent.show_toast(message);
    }
    // ZMY SELF-ADDED CODE END
}

// ZMY SELF-ADDED CODE START
/// Show a provider-neutral status when configured-provider usage is exhausted.
// ZMY SELF-ADDED CODE END
/// Only the driver can reach this: the PromptResponse handler calls it, and viewers never receive that response.
pub(super) fn open_free_usage_upsell(agent: &mut AgentView, auth_method: Option<String>) {
    open_supergrok_upsell(agent, UpsellReason::FreeUsageLimit, auth_method);
}

// ZMY SELF-ADDED CODE START
/// Show a provider-neutral status for an unavailable command.
// ZMY SELF-ADDED CODE END
pub(super) fn open_restricted_command_upsell(
    agent: &mut AgentView,
    auth_method: Option<String>,
) -> bool {
    open_supergrok_upsell(agent, UpsellReason::RestrictedCommand, auth_method)
}

// ZMY SELF-ADDED CODE START
/// Which provider limitation should be explained to the user.
// ZMY SELF-ADDED CODE END
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UpsellReason {
    /// Free-usage quota exhausted (429 paywall).
    FreeUsageLimit,
    /// A tier-restricted slash command was invoked.
    RestrictedCommand,
}

// ZMY SELF-ADDED CODE START
/// Shared provider-neutral status builder.
// ZMY SELF-ADDED CODE END
fn open_supergrok_upsell(
    agent: &mut AgentView,
    reason: UpsellReason,
    auth_method: Option<String>,
) -> bool {
    // ZMY SELF-ADDED CODE START
    // Replaces official subscription upgrade content and links with provider guidance.
    let _ = auth_method;
    let message = match reason {
        UpsellReason::FreeUsageLimit => {
            "The configured provider reports that its usage limit was reached. Check its quota or configure another provider."
        }
        UpsellReason::RestrictedCommand => {
            "This command is unavailable with the configured provider. Choose a provider or model that supports it."
        }
    };
    agent.show_toast(message);
    // ZMY SELF-ADDED CODE END
    true
}

/// Apply an [`AutoTopupFetch`] outcome to a cached `auto_topup` slot.
/// `Resolved` sets it, `Cleared` resets it to "unknown" (no credits), and `Unchanged` keeps the last-known-good value (the fetch failed).
pub(super) fn apply_auto_topup(
    slot: &mut Option<crate::views::credit_bar::AutoTopupInfo>,
    fetch: &crate::views::credit_bar::AutoTopupFetch,
) {
    use crate::views::credit_bar::AutoTopupFetch;
    match fetch {
        AutoTopupFetch::Resolved(rule) => *slot = Some(rule.clone()),
        AutoTopupFetch::Cleared => *slot = None,
        AutoTopupFetch::Unchanged => {}
    }
}

// TaskResult handlers.

pub(super) fn handle_billing_fetched(
    app: &mut AppView,
    agent_id: AgentId,
    balance: Option<crate::views::credit_bar::CreditBalance>,
    silent: bool,
    subscription_tier: Option<String>,
    autotopup: crate::views::credit_bar::AutoTopupFetch,
    nonce: u64,
) -> Vec<Effect> {
    // Parse/transport failures route to `BillingError`, so a `None` balance here means the response carried no billing config
    // Clear the cached balance and polling so the status bar agrees with the "No billing data available." message rather than showing a stale value
    app.credit_balance = balance.clone();
    apply_auto_topup(&mut app.auto_topup, &autotopup);
    app.billing_poll_wanted = balance
        .as_ref()
        .map(|b| b.usage_pct >= 99.0)
        .unwrap_or(false);
    if let Some(tier) = subscription_tier {
        app.subscription_tier = Some(tier);
    }
    // Render the `/usage` summary from the now-current cached rule.
    let summary_topup = app.auto_topup.clone();
    let tier_now = app.subscription_tier.clone();
    if let Some(agent) = app.agents.get_mut(&agent_id) {
        // Gateway/chat-kind: do not attach Build coding credits.
        let mut topup = agent.auto_topup.clone();
        apply_auto_topup(&mut topup, &autotopup);
        agent.apply_credit_balance(balance.clone(), topup);
        // The open usage modal renders from the mirrors updated above
        // Only its own fetch generation may settle the loading/error flags (background refreshes carry nonce 0)
        if let Some(state) = super::status::usage_modal_state_mut(agent)
            && state.fetch_nonce == nonce
        {
            state.billing_loading = false;
            state.billing_error = None;
            state.ctx.subscription_tier = tier_now;
        }
        if !silent && !agent.chat_kind {
            let msg = match &balance {
                Some(bal) => {
                    crate::views::credit_bar::format_usage_summary(bal, summary_topup.as_ref())
                }
                None => "No billing data available.".to_string(),
            };
            agent.scrollback.push_block(RenderBlock::System(
                crate::scrollback::blocks::SystemMessageBlock::new(msg),
            ));
        }
    }
    vec![]
}

pub(super) fn handle_gate_refreshed(
    app: &mut AppView,
    settings: Option<xai_grok_shell::util::config::RemoteSettings>,
) -> Vec<Effect> {
    let Some(rs) = settings else {
        return vec![];
    };
    app.usage_billing_redirect_url = rs.usage_billing_redirect_url.clone();
    if let Some(secs) = rs.subscription_watch_interval_secs {
        app.subscription_watch_interval_secs = Some(secs);
    }
    match AppView::gate_from_settings(&rs) {
        Some(gate) => app.impose_gate(gate),
        None => app.lift_gate(),
    }
}

/// A re-check snapshot built before hydration can land after it, so for the same account a resolved capability is kept. A genuine loss (removed from the team mid-session) then waits for the next launch, the right way to be wrong: the value is advisory and the row is editable when unknown.
fn apply_recheck_meta(app: &mut AppView, mut meta: xai_grok_login::AuthMeta) {
    let same_account = crate::app::app_view::AuthIdentity {
        email: meta.email.clone(),
        team_id: meta.team_id.clone(),
        team_principal: meta.is_team_principal,
    }
    .matches(&app.auth_identity());
    if same_account && meta.can_administer_team.is_none() {
        meta.can_administer_team = app.can_administer_team;
    }
    app.apply_auth_meta(&meta);
}

/// `x.ai/auth/check_subscription` completed.
/// A failed check only promotes the deferred gate it was verifying (the `verify` generation).
/// Generic watch, focus, and paywall-chain failures never touch it.
pub(super) fn handle_check_subscription_complete(
    app: &mut AppView,
    verify: Option<u64>,
    meta: Option<serde_json::Value>,
) -> Vec<Effect> {
    let was_blocked = !app.has_access();
    let applied = match meta {
        Some(meta_val) => {
            match serde_json::from_value::<xai_grok_login::AuthMeta>(meta_val) {
                Ok(auth_meta) => {
                    apply_recheck_meta(app, auth_meta);
                    true
                }
                Err(e) => {
                    // The shell sent meta we can't decode, a protocol bug rather than a transient failure
                    // The check result is lost, so a verify deferral falls through to promotion below
                    crate::unified_log::error(
                        "subscription.check.meta_parse_failed",
                        None,
                        Some(serde_json::json!({
                            "verify": verify,
                            "error": e.to_string(),
                        })),
                    );
                    false
                }
            }
        }
        // A `None` meta means the shell reports "not authenticated" or the check RPC failed (already logged as subscription.check.rpc_failed)
        None => false,
    };
    if !applied && let Some(generation) = verify {
        app.promote_deferred_gate(generation, "check_failed");
    }
    crate::unified_log::info(
        "subscription.check.complete",
        None,
        Some(serde_json::json!({
            "verify": verify,
            "meta_applied": applied,
            "was_blocked": was_blocked,
            "gated": !app.has_access(),
            "tier": app.subscription_tier,
        })),
    );
    maybe_start_paywall_chain(app, was_blocked)
}

/// Safety net for a hung verification check: show the still-pending deferred gate, erring on the side of blocking.
pub(super) fn handle_gate_verify_timeout(app: &mut AppView, generation: u64) -> Vec<Effect> {
    let was_blocked = !app.has_access();
    app.promote_deferred_gate(generation, "verify_timeout");
    maybe_start_paywall_chain(app, was_blocked)
}

/// Start the 5s paywall auto-check chain when the app goes from ungated to gated.
/// A paywall shown after a failed verification check then lifts itself exactly like the one shown at login.
/// The guard keeps repeated checks and steady-state paywall-poller responses from starting extra timers.
fn maybe_start_paywall_chain(app: &mut AppView, was_blocked: bool) -> Vec<Effect> {
    if !was_blocked && !app.has_access() && app.paywall_check_started.is_none() {
        app.paywall_check_started = Some(std::time::Instant::now());
        return vec![Effect::SchedulePaywallCheck];
    }
    vec![]
}

pub(super) fn handle_credit_limit_recheck_complete(
    app: &mut AppView,
    agent_id: AgentId,
    meta: Option<serde_json::Value>,
) -> Vec<Effect> {
    let old_tier = app.subscription_tier.clone();
    if let Some(meta_val) = meta
        && let Ok(auth_meta) = serde_json::from_value::<xai_grok_login::AuthMeta>(meta_val)
    {
        apply_recheck_meta(app, auth_meta);
    }
    let tier_changed = app.subscription_tier != old_tier && app.subscription_tier.is_some();

    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };

    // If the user already submitted another prompt while the recheck ran, don't retry the stashed one; they've moved on The tier update (above) still takes effect
    // The tier update (above) still takes effect
    let user_moved_on = !agent.session.state.is_idle() || !agent.session.pending_prompts.is_empty();

    if tier_changed && !user_moved_on {
        if let Some(prompt) = agent.credit_limit_stashed_prompt.take() {
            let tier_name = app.subscription_tier.as_deref().unwrap_or("a higher tier");
            agent.scrollback.push_block(RenderBlock::system(format!(
                "Subscription upgraded to {tier_name}. Retrying\u{2026}"
            )));
            agent.session.enqueue_in_flight_prompt_front(prompt);
        }
    } else if !user_moved_on {
        let balance = agent
            .credit_balance
            .as_ref()
            .or(app.credit_balance.as_ref());
        let mode = credit_limit_upsell_mode(balance);
        let max_tier = is_max_tier(app.subscription_tier.as_deref());
        open_credit_limit_upsell(agent, mode, max_tier);
        // Keep the stashed prompt so Try Again can resubmit after the user buys credits or the limit resets.
        // user buys credits or the limit resets.
    } else {
        agent.credit_limit_stashed_prompt = None;
    }

    let mut drain = maybe_drain_queue(agent, &mut app.pending_image_notices);
    drain.effects.push(Effect::FetchBilling {
        agent_id,
        silent: true,
        nonce: Default::default(),
    });
    note_peek_page_flip(app, agent_id, drain.page_flip_entry);
    drain.effects
}

/// Resubmit the prompt that hit the credit limit (modal option or card button).
pub(super) fn dispatch_retry_credit_limit_prompt(app: &mut AppView) -> Vec<Effect> {
    use crate::app::app_view::ActiveView;

    let ActiveView::Agent(agent_id) = app.active_view else {
        return vec![];
    };
    let Some(agent) = app.agents.get_mut(&agent_id) else {
        return vec![];
    };
    let Some(prompt) = agent.credit_limit_stashed_prompt.take() else {
        agent.show_toast("No prompt to retry.");
        agent
            .scrollback
            .push_block(RenderBlock::system("No prompt to retry."));
        return vec![];
    };
    agent.session.enqueue_in_flight_prompt_front(prompt);
    let drain = maybe_drain_queue(agent, &mut app.pending_image_notices);
    if drain.effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::SendPrompt { .. }
                | Effect::SendPromptBlocks { .. }
                | Effect::SendBashCommand { .. }
        )
    }) {
        agent
            .scrollback
            .push_block(RenderBlock::system("Trying again\u{2026}"));
    }
    note_peek_page_flip(app, agent_id, drain.page_flip_entry);
    drain.effects
}

// Action handlers.

pub(super) fn dispatch_open_supergrok_url(app: &mut AppView) -> Vec<Effect> {
    // ZMY SELF-ADDED CODE START
    app.show_toast(
        "Official plan links are disabled. Configure a custom provider to use this feature.",
    );
    // ZMY SELF-ADDED CODE END
    vec![]
}
