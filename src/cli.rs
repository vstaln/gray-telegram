//! CLI tree: `gray-telegram --config <path> <subcommand>`.
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Standalone Telegram transport and setup for gray.
#[derive(Debug, Parser)]
#[command(name = "gray-telegram", bin_name = "gray telegram")]
pub struct Cli {
    /// Config file path.
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Command,
}

impl Cli {
    pub fn config_path(&self) -> PathBuf {
        match &self.config {
            Some(p) => p.clone(),
            None => crate::config::default_path(),
        }
    }
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Interactive setup. The token is checked with Telegram (getMe);
    /// home chat defaults to your own DM. `--pair` discovers your IDs by
    /// DMing a one-time code to the bot.
    Setup {
        /// Discover the owner's IDs by DMing a one-time code to the bot.
        #[arg(long)]
        pair: bool,
    },
    Run,
    Sidecar,
    /// Print the plugin manifest (what `gray plugin install telegram` reads).
    #[command(hide = true)]
    Manifest,
    Register,
    Install,
    Status,
    Stop,
    Restart,
    Doctor,
    Uninstall,
    Limits {
        #[arg(long)]
        timeout_seconds: Option<u64>,
        #[arg(long)]
        concurrency: Option<u64>,
        #[arg(long)]
        max_requests: Option<u64>,
    },
    Budget {
        #[command(subcommand)]
        action: BudgetAction,
    },
    Share {
        #[arg(long)]
        skill: Vec<String>,
        #[arg(long)]
        context: Vec<String>,
        #[arg(long, name = "plugin-argv")]
        plugin_argv: Vec<String>,
        #[arg(long)]
        clear: bool,
    },
    Queue {
        #[command(subcommand)]
        action: QueueAction,
    },
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },
    Allowlist {
        #[command(subcommand)]
        action: AllowlistAction,
    },
    Pairing {
        #[command(subcommand)]
        action: PairingAction,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum BudgetAction {
    Status,
    Set {
        #[arg(long, name = "daily-usd")]
        daily_usd: f64,
        #[arg(long, name = "turn-usd")]
        turn_usd: f64,
        #[arg(long, name = "input-per-million")]
        input_per_million: f64,
        #[arg(long, name = "output-per-million")]
        output_per_million: f64,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum QueueAction {
    List,
    Cancel { id: String },
}

#[derive(Debug, Clone, Subcommand)]
pub enum ScheduleAction {
    Add {
        #[arg(long)]
        every: u64,
        prompt: String,
    },
    List,
    Remove {
        id: String,
    },
}

#[derive(Debug, Clone, Subcommand)]
pub enum AllowlistAction {
    Add { id: String },
    Remove { id: String },
    List,
}

#[derive(Debug, Clone, Subcommand)]
pub enum PairingAction {
    /// Approve a code the bot handed an unconfigured DMer: they join the
    /// allowlist (or become the owner if none is set yet).
    Approve {
        /// Platform the code was issued for. Only `telegram` exists.
        platform: String,
        /// The code from the bot's DM reply.
        code: String,
    },
}

/// Register the outgoing `telegram_send`/`telegram_file` tools in gray's plugin lock.
/// Matches `gray_plugin::lock::LockEntry` (explicit argv, no fake install).
pub fn register(config: &Value, config_path: &Path) -> Result<(), String> {
    use serde_json::{json, Value};
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .unwrap_or("");
    let lock = Path::new(gray_home).join("plugins/lock.json");
    let mut data: Value = std::fs::read(&lock)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({"schema": 1, "plugins": {}}));
    if data.get("schema").and_then(Value::as_u64) != Some(1)
        || !data.get("plugins").is_some_and(Value::is_object)
    {
        return Err("Unsupported gray plugin lock; not changing it".to_string());
    }
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|_| "cannot locate plugin binary".to_string())?;
    let resolved = config_path
        .to_str()
        .map(str::to_string)
        .unwrap_or_else(|| config_path.to_string_lossy().into_owned());
    let argv = vec![
        Value::String(exe.clone()),
        Value::String("sidecar".to_string()),
        Value::String("--config".to_string()),
        Value::String(resolved),
    ];
    let prev_exe = data
        .pointer("/plugins/telegram/argv/0")
        .and_then(Value::as_str);
    if prev_exe.is_some_and(|p| p != exe && Path::new(p).is_file()) {
        return Err("Another telegram plugin is registered; refusing to overwrite it".to_string());
    }
    let installed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default();
    // Merge into any existing row: gray now keeps cli_argv and the consent
    // record (granted_capabilities, capabilities_hash) in the same lock entry,
    // so replacing the object would drop them.
    let entry = &mut data["plugins"]["telegram"];
    if !entry.is_object() {
        *entry = json!({});
    }
    entry["ecosystem"] = json!("gray-native");
    entry["version"] = json!(env!("CARGO_PKG_VERSION"));
    entry["source"] = json!("https://github.com/vstaln/gray-telegram");
    entry["argv"] = Value::Array(argv);
    entry["adapter_version"] = json!("1.1");
    entry["installed_at"] = json!(installed_at);
    if entry.get("hash").is_none() {
        entry["hash"] = json!("");
    }
    if entry.get("scope").is_none() {
        entry["scope"] = json!("user");
    }
    if entry.get("enabled").is_none() {
        entry["enabled"] = json!(true);
    }
    // Re-register heals a row whose CLI binary moved (reinstall, cargo
    // install to a new prefix); a working custom cli_argv is kept.
    let cli_ok = entry
        .get("cli_argv")
        .and_then(|a| a.get(0))
        .and_then(Value::as_str)
        .is_some_and(|p| Path::new(p).is_file());
    if !cli_ok {
        entry["cli_argv"] = json!([exe]);
    }
    crate::config::atomic_json(&lock, &data)
}

/// Parse the service-install answer. Python parity (`cli.py`):
/// `input('Enable and start the background service now? [Y/n] ').strip().lower()`
/// in `('', 'y', 'yes')` — default yes.
pub fn install_confirmed(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "" | "y" | "yes")
}

/// Dispatch a parsed subcommand. Arms without a landed task fail closed so
/// no half-wired command can run.
pub fn run(cmd: &Command, config_path: &Path) -> Result<(), String> {
    use serde_json::Value;
    match cmd {
        Command::Sidecar => crate::sidecar::serve(config_path),
        Command::Manifest => {
            use clap::CommandFactory;
            let mut m = crate::sidecar::MANIFEST.clone();
            m["completion"] = Cli::command()
                .get_subcommands()
                .filter(|c| {
                    !c.is_hide_set() && !["run", "sidecar", "register"].contains(&c.get_name())
                })
                .map(|c| Value::String(c.get_name().to_string()))
                .collect();
            m["sidecar_args"] = serde_json::json!(["sidecar"]);
            println!("{m}");
            Ok(())
        }
        Command::Register => {
            let config: Value = crate::config::load_config(config_path)?;
            register(&config, config_path)
        }
        Command::Setup { pair } => {
            use crate::setup::Prompter;
            let mut io = crate::setup::Tty;
            // setup is async; drive it on a throwaway current-thread runtime
            // so `run` stays sync like the other CLI arms.
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "cannot start setup runtime".to_string())?;
            let done = rt.block_on(crate::setup::run(config_path, &mut io, *pair))?;
            if done {
                let config: Value = crate::config::load_config(config_path)?;
                register(&config, config_path)?;
                println!("Outgoing tool registered with gray.");
                if crate::service::installed(config_path) {
                    // A reconfigured bot takes effect now.
                    println!("{}", crate::service::restart(config_path)?);
                    return Ok(());
                }
                let answer = io.prompt("Enable and start the background service now? [Y/n] ")?;
                if install_confirmed(&answer) {
                    let line = crate::service::install(config_path)?;
                    println!("{line}");
                    println!("Run gray telegram status to check it.");
                    if matches!(
                        crate::service::detect(),
                        crate::service::Supervisor::SystemdUser { .. }
                    ) {
                        println!("For operation after logout: loginctl enable-linger \"$USER\"");
                    }
                } else {
                    println!(
                        "Run gray telegram install when ready, or gray telegram run in the foreground."
                    );
                }
            }
            Ok(())
        }
        Command::Run => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|_| "cannot start gateway runtime".to_string())?;
            rt.block_on(crate::gateway::run(config_path))
        }
        Command::Status => {
            println!("{}", crate::service::status(config_path)?);
            Ok(())
        }
        Command::Stop => {
            println!("{}", crate::service::stop(config_path)?);
            Ok(())
        }
        Command::Restart => {
            println!("{}", crate::service::restart(config_path)?);
            Ok(())
        }
        Command::Uninstall => {
            let line = crate::service::uninstall(config_path)?;
            println!("{line}");
            println!("Service removed. Private configuration and sessions retained; registered outgoing tool retained.");
            Ok(())
        }
        Command::Install => {
            let line = crate::service::install(config_path)?;
            println!("{line}");
            if matches!(
                crate::service::detect(),
                crate::service::Supervisor::SystemdUser { .. }
            ) {
                println!("To survive logout, enable user linger: loginctl enable-linger \"$USER\"");
            }
            Ok(())
        }
        Command::Doctor => {
            let config = crate::config::load_config(config_path)?;
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| "cannot start runtime".to_string())?;
            rt.block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    crate::doctor::doctor(&config),
                )
                .await
                .map_err(|_| "doctor timed out after 30s".to_string())?
            })
        }
        Command::Share {
            skill,
            context,
            plugin_argv,
            clear,
        } => {
            let mut config = crate::config::load_config(config_path)?;
            if *clear {
                config["shared_skills"] = Value::Array(Vec::new());
                config["shared_context"] = Value::Array(Vec::new());
                config["shared_plugins"] = Value::Array(Vec::new());
            }
            let mut skills: Vec<String> = config
                .get("shared_skills")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            for s in skill {
                if !skills.contains(s) {
                    skills.push(s.clone());
                }
            }
            config["shared_skills"] = Value::Array(skills.into_iter().map(Value::String).collect());

            let mut contexts: Vec<String> = config
                .get("shared_context")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            for c in context {
                if !contexts.contains(c) {
                    contexts.push(c.clone());
                }
            }
            config["shared_context"] =
                Value::Array(contexts.into_iter().map(Value::String).collect());

            let mut plugins: Vec<Value> = config
                .get("shared_plugins")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for p_str in plugin_argv {
                let parsed: Value = serde_json::from_str(p_str)
                    .map_err(|_| "Invalid plugin-argv JSON".to_string())?;
                plugins.push(parsed);
            }
            config["shared_plugins"] = Value::Array(plugins);

            let tmp_dir =
                std::env::temp_dir().join(format!("gray-cap-{}", crate::durable::uuid_hex()));
            std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("cannot create tempdir: {e}"))?;
            let res = crate::capabilities::prepare(&config, &tmp_dir);
            let _ = std::fs::remove_dir_all(&tmp_dir);
            res?;
            crate::config::save_config(config_path, &config)?;
            println!("Shared capabilities saved. Only select trusted code/non-secret context. Restart to apply.");
            Ok(())
        }
        Command::Limits {
            timeout_seconds,
            concurrency,
            max_requests,
        } => {
            let mut config = crate::config::load_config(config_path)?;
            if let Some(n) = timeout_seconds {
                config["timeout_seconds"] = Value::from(*n);
            }
            if let Some(n) = concurrency {
                config["concurrency"] = Value::from(*n);
            }
            if let Some(n) = max_requests {
                config["max_requests"] = Value::from(*n);
            }
            crate::config::save_config(config_path, &config)?;
            println!("Limits saved; restart the gateway to apply.");
            Ok(())
        }
        Command::Budget { action } => match action {
            BudgetAction::Set {
                daily_usd,
                turn_usd,
                input_per_million,
                output_per_million,
            } => {
                let mut config = crate::config::load_config(config_path)?;
                let gray_home = config
                    .get("gray_home")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "gray_home missing".to_string())?;
                let provider_bytes = std::fs::read(Path::new(gray_home).join("config.json"))
                    .map_err(|_| "gray provider configuration is missing".to_string())?;
                let provider: Value = serde_json::from_slice(&provider_bytes)
                    .map_err(|_| "Invalid gray config.json".to_string())?;
                let model = provider
                    .get("model")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "model missing".to_string())?;

                let new_budget = serde_json::json!({
                    "model": model,
                    "daily_usd": daily_usd,
                    "turn_usd": turn_usd,
                    "input_per_million": input_per_million,
                    "output_per_million": output_per_million,
                });
                crate::budget::validate(&new_budget, model)?;
                config["budget"] = new_budget;
                crate::config::save_config(config_path, &config)?;
                println!("Budget saved for selected model; restart the gateway to apply.");
                Ok(())
            }
            BudgetAction::Status => {
                let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
                let b = crate::budget::Budget::new(&parent.join("budget.sqlite"))?;
                println!("Accounted/reserved micro-USD: {}", b.total());
                Ok(())
            }
        },
        Command::Schedule { action } => {
            let config = crate::config::load_config(config_path)?;
            let store = crate::gateway::open_store(config_path)?;
            match action {
                ScheduleAction::Add { every, prompt } => {
                    // No channel context here, so a CLI-added job targets the
                    // configured home channel and shares that channel's gray
                    // home — the same result `/cron add` gives in the home
                    // channel, so the two entry points cannot drift.
                    let home = config
                        .get("chat_id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "chat_id missing".to_string())?;
                    let job_id = crate::durable::uuid_hex();
                    store.schedule_add(
                        &job_id,
                        *every,
                        prompt,
                        home,
                        &format!("chat:{home}"),
                        crate::durable::now_secs(),
                    )?;
                    println!("{job_id}");
                    Ok(())
                }
                ScheduleAction::List => {
                    for job in store.schedules()? {
                        println!(
                            "{} {} {} {}",
                            job.id,
                            job.interval,
                            job.status,
                            if job.channel.is_empty() {
                                "(home)"
                            } else {
                                job.channel.as_str()
                            }
                        );
                    }
                    Ok(())
                }
                ScheduleAction::Remove { id } => {
                    store.schedule_remove(id)?;
                    Ok(())
                }
            }
        }
        Command::Queue { action } => {
            let store = crate::gateway::open_store(config_path)?;
            match action {
                QueueAction::List => {
                    for item in store.items()? {
                        println!(
                            "{} {} {} {}",
                            item.0,
                            item.1,
                            item.2,
                            item.3.as_deref().unwrap_or("")
                        );
                    }
                    Ok(())
                }
                QueueAction::Cancel { id } => {
                    store.cancel(id)?;
                    Ok(())
                }
            }
        }
        Command::Pairing { action } => match action {
            PairingAction::Approve { platform, code } => {
                let line = crate::pairing::approve(config_path, platform, code)?;
                println!("{line}");
                Ok(())
            }
        },
        Command::Allowlist { action } => {
            let mut config = crate::config::load_config(config_path)?;
            let mut allowed: Vec<String> = config
                .get("allowed_users")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            match action {
                AllowlistAction::Add { id } => {
                    if !crate::config::user_id(&Value::String(id.clone())) {
                        return Err("Invalid Telegram user ID".to_string());
                    }
                    if !allowed.contains(id) {
                        allowed.push(id.clone());
                        config["allowed_users"] =
                            Value::Array(allowed.into_iter().map(Value::String).collect());
                        crate::config::save_config(config_path, &config)?;
                    }
                    Ok(())
                }
                AllowlistAction::Remove { id } => {
                    if !crate::config::user_id(&Value::String(id.clone())) {
                        return Err("Invalid Telegram user ID".to_string());
                    }
                    allowed.retain(|x| x != id);
                    config["allowed_users"] =
                        Value::Array(allowed.into_iter().map(Value::String).collect());
                    crate::config::save_config(config_path, &config)?;
                    Ok(())
                }
                AllowlistAction::List => {
                    for id in allowed {
                        println!("{id}");
                    }
                    Ok(())
                }
            }
        }
    }
}
