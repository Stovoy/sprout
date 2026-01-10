use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Local, TimeZone};
use clap::{Parser, Subcommand};
use comfy_table::{Cell, Table};
use serde::{Deserialize, Serialize};

#[derive(Parser)]
#[command(name = "sprout", version, about = "Minimal git worktree manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Create { worktree: String },
    Cd { worktree: String },
    Base,
    List,
    Ls,
    Delete { worktree: String },
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand, Clone)]
enum ConfigAction {
    Get { key: String },
    Set { key: String, value: String },
}

#[derive(Deserialize, Serialize, Default)]
struct Config {
    branch_prefix: Option<String>,
    copy_paths: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Clone)]
struct WorktreeEntry {
    name: String,
    path: String,
    source_repo: String,
    branch: String,
    created_at: i64,
}

#[derive(Serialize, Deserialize, Default)]
struct Metadata {
    worktrees: Vec<WorktreeEntry>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Create { worktree } => create_worktree(&worktree),
        Commands::Cd { worktree } => cd_worktree(&worktree),
        Commands::Base => cd_base(),
        Commands::List | Commands::Ls => list_worktrees(),
        Commands::Delete { worktree } => delete_worktree(&worktree),
        Commands::Config { action } => config_cmd(action),
    }
}

fn create_worktree(name: &str) -> Result<()> {
    let repo_root = fs::canonicalize(git_repo_root()?)?;
    let paths = sprout_paths()?;
    fs::create_dir_all(&paths.worktrees_dir)?;

    let worktree_path = paths.worktrees_dir.join(name);
    if worktree_path.exists() {
        bail!("worktree already exists at {}", worktree_path.display());
    }

    let config = load_config(&paths.config_path)?;
    let mut metadata = load_metadata(&paths.metadata_path)?;
    if metadata.worktrees.iter().any(|entry| entry.name == name) {
        bail!("worktree name already exists: {}", name);
    }
    let prefix = config.branch_prefix.unwrap_or_else(|| "sprout/".to_string());
    let branch = if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{}{}", prefix, name)
    };

    run_git(
        &repo_root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            &branch,
            worktree_path.to_str().ok_or_else(|| anyhow!("invalid path"))?,
        ],
    )?;

    if let Some(copy_paths) = &config.copy_paths {
        copy_config_paths(&repo_root, &worktree_path, copy_paths)?;
    }

    let worktree_path = fs::canonicalize(&worktree_path)?;
    metadata.worktrees.push(WorktreeEntry {
        name: name.to_string(),
        path: worktree_path.to_string_lossy().to_string(),
        source_repo: repo_root.to_string_lossy().to_string(),
        branch,
        created_at: now_ts()?,
    });
    save_metadata(&paths.metadata_path, &metadata)?;

    print_cd_path(&worktree_path)
}

fn cd_worktree(name: &str) -> Result<()> {
    let paths = sprout_paths()?;
    let metadata = load_metadata(&paths.metadata_path)?;
    let entry = metadata
        .worktrees
        .iter()
        .find(|entry| entry.name == name)
        .ok_or_else(|| anyhow!("unknown worktree: {}", name))?;

    print_cd_path(Path::new(&entry.path))
}

fn cd_base() -> Result<()> {
    let repo_root = fs::canonicalize(git_repo_root()?)?;
    let paths = sprout_paths()?;
    let metadata = load_metadata(&paths.metadata_path)?;
    let repo_root_str = repo_root.to_string_lossy();
    let entry = metadata.worktrees.iter().find(|entry| {
        canonicalize_string(&entry.path).unwrap_or_else(|| entry.path.clone())
            == repo_root_str
    });

    match entry {
        Some(entry) => print_cd_path(Path::new(&entry.source_repo)),
        None => {
            let repo_root = canonicalize_for_cd(&repo_root)?;
            println!("{}", repo_root.display());
            Ok(())
        }
    }
}

fn list_worktrees() -> Result<()> {
    let paths = sprout_paths()?;
    let metadata = load_metadata(&paths.metadata_path)?;
    let mut rows = Vec::new();

    for entry in metadata.worktrees {
        let last_commit = git_last_commit_ts(&entry.path).unwrap_or(0);
        rows.push((last_commit, entry));
    }

    rows.sort_by(|a, b| b.0.cmp(&a.0));

    let mut table = Table::new();
    table.load_preset(comfy_table::presets::ASCII_MARKDOWN);
    table.set_header(vec!["Name", "Repo", "Path", "Branch", "Last Commit"]);    
    for (ts, entry) in rows {
        let display_repo = display_path(&entry.source_repo);
        let display_path = display_path(&entry.path);
        table.add_row(vec![
            Cell::new(entry.name),
            Cell::new(display_repo),
            Cell::new(display_path),
            Cell::new(entry.branch),
            Cell::new(format_ts(ts)),
        ]);
    }

    println!("{table}");
    Ok(())
}

fn delete_worktree(name: &str) -> Result<()> {
    let paths = sprout_paths()?;
    let mut metadata = load_metadata(&paths.metadata_path)?;
    let index = metadata
        .worktrees
        .iter()
        .position(|entry| entry.name == name)
        .ok_or_else(|| anyhow!("unknown worktree: {}", name))?;
    let entry = metadata.worktrees.remove(index);

    run_git(
        Path::new(&entry.source_repo),
        &["worktree", "remove", &entry.path],
    )?;
    save_metadata(&paths.metadata_path, &metadata)?;
    Ok(())
}

fn git_repo_root() -> Result<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .stdout(Stdio::piped())
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        bail!("not in a git repository");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(PathBuf::from(text.trim()))
}

fn run_git(repo: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .context("failed to run git")?;
    if !status.success() {
        bail!("git command failed");
    }
    Ok(())
}

fn git_last_commit_ts(worktree_path: &str) -> Result<i64> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["log", "-1", "--format=%ct"])
        .stdout(Stdio::piped())
        .output()
        .context("failed to run git")?;
    if !output.status.success() {
        return Ok(0);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text.trim().parse::<i64>().unwrap_or(0))
}

fn load_config(path: &Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    let contents = fs::read_to_string(path)?;
    let config = toml::from_str(&contents)?;
    Ok(config)
}

fn save_config(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let contents = toml::to_string_pretty(config)?;
    fs::write(path, contents)?;
    Ok(())
}

fn load_metadata(path: &Path) -> Result<Metadata> {
    if !path.exists() {
        return Ok(Metadata::default());
    }
    let contents = fs::read_to_string(path)?;
    let metadata = serde_json::from_str(&contents)?;
    Ok(metadata)
}

fn save_metadata(path: &Path, metadata: &Metadata) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let contents = serde_json::to_string_pretty(metadata)?;
    fs::write(path, contents)?;
    Ok(())
}

fn sprout_paths() -> Result<SproutPaths> {
    let home = dirs::home_dir().context("home directory not found")?;
    let root = home.join(".sprout");
    Ok(SproutPaths {
        worktrees_dir: root.join("worktrees"),
        metadata_path: root.join("metadata.json"),
        config_path: root.join("config.toml"),
    })
}

fn print_cd_path(path: &Path) -> Result<()> {
    let path = canonicalize_for_cd(path)?;
    println!("{}", path.display());
    Ok(())
}

fn canonicalize_for_cd(path: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        if let Some(stripped) = strip_verbatim_windows_prefix(&canonical) {
            return Ok(stripped);
        }
    }
    Ok(canonical)
}

fn display_path(path: &str) -> String {
    canonicalize_for_cd(Path::new(path))
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string())
}

#[cfg(windows)]
fn strip_verbatim_windows_prefix(path: &Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let prefix = match components.next()? {
        Component::Prefix(prefix) => prefix.kind(),
        _ => return None,
    };
    let rest: PathBuf = components.collect();
    match prefix {
        Prefix::VerbatimDisk(letter) => {
            let drive = format!("{}:", letter as char);
            Some(PathBuf::from(drive).join(rest))
        }
        Prefix::VerbatimUNC(server, share) => {
            let mut base = PathBuf::from(r"\\");
            base.push(server);
            base.push(share);
            Some(base.join(rest))
        }
        _ => None,
    }
}

fn now_ts() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs() as i64)
}

fn format_ts(ts: i64) -> String {
    if ts <= 0 {
        return "-".to_string();
    }
    let dt: DateTime<Local> = Local.timestamp_opt(ts, 0).single().unwrap_or_else(|| {
        Local
            .timestamp_opt(0, 0)
            .single()
            .unwrap()
    });
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn canonicalize_string(path: &str) -> Option<String> {
    fs::canonicalize(path)
        .ok()
        .map(|path| path.to_string_lossy().to_string())
}

struct SproutPaths {
    worktrees_dir: PathBuf,
    metadata_path: PathBuf,
    config_path: PathBuf,
}

fn config_cmd(action: ConfigAction) -> Result<()> {
    let paths = sprout_paths()?;
    match action {
        ConfigAction::Get { key } => config_get(&paths.config_path, &key),
        ConfigAction::Set { key, value } => config_set(&paths.config_path, &key, &value),
    }
}

fn config_get(path: &Path, key: &str) -> Result<()> {
    let config = load_config(path)?;
    match key {
        "branch_prefix" => {
            let value = config.branch_prefix.unwrap_or_default();
            println!("{value}");
            Ok(())
        }
        "copy_paths" => {
            let value = config.copy_paths.unwrap_or_default();
            println!("{}", value.join(","));
            Ok(())
        }
        _ => bail!("unknown config key: {}", key),
    }
}

fn config_set(path: &Path, key: &str, value: &str) -> Result<()> {
    let mut config = load_config(path)?;
    match key {
        "branch_prefix" => {
            config.branch_prefix = Some(value.to_string());
        }
        "copy_paths" => {
            let entries: Vec<String> = value
                .split(',')
                .map(|entry| entry.trim())
                .filter(|entry| !entry.is_empty())
                .map(|entry| entry.to_string())
                .collect();
            if entries.is_empty() {
                config.copy_paths = None;
            } else {
                config.copy_paths = Some(entries);
            }
        }
        _ => bail!("unknown config key: {}", key),
    }
    save_config(path, &config)?;
    Ok(())
}

fn copy_config_paths(repo_root: &Path, worktree_root: &Path, paths: &[String]) -> Result<()> {
    for entry in paths {
        let relative = Path::new(entry);
        if relative.is_absolute() {
            eprintln!("copy_paths entry is absolute, skipping: {}", entry);
            continue;
        }
        let source = repo_root.join(relative);
        if !source.exists() {
            eprintln!(
                "copy_paths entry not found, skipping: {}",
                source.display()
            );
            continue;
        }
        let destination = worktree_root.join(relative);
        copy_recursively(&source, &destination)
            .with_context(|| format!("copying {}", source.display()))?;
    }
    Ok(())
}

fn copy_recursively(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        eprintln!("copy_paths entry is a symlink, skipping: {}", source.display());
        return Ok(());
    }

    if metadata.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let child_source = entry.path();
            let child_destination = destination.join(entry.file_name());
            copy_recursively(&child_source, &child_destination)?;
        }
        return Ok(());
    }

    if metadata.is_file() {
        if destination.exists() {
            eprintln!(
                "copy_paths destination exists, skipping: {}",
                destination.display()
            );
            return Ok(());
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
    }

    Ok(())
}
