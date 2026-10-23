//! A result row, whatever its source.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    App,
    Command,
    File,
    Dir,
    /// Calculator answer.
    Calc,
    /// A raw shell line for the inline terminal (`>` prefix).
    Run,
}

impl Kind {
    pub fn group(self) -> &'static str {
        match self {
            Kind::App => "apps",
            Kind::Command | Kind::Run => "commands",
            Kind::File | Kind::Dir => "files",
            Kind::Calc => "calc",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub kind: Kind,
    /// Stable id for usage history: `app:<desktop id>`, `cmd:<name>`, `file:<path>`.
    pub id: String,
    pub name: String,
    /// Second line: parent folder, command path, generic name.
    pub detail: String,
    /// Icon name or serialized GIcon; empty for none.
    pub icon: String,
    /// What the actions act on: desktop id, command line, path, answer, shell line.
    pub target: String,
    pub score: f64,
}
