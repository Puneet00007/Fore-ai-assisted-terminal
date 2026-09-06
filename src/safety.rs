//! Safety classifier: how much can this command hurt?
//!
//! We parse with tree-sitter-bash instead of regex-matching the string, because:
//!   - `echo "never run rm -rf /"` must NOT be flagged (the danger is inside a string)
//!   - `sudo -u deploy rm -rf ./cache` MUST be flagged (rm hides behind sudo)
//!   - `make clean && rm -rf build` has two commands; the worst one wins
//!   - `curl … | sh` is a pipeline whose danger comes from the *combination*
//!
//! The output is a risk level plus human-readable reasons. The daemon attaches it to
//! every command it proposes, and the shell paints it as a badge.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Only reads state: ls, cat, git status, grep…
    ReadOnly,
    /// Changes local, recoverable state: mkdir, git add, npm install…
    Mutating,
    /// Talks to the outside world with side effects: git push, curl -X POST, kubectl apply…
    Remote,
    /// Runs as root or changes system state.
    Privileged,
    /// Hard-to-undo data loss: rm -rf, git push --force, DROP TABLE, dd, mkfs…
    Destructive,
}

impl Risk {
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Risk::ReadOnly => "read-only",
            Risk::Mutating => "mutating",
            Risk::Remote => "remote",
            Risk::Privileged => "privileged",
            Risk::Destructive => "DESTRUCTIVE",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assessment {
    pub risk: Risk,
    pub reasons: Vec<String>,
}

const READ_ONLY: &[&str] = &[
    "ls", "cat", "less", "more", "head", "tail", "grep", "rg", "find", "fd", "pwd", "echo",
    "printf", "which", "whereis", "type", "file", "stat", "du", "df", "wc", "sort", "uniq",
    "cut", "awk", "sed", "tr", "diff", "cmp", "man", "help", "env", "printenv", "date", "cal",
    "uptime", "whoami", "id", "hostname", "uname", "ps", "top", "htop", "jq", "yq", "bat",
    "tree", "exa", "eza", "lsof", "netstat", "ss", "ping", "dig", "nslookup", "history", "true",
    "false", "test", "sleep", "cd", "pushd", "popd", "dirs", "basename", "dirname", "realpath",
    "readlink", "md5sum", "sha256sum", "xxd", "hexdump", "strings", "nproc", "free", "arch",
];

/// Subcommands of well-known tools that only read.
const READ_ONLY_SUB: &[(&str, &[&str])] = &[
    ("git", &["status", "log", "diff", "show", "branch", "blame", "remote", "stash list", "ls-files", "rev-parse", "describe", "tag", "reflog", "grep", "fetch"]),
    ("cargo", &["check", "build", "test", "bench", "doc", "clippy", "fmt", "tree", "metadata", "run"]),
    ("npm", &["test", "run", "ls", "list", "view", "outdated", "audit", "start"]),
    ("pnpm", &["test", "run", "ls", "list", "outdated", "start"]),
    ("yarn", &["test", "run", "list", "outdated", "start"]),
    ("docker", &["ps", "images", "logs", "inspect", "stats", "top", "version", "info", "port", "diff"]),
    ("kubectl", &["get", "describe", "logs", "top", "explain", "version", "config", "api-resources"]),
    ("terraform", &["plan", "show", "output", "validate", "fmt", "state list"]),
    ("aws", &["sts", "s3 ls"]),
    ("gh", &["pr view", "pr list", "pr status", "issue list", "issue view", "repo view", "run list", "run view"]),
    ("pip", &["list", "show", "freeze", "check"]),
    ("python", &[]), ("python3", &[]), ("node", &[]), ("go", &["build", "test", "vet", "fmt", "run"]),
    ("systemctl", &["status", "list-units", "is-active", "show"]),
    ("brew", &["list", "info", "search", "outdated"]),
    ("apt", &["list", "search", "show"]), ("apt-get", &[]),
];

const REMOTE_CMDS: &[&str] = &["ssh", "scp", "rsync", "sftp", "ftp", "telnet", "nc", "ncat", "wget", "curl"];

const DESTRUCTIVE_CMDS: &[&str] = &["mkfs", "mkfs.ext4", "mkfs.xfs", "fdisk", "parted", "wipefs", "shred", "dd"];

/// Classify a full command line.
pub fn assess(cmdline: &str) -> Assessment {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .expect("bash grammar");
    let Some(tree) = parser.parse(cmdline, None) else {
        return Assessment { risk: Risk::Mutating, reasons: vec!["could not parse; assuming it mutates".into()] };
    };

    let src = cmdline.as_bytes();
    let mut ctx = Ctx { src, risk: Risk::ReadOnly, reasons: Vec::new(), seen: HashSet::new(), in_pipeline_from_net: false };
    walk(tree.root_node(), &mut ctx);
    // Redirections that truncate files: `> file`
    if has_truncating_redirect(tree.root_node(), src) {
        ctx.bump(Risk::Mutating, "`>` overwrites a file");
    }
    Assessment { risk: ctx.risk, reasons: ctx.reasons }
}

struct Ctx<'a> {
    src: &'a [u8],
    risk: Risk,
    reasons: Vec<String>,
    seen: HashSet<String>,
    in_pipeline_from_net: bool,
}

impl Ctx<'_> {
    fn bump(&mut self, r: Risk, why: impl Into<String>) {
        let why = why.into();
        if self.seen.insert(why.clone()) {
            self.reasons.push(why);
        }
        if r > self.risk {
            self.risk = r;
        }
    }
    fn text(&self, n: Node) -> &str {
        n.utf8_text(self.src).unwrap_or("")
    }
}

fn walk(node: Node, ctx: &mut Ctx) {
    match node.kind() {
        "pipeline" => {
            // Track "network fetch piped into an interpreter".
            let mut cursor = node.walk();
            let cmds: Vec<Node> = node.named_children(&mut cursor).collect();
            let mut prev_net = false;
            for c in cmds {
                if c.kind() == "command" {
                    let name = command_name(c, ctx);
                    if prev_net && matches!(name.as_str(), "sh" | "bash" | "zsh" | "python" | "python3" | "perl" | "ruby" | "node") {
                        ctx.bump(Risk::Destructive, "downloads code from the network and executes it (`curl … | sh`)");
                    }
                    prev_net = matches!(name.as_str(), "curl" | "wget");
                }
                walk(c, ctx);
            }
            return;
        }
        "command" => classify_command(node, ctx),
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, ctx);
    }
}

/// The executable name, with `sudo`/`doas`/`env`/`time`/`nohup` wrappers peeled off.
fn command_name(node: Node, ctx: &Ctx) -> String {
    let words = words_of(node, ctx);
    let mut i = 0;
    while i < words.len() {
        let w = words[i].rsplit('/').next().unwrap_or(&words[i]);
        match w {
            "sudo" | "doas" | "time" | "nohup" | "nice" | "env" | "command" | "builtin" | "exec" | "timeout" | "caffeinate" | "stdbuf" | "unbuffer" | "hyperfine" => {
                let wrapper = w.to_string();
                i += 1;
                // skip flags of the wrapper (e.g. `sudo -u deploy`, `time -f %e`, `timeout 5`)
                if wrapper == "timeout" && i < words.len() && !words[i].starts_with('-') { i += 1; }
                while i < words.len() && words[i].starts_with('-') {
                    // flags that take a value
                    if matches!(words[i].as_str(), "-u" | "-g" | "-n" | "-I" | "-f" | "-o" | "-a" | "-e" | "-i" | "-s" | "-k") { i += 1; }
                    i += 1;
                }
                // env FOO=bar cmd
                while i < words.len() && words[i].contains('=') && !words[i].starts_with('-') { i += 1; }
            }
            _ => break,
        }
    }
    words.get(i).cloned().unwrap_or_default()
}

fn words_of(node: Node, ctx: &Ctx) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for ch in node.children(&mut cursor) {
        match ch.kind() {
            "command_name" | "word" | "number" | "string" | "raw_string" | "concatenation" => {
                out.push(ctx.text(ch).trim_matches(|c| c == '"' || c == '\'').to_string())
            }
            "variable_assignment" => {} // FOO=bar prefix
            _ => {}
        }
    }
    out
}

fn classify_command(node: Node, ctx: &mut Ctx) {
    let words = words_of(node, ctx);
    if words.is_empty() {
        return;
    }
    let all = words.clone();
    let has_sudo = all.first().is_some_and(|w| w == "sudo" || w == "doas");
    let name = command_name(node, ctx);
    let name = name.rsplit('/').next().unwrap_or(&name).to_string();
    // arguments after the real command name
    let pos = all.iter().position(|w| w.rsplit('/').next().unwrap_or(w) == name).unwrap_or(0);
    let args: Vec<&str> = all[pos + 1..].iter().map(String::as_str).collect();
    let arg_str = args.join(" ");
    let flags: Vec<&str> = args.iter().copied().filter(|a| a.starts_with('-')).collect();
    let has_flag = |short: char, long: &str| {
        flags.iter().any(|f| (f.starts_with("--") && *f == long) || (!f.starts_with("--") && f.contains(short)))
    };

    if has_sudo {
        ctx.bump(Risk::Privileged, "runs as root (sudo)");
    }

    // `find … -exec CMD … ;` / `-execdir` / `-ok` / `-delete`: the danger is in the payload.
    // Same for `xargs CMD` and `parallel CMD`. Recurse on the embedded command.
    if matches!(name.as_str(), "find" | "fd" | "fdfind") {
        if args.contains(&"-delete") {
            ctx.bump(Risk::Destructive, format!("{name} -delete removes every matched file"));
        }
        if let Some(i) = args.iter().position(|a| matches!(*a, "-exec" | "-execdir" | "-ok" | "-okdir" | "-x" | "--exec" | "-X" | "--exec-batch")) {
            let inner: Vec<&str> = args[i + 1..].iter().copied().take_while(|a| *a != ";" && *a != "+" && *a != "\\;").collect();
            if !inner.is_empty() {
                let inner_line = inner.join(" ");
                let sub = assess(&inner_line);
                if sub.reasons.is_empty() {
                    if sub.risk > Risk::ReadOnly { ctx.bump(sub.risk, format!("{name} -exec runs `{inner_line}` per file")); }
                } else {
                    for r in sub.reasons { ctx.bump(sub.risk, format!("{name} -exec → {r}")); }
                }
            }
        }
        return;
    }
    if matches!(name.as_str(), "xargs" | "parallel") {
        let inner: Vec<&str> = args.iter().copied().skip_while(|a| a.starts_with('-') || a.chars().all(|c| c.is_ascii_digit())).collect();
        if !inner.is_empty() {
            let sub = assess(&inner.join(" "));
            for r in sub.reasons { ctx.bump(sub.risk, format!("{name} → {r}")); }
        }
        return;
    }

    match name.as_str() {
        // ---- destructive file ops --------------------------------------------
        "rm" => {
            let recursive = has_flag('r', "--recursive") || has_flag('R', "--recursive");
            let force = has_flag('f', "--force");
            let targets: Vec<&str> = args.iter().copied().filter(|a| !a.starts_with('-')).collect();
            let scary_target = targets.iter().any(|t| matches!(*t, "/" | "/*" | "~" | "~/" | "*" | "." | ".." | "$HOME" | "/usr" | "/etc" | "/var" | "/home"));
            if scary_target {
                ctx.bump(Risk::Destructive, format!("rm targets `{}`", targets.join(" ")));
            } else if recursive {
                ctx.bump(Risk::Destructive, if force { "rm -rf: recursive, forced, no trash" } else { "rm -r: recursive delete" });
            } else {
                ctx.bump(Risk::Mutating, "deletes files (not recursive)");
            }
        }
        "dd" | "mkfs" | "fdisk" | "parted" | "wipefs" | "shred" => {
            ctx.bump(Risk::Destructive, format!("{name} writes raw disk/partition data"));
        }
        "chmod" | "chown" | "chgrp" => {
            let recursive = has_flag('R', "--recursive");
            let scary = args.iter().any(|a| matches!(*a, "/" | "/*" | "777" | "-R"));
            if recursive && scary { ctx.bump(Risk::Destructive, format!("{name} -R on a broad target")); }
            else { ctx.bump(Risk::Mutating, format!("{name} changes permissions/ownership")); }
        }
        "sed" | "perl" if has_flag('i', "--in-place") => {
            ctx.bump(Risk::Mutating, format!("{name} -i edits files in place"));
        }
        "mv" | "cp" | "ln" | "mkdir" | "touch" | "tee" | "truncate" | "tar" | "unzip" | "zip" | "gzip" | "gunzip" | "install" | "rmdir" => {
            ctx.bump(Risk::Mutating, format!("{name} changes files"));
        }
        "kill" | "killall" | "pkill" => {
            if args.iter().any(|a| *a == "-9" || *a == "-KILL" || *a == "1") { ctx.bump(Risk::Privileged, "force-kills processes"); }
            else { ctx.bump(Risk::Mutating, "signals processes"); }
        }
        "shutdown" | "reboot" | "halt" | "poweroff" | "init" | "systemctl" if name != "systemctl" || args.iter().any(|a| matches!(*a, "stop" | "restart" | "disable" | "mask" | "poweroff" | "reboot")) => {
            ctx.bump(Risk::Privileged, format!("{name} changes system/service state"));
        }
        "systemctl" | "service" | "launchctl" => {
            if !args.iter().any(|a| matches!(*a, "status" | "list-units" | "is-active" | "show" | "cat")) {
                ctx.bump(Risk::Privileged, format!("{name} changes service state"));
            }
        }
        "apt" | "apt-get" | "yum" | "dnf" | "pacman" | "brew" | "snap" => {
            if args.iter().any(|a| matches!(*a, "install" | "remove" | "purge" | "upgrade" | "autoremove" | "uninstall" | "-S" | "-R" | "-Rns" | "-Syu")) {
                ctx.bump(Risk::Privileged, format!("{name} installs/removes system packages"));
            }
        }
        "chroot" | "mount" | "umount" | "modprobe" | "insmod" | "sysctl" | "iptables" | "nft" | "ufw" | "setenforce" | "crontab" => {
            ctx.bump(Risk::Privileged, format!("{name} changes system configuration"));
        }

        // ---- git ------------------------------------------------------------------
        "git" => {
            let sub = args.first().copied().unwrap_or("");
            match sub {
                "push" => {
                    if arg_str.contains("--force") || arg_str.contains(" -f") || args.contains(&"-f") || arg_str.contains("+") {
                        ctx.bump(Risk::Destructive, "git push --force rewrites remote history");
                    } else if arg_str.contains("--delete") || args.iter().any(|a| a.starts_with(':')) {
                        ctx.bump(Risk::Destructive, "git push deletes a remote branch");
                    } else {
                        ctx.bump(Risk::Remote, "git push publishes commits");
                    }
                }
                "reset" if arg_str.contains("--hard") => ctx.bump(Risk::Destructive, "git reset --hard discards uncommitted work"),
                "clean" if has_flag('f', "--force") => ctx.bump(Risk::Destructive, "git clean -f deletes untracked files"),
                "checkout" if args.iter().any(|a| *a == "--" || *a == ".") => ctx.bump(Risk::Destructive, "git checkout -- discards local changes"),
                "restore" if !arg_str.contains("--staged") => ctx.bump(Risk::Destructive, "git restore discards local changes"),
                "branch" if has_flag('D', "--delete") || arg_str.contains("--delete --force") => ctx.bump(Risk::Destructive, "git branch -D force-deletes a branch"),
                "stash" if args.get(1).is_some_and(|a| *a == "drop" || *a == "clear") => ctx.bump(Risk::Destructive, "git stash drop/clear loses stashed work"),
                "rebase" | "commit" if arg_str.contains("--amend") || sub == "rebase" => ctx.bump(Risk::Mutating, format!("git {sub} rewrites local history")),
                "add" | "commit" | "stash" | "merge" | "cherry-pick" | "switch" | "checkout" | "tag" | "init" | "apply" | "rm" | "mv" | "worktree" => ctx.bump(Risk::Mutating, format!("git {sub} changes the repo")),
                "pull" | "clone" | "fetch" | "remote" | "submodule" => ctx.bump(Risk::Remote, format!("git {sub} talks to a remote")),
                _ => {}
            }
        }

        // ---- containers / infra ----------------------------------------------------
        "docker" | "podman" | "docker-compose" => {
            let sub = args.first().copied().unwrap_or("");
            match sub {
                "system" if args.get(1) == Some(&"prune") => ctx.bump(Risk::Destructive, "docker system prune deletes images/containers/volumes"),
                "rm" | "rmi" | "volume" | "prune" | "network" if arg_str.contains("rm") || arg_str.contains("prune") => ctx.bump(Risk::Destructive, format!("{name} {sub} deletes container state")),
                "run" | "exec" | "build" | "start" | "stop" | "restart" | "kill" | "up" | "down" | "compose" | "pull" | "push" | "tag" | "cp" | "create" | "compose up" => ctx.bump(if matches!(sub, "push" | "pull") { Risk::Remote } else { Risk::Mutating }, format!("{name} {sub}")),
                _ => {}
            }
        }
        "kubectl" | "helm" | "k" => {
            let sub = args.first().copied().unwrap_or("");
            match sub {
                "delete" | "uninstall" => ctx.bump(Risk::Destructive, format!("{name} {sub} removes cluster resources")),
                "apply" | "create" | "patch" | "edit" | "scale" | "rollout" | "install" | "upgrade" | "set" | "label" | "annotate" | "drain" | "cordon" | "taint" | "exec" | "port-forward" | "run" => ctx.bump(Risk::Remote, format!("{name} {sub} changes the cluster")),
                _ => {}
            }
            if arg_str.contains("prod") && !matches!(sub, "get" | "describe" | "logs" | "top" | "explain" | "version" | "config" | "api-resources" | "status" | "list" | "history") {
                ctx.bump(Risk::Privileged, "writes to a production context");
            }
        }
        "terraform" | "tofu" | "pulumi" => {
            let sub = args.first().copied().unwrap_or("");
            match sub {
                "destroy" => ctx.bump(Risk::Destructive, format!("{name} destroy tears down infrastructure")),
                "apply" | "import" | "taint" | "up" => {
                    if arg_str.contains("-auto-approve") || arg_str.contains("--yes") { ctx.bump(Risk::Destructive, format!("{name} {sub} with auto-approve — no review step")); }
                    else { ctx.bump(Risk::Remote, format!("{name} {sub} changes infrastructure")); }
                }
                _ => {}
            }
        }
        "aws" | "gcloud" | "az" => {
            if args.iter().any(|a| matches!(*a, "rm" | "rb" | "delete" | "terminate-instances" | "delete-bucket" | "delete-stack" | "delete-db-instance")) {
                ctx.bump(Risk::Destructive, format!("{name} deletes cloud resources"));
            } else if args.iter().any(|a| matches!(*a, "cp" | "sync" | "mv" | "put-object" | "create" | "run-instances" | "deploy" | "update")) {
                ctx.bump(Risk::Remote, format!("{name} changes cloud resources"));
            }
            if arg_str.contains("--recursive") && arg_str.contains(" rm") { ctx.bump(Risk::Destructive, "recursive cloud delete"); }
        }

        // ---- package managers (local) ---------------------------------------------
        "npm" | "pnpm" | "yarn" | "pip" | "pip3" | "cargo" | "gem" | "composer" | "uv" | "poetry" | "bun" => {
            if args.iter().any(|a| matches!(*a, "publish")) { ctx.bump(Risk::Remote, format!("{name} publish uploads a package")); }
            else if args.iter().any(|a| matches!(*a, "install" | "i" | "add" | "uninstall" | "remove" | "rm" | "update" | "upgrade" | "ci" | "sync")) { ctx.bump(Risk::Mutating, format!("{name} changes installed packages")); }
            if has_sudo && matches!(name.as_str(), "pip" | "pip3" | "npm") { ctx.bump(Risk::Privileged, format!("sudo {name} pollutes system packages")); }
        }

        // ---- databases ----------------------------------------------------------------
        "psql" | "mysql" | "sqlite3" | "mongo" | "mongosh" | "redis-cli" => {
            let up = arg_str.to_ascii_uppercase();
            if ["DROP ", "TRUNCATE ", "DELETE FROM", "FLUSHALL", "FLUSHDB"].iter().any(|k| up.contains(k)) {
                ctx.bump(Risk::Destructive, format!("{name} runs a destructive SQL/command"));
            } else if ["INSERT ", "UPDATE ", "ALTER ", "CREATE "].iter().any(|k| up.contains(k)) {
                ctx.bump(Risk::Mutating, format!("{name} writes to the database"));
            }
        }

        // ---- network -------------------------------------------------------------------
        "curl" | "wget" | "http" | "httpie" => {
            let writes = args.iter().any(|a| matches!(*a, "-X" | "--request" | "-d" | "--data" | "--data-raw" | "--data-binary" | "-F" | "--form" | "-T" | "--upload-file" | "--json"))
                || args.iter().any(|a| a.starts_with("-X") && a.len() > 2);
            let post_like = arg_str.contains("POST") || arg_str.contains("PUT") || arg_str.contains("DELETE") || arg_str.contains("PATCH");
            if writes || post_like { ctx.bump(Risk::Remote, format!("{name} sends data to a server")); }
            if args.iter().any(|a| matches!(*a, "-o" | "-O" | "--output")) { ctx.bump(Risk::Mutating, format!("{name} writes a downloaded file")); }
        }
        "ssh" | "scp" | "rsync" | "sftp" => {
            if name == "rsync" && arg_str.contains("--delete") { ctx.bump(Risk::Destructive, "rsync --delete removes files at the destination"); }
            else { ctx.bump(Risk::Remote, format!("{name} connects to a remote host")); }
        }
        "gh" | "glab" => {
            if args.iter().any(|a| matches!(*a, "delete" | "close" | "merge")) { ctx.bump(Risk::Remote, format!("{name} changes remote GitHub state")); }
            else if args.iter().any(|a| matches!(*a, "create" | "edit" | "comment" | "release")) { ctx.bump(Risk::Remote, format!("{name} writes to GitHub")); }
        }

        // ---- interpreters with inline code: opaque, assume mutating ------------------
        "python" | "python3" | "node" | "ruby" | "perl" | "sh" | "bash" | "zsh" | "eval" | "source" | "." => {
            if args.iter().any(|a| matches!(*a, "-c" | "-e" | "-r")) { ctx.bump(Risk::Mutating, format!("{name} runs inline code (unknown effects)")); }
            else if name == "eval" || name == "source" || name == "." { ctx.bump(Risk::Mutating, format!("{name} executes arbitrary shell code")); }
        }
        "chattr" | "setfacl" | "passwd" | "useradd" | "userdel" | "usermod" | "visudo" => {
            ctx.bump(Risk::Privileged, format!("{name} changes accounts/security settings"));
        }
        "make" | "just" | "task" | "rake" | "gradle" | "gradlew" | "mvn" => {
            // Build tools run arbitrary recipes. Common read-only-ish targets stay green.
            let target = args.iter().copied().find(|a| !a.starts_with('-')).unwrap_or("");
            if matches!(target, "test" | "check" | "lint" | "fmt" | "build" | "" | "help" | "list" | "--list") {
                // treat as build-ish: mutating (writes artifacts) but not scary
                ctx.bump(Risk::Mutating, format!("{name} {target}: build tool writes artifacts"));
            } else {
                ctx.bump(Risk::Mutating, format!("{name} {target} runs an arbitrary recipe"));
            }
        }
        "export" | "unset" | "alias" | "unalias" | "set" | "cd" | "pushd" | "popd" => {}
        _ => {
            // Unknown or read-only. Known read-only list stays ReadOnly; unknown gets Mutating
            // ONLY if it has a flag that smells destructive.
            let known_ro = READ_ONLY.contains(&name.as_str()) || READ_ONLY_SUB.iter().any(|(n, subs)| *n == name && (subs.is_empty() || subs.iter().any(|s| arg_str.starts_with(s))));
            if !known_ro && !name.is_empty() {
                if flags.iter().any(|f| matches!(*f, "--delete" | "--purge" | "--hard" | "--no-preserve-root")) {
                    ctx.bump(Risk::Destructive, format!("{name} with a delete/purge flag"));
                } else if REMOTE_CMDS.contains(&name.as_str()) {
                    ctx.bump(Risk::Remote, format!("{name} uses the network"));
                } else if DESTRUCTIVE_CMDS.contains(&name.as_str()) {
                    ctx.bump(Risk::Destructive, format!("{name} is inherently destructive"));
                } else if flags.iter().any(|f| matches!(*f, "--force" | "-f")) {
                    ctx.bump(Risk::Mutating, format!("`{name}` with a force flag (unknown tool)"));
                } else {
                    // Unknown program: be honest that we don't know.
                    ctx.bump(Risk::Mutating, format!("`{name}` is not in the read-only allowlist"));
                }
            }
        }
    }
    let _ = &mut ctx.in_pipeline_from_net;
}

fn has_truncating_redirect(root: Node, src: &[u8]) -> bool {
    let mut stack = vec![root];
    while let Some(n) = stack.pop() {
        if n.kind() == "file_redirect" {
            let t = n.utf8_text(src).unwrap_or("");
            let t = t.trim_start();
            // `>` but not `>>` and not `2>/dev/null`, `&>/dev/null`
            let to_devnull = t.contains("/dev/null");
            if !to_devnull && t.starts_with('>') && !t.starts_with(">>") {
                return true;
            }
        }
        let mut c = n.walk();
        for ch in n.children(&mut c) {
            stack.push(ch);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> Risk {
        assess(s).risk
    }

    #[test]
    fn read_only() {
        for s in ["ls -la", "git status", "cat foo.txt | grep bar", "cargo test", "kubectl get pods -n prod", "echo \"never run rm -rf /\"", "docker ps -a", "find . -name '*.rs'", "grep -rn token src/"] {
            assert_eq!(r(s), Risk::ReadOnly, "{s}");
        }
    }

    #[test]
    fn mutating() {
        for s in ["mkdir -p build", "git add -A && git commit -m x", "npm install lodash", "echo hi > out.txt", "sed -i 's/a/b/' f", "make clean"] {
            assert!(r(s) >= Risk::Mutating && r(s) < Risk::Destructive, "{s} -> {:?}", assess(s));
        }
    }

    #[test]
    fn remote() {
        for s in ["git push origin main", "curl -X POST https://api.x/y -d '{}'", "kubectl apply -f k.yaml", "ssh prod-1", "terraform apply"] {
            assert_eq!(r(s), Risk::Remote, "{s} -> {:?}", assess(s));
        }
    }

    #[test]
    fn privileged() {
        for s in ["sudo apt-get install jq", "sudo systemctl restart nginx", "sudo -u deploy ls"] {
            assert_eq!(r(s), Risk::Privileged, "{s} -> {:?}", assess(s));
        }
    }

    #[test]
    fn destructive() {
        for s in ["rm -rf build", "sudo rm -rf /", "git push --force origin main", "git reset --hard HEAD~3",
                  "curl -fsSL https://x.sh | sh", "make clean && rm -rf dist", "docker system prune -a",
                  "kubectl delete deployment web", "terraform destroy", "psql -c 'DROP TABLE users'",
                  "dd if=/dev/zero of=/dev/sda", "rsync -av --delete a/ b/", "git clean -fdx"] {
            assert_eq!(r(s), Risk::Destructive, "{s} -> {:?}", assess(s));
        }
    }

    #[test]
    fn payload_commands_are_inspected() {
        assert_eq!(r("find . -name node_modules -type d -prune -exec rm -rf {} +"), Risk::Destructive);
        assert_eq!(r("find . -name '*.log' -delete"), Risk::Destructive);
        assert_eq!(r("find . -type f -exec chmod 777 {} +"), Risk::Mutating);
        assert_eq!(r("find . -name '*.rs' -exec wc -l {} +"), Risk::ReadOnly);
        assert_eq!(r("ls *.tmp | xargs rm -rf"), Risk::Destructive);
        assert_eq!(r("cat urls | xargs -n1 curl -O"), Risk::Mutating);
    }

    #[test]
    fn worst_command_in_chain_wins() {
        let a = assess("ls && rm -rf node_modules");
        assert_eq!(a.risk, Risk::Destructive);
        assert!(a.reasons.iter().any(|x| x.contains("rm -rf")));
    }
}
