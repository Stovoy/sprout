use std::env;
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
            "-b",
            &branch,
            worktree_path.to_str().ok_or_else(|| anyhow!("invalid path"))?,
        ],
    )?;

    let worktree_path = fs::canonicalize(&worktree_path)?;
    metadata.worktrees.push(WorktreeEntry {
        name: name.to_string(),
        path: worktree_path.to_string_lossy().to_string(),
        source_repo: repo_root.to_string_lossy().to_string(),
        branch,
        created_at: now_ts()?,
    });
    save_metadata(&paths.metadata_path, &metadata)?;

    launch_shell(&worktree_path)
}

fn cd_worktree(name: &str) -> Result<()> {
    let paths = sprout_paths()?;
    let metadata = load_metadata(&paths.metadata_path)?;
    let entry = metadata
        .worktrees
        .iter()
        .find(|entry| entry.name == name)
        .ok_or_else(|| anyhow!("unknown worktree: {}", name))?;

    launch_shell(Path::new(&entry.path))
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
        Some(entry) => launch_shell(Path::new(&entry.source_repo)),
        None => {
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
        table.add_row(vec![
            Cell::new(entry.name),
            Cell::new(entry.source_repo),
            Cell::new(entry.path),
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
    let home = env::var("HOME").context("HOME not set")?;
    let root = PathBuf::from(home).join(".sprout");
    Ok(SproutPaths {
        worktrees_dir: root.join("worktrees"),
        metadata_path: root.join("metadata.json"),
        config_path: root.join("config.toml"),
    })
}

fn launch_shell(path: &Path) -> Result<()> {
    let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let status = Command::new(shell)
        .arg("-i")
        .current_dir(path)
        .status()
        .context("failed to launch shell")?;
    if !status.success() {
        bail!("shell exited with error");
    }
    Ok(())
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
        _ => bail!("unknown config key: {}", key),
    }
}

fn config_set(path: &Path, key: &str, value: &str) -> Result<()> {
    let mut config = load_config(path)?;
    match key {
        "branch_prefix" => {
            config.branch_prefix = Some(value.to_string());
        }
        _ => bail!("unknown config key: {}", key),
    }
    save_config(path, &config)?;
    Ok(())
}
