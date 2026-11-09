//! A command as a type: [`CliCommand`] pairs a command's clap `Args` and
//! [`CommandSpec`] with its `run`, and [`Entry::of`] erases the type. The list
//! of entries in `commands::entries` is the only place a command is registered;
//! the clap tree ([`command_tree`]), the schema registry and dispatch are all
//! derived from it.
//!
//! Split from [`crate::command_spec`] so that module stays pure data: only
//! running a command needs the [`Invocation`].

use std::pin::Pin;

use clap::{ArgMatches, Command, FromArgMatches};

use crate::command_spec::{CommandSpec, GroupSpec};
use crate::errors::CliError;
use crate::invocation::Invocation;

pub type RunFuture<'a> = Pin<Box<dyn Future<Output = Result<(), anyhow::Error>> + 'a>>;

pub trait CliCommand: 'static {
    type Args: clap::Args + FromArgMatches + 'static;

    const SPEC: CommandSpec;

    /// Rejections that need more than a per-arg [`Check`]. Runs right after them.
    fn validate(_args: &Self::Args) -> Result<(), CliError> {
        Ok(())
    }

    fn run<'a>(
        inv: &'a Invocation,
        args: &'a Self::Args,
    ) -> impl Future<Output = Result<(), anyhow::Error>> + 'a;
}

pub trait Bound {
    fn validate(&self) -> Result<(), CliError>;
    fn run<'a>(&'a self, inv: &'a Invocation) -> RunFuture<'a>;
}

struct Parsed<C: CliCommand>(C::Args);

impl<C: CliCommand> Bound for Parsed<C> {
    fn validate(&self) -> Result<(), CliError> {
        C::validate(&self.0)
    }

    fn run<'a>(&'a self, inv: &'a Invocation) -> RunFuture<'a> {
        Box::pin(C::run(inv, &self.0))
    }
}

/// A registered command with its type erased.
pub struct Entry {
    pub spec: &'static CommandSpec,
    augment: fn(Command) -> Command,
    bind: fn(&ArgMatches) -> Result<Box<dyn Bound>, clap::Error>,
}

impl Entry {
    #[must_use]
    pub fn of<C: CliCommand>() -> Self {
        Self {
            spec: &C::SPEC,
            augment: <C::Args as clap::Args>::augment_args,
            bind: |matches| {
                Ok(Box::new(Parsed::<C>(C::Args::from_arg_matches(matches)?)) as Box<dyn Bound>)
            },
        }
    }

    pub fn bind(&self, matches: &ArgMatches) -> Result<Box<dyn Bound>, clap::Error> {
        (self.bind)(matches)
    }
}

struct Node<'a> {
    name: &'static str,
    entry: Option<&'a Entry>,
    group: Option<&'a GroupSpec>,
    children: Vec<Node<'a>>,
}

impl<'a> Node<'a> {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            entry: None,
            group: None,
            children: Vec::new(),
        }
    }

    fn at(&mut self, path: &[&'static str]) -> &mut Node<'a> {
        let Some((first, rest)) = path.split_first() else {
            return self;
        };
        let index = match self.children.iter().position(|child| child.name == *first) {
            Some(index) => index,
            None => {
                self.children.push(Node::new(first));
                self.children.len() - 1
            }
        };
        self.children[index].at(rest)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(self.name);
        if let Some(entry) = self.entry {
            let spec = entry.spec;
            command = (entry.augment)(command).about(spec.about);
            if let Some(long_about) = spec.long_about {
                command = command.long_about(long_about);
            }
            for alias in spec.cli_aliases {
                command = command.alias(*alias);
            }
        } else if let Some(group) = self.group {
            command = command.about(group.about);
            if let Some(long_about) = group.long_about {
                command = command.long_about(long_about);
            }
            for alias in group.aliases {
                command = command.alias(*alias);
            }
            command = command
                .subcommand_required(true)
                .arg_required_else_help(true);
        }
        for (order, child) in self.children.iter().enumerate() {
            command = command.subcommand(child.command().display_order(order));
        }
        command
    }
}

/// The clap subcommand tree of `entries`, in registration order, under a bare
/// `hyperliquid` command. Groups come from `groups`.
#[must_use]
pub fn command_tree(entries: &[Entry], groups: &[GroupSpec]) -> Command {
    let mut root = Node::new("hyperliquid");
    for entry in entries {
        let node = root.at(entry.spec.path);
        assert!(
            node.entry.is_none(),
            "command {} is registered twice",
            entry.spec.key()
        );
        node.entry = Some(entry);
    }
    for group in groups {
        let node = root.at(group.path);
        assert!(
            node.entry.is_none() && !node.children.is_empty(),
            "group {} has no commands or is itself a command",
            group.path.join(" ")
        );
        node.group = Some(group);
    }
    let mut command = Command::new("hyperliquid");
    for (order, child) in root.children.iter().enumerate() {
        command = command.subcommand(child.command().display_order(order));
    }
    command
}
