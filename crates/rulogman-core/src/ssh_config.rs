//! Reading OpenSSH's own client configuration into profiles.
//!
//! A one-way, one-time import: `~/.ssh/config` is read, never written, and the
//! profiles that come out of it are ordinary profiles from then on — nothing
//! ties them back to the file, so editing either afterwards leaves the other
//! alone.
//!
//! Only what a profile can hold is read, and only the way OpenSSH reads it:
//!
//! * every `Host` line names patterns, and each **concrete** one — no `*`, no
//!   `?`, no leading `!` — becomes a profile named after it;
//! * a profile's options are collected from every `Host` block whose patterns
//!   match its name, top to bottom, the **first value winning** — so a
//!   `Host *` at the end supplies defaults and one at the top overrides —
//!   except `IdentityFile`, `LocalForward` and `DynamicForward`, which add up
//!   as they do in OpenSSH;
//! * `Include` is followed — globs, `~`, and paths relative to `~/.ssh` — to
//!   the depth OpenSSH allows;
//! * `Match` blocks are skipped, since their conditions are about the moment
//!   of connecting, and counted so the user can be told.
//!
//! The options used are `HostName`, `User`, `Port`, `IdentityFile`,
//! `ProxyJump`, `LocalForward` and `DynamicForward`; a `ProxyCommand` is
//! noted, since it cannot be carried over. `IdentityFile` and `HostName` expand
//! the tokens that mean the same thing outside a live connection — `%%`, `%d`,
//! `%h`, `%n`, `%p`, `%r`, `%u` and `~` — and an `IdentityFile` using any other
//! is passed over with a note rather than guessed at.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::profile::{AuthMethod, HopRule, SessionProfile, TunnelRule};

/// Folder the imported profiles are filed under.
///
/// The file's own name rather than a translated word: it names where the
/// profiles came from, which reads the same in every language, and it is data
/// that stays as it was written whatever language the interface later uses.
pub const IMPORT_FOLDER: &str = "SSH config";

/// How deep `Include` may nest, OpenSSH's own `READCONF_MAX_DEPTH`.
const MAX_INCLUDE_DEPTH: usize = 16;

/// How many jump hosts a chain may resolve to before it is taken for a loop.
const MAX_JUMP_HOPS: usize = 16;

/// The SSH port, for a `Port` that says nothing usable.
const DEFAULT_PORT: u16 = 22;

/// What the import needs to know about this machine, passed in so that the
/// reading itself can be tested without one.
#[derive(Debug, Clone)]
pub struct ImportContext {
    /// The home directory: what `~` and `%d` mean, and where `~/.ssh` is.
    pub home: PathBuf,
    /// The local login name: what `%u` means, and the user of a host whose
    /// configuration names none, as it is for `ssh`.
    pub local_user: String,
    /// Whether an SSH agent is reachable, which decides how a host without an
    /// `IdentityFile` authenticates: with the agent, or with a password.
    pub agent_available: bool,
}

/// Where `~/.ssh/config` is on this machine, or `None` with no home directory.
///
/// `%USERPROFILE%\.ssh\config` on Windows, which is where Windows' own
/// OpenSSH client reads it.
pub fn default_config_path() -> Option<PathBuf> {
    let dirs = directories::BaseDirs::new()?;
    Some(dirs.home_dir().join(".ssh").join("config"))
}

/// Something the import could not carry over for one host, for the preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportNote {
    /// The host is reached through a `ProxyCommand`, which a profile cannot
    /// hold; the profile connects directly.
    ProxyCommand,
    /// An `IdentityFile` uses a token only a live connection can fill in, and
    /// was passed over.
    UnexpandedToken(String),
    /// An `IdentityFile` names a key that is not there, and was passed over.
    MissingKey(PathBuf),
    /// A `Port` that is not a port number; 22 is used.
    BadPort(String),
    /// A `LocalForward` or `DynamicForward` a profile cannot hold — a Unix
    /// socket, or one that does not parse — given as written.
    UnsupportedForward(String),
    /// A `ProxyJump` chain that loops back on itself or runs too long; the
    /// hops resolved before it went wrong are kept.
    JumpLoop,
}

/// One host read from the configuration, as the profile it would become.
#[derive(Debug, Clone)]
pub struct ImportedHost {
    /// The profile, with a fresh id, filed under [`IMPORT_FOLDER`], holding no
    /// secret. Not saved anywhere yet.
    pub profile: SessionProfile,
    /// What could not be carried over, in the order it was met.
    pub notes: Vec<ImportNote>,
}

/// What reading a configuration found.
#[derive(Debug, Clone, Default)]
pub struct SshConfigImport {
    /// One entry per concrete host alias, in the order the file names them.
    pub hosts: Vec<ImportedHost>,
    /// How many `Match` blocks were skipped.
    pub match_blocks: usize,
    /// Included files that exist but could not be read.
    pub unreadable: Vec<PathBuf>,
}

/// Reads the configuration at `path` and everything it includes.
///
/// # Errors
///
/// Fails only when `path` itself cannot be read — a missing file included,
/// which the caller can tell apart by [`io::ErrorKind::NotFound`]. A file it
/// includes that cannot be read is listed in
/// [`SshConfigImport::unreadable`] instead, the way `ssh` carries on past one.
pub fn read_config(path: &Path, context: &ImportContext) -> io::Result<SshConfigImport> {
    let text = fs::read(path)?;
    let mut parser = Parser {
        context,
        blocks: vec![Block::global()],
        unreadable: Vec::new(),
    };
    parser.parse(&String::from_utf8_lossy(&text), 0);
    Ok(parser.finish())
}

/// One `Host` or `Match` block, or the options before the first of either.
#[derive(Debug, Clone)]
struct Block {
    /// The `Host` patterns; `["*"]` for the options before any block.
    patterns: Vec<String>,
    /// Whether this is a `Match` block, which is skipped.
    is_match: bool,
    /// `(keyword in lower case, arguments)`, in file order.
    options: Vec<(String, Vec<String>)>,
}

impl Block {
    /// The block lines before any `Host` or `Match` belong to, which applies
    /// to every host.
    fn global() -> Self {
        Self {
            patterns: vec!["*".to_owned()],
            is_match: false,
            options: Vec::new(),
        }
    }

    /// Whether this block's patterns select `host`.
    ///
    /// Any positive pattern matching, and no negated one: OpenSSH's rule, so
    /// that `Host * !bastion` means "everything but the bastion".
    fn matches(&self, host: &str) -> bool {
        if self.is_match {
            return false;
        }
        let host = host.to_lowercase();
        let mut matched = false;
        for pattern in &self.patterns {
            if let Some(negated) = pattern.strip_prefix('!') {
                if wildcard_match(&negated.to_lowercase(), &host) {
                    return false;
                }
            } else if wildcard_match(&pattern.to_lowercase(), &host) {
                matched = true;
            }
        }
        matched
    }
}

/// The options that apply to one host, gathered from every block selecting it.
#[derive(Debug, Default)]
struct HostOptions {
    /// Single-valued options, first value kept: `(keyword, first argument)`.
    single: Vec<(String, Vec<String>)>,
    /// Every `IdentityFile`, in order.
    identity_files: Vec<String>,
    /// Every `LocalForward`'s arguments, in order.
    local_forwards: Vec<Vec<String>>,
    /// Every `DynamicForward`'s argument, in order.
    dynamic_forwards: Vec<String>,
}

impl HostOptions {
    /// The first value given for `keyword` (lower case), if any.
    fn get(&self, keyword: &str) -> Option<&str> {
        self.single
            .iter()
            .find(|(name, _)| name == keyword)
            .and_then(|(_, args)| args.first())
            .map(String::as_str)
    }
}

/// Reads files into blocks.
struct Parser<'a> {
    context: &'a ImportContext,
    blocks: Vec<Block>,
    unreadable: Vec<PathBuf>,
}

impl Parser<'_> {
    /// Parses one file's text, `depth` includes deep, into the blocks.
    fn parse(&mut self, text: &str, depth: usize) {
        for line in text.lines() {
            let Some((keyword, args)) = split_line(line) else {
                continue;
            };
            match keyword.as_str() {
                "host" => self.blocks.push(Block {
                    patterns: args,
                    is_match: false,
                    options: Vec::new(),
                }),
                "match" => self.blocks.push(Block {
                    patterns: Vec::new(),
                    is_match: true,
                    options: Vec::new(),
                }),
                "include" => self.include(&args, depth),
                _ => {
                    if let Some(block) = self.blocks.last_mut() {
                        block.options.push((keyword, args));
                    }
                }
            }
        }
    }

    /// Follows an `Include` line.
    ///
    /// An included file starts in the block the `Include` stood in, and a
    /// `Host` inside it ends that block only for the rest of the included
    /// file: once it is done, the lines after the `Include` belong to the
    /// block they were written in, as they do for `ssh`.
    fn include(&mut self, args: &[String], depth: usize) {
        if depth >= MAX_INCLUDE_DEPTH {
            return;
        }
        let ssh_dir = self.context.home.join(".ssh");
        for arg in args {
            let pattern = expand_home(arg, &self.context.home);
            let pattern = if pattern.is_absolute() {
                pattern
            } else {
                ssh_dir.join(pattern)
            };
            for file in glob(&pattern) {
                let outer = self.blocks.len();
                let header = self
                    .blocks
                    .last()
                    .map(|block| (block.patterns.clone(), block.is_match));
                match fs::read(&file) {
                    Ok(text) => self.parse(&String::from_utf8_lossy(&text), depth + 1),
                    Err(_) => {
                        self.unreadable.push(file);
                        continue;
                    }
                }
                if self.blocks.len() != outer
                    && let Some((patterns, is_match)) = header
                {
                    self.blocks.push(Block {
                        patterns,
                        is_match,
                        options: Vec::new(),
                    });
                }
            }
        }
    }

    /// Turns the blocks into one profile per concrete alias.
    fn finish(self) -> SshConfigImport {
        let match_blocks = self.blocks.iter().filter(|block| block.is_match).count();
        let mut seen = HashSet::new();
        let mut hosts = Vec::new();
        for block in self.blocks.iter().filter(|block| !block.is_match) {
            for pattern in &block.patterns {
                if pattern.starts_with('!') || pattern.contains(['*', '?']) {
                    continue;
                }
                if seen.insert(pattern.clone()) {
                    hosts.push(self.host(pattern));
                }
            }
        }
        SshConfigImport {
            hosts,
            match_blocks,
            unreadable: self.unreadable,
        }
    }

    /// Everything that applies to `host`, first value winning.
    fn options_for(&self, host: &str) -> HostOptions {
        let mut options = HostOptions::default();
        for block in self.blocks.iter().filter(|block| block.matches(host)) {
            for (keyword, args) in &block.options {
                match keyword.as_str() {
                    "identityfile" => options.identity_files.extend(args.first().cloned()),
                    "localforward" => options.local_forwards.push(args.clone()),
                    "dynamicforward" => options.dynamic_forwards.extend(args.first().cloned()),
                    _ => {
                        if options.get(keyword).is_none() && !args.is_empty() {
                            options.single.push((keyword.clone(), args.clone()));
                        }
                    }
                }
            }
        }
        options
    }

    /// The profile `alias` becomes.
    fn host(&self, alias: &str) -> ImportedHost {
        let mut notes = Vec::new();
        let options = self.options_for(alias);
        let target = self.endpoint(alias, &options, None, None, &mut notes);

        let mut profile =
            SessionProfile::new(alias, target.host, target.port, target.user, target.auth);
        profile.folder = Some(IMPORT_FOLDER.to_owned());

        match options.get("proxyjump") {
            Some(jump) if !jump.eq_ignore_ascii_case("none") => {
                let mut visited = vec![alias.to_lowercase()];
                profile.hops = self.jumps(jump, &mut visited, &mut notes);
            }
            Some(_) => {}
            None => {
                if options
                    .get("proxycommand")
                    .is_some_and(|command| !command.eq_ignore_ascii_case("none"))
                {
                    notes.push(ImportNote::ProxyCommand);
                }
            }
        }

        for args in &options.local_forwards {
            match local_forward(args) {
                Some(rule) => profile.tunnels.push(rule),
                None => notes.push(ImportNote::UnsupportedForward(format!(
                    "LocalForward {}",
                    args.join(" ")
                ))),
            }
        }
        for arg in &options.dynamic_forwards {
            match dynamic_forward(arg) {
                Some(rule) => profile.tunnels.push(rule),
                None => notes.push(ImportNote::UnsupportedForward(format!(
                    "DynamicForward {arg}"
                ))),
            }
        }

        ImportedHost { profile, notes }
    }

    /// The hops a `ProxyJump` value stands for, each resolved through the
    /// configuration under the name it is written as — its own `ProxyJump`
    /// first, as `ssh` reaches a jump host through its own jumps.
    fn jumps(
        &self,
        value: &str,
        visited: &mut Vec<String>,
        notes: &mut Vec<ImportNote>,
    ) -> Vec<HopRule> {
        let mut hops = Vec::new();
        for spec in value
            .split(',')
            .map(str::trim)
            .filter(|spec| !spec.is_empty())
        {
            let (user, host, port) = parse_jump(spec);
            let key = host.to_lowercase();
            if visited.contains(&key) || visited.len() > MAX_JUMP_HOPS {
                if !notes.contains(&ImportNote::JumpLoop) {
                    notes.push(ImportNote::JumpLoop);
                }
                return hops;
            }
            visited.push(key);
            let options = self.options_for(host);
            if let Some(inner) = options.get("proxyjump")
                && !inner.eq_ignore_ascii_case("none")
            {
                hops.extend(self.jumps(inner, visited, notes));
            }
            let end = self.endpoint(host, &options, user, port, notes);
            hops.push(HopRule {
                id: uuid::Uuid::new_v4(),
                host: end.host,
                port: end.port,
                username: end.user,
                auth: end.auth,
                save_secret: false,
            });
            visited.pop();
        }
        hops
    }

    /// Where `name` is reached and as whom, from its options, with a user and
    /// port written into a jump specification taking precedence.
    fn endpoint(
        &self,
        name: &str,
        options: &HostOptions,
        user: Option<&str>,
        port: Option<&str>,
        notes: &mut Vec<ImportNote>,
    ) -> Endpoint {
        let port_text = port.or_else(|| options.get("port"));
        let port = match port_text {
            None => DEFAULT_PORT,
            Some(text) => match text.parse::<u16>() {
                Ok(port) if port != 0 => port,
                _ => {
                    notes.push(ImportNote::BadPort(text.to_owned()));
                    DEFAULT_PORT
                }
            },
        };
        let user = user
            .or_else(|| options.get("user"))
            .unwrap_or(&self.context.local_user)
            .to_owned();
        let host = options
            .get("hostname")
            .and_then(|value| self.expand(value, name, name, port, &user))
            .unwrap_or_else(|| name.to_owned());

        let mut auth = None;
        for file in &options.identity_files {
            if file.eq_ignore_ascii_case("none") {
                continue;
            }
            let Some(expanded) = self.expand(file, name, &host, port, &user) else {
                notes.push(ImportNote::UnexpandedToken(file.clone()));
                continue;
            };
            let path = expand_home(&expanded, &self.context.home);
            let path = if path.is_absolute() {
                path
            } else {
                self.context.home.join(path)
            };
            if path.is_file() {
                auth = Some(AuthMethod::PublicKey { key_path: path });
                break;
            }
            notes.push(ImportNote::MissingKey(path));
        }
        let auth = auth.unwrap_or(if self.context.agent_available {
            AuthMethod::Agent
        } else {
            AuthMethod::Password
        });
        Endpoint {
            host,
            port,
            user,
            auth,
        }
    }

    /// `value` with its `%` tokens filled in, or `None` when it uses one only a
    /// live connection knows.
    fn expand(
        &self,
        value: &str,
        alias: &str,
        host: &str,
        port: u16,
        user: &str,
    ) -> Option<String> {
        let mut out = String::with_capacity(value.len());
        let mut chars = value.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            match chars.next()? {
                '%' => out.push('%'),
                'd' => out.push_str(&self.context.home.to_string_lossy()),
                'h' => out.push_str(host),
                'n' => out.push_str(alias),
                'p' => out.push_str(&port.to_string()),
                'r' => out.push_str(user),
                'u' => out.push_str(&self.context.local_user),
                _ => return None,
            }
        }
        Some(out)
    }
}

/// A resolved host: address, port, login and how it authenticates.
struct Endpoint {
    host: String,
    port: u16,
    user: String,
    auth: AuthMethod,
}

/// A configuration line as `(keyword in lower case, arguments)`, or `None` for
/// a blank line or a comment.
///
/// `Keyword value`, `Keyword=value` and `Keyword = value` are all one form;
/// double quotes keep white space inside an argument, and an unquoted argument
/// starting with `#` ends the line.
fn split_line(line: &str) -> Option<(String, Vec<String>)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let keyword = line[..end].to_lowercase();
    let mut rest = line[end..].trim_start();
    if let Some(after) = rest.strip_prefix('=') {
        rest = after.trim_start();
    }

    let mut args = Vec::new();
    let mut chars = rest.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(&first) = chars.peek() else {
            break;
        };
        if first == '#' {
            break;
        }
        let mut arg = String::new();
        if first == '"' {
            chars.next();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                arg.push(c);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                arg.push(c);
                chars.next();
            }
        }
        args.push(arg);
    }
    Some((keyword, args))
}

/// `~` or `~/…` turned into the home directory; anything else as it is.
fn expand_home(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// `[user@]host[:port]`, or the same as an `ssh://` URI, taken apart.
fn parse_jump(spec: &str) -> (Option<&str>, &str, Option<&str>) {
    let spec = spec.strip_prefix("ssh://").unwrap_or(spec);
    let (user, rest) = match spec.rsplit_once('@') {
        Some((user, rest)) => (Some(user), rest),
        None => (None, spec),
    };
    let (host, port) = split_host_port(rest);
    (user, host, port)
}

/// `host:port`, `[v6]:port`, `host` or `[v6]`, taken apart.
fn split_host_port(text: &str) -> (&str, Option<&str>) {
    if let Some(inner) = text.strip_prefix('[')
        && let Some((host, rest)) = inner.split_once(']')
    {
        return (host, rest.strip_prefix(':'));
    }
    match text.rsplit_once(':') {
        // More than one colon and no brackets is a bare IPv6 address.
        Some((host, port)) if !host.contains(':') => (host, Some(port)),
        _ => (text, None),
    }
}

/// The listener half of a forwarding: `[bind:]port`, `*` binding everywhere.
fn listen_address(text: &str) -> Option<(String, u16)> {
    if text.contains('/') {
        return None;
    }
    let (bind, port) = match split_host_port(text) {
        (port, None) => (None, port),
        (bind, Some(port)) => (Some(bind), port),
    };
    let port = port.parse::<u16>().ok().filter(|port| *port != 0)?;
    let bind = match bind {
        None | Some("") => "127.0.0.1".to_owned(),
        Some("*") => "0.0.0.0".to_owned(),
        Some(bind) => bind.to_owned(),
    };
    Some((bind, port))
}

/// A `LocalForward` line's arguments as a rule, when a profile can hold it.
fn local_forward(args: &[String]) -> Option<TunnelRule> {
    let [listen, target] = args else {
        return None;
    };
    let (bind_address, local_port) = listen_address(listen)?;
    if target.contains('/') {
        return None;
    }
    let (host, port) = split_host_port(target);
    let remote_port = port?.parse::<u16>().ok().filter(|port| *port != 0)?;
    if host.is_empty() {
        return None;
    }
    Some(TunnelRule {
        bind_address,
        ..TunnelRule::local(local_port, host, remote_port)
    })
}

/// A `DynamicForward` line's argument as a rule, when a profile can hold it.
fn dynamic_forward(arg: &str) -> Option<TunnelRule> {
    let (bind_address, local_port) = listen_address(arg)?;
    Some(TunnelRule {
        bind_address,
        ..TunnelRule::dynamic(local_port)
    })
}

/// Whether `pattern`, with `*` for any run and `?` for any one character,
/// matches the whole of `text`.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

/// The existing files a path with `*` and `?` in its components names, in
/// sorted order, the way `glob(3)` lists them for `ssh`.
///
/// A component without wildcards is taken as it is; one with them is matched
/// against the directory's entries, leaving out the hidden ones unless the
/// pattern itself starts with a dot.
fn glob(pattern: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::new()];
    for component in pattern.components() {
        let part = component.as_os_str().to_string_lossy();
        if !part.contains(['*', '?']) {
            for candidate in &mut candidates {
                candidate.push(component.as_os_str());
            }
            continue;
        }
        let mut next = Vec::new();
        for dir in &candidates {
            let Ok(entries) = fs::read_dir(dir) else {
                continue;
            };
            let mut names: Vec<String> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.') || part.starts_with('.'))
                .filter(|name| wildcard_match(&part, name))
                .collect();
            names.sort();
            next.extend(names.into_iter().map(|name| dir.join(name)));
        }
        candidates = next;
    }
    candidates.retain(|path| path.is_file());
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A home directory with an `.ssh` folder, and a context pointing at it.
    fn home() -> (tempfile::TempDir, ImportContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join(".ssh")).expect(".ssh");
        let context = ImportContext {
            home: dir.path().to_path_buf(),
            local_user: "me".to_owned(),
            agent_available: false,
        };
        (dir, context)
    }

    /// Writes `text` as `~/.ssh/<name>` and returns its path.
    fn write(context: &ImportContext, name: &str, text: &str) -> PathBuf {
        let path = context.home.join(".ssh").join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(&path, text).expect("write");
        path
    }

    fn import(context: &ImportContext, text: &str) -> SshConfigImport {
        let path = write(context, "config", text);
        read_config(&path, context).expect("read")
    }

    fn names(import: &SshConfigImport) -> Vec<&str> {
        import
            .hosts
            .iter()
            .map(|host| host.profile.name.as_str())
            .collect()
    }

    #[test]
    fn a_concrete_host_becomes_a_profile_and_patterns_do_not() {
        let (_dir, context) = home();
        let import = import(
            &context,
            "Host web1 web2 *.prod !web3\n  User deploy\nHost db\n  HostName 10.0.0.5\n  Port 2222\n",
        );
        assert_eq!(names(&import), ["web1", "web2", "db"]);
        let db = &import.hosts[2].profile;
        assert_eq!((db.host.as_str(), db.port), ("10.0.0.5", 2222));
        assert_eq!(
            db.username, "me",
            "no User means the local login, as for ssh"
        );
        assert_eq!(db.folder.as_deref(), Some(IMPORT_FOLDER));
        assert!(!db.save_secret);
        let web1 = &import.hosts[0].profile;
        assert_eq!(
            (web1.host.as_str(), web1.username.as_str()),
            ("web1", "deploy")
        );
    }

    #[test]
    fn the_first_value_wins_so_a_trailing_wildcard_is_a_default() {
        let (_dir, context) = home();
        let import = import(
            &context,
            "User top\nHost app\n  User app-user\n  Port 2200\nHost *\n  User fallback\n  Port 22\n  HostName %h.example.com\n",
        );
        let app = &import.hosts[0].profile;
        // `User top` precedes every block, so it applies first and wins.
        assert_eq!(app.username, "top");
        assert_eq!(app.port, 2200);
        assert_eq!(app.host, "app.example.com");
    }

    #[test]
    fn keywords_take_equals_signs_quotes_any_case_and_trailing_comments() {
        let (_dir, context) = home();
        let import = import(
            &context,
            "# a comment\nHOST=\"my box\"\n  hostname = box.lan # where it is\n  USER=\"root\"\n",
        );
        let profile = &import.hosts[0].profile;
        assert_eq!(profile.name, "my box");
        assert_eq!(profile.host, "box.lan");
        assert_eq!(profile.username, "root");
    }

    #[test]
    fn match_blocks_are_skipped_and_counted() {
        let (_dir, context) = home();
        let import = import(
            &context,
            "Match host foo exec \"true\"\n  User nobody\nHost foo\n  HostName foo.lan\n",
        );
        assert_eq!(import.match_blocks, 1);
        assert_eq!(import.hosts[0].profile.username, "me");
    }

    #[test]
    fn includes_follow_globs_relative_to_dot_ssh_in_sorted_order() {
        let (_dir, context) = home();
        write(&context, "conf.d/b.conf", "Host beta\n  HostName b.lan\n");
        write(&context, "conf.d/a.conf", "Host alpha\n  HostName a.lan\n");
        write(&context, "conf.d/.hidden", "Host hidden\n");
        let import = import(&context, "Include conf.d/*\nHost gamma\n");
        assert_eq!(names(&import), ["alpha", "beta", "gamma"]);
        assert_eq!(import.hosts[0].profile.host, "a.lan");
    }

    #[test]
    fn lines_after_an_include_belong_to_the_block_it_stood_in() {
        let (_dir, context) = home();
        write(&context, "extra", "Host other\n  User other-user\n");
        let import = import(
            &context,
            "Host main\n  Include ~/.ssh/extra\n  User main-user\n",
        );
        let main = &import.hosts[0].profile;
        assert_eq!(main.name, "main");
        assert_eq!(main.username, "main-user");
        assert_eq!(import.hosts[1].profile.username, "other-user");
    }

    #[test]
    fn an_include_that_includes_itself_stops_at_the_depth_limit() {
        let (_dir, context) = home();
        let import = import(&context, "Include config\nHost loop\n");
        assert_eq!(names(&import), ["loop"]);
    }

    #[test]
    fn identity_files_expand_tokens_and_fall_through_to_one_that_exists() {
        let (_dir, mut context) = home();
        write(&context, "id_box_deploy", "key");
        context.agent_available = true;
        let import = import(
            &context,
            "Host box\n  User deploy\n  IdentityFile ~/.ssh/missing\n  IdentityFile %d/.ssh/id_%n_%r\n  IdentityFile ~/.ssh/id_%C\n",
        );
        let host = &import.hosts[0];
        assert_eq!(
            host.profile.auth,
            AuthMethod::PublicKey {
                key_path: context.home.join(".ssh/id_box_deploy"),
            }
        );
        assert_eq!(
            host.notes,
            [ImportNote::MissingKey(context.home.join(".ssh/missing"))]
        );
    }

    #[test]
    fn without_a_usable_key_the_agent_or_a_password_is_used() {
        let (_dir, mut context) = home();
        let text = "Host a\n  IdentityFile ~/.ssh/id_%C\n";
        let import_without = import(&context, text);
        assert_eq!(import_without.hosts[0].profile.auth, AuthMethod::Password);
        assert_eq!(
            import_without.hosts[0].notes,
            [ImportNote::UnexpandedToken("~/.ssh/id_%C".to_owned())]
        );
        context.agent_available = true;
        assert_eq!(
            import(&context, text).hosts[0].profile.auth,
            AuthMethod::Agent
        );
    }

    #[test]
    fn proxy_jump_resolves_aliases_and_their_own_jumps() {
        let (_dir, context) = home();
        let import = import(
            &context,
            "Host outer\n  HostName outer.example.com\n  User gate\nHost inner\n  HostName 10.1.0.1\n  ProxyJump outer\nHost target\n  HostName 10.2.0.9\n  ProxyJump admin@inner:2022,raw.example.net\n",
        );
        let target = import
            .hosts
            .iter()
            .find(|host| host.profile.name == "target")
            .expect("target");
        let hops: Vec<(&str, u16, &str)> = target
            .profile
            .hops
            .iter()
            .map(|hop| (hop.host.as_str(), hop.port, hop.username.as_str()))
            .collect();
        assert_eq!(
            hops,
            [
                ("outer.example.com", 22, "gate"),
                ("10.1.0.1", 2022, "admin"),
                ("raw.example.net", 22, "me"),
            ]
        );
        assert!(target.notes.is_empty(), "{:?}", target.notes);
        let ids: HashSet<_> = target.profile.hops.iter().map(|hop| hop.id).collect();
        assert_eq!(ids.len(), 3, "every hop needs its own keychain id");
    }

    #[test]
    fn a_jump_loop_is_cut_and_noted() {
        let (_dir, context) = home();
        let import = import(&context, "Host a\n  ProxyJump b\nHost b\n  ProxyJump a\n");
        let a = &import.hosts[0];
        assert_eq!(a.profile.hops.len(), 1);
        assert_eq!(a.profile.hops[0].host, "b");
        assert_eq!(a.notes, [ImportNote::JumpLoop]);
    }

    #[test]
    fn a_proxy_command_is_noted_unless_proxy_jump_came_first() {
        let (_dir, context) = home();
        let import = import(
            &context,
            "Host a\n  ProxyCommand nc %h %p\nHost b\n  ProxyJump a\n  ProxyCommand nc %h %p\nHost c\n  ProxyCommand none\n",
        );
        assert_eq!(import.hosts[0].notes, [ImportNote::ProxyCommand]);
        assert!(import.hosts[1].notes.is_empty());
        assert!(import.hosts[2].notes.is_empty());
    }

    #[test]
    fn forwards_become_rules_and_sockets_are_noted() {
        let (_dir, context) = home();
        let import = import(
            &context,
            "Host f\n  LocalForward 15432 db.internal:5432\n  LocalForward *:8080 [::1]:80\n  DynamicForward 1080\n  LocalForward /tmp/sock /run/x.sock\n",
        );
        let host = &import.hosts[0];
        assert_eq!(
            host.profile.tunnels,
            [
                TunnelRule::local(15432, "db.internal", 5432),
                TunnelRule {
                    bind_address: "0.0.0.0".to_owned(),
                    ..TunnelRule::local(8080, "::1", 80)
                },
                TunnelRule::dynamic(1080),
            ]
        );
        assert_eq!(
            host.notes,
            [ImportNote::UnsupportedForward(
                "LocalForward /tmp/sock /run/x.sock".to_owned()
            )]
        );
    }

    #[test]
    fn a_bad_port_is_noted_and_22_used() {
        let (_dir, context) = home();
        let import = import(&context, "Host p\n  Port ssh\n");
        assert_eq!(import.hosts[0].profile.port, 22);
        assert_eq!(
            import.hosts[0].notes,
            [ImportNote::BadPort("ssh".to_owned())]
        );
    }

    #[test]
    fn a_missing_file_is_an_error_the_caller_can_tell_apart() {
        let (_dir, context) = home();
        let error = read_config(&context.home.join(".ssh/config"), &context).expect_err("missing");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn wildcards_match_whole_names() {
        assert!(wildcard_match("*.prod", "web.prod"));
        assert!(!wildcard_match("*.prod", "web.prod.old"));
        assert!(wildcard_match("web?", "web1"));
        assert!(!wildcard_match("web?", "web12"));
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("a*b*c", "aXbYc"));
    }
}
