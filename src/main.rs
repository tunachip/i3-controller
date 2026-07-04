use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const SAMPLE_CONFIG: &str = r#"# i3-controller config
# Blocks start with [app], [kill], [text], [refresh], or [rerun].
# Values are key=value. Comments and blank lines are ignored.

[kill]
name=old chromium sessions
pattern=chromium-browser --app=
signal=TERM

[app]
name=dashboard
kind=web
url=https://status.example.test
browser=chromium-browser
profile=/tmp/i3-controller-dashboard
workspace=1
match=title="Status"
layout=move position 0 0; resize set 1280 720
startup_delay=3

[text]
target=dashboard
delay=5
commands=type:lab-user; key:Tab; type_env:LAB_DASHBOARD_PASSWORD; key:Return

[refresh]
target=dashboard
after=30m
action=reload
repeat=true

[app]
name=terminal
kind=native
command=alacritty
args=--title lab-shell
workspace=2
match=title="lab-shell"
layout=move position 1280 0; resize set 1280 720

[rerun]
after=6h
"#;

#[derive(Debug, Default)]
struct Config {
    apps: Vec<App>,
    kills: Vec<KillRule>,
    text_entries: Vec<TextEntry>,
    refreshes: Vec<TimedEvent>,
    rerun: Option<DurationSpec>,
}

#[derive(Debug, Default)]
struct App {
    name: String,
    kind: AppKind,
    command: String,
    args: String,
    url: String,
    browser: String,
    profile: String,
    workspace: String,
    match_criteria: String,
    layout: String,
    startup_delay: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum AppKind {
    #[default]
    Native,
    Web,
}

#[derive(Debug, Default)]
struct KillRule {
    name: String,
    pattern: String,
    signal: String,
}

#[derive(Debug, Default)]
struct TextEntry {
    target: String,
    delay: u64,
    commands: Vec<TextCommand>,
}

#[derive(Debug)]
enum TextCommand {
    Type(String),
    TypeEnv(String),
    Key(String),
}

#[derive(Debug, Default)]
struct TimedEvent {
    target: String,
    after: DurationSpec,
    action: RefreshAction,
    repeat: bool,
}

#[derive(Debug, Default, Clone)]
struct DurationSpec {
    raw: String,
    seconds: u64,
}

#[derive(Debug, Default, PartialEq, Eq)]
enum RefreshAction {
    #[default]
    Reload,
    Relaunch,
}

#[derive(Debug, Default)]
struct Block {
    name: String,
    pairs: Vec<(String, String)>,
}

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("init") => {
            let path = args
                .next()
                .unwrap_or_else(|| "i3-controller.conf".to_string());
            fs::write(&path, SAMPLE_CONFIG.trim_start())?;
            println!("wrote {path}");
        }
        Some("generate") => {
            let config_path = args
                .next()
                .ok_or("usage: i3-controller generate <config> [output]")?;
            let output_path = args.next();
            let config_text = fs::read_to_string(&config_path)?;
            let config = parse_config(&config_text)?;
            let script = generate_script(&config)?;

            if let Some(path) = output_path {
                fs::write(&path, script)?;
                println!("wrote {path}");
            } else {
                print!("{script}");
            }
        }
        Some("help") | Some("--help") | Some("-h") | None => print_help(),
        Some(command) => return Err(format!("unknown command: {command}").into()),
    }

    Ok(())
}

fn print_help() {
    println!("i3-controller\n");

    println!("USAGE:");
    println!("  i3-controller init [config]");
    println!("  i3-controller generate <config> [output]\n");

    println!("Generated scripts require i3-msg, xdotool, pkill, and a POSIX shell.");
}

fn parse_config(input: &str) -> Result<Config> {
    let mut blocks = Vec::new();
    let mut current: Option<Block> = None;

    for (index, raw_line) in input.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if line.starts_with('[') && line.ends_with(']') {
            if let Some(block) = current.take() {
                blocks.push(block);
            }
            current = Some(Block {
                name: line[1..line.len() - 1].trim().to_ascii_lowercase(),
                pairs: Vec::new(),
            });
            continue;
        }

        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {} is not key=value: {raw_line}", index + 1).into());
        };

        let Some(block) = current.as_mut() else {
            return Err(format!("line {} appears before a block header", index + 1).into());
        };

        block.pairs.push((
            key.trim().to_ascii_lowercase(),
            value.trim().to_string()
        ));
    }

    if let Some(block) = current {
        blocks.push(block);
    }

    let mut config = Config::default();
    for block in blocks {
        match block.name.as_str() {
            "app"     => config.apps.push(parse_app(&block)?),
            "kill"    => config.kills.push(parse_kill(&block)?),
            "text"    => config.text_entries.push(parse_text_entry(&block)?),
            "refresh" => config.refreshes.push(parse_timed_event(&block)?),
            "rerun"   => config.rerun = Some(parse_rerun(&block)?),
            other     => return Err(format!("unknown block [{other}]").into()),
        }
    }

    validate_config(&config)?;
    Ok(config)
}

fn parse_app(block: &Block) -> Result<App> {
    let mut app = App {
        browser: "chromium-browser".to_string(),
        ..App::default()
    };

    for (key, value) in &block.pairs {
        match key.as_str() {
            "name" => app.name = value.clone(),
            "kind" => {
                app.kind = match value.to_ascii_lowercase().as_str() {
                    "native" => AppKind::Native,
                    "web"    => AppKind::Web,
                    _ => return Err(format!("unknown app kind for {}: {value}", app.name).into()),
                }
            }
            "command"       => app.command = value.clone(),
            "args"          => app.args = value.clone(),
            "url"           => app.url = value.clone(),
            "browser"       => app.browser = value.clone(),
            "profile"       => app.profile = value.clone(),
            "workspace"     => app.workspace = value.clone(),
            "match"         => app.match_criteria = value.clone(),
            "layout"        => app.layout = value.clone(),
            "startup_delay" => app.startup_delay = value.parse()?,
            _ => return Err(format!("unknown app key for {}: {key}", app.name).into()),
        }
    }

    if app.name.is_empty() {
        return Err("[app] requires name".into());
    }

    match app.kind {
        AppKind::Native if app.command.is_empty() => {
            return Err(format!("app {} requires command", app.name).into());
        }
        AppKind::Web if app.url.is_empty() => {
            return Err(format!("web app {} requires url", app.name).into());
        }
        _ => {}
    }

    Ok(app)
}

fn parse_kill(block: &Block) -> Result<KillRule> {
    let mut kill = KillRule {
        signal: "TERM".to_string(),
        ..KillRule::default()
    };

    for (key, value) in &block.pairs {
        match key.as_str() {
            "name"    => kill.name = value.clone(),
            "pattern" => kill.pattern = value.clone(),
            "signal"  => kill.signal = value.clone(),
            _         => return Err(format!("unknown kill key: {key}").into()),
        }
    }

    if kill.pattern.is_empty() {
        return Err("[kill] requires pattern".into());
    }

    Ok(kill)
}

fn parse_text_entry(block: &Block) -> Result<TextEntry> {
    let mut entry = TextEntry::default();

    for (key, value) in &block.pairs {
        match key.as_str() {
            "target"   => entry.target = value.clone(),
            "delay"    => entry.delay = value.parse()?,
            "commands" => entry.commands = parse_text_commands(value)?,
            _          => return Err(format!("unknown text key: {key}").into()),
        }
    }

    if entry.target.is_empty() {
        return Err("[text] requires target".into());
    }
    if entry.commands.is_empty() {
        return Err(format!("text entry for {} requires commands", entry.target).into());
    }

    Ok(entry)
}

fn parse_timed_event(block: &Block) -> Result<TimedEvent> {
    let mut event = TimedEvent::default();

    for (key, value) in &block.pairs {
        match key.as_str() {
            "target" => event.target = value.clone(),
            "after"  => event.after = parse_duration(value)?,
            "action" => {
                event.action = match value.to_ascii_lowercase().as_str() {
                    "reload"   => RefreshAction::Reload,
                    "relaunch" => RefreshAction::Relaunch,
                    _          => return Err(format!("unknown refresh action: {value}").into()),
                }
            }
            "repeat" => event.repeat = parse_bool(value)?,
            _ => return Err(format!("unknown refresh key: {key}").into()),
        }
    }

    if event.target.is_empty() {
        return Err("[refresh] requires target".into());
    }
    if event.after.seconds == 0 {
        return Err(format!("refresh for {} requires after", event.target).into());
    }

    Ok(event)
}

fn parse_rerun(block: &Block) -> Result<DurationSpec> {
    for (key, value) in &block.pairs {
        if key == "after" {
            return parse_duration(value);
        }
        return Err(format!("unknown rerun key: {key}").into());
    }

    Err("[rerun] requires after".into())
}

fn parse_text_commands(value: &str) -> Result<Vec<TextCommand>> {
    let mut commands = Vec::new();
    for segment in value.split(';') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let Some((kind, payload)) = segment.split_once(':') else {
            return Err(format!("text command must be kind:value: {segment}").into());
        };
        match kind.trim().to_ascii_lowercase().as_str() {
            "type"     => commands.push(TextCommand::Type(payload.trim().to_string())),
            "type_env" => commands.push(TextCommand::TypeEnv(payload.trim().to_string())),
            "key"      => commands.push(TextCommand::Key(payload.trim().to_string())),
            _          => return Err(format!("unknown text command: {kind}").into()),
        }
    }
    Ok(commands)
}

fn parse_duration(value: &str) -> Result<DurationSpec> {
    let trimmed = value.trim();
    let digits = trimmed.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    let unit = trimmed[digits.len()..].to_ascii_lowercase();
    let amount: u64 = digits.parse()?;
    let multiplier = match unit.as_str() {
        "" | "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 60 * 60,
        _ => return Err(format!("unsupported duration unit: {trimmed}").into()),
    };

    Ok(DurationSpec {
        raw: trimmed.to_string(),
        seconds: amount * multiplier,
    })
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(format!("invalid bool: {value}").into()),
    }
}

fn validate_config(config: &Config) -> Result<()> {
    for entry in &config.text_entries {
        find_app(config, &entry.target)?;
    }
    for event in &config.refreshes {
        find_app(config, &event.target)?;
    }
    Ok(())
}

fn generate_script(config: &Config) -> Result<String> {
    let mut script = String::new();
    writeln!(script, "#!/usr/bin/env sh")?;
    writeln!(script, "set -eu")?;
    writeln!(script)?;
    writeln!(script, "SELF=\"$0\"")?;
    writeln!(
        script,
        "STATE_DIR=\"${{XDG_RUNTIME_DIR:-/tmp}}/i3-controller-$(basename \"$SELF\" | tr -c 'A-Za-z0-9._-' '-')\""
    )?;
    writeln!(script, "mkdir -p \"$STATE_DIR\"")?;
    writeln!(script, "for pid_file in \"$STATE_DIR\"/*.pid; do")?;
    writeln!(script, "  [ -e \"$pid_file\" ] || continue")?;
    writeln!(script, "  old_pid=$(cat \"$pid_file\" 2>/dev/null || true)")?;
    writeln!(
        script,
        "  if [ -n \"$old_pid\" ] && [ \"$old_pid\" != \"$$\" ]; then kill \"$old_pid\" >/dev/null 2>&1 || true; fi"
    )?;
    writeln!(script, "  rm -f \"$pid_file\"")?;
    writeln!(script, "done")?;
    writeln!(
        script,
        "command -v i3-msg >/dev/null 2>&1 || {{ echo 'i3-msg is required' >&2; exit 1; }}"
    )?;
    writeln!(
        script,
        "command -v xdotool >/dev/null 2>&1 || {{ echo 'xdotool is required' >&2; exit 1; }}"
    )?;
    writeln!(script)?;

    for kill in &config.kills {
        let label = if kill.name.is_empty() {
            &kill.pattern
        } else {
            &kill.name
        };
        writeln!(script, "echo {}", shell_quote(&format!("stopping {label}")))?;
        writeln!(
            script,
            "pkill -{} -f {} >/dev/null 2>&1 || true",
            shell_word(&kill.signal),
            shell_quote(&kill.pattern)
        )?;
    }

    if !config.kills.is_empty() {
        writeln!(script)?;
    }

    for app in &config.apps {
        write_launch_app(&mut script, app)?;
    }

    for entry in &config.text_entries {
        let app = find_app(config, &entry.target)?;
        write_text_entry(&mut script, app, entry)?;
    }

    for event in &config.refreshes {
        let app = find_app(config, &event.target)?;
        write_timed_event(&mut script, app, event)?;
    }

    if let Some(duration) = &config.rerun {
        writeln!(script, "# Rerun the full script after {}.", duration.raw)?;
        writeln!(
            script,
            "( sleep {}; exec \"$SELF\" ) >/tmp/i3-controller-rerun.log 2>&1 &",
            duration.seconds
        )?;
        writeln!(script, "echo $! > \"$STATE_DIR/rerun.pid\"")?;
    }

    Ok(script)
}

fn write_launch_app(script: &mut String, app: &App) -> Result<()> {
    writeln!(
        script,
        "echo {}",
        shell_quote(&format!("launching {}", app.name))
    )?;
    if !app.workspace.is_empty() {
        writeln!(
            script,
            "i3-msg {}",
            shell_quote(&format!("workspace {}", app.workspace))
        )?;
    }

    match app.kind {
        AppKind::Native => {
            let command = join_command(&app.command, &app.args);
            writeln!(
                script,
                "i3-msg {}",
                shell_quote(&format!("exec --no-startup-id {command}"))
            )?;
        }
        AppKind::Web => {
            let mut command = format!(
                "{} --app={} --no-first-run",
                shell_word(&app.browser),
                shell_word(&app.url)
            );
            if !app.profile.is_empty() {
                write!(command, " --user-data-dir={}", shell_word(&app.profile))?;
            }
            writeln!(
                script,
                "i3-msg {}",
                shell_quote(&format!("exec --no-startup-id {command}"))
            )?;
        }
    }

    let delay = app.startup_delay.max(1);
    writeln!(script, "sleep {delay}")?;

    if !app.match_criteria.is_empty() {
        if !app.workspace.is_empty() {
            writeln!(
                script,
                "i3-msg {}",
                shell_quote(&format!(
                    "[{}] move to workspace {}",
                    app.match_criteria, app.workspace
                ))
            )?;
        }
        for command in app
            .layout
            .split(';')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            writeln!(
                script,
                "i3-msg {}",
                shell_quote(&format!("[{}] {command}", app.match_criteria))
            )?;
        }
    }

    writeln!(script)?;
    Ok(())
}

fn write_text_entry(script: &mut String, app: &App, entry: &TextEntry) -> Result<()> {
    writeln!(script, "# Text-entry automation for {}.", app.name)?;
    writeln!(script, "( sleep {}", entry.delay)?;
    focus_app(script, app)?;
    for command in &entry.commands {
        match command {
            TextCommand::Type(text) => {
                writeln!(script, "  xdotool type --delay 20 -- {}", shell_quote(text))?
            }
            TextCommand::TypeEnv(name) => {
                validate_env_name(name)?;
                writeln!(script, "  xdotool type --delay 20 -- \"${{{name}:-}}\"")?;
            }
            TextCommand::Key(key) => writeln!(script, "  xdotool key {}", shell_word(key))?,
        }
    }
    writeln!(
        script,
        ") >/tmp/i3-controller-{}-text.log 2>&1 &",
        file_safe(&app.name)
    )?;
    writeln!(
        script,
        "echo $! > \"$STATE_DIR/{}-text.pid\"",
        file_safe(&app.name)
    )?;
    writeln!(script)?;
    Ok(())
}

fn write_timed_event(script: &mut String, app: &App, event: &TimedEvent) -> Result<()> {
    writeln!(
        script,
        "# Timed {} for {} after {}.",
        match event.action {
            RefreshAction::Reload => "reload",
            RefreshAction::Relaunch => "relaunch",
        },
        app.name,
        event.after.raw
    )?;
    if event.repeat {
        writeln!(script, "(")?;
        writeln!(script, "  while true; do")?;
        writeln!(script, "    sleep {}", event.after.seconds)?;
        write_refresh_body(script, app, event, "    ")?;
        writeln!(script, "  done")?;
        writeln!(
            script,
            ") >/tmp/i3-controller-{}-refresh.log 2>&1 &",
            file_safe(&app.name)
        )?;
        writeln!(
            script,
            "echo $! > \"$STATE_DIR/{}-refresh.pid\"",
            file_safe(&app.name)
        )?;
    } else {
        writeln!(script, "( sleep {}", event.after.seconds)?;
        write_refresh_body(script, app, event, "  ")?;
        writeln!(
            script,
            ") >/tmp/i3-controller-{}-refresh.log 2>&1 &",
            file_safe(&app.name)
        )?;
        writeln!(
            script,
            "echo $! > \"$STATE_DIR/{}-refresh.pid\"",
            file_safe(&app.name)
        )?;
    }
    writeln!(script)?;
    Ok(())
}

fn write_refresh_body(
    script: &mut String,
    app: &App,
    event: &TimedEvent,
    indent: &str,
) -> Result<()> {
    match event.action {
        RefreshAction::Reload => {
            focus_app_with_indent(script, app, indent)?;
            writeln!(script, "{indent}xdotool key ctrl+r")?;
        }
        RefreshAction::Relaunch => {
            if !app.match_criteria.is_empty() {
                writeln!(
                    script,
                    "{indent}i3-msg {}",
                    shell_quote(&format!("[{}] kill", app.match_criteria))
                )?;
            }
            write!(script, "{indent}")?;
            write_launch_app(script, app)?;
        }
    }
    Ok(())
}

fn focus_app(script: &mut String, app: &App) -> Result<()> {
    focus_app_with_indent(script, app, "  ")
}

fn focus_app_with_indent(script: &mut String, app: &App, indent: &str) -> Result<()> {
    if !app.match_criteria.is_empty() {
        writeln!(
            script,
            "{indent}i3-msg {}",
            shell_quote(&format!("[{}] focus", app.match_criteria))
        )?;
        writeln!(script, "{indent}sleep 1")?;
    }
    Ok(())
}

fn find_app<'a>(config: &'a Config, name: &str) -> Result<&'a App> {
    config
        .apps
        .iter()
        .find(|app| app.name == name)
        .ok_or_else(|| format!("unknown app target: {name}").into())
}

fn join_command(command: &str, args: &str) -> String {
    if args.trim().is_empty() {
        shell_word(command)
    } else {
        format!("{} {}", shell_word(command), args.trim())
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn shell_word(value: &str) -> String {
    if value.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | ':' | '=' | '@' | '+')
    }) {
        value.to_string()
    } else {
        shell_quote(value)
    }
}

fn file_safe(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn validate_env_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("empty environment variable name".into());
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return Err(format!("invalid environment variable name: {name}").into());
    }
    if chars.all(|c| c == '_' || c.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(format!("invalid environment variable name: {name}").into())
    }
}

#[allow(dead_code)]
fn ensure_executable_path(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)?;
    if metadata.is_file() {
        Ok(())
    } else {
        Err(format!("not a file: {}", path.display()).into())
    }
}
