//! `floki-cli run collection.json`: runs a collection with its scripts and exits non-zero on failure, for CI.

use floki::model::{environment_from_postman, environment_to_postman, Environment, Item, KeyValue, Workspace};
use floki::runner::{self, Event, Options, StepResult};
use serde_json::{json, Value};
use std::io::{IsTerminal, Write};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

const USAGE: &str = "Usage: floki-cli run <collection.json> [options]

Runs every request in a Postman or Floki collection export, with its pre-request and
post-response scripts, and exits with status 1 if any request or test failed.

Options:
  -e, --environment <file>        Environment file (Postman or Floki export)
  -g, --globals <file>            Globals file, same format as an environment
  -d, --data <file>               CSV or JSON data file; one iteration per row
  -n, --iterations <n>            Number of iterations (default: one per data row, or 1)
      --folder <name>             Run only the folder with this name
      --delay <ms>                Wait between requests
      --bail                      Stop at the first failed request or test
  -k, --insecure                  Skip TLS certificate verification
      --export-environment <file> Write the environment, with script changes, after the run
      --export-globals <file>     Write the globals after the run
      --reporter-json <file>      Write every result as JSON
  -h, --help                      Show this help
  -V, --version                   Show the version";

#[derive(Default)]
struct Args {
    collection: String,
    environment: Option<String>,
    globals: Option<String>,
    data: Option<String>,
    folder: Option<String>,
    export_environment: Option<String>,
    export_globals: Option<String>,
    reporter_json: Option<String>,
    insecure: bool,
    options: Options,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    match args.next().as_deref() {
        Some("run") => {}
        Some("-h" | "--help") | None => return Err(String::new()),
        Some("-V" | "--version") => {
            println!("floki-cli {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        Some(other) => return Err(format!("Unknown command \"{other}\"")),
    }
    let mut a = Args::default();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "-e" | "--environment" => a.environment = Some(value(&arg)?),
            "-g" | "--globals" => a.globals = Some(value(&arg)?),
            "-d" | "--data" | "--iteration-data" => a.data = Some(value(&arg)?),
            "-n" | "--iterations" | "--iteration-count" => {
                a.options.iterations = value(&arg)?.parse().map_err(|_| format!("{arg} needs a number"))?
            }
            "--folder" => a.folder = Some(value(&arg)?),
            "--delay" | "--delay-request" => a.options.delay_ms = value(&arg)?.parse().map_err(|_| format!("{arg} needs a number"))?,
            "--bail" => a.options.bail = true,
            "-k" | "--insecure" => a.insecure = true,
            "--export-environment" => a.export_environment = Some(value(&arg)?),
            "--export-globals" => a.export_globals = Some(value(&arg)?),
            "--reporter-json" => a.reporter_json = Some(value(&arg)?),
            "-h" | "--help" => return Err(String::new()),
            flag if flag.starts_with('-') => return Err(format!("Unknown option \"{flag}\"")),
            path if a.collection.is_empty() => a.collection = path.into(),
            extra => return Err(format!("Unexpected argument \"{extra}\"")),
        }
    }
    if a.collection.is_empty() {
        return Err("Name a collection file to run".into());
    }
    Ok(a)
}

fn read(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("Could not read {path}: {e}"))
}

fn read_variables(path: &str) -> Result<(String, Vec<KeyValue>), String> {
    let doc: Value = serde_json::from_str(&read(path)?).map_err(|e| format!("{path} is not valid JSON: {e}"))?;
    environment_from_postman(&doc).ok_or(format!("{path} is not an environment export"))
}

fn find_folder(items: &[Item], name: &str) -> Option<u64> {
    items.iter().find_map(|item| match item {
        Item::Folder(f) if f.name == name => Some(f.id),
        Item::Folder(f) => find_folder(&f.items, name),
        Item::Request(_) => None,
    })
}

struct Colors(bool);

impl Colors {
    fn paint(&self, code: &str, text: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }
}

fn print_step(step: &StepResult, c: &Colors) {
    let status = match (&step.status, &step.error, &step.skipped) {
        (Some(code), _, _) => format!("{code} {} · {} ms", step.reason, step.time_ms),
        (_, Some(e), _) => c.paint("31", e),
        (_, _, Some(reason)) => c.paint("2", reason),
        _ => String::new(),
    };
    println!("→ {} {}  {}", c.paint("1", &step.method), step.name, status);
    if step.status.is_some() || step.error.is_some() {
        println!("  {}", c.paint("2", &step.url));
    }
    for log in &step.logs {
        println!("  {} {}", c.paint("2", "│"), log.text);
    }
    for test in &step.tests {
        match (&test.error, test.passed) {
            (_, true) => println!("  {} {}", c.paint("32", "✓"), test.name),
            (Some(e), false) => println!("  {} {} — {}", c.paint("31", "✗"), test.name, c.paint("31", e)),
            (None, false) => println!("  {} {}", c.paint("31", "✗"), test.name),
        }
    }
    for e in &step.script_errors {
        println!("  {} {}", c.paint("31", "!"), c.paint("31", e));
    }
}

fn step_json(s: &StepResult) -> Value {
    json!({
        "iteration": s.iteration, "name": s.name, "method": s.method, "url": s.url,
        "status": s.status, "time_ms": s.time_ms as u64, "size": s.size, "error": s.error, "skipped": s.skipped,
        "tests": s.tests.iter().map(|t| json!({"name": t.name, "passed": t.passed, "error": t.error})).collect::<Vec<_>>(),
        "script_errors": s.script_errors,
        "logs": s.logs.iter().map(|l| &l.text).collect::<Vec<_>>(),
    })
}

fn run(args: Args) -> Result<bool, String> {
    let mut ws = Workspace::default();
    let collection = ws.import_postman(&read(&args.collection)?).map_err(|e| format!("{}: {e}", args.collection))?;
    if args.insecure {
        let mut ids = Vec::new();
        fn collect(items: &[Item], ids: &mut Vec<u64>) {
            for item in items {
                match item {
                    Item::Request(r) => ids.push(r.id),
                    Item::Folder(f) => collect(&f.items, ids),
                }
            }
        }
        collect(&ws.collections[0].items, &mut ids);
        for id in ids {
            if let Some(r) = ws.request_mut(id) {
                r.verify_ssl = false;
            }
        }
    }
    let mut env_name = String::new();
    if let Some(path) = &args.environment {
        let (name, values) = read_variables(path)?;
        env_name = name.clone();
        let id = ws.alloc_id();
        ws.environments.push(Environment { id, name, values });
        ws.active_environment = Some(id);
    }
    if let Some(path) = &args.globals {
        ws.globals = read_variables(path)?.1;
    }
    let mut options = args.options.clone();
    if let Some(path) = &args.data {
        options.data = runner::parse_data(&read(path)?).map_err(|e| format!("{path}: {e}"))?;
    }
    let target = match &args.folder {
        Some(name) => find_folder(&ws.collections[0].items, name).ok_or(format!("No folder named \"{name}\" in the collection"))?,
        None => collection,
    };

    let colors = Colors(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none());
    println!("{}\n", colors.paint("1", &ws.collections[0].name));
    let mut steps = Vec::new();
    let mut outcome = None;
    let mut iteration = usize::MAX;
    runner::run(&ws, target, &options, &AtomicBool::new(false), |event| match event {
        Event::Step(step) => {
            if step.iteration != iteration && options.iteration_count() > 1 {
                iteration = step.iteration;
                println!("{}", colors.paint("2", &format!("Iteration {}/{}", iteration + 1, options.iteration_count())));
            }
            print_step(&step, &colors);
            let _ = std::io::stdout().flush();
            steps.push(step);
        }
        Event::Done { summary, writes, .. } => outcome = Some((summary, writes)),
    });
    let (summary, writes) = outcome.expect("the runner always finishes");
    let first = runner::requests_in(&ws, target).first().map(|(id, _)| *id);
    ws.apply_writes(first, &writes);

    let failed = summary.failed_requests > 0;
    let line = format!(
        "{} requests, {} failed · {} tests, {} failed · {:.1} s",
        summary.requests,
        summary.failed_requests,
        summary.tests,
        summary.failed_tests,
        summary.elapsed.as_secs_f32()
    );
    println!("\n{}", colors.paint(if failed { "31" } else { "32" }, &line));

    if let Some(path) = &args.export_environment {
        let values = ws.environments.first().map(|e| e.values.clone()).unwrap_or_default();
        std::fs::write(path, environment_to_postman(&env_name, &values)).map_err(|e| format!("Could not write {path}: {e}"))?;
    }
    if let Some(path) = &args.export_globals {
        std::fs::write(path, environment_to_postman("Globals", &ws.globals)).map_err(|e| format!("Could not write {path}: {e}"))?;
    }
    if let Some(path) = &args.reporter_json {
        let report = json!({
            "collection": ws.collections[0].name,
            "summary": {
                "iterations": summary.iterations, "requests": summary.requests, "failed_requests": summary.failed_requests,
                "tests": summary.tests, "failed_tests": summary.failed_tests, "duration_ms": summary.elapsed.as_millis() as u64,
            },
            "results": steps.iter().map(step_json).collect::<Vec<_>>(),
        });
        std::fs::write(path, serde_json::to_string_pretty(&report).unwrap_or_default())
            .map_err(|e| format!("Could not write {path}: {e}"))?;
    }
    Ok(!failed)
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(e) if e.is_empty() => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(2)
        }
    }
}
