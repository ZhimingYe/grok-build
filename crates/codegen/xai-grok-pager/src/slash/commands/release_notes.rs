use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand, slash_meta};

// ZMY SELF-ADDED CODE START
// Removed the Action import because release notes no longer fetch or display upstream content.
// ZMY SELF-ADDED CODE END
pub struct ReleaseNotesCommand;

impl SlashCommand for ReleaseNotesCommand {
    slash_meta! {
        name: "release-notes",
        aliases: ["changelog"],
        description: "View release notes for the current version",
        usage: "/release-notes",
    }

    fn run(&self, _ctx: &mut CommandExecCtx, _args: &str) -> CommandResult {
        // ZMY SELF-ADDED CODE START
        CommandResult::Error(xai_grok_env::OFFICIAL_SERVICES_DISABLED.to_owned())
        // ZMY SELF-ADDED CODE END
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_notes_metadata() {
        let cmd = ReleaseNotesCommand;
        assert_eq!(cmd.name(), "release-notes");
        assert_eq!(cmd.aliases(), &["changelog"]);
        assert!(!cmd.takes_args());
    }

    #[test]
    fn release_notes_returns_action_or_error() {
        let models = crate::acp::model_state::ModelState::default();
        let mut ctx = super::super::tests::make_ctx(&models);
        let result = ReleaseNotesCommand.run(&mut ctx, "");
        assert!(
            matches!(result, CommandResult::Action(_) | CommandResult::Error(_)),
            "expected Action or Error, got {result:?}"
        );
    }
}
