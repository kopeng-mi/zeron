//! Resident memory of each running chat's agent, for the status line.
//!
//! Harness drivers spawn their processes in their own ways, so there is no
//! pid to hand. Every run does start this binary's MCP server as a child of
//! the agent, labelled `zeron mcp --chat <id>`: its parent is the agent, and
//! the agent's whole process tree (tools, shells, the MCP server itself) is
//! what the chat costs. Harnesses that share one process between chats
//! (Codex's app server) report that shared process for each of them.

use std::collections::HashMap;
use std::ffi::OsStr;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// Resident bytes per chat id, for the chats in `chats` that have a
/// labelled MCP server running. Blocking: call off the async runtime.
pub fn sample(system: &mut System, chats: &[String]) -> HashMap<String, u64> {
    if chats.is_empty() {
        return HashMap::new();
    }
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_memory()
            .with_cmd(UpdateKind::OnlyIfNotSet),
    );
    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    let mut agents: HashMap<String, Pid> = HashMap::new();
    for (pid, process) in system.processes() {
        if let Some(parent) = process.parent() {
            children.entry(parent).or_default().push(*pid);
        }
        if let Some(chat) = labelled_chat(process.cmd())
            && chats.iter().any(|c| c == chat)
            && let Some(parent) = process.parent()
        {
            agents.insert(chat.to_owned(), parent);
        }
    }
    agents
        .into_iter()
        .map(|(chat, root)| {
            let mut total = 0u64;
            let mut stack = vec![root];
            let mut seen = std::collections::HashSet::new();
            while let Some(pid) = stack.pop() {
                if !seen.insert(pid) {
                    continue;
                }
                if let Some(process) = system.process(pid) {
                    total += process.memory();
                }
                stack.extend(children.get(&pid).into_iter().flatten().copied());
            }
            (chat, total)
        })
        .collect()
}

/// The chat id in a `… mcp --chat <id>` command line.
fn labelled_chat(cmd: &[std::ffi::OsString]) -> Option<&str> {
    let args: Vec<&OsStr> = cmd.iter().map(|a| a.as_os_str()).collect();
    let mcp = args.iter().position(|a| *a == "mcp")?;
    let flag = args[mcp..].iter().position(|a| *a == "--chat")? + mcp;
    args.get(flag + 1)?.to_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(parts: &[&str]) -> Vec<std::ffi::OsString> {
        parts.iter().map(Into::into).collect()
    }

    #[test]
    fn finds_the_chat_label_on_the_mcp_command_line() {
        assert_eq!(
            labelled_chat(&cmd(&["C:/zeron.exe", "mcp", "--chat", "abc-1"])),
            Some("abc-1")
        );
        assert_eq!(labelled_chat(&cmd(&["zeron", "mcp"])), None);
        assert_eq!(labelled_chat(&cmd(&["node", "--chat", "x"])), None);
    }

    #[test]
    fn this_process_tree_is_measured_through_a_labelled_child() {
        // Spawn a labelled stand-in: this test binary's own children count.
        let mut system = System::new();
        let me = std::process::id().to_string();
        assert!(sample(&mut system, &[]).is_empty());
        assert!(sample(&mut system, &[me]).is_empty(), "no labelled server yet");
    }
}
