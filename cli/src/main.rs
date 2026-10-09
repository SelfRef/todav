//! todav — debugging/scripting front-end for todav-core.
//! Account comes from TODAV_URL / TODAV_USER / TODAV_PASS (or a .env file in the cwd).

use std::process::exit;
use todav_core::{Client, List};

const USAGE: &str = "usage: todav <command>
  lists                         show lists
  ls <list> [-a]                show tasks (-a: include done)
  add <list> <summary> [-c cat] add a task
  done <uid> | undo <uid>       tick / untick
  rm <uid>                      delete
  sync                          push + pull everything
  push-register <resource>      register a web-push endpoint for every list
  listen                        ntfy (NTFY_URL) push loop → sync";

fn main() {
    load_dotenv();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        exit(1);
    }
}

fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::env::var("TODAV_DATA").unwrap_or_else(|_| {
        let base = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
            format!("{}/.local/share", std::env::var("HOME").unwrap_or_default())
        });
        format!("{base}/todav-cli")
    });
    let client = Client::open(data)?;
    let env = |k: &str| std::env::var(k).map_err(|_| format!("{k} not set"));
    client.set_account(env("TODAV_URL")?, env("TODAV_USER")?, env("TODAV_PASS")?)?;

    let arg = |i: usize| args.get(i).cloned().ok_or(USAGE);
    match args.first().map(String::as_str) {
        Some("lists") => {
            for l in client.lists() {
                println!("{:>3}  {}  ({})", l.open_count, l.display_name, l.href);
            }
        }
        Some("ls") => {
            let list = find_list(&client, &arg(1)?)?;
            let line = |t: &todav_core::Task| {
                let indent = if t.parent_uid.is_some() { "  ↳ " } else { "" };
                let mark = if t.done { "[x]" } else { "[ ]" };
                println!("  {mark} {indent}{}  {}", t.summary, t.uid);
            };
            for g in client.grouped(list.href.clone()) {
                println!("{}", g.name.as_deref().unwrap_or("Other"));
                g.tasks.iter().for_each(line);
            }
            if args.iter().any(|a| a == "-a") {
                println!("Done");
                client.finished(list.href).iter().for_each(line);
            }
        }
        Some("add") => {
            let list = find_list(&client, &arg(1)?)?;
            let cat = args
                .iter()
                .position(|a| a == "-c")
                .and_then(|i| args.get(i + 1))
                .cloned();
            let t = client.add_task(list.href, arg(2)?, cat, None, false)?;
            println!("{}", t.uid);
            println!("{:?}", client.sync()?);
        }
        Some(c @ ("done" | "undo")) => {
            client.set_done(arg(1)?, c == "done")?;
            println!("{:?}", client.sync()?);
        }
        Some("rm") => {
            client.delete_task(arg(1)?)?;
            println!("{:?}", client.sync()?);
        }
        Some("sync") => println!("{:?}", client.sync()?),
        Some("push-register") => {
            client.sync()?;
            for r in client.push_register(arg(1)?, 7 * 86400)? {
                println!(
                    "{} → {} (expires {})",
                    r.list_href, r.registration_href, r.expires
                );
            }
        }
        Some("listen") => {
            println!("{:?}", client.sync()?);
            client.listen(env("NTFY_URL")?, &|r| match r {
                Ok(r) => println!("{r:?}"),
                Err(e) => eprintln!("error: {e}"),
            })?;
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

fn find_list(client: &Client, name: &str) -> Result<List, String> {
    client
        .lists()
        .into_iter()
        .find(|l| l.href == name || l.display_name.eq_ignore_ascii_case(name))
        .ok_or_else(|| format!("no list '{name}' (run `todav sync` first?)"))
}

fn load_dotenv() {
    let Ok(text) = std::fs::read_to_string(".env") else {
        return;
    };
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=')
            && !k.trim_start().starts_with('#')
            && std::env::var_os(k.trim()).is_none()
        {
            // SAFETY: single-threaded, before anything else runs.
            unsafe { std::env::set_var(k.trim(), v.trim()) };
        }
    }
}
