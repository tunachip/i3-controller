use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const SAMPLE_CONFIG: &str = r#"# i3-controller config
# Blocks start with [app], [kill], [text], [refresh], or [rerun].
# Values are key=value. Comments and blank lines are ignored.
# Text commands support type:literal text, type_env:ENV_VAR, key:KeyName, and wait:2.

[kill]
name=old chromium sessions
pattern=chromium-browser --app=
signal=TERM

[app]
name=dashboard
kind=web
url=https://status.example.test
browser=chromium-browser
browser_args=--ignore-certificate-errors
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
    browser_args: String,
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
    Wait(u64),
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
        Some("wizard") => {
            let path = args
                .next()
                .unwrap_or_else(|| "i3-controller.conf".to_string());
            run_wizard(Path::new(&path))?;
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
    println!("  i3-controller wizard [config]");
    println!("  i3-controller generate <config> [output]\n");

    println!("Generated scripts require i3-msg, xdotool, pkill, and a POSIX shell.");
    println!(
        "The wizard can preview layouts from inside i3 and can import Chromium URLs when Chromium exposes a DevTools port."
    );
}

fn run_wizard(path: &Path) -> Result<()> {
    println!("i3-controller interactive config creator\n");
    println!(
        "Press Enter to accept defaults. Slow, explicit delays are preferred for lab machines.\n"
    );

    let mut config =
        if path.exists() && yes_no(&format!("Load existing {}?", path.display()), true)? {
            parse_config(&fs::read_to_string(path)?)?
        } else {
            Config::default()
        };

    if yes_no("Add process kill rules before launching apps?", false)? {
        loop {
            let pattern = prompt("Process pattern to stop", "")?;
            if pattern.is_empty() {
                break;
            }
            config.kills.push(KillRule {
                name: prompt("Friendly name", &pattern)?,
                pattern,
                signal: prompt("Signal", "TERM")?,
            });
            if !yes_no("Add another kill rule?", false)? {
                break;
            }
        }
    }

    loop {
        let name = prompt("App name", "")?;
        if name.is_empty() {
            if config.apps.is_empty() {
                println!("At least one app is required.");
                continue;
            }
            break;
        }

        let kind = choose("App type", &["web", "native"], "web")?;
        let mut app = App {
            name,
            kind: if kind == "web" {
                AppKind::Web
            } else {
                AppKind::Native
            },
            browser: "chromium-browser".to_string(),
            startup_delay: 5,
            ..App::default()
        };

        if app.kind == AppKind::Web {
            app.browser = prompt("Browser command", "chromium-browser")?;
            app.browser_args = prompt("Extra browser flags", "")?;
            app.url = prompt_required_value("Web app URL", &choose_web_url()?)?;
            app.profile = prompt(
                "Dedicated browser profile path",
                &format!("/tmp/i3-controller-{}", file_safe(&app.name)),
            )?;
        } else {
            app.command = prompt_required("Command")?;
            println!(
                "Args are passed through a shell. For terminals, use something like: -e sh -lc 'cd ~/lab && ./plc_pinger.sh; exec sh'"
            );
            app.args = prompt("Arguments", "")?;
        }

        app.workspace = prompt(
            "Workspace",
            &format!("i3-controller-{}", file_safe(&app.name)),
        )?;
        app.match_criteria = prompt("i3 match criteria, for example title=\"Dashboard\"", "")?;
        if app.match_criteria.is_empty() {
            app.match_criteria = prompt("Window title to match exactly", "")?;
            if !app.match_criteria.is_empty() {
                app.match_criteria =
                    format!("title=\"{}\"", app.match_criteria.replace('"', "\\\""));
            }
        }
        app.layout = prompt(
            "Layout commands separated by semicolons",
            "move position 0 0; resize set 1280 720",
        )?;
        app.startup_delay = prompt_u64("Seconds to wait after launching", 5)?;

        config.apps.push(app);

        if yes_no("Add login/text-entry automation for this app?", false)? {
            let app_name = config
                .apps
                .last()
                .expect("app was just pushed")
                .name
                .clone();
            let commands = build_text_commands()?;
            if !commands.is_empty() {
                config.text_entries.push(TextEntry {
                    target: app_name,
                    delay: prompt_u64("Seconds to wait before text entry starts", 5)?,
                    commands,
                });
            }
        }

        if yes_no("Add timed refresh/relaunch for this app?", false)? {
            let app_name = config
                .apps
                .last()
                .expect("app was just pushed")
                .name
                .clone();
            let action = choose("Timed action", &["reload", "relaunch"], "reload")?;
            config.refreshes.push(TimedEvent {
                target: app_name,
                after: parse_duration(&prompt("Run action after", "30m")?)?,
                action: if action == "relaunch" {
                    RefreshAction::Relaunch
                } else {
                    RefreshAction::Reload
                },
                repeat: yes_no("Repeat this timed action forever?", true)?,
            });
        }

        if command_exists("i3-msg")
            && yes_no("Preview current config in an i3 test workspace?", false)?
        {
            preview_config(&config)?;
        }

        if !yes_no("Add another app?", true)? {
            break;
        }
    }

    if yes_no("Rerun the full script on a timer?", false)? {
        config.rerun = Some(parse_duration(&prompt("Rerun after", "6h")?)?);
    }

    validate_config(&config)?;
    let text = config_to_string(&config)?;
    fs::write(path, text)?;
    println!("wrote {}", path.display());

    if yes_no("Generate launch script beside the config?", true)? {
        let script_path = path.with_extension("sh");
        fs::write(&script_path, generate_script(&config)?)?;
        println!("wrote {}", script_path.display());
    }

    Ok(())
}

fn choose_web_url() -> Result<String> {
    let urls = chromium_debug_urls();
    if urls.is_empty() {
        println!(
            "No Chromium DevTools URLs found. To enable import, launch Chromium with --remote-debugging-port=9222."
        );
        return Ok(String::new());
    }

    println!("Open Chromium URLs:");
    for (index, url) in urls.iter().enumerate() {
        println!("  {}. {}", index + 1, url);
    }
    let choice = prompt("Pick a URL number or enter a URL", "1")?;
    if let Ok(index) = choice.parse::<usize>() {
        if let Some(url) = urls.get(index.saturating_sub(1)) {
            return Ok(url.clone());
        }
    }
    Ok(choice)
}

fn build_text_commands() -> Result<Vec<TextCommand>> {
    println!("Build text-entry steps. Use explicit waits when pages are slow.");
    println!("Step types: type, env, key, wait, record, done.");
    println!("Recording uses xinput/xmodmap when available and stops when you press End.\n");

    let mut commands = Vec::new();
    loop {
        let step = choose(
            "Step type",
            &["type", "key", "wait", "env", "record", "done"],
            "key",
        )?;
        match step.as_str() {
            "type" => commands.push(TextCommand::Type(prompt("Text to type", "")?)),
            "env" => {
                let name = prompt("Environment variable name", "")?;
                validate_env_name(&name)?;
                commands.push(TextCommand::TypeEnv(name));
            }
            "key" => commands.push(TextCommand::Key(prompt("xdotool key name", "Tab")?)),
            "wait" => commands.push(TextCommand::Wait(prompt_u64("Wait seconds", 2)?)),
            "record" => commands.extend(record_key_commands()?),
            "done" => break,
            _ => unreachable!(),
        }
    }
    Ok(commands)
}

fn record_key_commands() -> Result<Vec<TextCommand>> {
    if !command_exists("xinput") || !command_exists("xmodmap") {
        println!("Recording requires xinput and xmodmap. Add key steps manually on this machine.");
        return Ok(Vec::new());
    }

    println!("Focus the target app, then press Enter here to start recording.");
    println!("Press End to finish recording. Add wait steps afterward if the app needs time.");
    let mut ignored = String::new();
    io::stdin().read_line(&mut ignored)?;

    let keymap = x_keymap()?;
    let mut child = Command::new("xinput")
        .args(["test-xi2", "--root"])
        .stdout(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().ok_or("failed to read xinput output")?;
    let reader = BufReader::new(stdout);
    let mut commands = Vec::new();
    let mut key_press = false;

    for line in reader.lines() {
        let line = line?;
        if line.contains("(KeyPress)") || line.contains("RawKeyPress") {
            key_press = true;
            continue;
        }
        if !key_press {
            continue;
        }
        let Some(code) = parse_xinput_detail(&line) else {
            continue;
        };
        key_press = false;
        let key = keymap.get(&code).cloned().unwrap_or(code);
        if key == "End" {
            let _ = child.kill();
            let _ = child.wait();
            println!("Recorded {} key steps.", commands.len());
            return Ok(commands);
        }
        if !is_modifier_key(&key) {
            commands.push(TextCommand::Key(key));
        }
    }

    let _ = child.wait();
    Ok(commands)
}

fn x_keymap() -> Result<std::collections::HashMap<String, String>> {
    let output = Command::new("xmodmap").arg("-pke").output()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        let Some((left, right)) = line.split_once('=') else {
            continue;
        };
        let Some(code) = left.split_whitespace().nth(1) else {
            continue;
        };
        let Some(name) = right.split_whitespace().find(|name| *name != "NoSymbol") else {
            continue;
        };
        map.insert(code.to_string(), name.to_string());
    }
    Ok(map)
}

fn parse_xinput_detail(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let value = trimmed.strip_prefix("detail:")?.trim();
    value.split_whitespace().next().map(str::to_string)
}

fn is_modifier_key(key: &str) -> bool {
    matches!(
        key,
        "Shift_L"
            | "Shift_R"
            | "Control_L"
            | "Control_R"
            | "Alt_L"
            | "Alt_R"
            | "Meta_L"
            | "Meta_R"
            | "Super_L"
            | "Super_R"
    )
}

fn preview_config(config: &Config) -> Result<()> {
    let workspace = "i3-controller-preview";
    let preview_path = env::temp_dir().join("i3-controller-preview.sh");
    let mut preview = Config {
        apps: Vec::new(),
        kills: Vec::new(),
        text_entries: Vec::new(),
        refreshes: Vec::new(),
        rerun: None,
    };
    for app in &config.apps {
        preview.apps.push(App {
            name: app.name.clone(),
            kind: if app.kind == AppKind::Web {
                AppKind::Web
            } else {
                AppKind::Native
            },
            command: app.command.clone(),
            args: app.args.clone(),
            url: app.url.clone(),
            browser: app.browser.clone(),
            browser_args: app.browser_args.clone(),
            profile: app.profile.clone(),
            workspace: workspace.to_string(),
            match_criteria: app.match_criteria.clone(),
            layout: app.layout.clone(),
            startup_delay: app.startup_delay,
        });
    }
    fs::write(&preview_path, generate_script(&preview)?)?;
    println!("Switching to workspace {workspace} and running a preview.");
    let _ = Command::new("i3-msg")
        .arg(format!("workspace {workspace}"))
        .status();
    Command::new("sh").arg(&preview_path).status()?;
    println!("Preview launched. Inspect the workspace before finalizing.");
    Ok(())
}

fn config_to_string(config: &Config) -> Result<String> {
    let mut output = String::new();
    writeln!(output, "# i3-controller config")?;
    writeln!(output, "# Created by `i3-controller wizard`.")?;
    writeln!(output)?;

    for kill in &config.kills {
        writeln!(output, "[kill]")?;
        write_pair(&mut output, "name", &kill.name)?;
        write_pair(&mut output, "pattern", &kill.pattern)?;
        write_pair(&mut output, "signal", &kill.signal)?;
        writeln!(output)?;
    }

    for app in &config.apps {
        writeln!(output, "[app]")?;
        write_pair(&mut output, "name", &app.name)?;
        write_pair(
            &mut output,
            "kind",
            if app.kind == AppKind::Web {
                "web"
            } else {
                "native"
            },
        )?;
        match app.kind {
            AppKind::Web => {
                write_pair(&mut output, "url", &app.url)?;
                write_pair(&mut output, "browser", &app.browser)?;
                write_pair(&mut output, "browser_args", &app.browser_args)?;
                write_pair(&mut output, "profile", &app.profile)?;
            }
            AppKind::Native => {
                write_pair(&mut output, "command", &app.command)?;
                write_pair(&mut output, "args", &app.args)?;
            }
        }
        write_pair(&mut output, "workspace", &app.workspace)?;
        write_pair(&mut output, "match", &app.match_criteria)?;
        write_pair(&mut output, "layout", &app.layout)?;
        write_pair(&mut output, "startup_delay", &app.startup_delay.to_string())?;
        writeln!(output)?;
    }

    for entry in &config.text_entries {
        writeln!(output, "[text]")?;
        write_pair(&mut output, "target", &entry.target)?;
        write_pair(&mut output, "delay", &entry.delay.to_string())?;
        write_pair(
            &mut output,
            "commands",
            &text_commands_to_string(&entry.commands),
        )?;
        writeln!(output)?;
    }

    for event in &config.refreshes {
        writeln!(output, "[refresh]")?;
        write_pair(&mut output, "target", &event.target)?;
        write_pair(&mut output, "after", &event.after.raw)?;
        write_pair(
            &mut output,
            "action",
            if event.action == RefreshAction::Relaunch {
                "relaunch"
            } else {
                "reload"
            },
        )?;
        write_pair(
            &mut output,
            "repeat",
            if event.repeat { "true" } else { "false" },
        )?;
        writeln!(output)?;
    }

    if let Some(rerun) = &config.rerun {
        writeln!(output, "[rerun]")?;
        write_pair(&mut output, "after", &rerun.raw)?;
    }

    Ok(output)
}

fn write_pair(output: &mut String, key: &str, value: &str) -> Result<()> {
    if !value.is_empty() {
        writeln!(output, "{key}={value}")?;
    }
    Ok(())
}

fn text_commands_to_string(commands: &[TextCommand]) -> String {
    commands
        .iter()
        .map(|command| match command {
            TextCommand::Type(text) => format!("type:{text}"),
            TextCommand::TypeEnv(name) => format!("type_env:{name}"),
            TextCommand::Key(key) => format!("key:{key}"),
            TextCommand::Wait(seconds) => format!("wait:{seconds}"),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn chromium_debug_urls() -> Vec<String> {
    let Ok(mut stream) = TcpStream::connect("127.0.0.1:9222") else {
        return Vec::new();
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(700)));
    let _ = stream
        .write_all(b"GET /json HTTP/1.1\r\nHost: 127.0.0.1:9222\r\nConnection: close\r\n\r\n");
    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() {
        return Vec::new();
    }

    let mut urls = Vec::new();
    let mut rest = response.as_str();
    while let Some(start) = rest.find("\"url\"") {
        rest = &rest[start + 5..];
        let Some(colon) = rest.find(':') else { break };
        rest = rest[colon + 1..].trim_start();
        if !rest.starts_with('"') {
            continue;
        }
        rest = &rest[1..];
        let Some(end) = rest.find('"') else { break };
        let url = unescape_json_string(&rest[..end]);
        rest = &rest[end + 1..];
        if url.starts_with("http://") || url.starts_with("https://") {
            urls.push(url);
        }
    }
    urls.sort();
    urls.dedup();
    urls
}

fn unescape_json_string(value: &str) -> String {
    value
        .replace("\\/", "/")
        .replace("\\\"", "\"")
        .replace("\\\\", "\\")
}

fn prompt(label: &str, default: &str) -> Result<String> {
    if default.is_empty() {
        print!("{label}: ");
    } else {
        print!("{label} [{default}]: ");
    }
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let value = input.trim();
    if value.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(value.to_string())
    }
}

fn prompt_required(label: &str) -> Result<String> {
    prompt_required_value(label, "")
}

fn prompt_required_value(label: &str, default: &str) -> Result<String> {
    loop {
        let value = prompt(label, default)?;
        if !value.trim().is_empty() {
            return Ok(value);
        }
        println!("{label} is required.");
    }
}

fn prompt_u64(label: &str, default: u64) -> Result<u64> {
    loop {
        let value = prompt(label, &default.to_string())?;
        match value.parse() {
            Ok(number) => return Ok(number),
            Err(_) => println!("Enter a whole number."),
        }
    }
}

fn yes_no(label: &str, default: bool) -> Result<bool> {
    let default_text = if default { "Y/n" } else { "y/N" };
    loop {
        let value = prompt(label, default_text)?;
        match value.to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            "y/n" => return Ok(default),
            _ => println!("Answer yes or no."),
        }
    }
}

fn choose(label: &str, choices: &[&str], default: &str) -> Result<String> {
    loop {
        let value = prompt(&format!("{label} ({})", choices.join("/")), default)?;
        if choices.contains(&value.as_str()) {
            return Ok(value);
        }
        println!("Choose one of: {}", choices.join(", "));
    }
}

fn command_exists(command: &str) -> bool {
    env::var_os("PATH")
        .and_then(|paths| {
            env::split_paths(&paths)
                .map(|path| path.join(command))
                .find(|path| path.is_file())
        })
        .is_some()
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

        block
            .pairs
            .push((key.trim().to_ascii_lowercase(), value.trim().to_string()));
    }

    if let Some(block) = current {
        blocks.push(block);
    }

    let mut config = Config::default();
    for block in blocks {
        match block.name.as_str() {
            "app" => config.apps.push(parse_app(&block)?),
            "kill" => config.kills.push(parse_kill(&block)?),
            "text" => config.text_entries.push(parse_text_entry(&block)?),
            "refresh" => config.refreshes.push(parse_timed_event(&block)?),
            "rerun" => config.rerun = Some(parse_rerun(&block)?),
            other => return Err(format!("unknown block [{other}]").into()),
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
                    "web" => AppKind::Web,
                    _ => return Err(format!("unknown app kind for {}: {value}", app.name).into()),
                }
            }
            "command" => app.command = value.clone(),
            "args" => app.args = value.clone(),
            "url" => app.url = value.clone(),
            "browser" => app.browser = value.clone(),
            "browser_args" => app.browser_args = value.clone(),
            "profile" => app.profile = value.clone(),
            "workspace" => app.workspace = value.clone(),
            "match" => app.match_criteria = value.clone(),
            "layout" => app.layout = value.clone(),
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
            "name" => kill.name = value.clone(),
            "pattern" => kill.pattern = value.clone(),
            "signal" => kill.signal = value.clone(),
            _ => return Err(format!("unknown kill key: {key}").into()),
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
            "target" => entry.target = value.clone(),
            "delay" => entry.delay = value.parse()?,
            "commands" => entry.commands = parse_text_commands(value)?,
            _ => return Err(format!("unknown text key: {key}").into()),
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
            "after" => event.after = parse_duration(value)?,
            "action" => {
                event.action = match value.to_ascii_lowercase().as_str() {
                    "reload" => RefreshAction::Reload,
                    "relaunch" => RefreshAction::Relaunch,
                    _ => return Err(format!("unknown refresh action: {value}").into()),
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
            "type" => commands.push(TextCommand::Type(payload.trim().to_string())),
            "type_env" => commands.push(TextCommand::TypeEnv(payload.trim().to_string())),
            "key" => commands.push(TextCommand::Key(payload.trim().to_string())),
            "wait" => commands.push(TextCommand::Wait(payload.trim().parse()?)),
            _ => return Err(format!("unknown text command: {kind}").into()),
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
    for app in &config.apps {
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
    }
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
    writeln!(
        script,
        "command -v setsid >/dev/null 2>&1 || {{ echo 'setsid is required' >&2; exit 1; }}"
    )?;
    writeln!(script)?;
    writeln!(script, "i3() {{")?;
    writeln!(script, "  if ! i3-msg \"$@\"; then")?;
    writeln!(script, "    echo \"i3-controller: i3-msg failed: $*\" >&2")?;
    writeln!(script, "  fi")?;
    writeln!(script, "}}")?;
    writeln!(script)?;
    writeln!(script, "i3_window_ids() {{")?;
    writeln!(
        script,
        "  i3-msg -t get_tree | tr ',{{}}' '\\n\\n\\n' | sed -n 's/.*\"window\":[[:space:]]*\\([0-9][0-9]*\\).*/\\1/p'"
    )?;
    writeln!(script, "}}")?;
    writeln!(script)?;
    writeln!(script, "i3_try_match() {{")?;
    writeln!(script, "  criteria=$1")?;
    writeln!(script, "  command=$2")?;
    writeln!(script, "  attempts=${{3:-30}}")?;
    writeln!(script, "  I3_MATCH_OUTPUT=")?;
    writeln!(script, "  while [ \"$attempts\" -gt 0 ]; do")?;
    writeln!(
        script,
        "    if I3_MATCH_OUTPUT=$(i3-msg \"[$criteria] $command\" 2>&1); then"
    )?;
    writeln!(script, "      return 0")?;
    writeln!(script, "    fi")?;
    writeln!(script, "    attempts=$((attempts - 1))")?;
    writeln!(script, "    [ \"$attempts\" -gt 0 ] && sleep 1")?;
    writeln!(script, "  done")?;
    writeln!(script, "  return 1")?;
    writeln!(script, "}}")?;
    writeln!(script)?;
    writeln!(script, "i3_match() {{")?;
    writeln!(script, "  if ! i3_try_match \"$@\"; then")?;
    writeln!(
        script,
        "    echo \"i3-controller: i3 command failed for [$1]: $2\" >&2"
    )?;
    writeln!(
        script,
        "    [ -n \"$I3_MATCH_OUTPUT\" ] && echo \"$I3_MATCH_OUTPUT\" >&2"
    )?;
    writeln!(script, "  fi")?;
    writeln!(script, "}}")?;
    writeln!(script)?;
    writeln!(script, "i3_app() {{")?;
    writeln!(script, "  mark=$1")?;
    writeln!(script, "  fallback=$2")?;
    writeln!(script, "  command=$3")?;
    writeln!(script, "  attempts=${{4:-30}}")?;
    writeln!(
        script,
        "  if i3_try_match \"con_mark=\\\"$mark\\\"\" \"$command\" \"$attempts\"; then"
    )?;
    writeln!(script, "    return 0")?;
    writeln!(script, "  fi")?;
    writeln!(script, "  if [ -n \"$fallback\" ]; then")?;
    writeln!(
        script,
        "    i3_match \"$fallback\" \"$command\" \"$attempts\""
    )?;
    writeln!(script, "  else")?;
    writeln!(
        script,
        "    echo \"i3-controller: no marked window found for $mark: $command\" >&2"
    )?;
    writeln!(
        script,
        "    [ -n \"$I3_MATCH_OUTPUT\" ] && echo \"$I3_MATCH_OUTPUT\" >&2"
    )?;
    writeln!(script, "  fi")?;
    writeln!(script, "}}")?;
    writeln!(script)?;
    writeln!(script, "i3_mark_new_window() {{")?;
    writeln!(script, "  mark=$1")?;
    writeln!(script, "  before_file=$2")?;
    writeln!(script, "  attempts=${{3:-30}}")?;
    writeln!(script, "  while [ \"$attempts\" -gt 0 ]; do")?;
    writeln!(script, "    for window_id in $(i3_window_ids); do")?;
    writeln!(
        script,
        "      if ! grep -qx \"$window_id\" \"$before_file\" 2>/dev/null; then"
    )?;
    writeln!(
        script,
        "        i3 \"[id=\\\"$window_id\\\"] mark --replace $mark\""
    )?;
    writeln!(
        script,
        "        echo \"$window_id\" > \"$STATE_DIR/$mark.window\""
    )?;
    writeln!(script, "        return 0")?;
    writeln!(script, "      fi")?;
    writeln!(script, "    done")?;
    writeln!(script, "    attempts=$((attempts - 1))")?;
    writeln!(script, "    [ \"$attempts\" -gt 0 ] && sleep 1")?;
    writeln!(script, "  done")?;
    writeln!(
        script,
        "  echo \"i3-controller: failed to identify new window for $mark\" >&2"
    )?;
    writeln!(script, "}}")?;
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

    for app in &config.apps {
        write_configure_app(&mut script, app)?;
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
    let mark = window_mark(app);
    writeln!(
        script,
        "i3_window_ids > \"$STATE_DIR/{}-before.windows\"",
        file_safe(&app.name)
    )?;
    if !app.workspace.is_empty() {
        writeln!(
            script,
            "i3 {}",
            shell_quote(&format!("workspace {}", app.workspace))
        )?;
    }

    match app.kind {
        AppKind::Native => {
            let command = native_exec_command(app);
            let command = detached_exec_command(app, &command);
            writeln!(
                script,
                "i3 {}",
                shell_quote(&format!("exec --no-startup-id {command}"))
            )?;
        }
        AppKind::Web => {
            let mut command = format!(
                "{} --app={} --no-first-run",
                shell_word(&app.browser),
                shell_word(&app.url)
            );
            if !app.browser_args.is_empty() {
                write!(command, " {}", app.browser_args.trim())?;
            }
            if !app.profile.is_empty() {
                write!(command, " --user-data-dir={}", shell_word(&app.profile))?;
            }
            let command = detached_exec_command(app, &command);
            writeln!(
                script,
                "i3 {}",
                shell_quote(&format!("exec --no-startup-id {command}"))
            )?;
        }
    }
    writeln!(
        script,
        "i3_mark_new_window {} \"$STATE_DIR/{}-before.windows\" {}",
        shell_quote(&mark),
        file_safe(&app.name),
        app.startup_delay.max(30)
    )?;

    writeln!(script)?;
    Ok(())
}

fn write_configure_app(script: &mut String, app: &App) -> Result<()> {
    let mark = window_mark(app);
    let fallback = app.match_criteria.trim();
    if !app.match_criteria.is_empty() || !app.layout.is_empty() || !app.workspace.is_empty() {
        writeln!(
            script,
            "echo {}",
            shell_quote(&format!("arranging {}", app.name))
        )?;
        let delay = app.startup_delay.max(1);
        writeln!(script, "sleep {delay}")?;
        writeln!(
            script,
            "i3_app {} {} {}",
            shell_quote(&mark),
            shell_quote(fallback),
            shell_quote("nop")
        )?;
        if !app.workspace.is_empty() {
            writeln!(
                script,
                "i3_app {} {} {}",
                shell_quote(&mark),
                shell_quote(fallback),
                shell_quote(&format!("move to workspace {}", app.workspace))
            )?;
        }
        if layout_requires_floating(&app.layout) {
            writeln!(
                script,
                "i3_app {} {} {}",
                shell_quote(&mark),
                shell_quote(fallback),
                shell_quote("floating enable")
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
                "i3_app {} {} {}",
                shell_quote(&mark),
                shell_quote(fallback),
                shell_quote(command)
            )?;
        }
        writeln!(script)?;
    }

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
            TextCommand::Wait(seconds) => writeln!(script, "  sleep {seconds}")?,
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
                    "{indent}i3 {}",
                    shell_quote(&format!("[{}] kill", app.match_criteria))
                )?;
            }
            write!(script, "{indent}")?;
            write_launch_app(script, app)?;
            write!(script, "{indent}")?;
            write_configure_app(script, app)?;
        }
    }
    Ok(())
}

fn focus_app(script: &mut String, app: &App) -> Result<()> {
    focus_app_with_indent(script, app, "  ")
}

fn focus_app_with_indent(script: &mut String, app: &App, indent: &str) -> Result<()> {
    if !app.match_criteria.is_empty() || !app.name.is_empty() {
        writeln!(
            script,
            "{indent}i3_app {} {} {}",
            shell_quote(&window_mark(app)),
            shell_quote(app.match_criteria.trim()),
            shell_quote("focus")
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

fn native_exec_command(app: &App) -> String {
    if app.args.trim().is_empty() {
        app.command.trim().to_string()
    } else {
        format!("{} {}", app.command.trim(), app.args.trim())
    }
}

fn layout_requires_floating(layout: &str) -> bool {
    layout
        .split(';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .any(|command| command.starts_with("move position") || command.starts_with("resize set"))
}

fn window_mark(app: &App) -> String {
    format!("i3-controller-{}", file_safe(&app.name))
}

fn detached_exec_command(app: &App, command: &str) -> String {
    let log_path = format!("/tmp/i3-controller-{}-launch.log", file_safe(&app.name));
    let detach_log_path = format!("/tmp/i3-controller-{}-detach.log", file_safe(&app.name));
    format!(
        "sh -lc {}",
        shell_quote(&format!(
            "setsid -f sh -lc {} >{} 2>&1 &",
            shell_quote(&format!("exec {command} >>{log_path} 2>&1")),
            shell_word(&detach_log_path)
        ))
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn two_app_config() -> Config {
        Config {
            apps: vec![
                App {
                    name: "first".to_string(),
                    kind: AppKind::Native,
                    command: "alacritty".to_string(),
                    args: "--title first".to_string(),
                    workspace: "1".to_string(),
                    match_criteria: "title=\"first\"".to_string(),
                    layout: "move position 0 0".to_string(),
                    startup_delay: 1,
                    ..App::default()
                },
                App {
                    name: "second".to_string(),
                    kind: AppKind::Native,
                    command: "alacritty".to_string(),
                    args: "--title second".to_string(),
                    workspace: "2".to_string(),
                    match_criteria: "title=\"second\"".to_string(),
                    layout: "move position 1280 0".to_string(),
                    startup_delay: 1,
                    ..App::default()
                },
            ],
            ..Config::default()
        }
    }

    #[test]
    fn generated_launchers_are_explicitly_backgrounded() {
        let script = generate_script(&two_app_config()).expect("script should generate");

        assert!(
            script.contains("setsid -f sh -lc"),
            "script should use setsid for detached launches"
        );
        assert!(
            script.contains("2>&1 &"),
            "detached launcher should be backgrounded so later apps can launch"
        );
    }

    #[test]
    fn generated_i3_commands_are_nonfatal() {
        let script = generate_script(&two_app_config()).expect("script should generate");

        assert!(
            script.contains("i3() {\n  if ! i3-msg \"$@\"; then"),
            "script should wrap i3-msg failures"
        );
        assert!(
            script.contains("i3_match() {"),
            "script should include retry helper for window criteria"
        );
        assert!(
            script.contains("i3_mark_new_window() {"),
            "script should identify and mark newly launched windows"
        );
        assert!(script.contains("i3_mark_new_window 'i3-controller-first'"));
        assert!(script.contains("i3_app 'i3-controller-first' 'title=\"first\"' 'nop'"));
        assert!(
            script.contains("i3_app 'i3-controller-first' 'title=\"first\"' 'floating enable'")
        );
        assert!(
            script.contains("i3_app 'i3-controller-first' 'title=\"first\"' 'move to workspace 1'")
        );
        assert!(script.contains("i3 'exec --no-startup-id"));
        assert!(script.contains("echo 'launching second'"));
    }

    #[test]
    fn generated_script_launches_all_apps_before_arranging() {
        let script = generate_script(&two_app_config()).expect("script should generate");

        let launch_second = script
            .find("echo 'launching second'")
            .expect("second app should be launched");
        let arrange_first = script
            .find("echo 'arranging first'")
            .expect("first app should be arranged");

        assert!(
            launch_second < arrange_first,
            "all apps should launch before any matched layout commands run"
        );
    }
}
