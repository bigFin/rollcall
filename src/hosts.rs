use std::{
    collections::{BTreeMap, BTreeSet},
    env, fmt, fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use glob::{Pattern, glob};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConnectivityState {
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshHost {
    pub alias: String,
    pub hostname: String,
    pub user: Option<String>,
    pub port: u16,
    pub source: String,
    pub identity_files: Vec<String>,
    pub connectivity: ConnectivityState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_error: Option<String>,
}

#[derive(Debug)]
pub enum HostDiscoveryError {
    MissingHome,
    ReadConfig { path: PathBuf, source: io::Error },
    InvalidInclude { pattern: String, message: String },
    StartSsh(io::Error),
}

impl fmt::Display for HostDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHome => write!(formatter, "HOME is not set"),
            Self::ReadConfig { path, source } => {
                write!(
                    formatter,
                    "could not read SSH config {}: {source}",
                    path.display()
                )
            }
            Self::InvalidInclude { pattern, message } => {
                write!(
                    formatter,
                    "invalid SSH Include pattern {pattern:?}: {message}"
                )
            }
            Self::StartSsh(source) => write!(formatter, "could not start ssh -G: {source}"),
        }
    }
}

pub fn discover(config_override: Option<&Path>) -> Result<Vec<SshHost>, HostDiscoveryError> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(HostDiscoveryError::MissingHome)?;
    let default_config = home.join(".ssh/config");
    let config_path = config_override.unwrap_or(&default_config);

    if !config_path.exists() {
        return Ok(Vec::new());
    }

    let aliases = discover_aliases(config_path, &home.join(".ssh"), &home)?;
    aliases
        .into_iter()
        .map(|(alias, source)| resolve_host(&alias, &source, config_override))
        .collect()
}

fn discover_aliases(
    config_path: &Path,
    include_base: &Path,
    home: &Path,
) -> Result<BTreeMap<String, PathBuf>, HostDiscoveryError> {
    let mut aliases = BTreeMap::new();
    let mut visited = BTreeSet::new();
    visit_config(config_path, include_base, home, &mut visited, &mut aliases)?;
    Ok(aliases)
}

fn visit_config(
    path: &Path,
    include_base: &Path,
    home: &Path,
    visited: &mut BTreeSet<PathBuf>,
    aliases: &mut BTreeMap<String, PathBuf>,
) -> Result<(), HostDiscoveryError> {
    let identity = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(identity) {
        return Ok(());
    }

    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(HostDiscoveryError::ReadConfig {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    for line in contents.lines() {
        let Some(words) = shlex::split(line) else {
            continue;
        };
        let Some((keyword, arguments)) = words.split_first() else {
            continue;
        };

        if keyword.eq_ignore_ascii_case("host") {
            for alias in arguments {
                if is_literal_alias(alias) {
                    aliases
                        .entry(alias.clone())
                        .or_insert_with(|| path.to_path_buf());
                }
            }
        } else if keyword.eq_ignore_ascii_case("include") {
            for include in arguments {
                for included_path in expand_include(include, include_base, home)? {
                    visit_config(&included_path, include_base, home, visited, aliases)?;
                }
            }
        }
    }

    Ok(())
}

fn is_literal_alias(candidate: &str) -> bool {
    !candidate.is_empty()
        && !candidate.starts_with('-')
        && !candidate.starts_with('!')
        && !candidate
            .chars()
            .any(|character| matches!(character, '*' | '?' | '[' | ']'))
}

fn expand_include(
    include: &str,
    include_base: &Path,
    home: &Path,
) -> Result<Vec<PathBuf>, HostDiscoveryError> {
    let path = if include == "~" {
        home.to_path_buf()
    } else if let Some(relative) = include.strip_prefix("~/") {
        home.join(relative)
    } else {
        let candidate = Path::new(include);
        if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            include_base.join(candidate)
        }
    };
    let pattern = path.to_string_lossy().into_owned();

    Pattern::new(&pattern).map_err(|error| HostDiscoveryError::InvalidInclude {
        pattern: pattern.clone(),
        message: error.to_string(),
    })?;

    let mut matches = glob(&pattern)
        .map_err(|error| HostDiscoveryError::InvalidInclude {
            pattern: pattern.clone(),
            message: error.to_string(),
        })?
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    matches.sort();
    Ok(matches)
}

fn resolve_host(
    alias: &str,
    source: &Path,
    config_override: Option<&Path>,
) -> Result<SshHost, HostDiscoveryError> {
    let mut command = Command::new("ssh");
    command.arg("-G");
    if let Some(config_path) = config_override {
        command.arg("-F").arg(config_path);
    }
    let output = command
        .arg("--")
        .arg(alias)
        .stdin(Stdio::null())
        .output()
        .map_err(HostDiscoveryError::StartSsh)?;

    if output.status.success() {
        let effective = String::from_utf8_lossy(&output.stdout);
        Ok(parse_effective_config(alias, source, &effective))
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Ok(SshHost {
            alias: alias.to_owned(),
            hostname: alias.to_owned(),
            user: None,
            port: 22,
            source: source.display().to_string(),
            identity_files: Vec::new(),
            connectivity: ConnectivityState::Unknown,
            resolution_error: Some(if message.is_empty() {
                format!("ssh -G exited with {}", output.status)
            } else {
                message
            }),
        })
    }
}

fn parse_effective_config(alias: &str, source: &Path, effective: &str) -> SshHost {
    let mut hostname = alias.to_owned();
    let mut user = None;
    let mut port = 22;
    let mut identity_files = Vec::new();

    for line in effective.lines() {
        let Some((keyword, value)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let value = value.trim();

        if keyword.eq_ignore_ascii_case("hostname") {
            hostname = value.to_owned();
        } else if keyword.eq_ignore_ascii_case("user") {
            user = Some(value.to_owned());
        } else if keyword.eq_ignore_ascii_case("port") {
            port = value.parse().unwrap_or(22);
        } else if keyword.eq_ignore_ascii_case("identityfile") {
            identity_files.push(value.to_owned());
        }
    }

    SshHost {
        alias: alias.to_owned(),
        hostname,
        user,
        port,
        source: source.display().to_string(),
        identity_files,
        connectivity: ConnectivityState::Unknown,
        resolution_error: None,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::tempdir;

    use super::{ConnectivityState, discover_aliases, parse_effective_config};

    #[test]
    fn discovers_literal_aliases_and_ignores_patterns() {
        let directory = tempdir().expect("temporary directory should be created");
        let ssh_directory = directory.path().join(".ssh");
        fs::create_dir(&ssh_directory).expect("SSH directory should be created");
        let config = ssh_directory.join("config");
        fs::write(
            &config,
            "\
Host topo coda
  User fin

Host *.example.com !blocked.example.com
  User deploy

Host -invalid
",
        )
        .expect("config should be written");

        let aliases = discover_aliases(&config, &ssh_directory, directory.path())
            .expect("aliases should be discovered");

        assert_eq!(
            aliases.keys().cloned().collect::<Vec<_>>(),
            ["coda", "topo"]
        );
    }

    #[test]
    fn follows_include_globs_once() {
        let directory = tempdir().expect("temporary directory should be created");
        let ssh_directory = directory.path().join(".ssh");
        let includes = ssh_directory.join("config.d");
        fs::create_dir_all(&includes).expect("include directory should be created");
        let config = ssh_directory.join("config");
        fs::write(&config, "Include config.d/*.conf\nHost laptop\n")
            .expect("config should be written");
        fs::write(includes.join("remote.conf"), "Host topo\nInclude config\n")
            .expect("included config should be written");

        let aliases = discover_aliases(&config, &ssh_directory, directory.path())
            .expect("aliases should be discovered");

        assert_eq!(
            aliases.keys().cloned().collect::<Vec<_>>(),
            ["laptop", "topo"]
        );
    }

    #[test]
    fn parses_effective_ssh_configuration() {
        let host = parse_effective_config(
            "topo",
            Path::new("/home/fin/.ssh/config"),
            "\
host topo
user fin
hostname 100.64.0.10
port 2222
identityfile ~/.ssh/id_ed25519
identityfile ~/.ssh/id_work
",
        );

        assert_eq!(host.alias, "topo");
        assert_eq!(host.hostname, "100.64.0.10");
        assert_eq!(host.user.as_deref(), Some("fin"));
        assert_eq!(host.port, 2222);
        assert_eq!(host.identity_files.len(), 2);
        assert_eq!(host.connectivity, ConnectivityState::Unknown);
        assert_eq!(host.resolution_error, None);
    }
}
