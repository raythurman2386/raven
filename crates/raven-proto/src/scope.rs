/// The operational scope of a session.
///
/// - [`Scope::Repo`] — the default. The agent operates inside a single repo
///   workspace; the sandbox is rooted at that workspace (Landlock RW only there,
///   seccomp network-block, `confirm_shell` gate). This is the reviewed harness.
/// - [`Scope::System`] — opt-in via `--system`. The agent administers the whole
///   OS: the sandbox is rooted at `/` (write-everywhere at the Landlock layer),
///   system scope uses its own system prompt and persistence, and
///   `confirm_shell` is forced on so no destructive command runs unconfirmed.
///   Intended for trusted single-user machines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Repo,
    System,
}

impl Scope {
    /// Whether this is the system-administration scope.
    pub fn is_system(self) -> bool {
        matches!(self, Scope::System)
    }

    /// Human-readable label for the scope.
    pub fn label(self) -> &'static str {
        match self {
            Scope::Repo => "repo",
            Scope::System => "system",
        }
    }
}
