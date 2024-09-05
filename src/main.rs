use std::{
    collections::HashSet,
    io::Write,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use clap::Parser;
use colored::Colorize;
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderName};
use reqwest::{Client, ClientBuilder, StatusCode, Url};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::{
    sync::mpsc::{self, Sender},
    task,
    time::{sleep, Instant},
};

static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"),);

const MIN_SIZE: usize = 200;

#[derive(Debug, Default)]
struct ResponseResult {
    from: String,
    url: String,
    status: Option<StatusCode>,
    size: Option<usize>,
    error: Option<String>,
    message: Option<String>,
}

fn truncate(s: String, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        None => s,
        Some((idx, _)) => format!("{}...", &s[..idx]),
    }
}

fn log_result(result: ResponseResult, state: &mut ResultState, todo: usize, verbose: bool) {
    let (size_string, size_error) = match result.size {
        Some(s) if s < MIN_SIZE => ((s / 1000).to_string().red(), true),
        Some(s) => ((s / 1000).to_string().green(), false),
        None => ("?".yellow(), true),
    };

    let (status, status_error) = match result.status {
        Some(status) if status.is_success() => (status.to_string().green(), false),
        Some(status) => (status.to_string().red(), true),
        None => ("ERROR".red(), true),
    };

    state.count += 1;

    let details = format!(
        "[{size_string: >5} KB] {} -> {}",
        truncate(result.from, 30),
        truncate(result.url, 60)
    );
    let line = format!(
        " {: <10} {status: <13} {details}",
        format!("[{}/{todo}]", state.count)
    );
    let whitespace = " ".repeat(state.last_len.saturating_sub(line.len()));

    if !status_error && !size_error {
        if verbose {
            println!("{line}");
        } else {
            print!("{line}{whitespace}\r");
        }
        state.last_len = line.len();
    } else {
        eprintln!("{line}{whitespace}");
        state.error_count += 1;
    }

    if verbose {
        if let Some(m) = result.message {
            println!("> {}", m);
        }

        if let Some(r) = result.error {
            println!("! {}", r.red());
        }
    }

    let _ = std::io::stdout().flush();
}

fn base_url(mut url: Url) -> Url {
    match url.path_segments_mut() {
        Ok(mut path) => {
            path.clear();
        }
        Err(error) => panic!("{error:?}"),
    };

    url.set_query(None);

    url
}

async fn extract_urls(body: &str, base: &Url, from: &Url, tx: Sender<Option<(Url, Url)>>) -> usize {
    static HREF: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"<a\s+(?:[^>]*?\s+)?href\s*=\s*(('(?<href_a>.*?)')|("(?<href_b>.*?)"))"#)
            .unwrap()
    });
    let captures = HREF
        .captures_iter(body)
        .filter_map(|r| r.name("href_a").or(r.name("href_b")))
        .map(|v| v.as_str())
        .filter_map(|href| {
            if href.starts_with('#') {
                return None;
            }

            if href.starts_with("http://") || href.starts_with("https://") {
                Url::parse(href).ok()
            } else {
                base.join(href).ok()
            }
        })
        .filter(|url| url.host() == base.host())
        .collect::<Vec<Url>>();

    for capture in &captures {
        tx.send(Some((capture.clone(), from.to_owned())))
            .await
            .unwrap();
    }

    captures.len()
}

async fn fetch(
    url: Url,
    from: Url,
    tx: Sender<Option<(Url, Url)>>,
    client: Client,
    fetch_permit: OwnedSemaphorePermit,
    running_average_response_time: Arc<Mutex<f64>>,
) -> ResponseResult {
    let mut result = ResponseResult {
        from: from.path().to_owned(),
        url: url.as_str().to_owned(),
        ..Default::default()
    };

    let start = Instant::now();
    let possible_response = client.get(url.clone()).send().await;
    drop(fetch_permit);
    let duration = start.elapsed().as_secs_f64();
    let mut running_average = running_average_response_time.lock().await;
    *running_average = *running_average * (9. / 10.) + duration * (1. / 10.);
    drop(running_average);

    let possible_body = match possible_response {
        Ok(response) => {
            result.status = Some(response.status());

            response.text().await
        }
        Err(error) => {
            result.status = error.status();
            result.error = Some(error.to_string());

            return result;
        }
    };

    match possible_body {
        Ok(body) => {
            result.size = Some(body.len());
            let base = base_url(url.clone());
            let count = extract_urls(&body, &base, &url, tx).await;
            if count > 0 {
                result.message = Some(format!("{count} URL's found"));
            }
        }
        Err(error) => {
            result.status = error.status();
            result.error = Some(error.to_string());
        }
    }

    result
}

#[derive(Debug, Default)]
struct ResultState {
    count: usize,
    error_count: usize,
    last_len: usize,
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct CmdLineArgs {
    #[arg(help = "Please provide an URL to check - like https://tweedegolf.nl/")]
    base_url: Url,
    #[arg(short('e'), long, num_args(1..))]
    exclude_pattern: Option<Regex>,
    #[arg(short('H'), long)]
    request_headers: Vec<String>,
    #[arg(short('b'), long)]
    verbose: bool,
    #[arg(default_value = "1000", short, long)]
    max_concurrent: u16,
}

#[tokio::main]
async fn main() {
    let args = CmdLineArgs::parse();
    let url = args.base_url;
    let verbose = args.verbose;

    let mut header_map = HeaderMap::new();
    for header in args.request_headers {
        let key_value: Vec<_> = header.split(':').collect();
        if key_value.len() != 2 {
            panic!("Please make sure to provide any headers as `<key>: <value>` pair, seperated by a colon")
        }
        let header_name = HeaderName::from_bytes(key_value[0].trim().as_bytes()).unwrap();
        header_map.append(header_name, key_value[1].trim().parse().unwrap());
    }

    let client = ClientBuilder::new()
        .connect_timeout(Duration::from_secs(15))
        .danger_accept_invalid_certs(true)
        .default_headers(header_map)
        .user_agent(APP_USER_AGENT)
        .build()
        .unwrap();

    let todo = Arc::new(AtomicUsize::new(0));

    let (tx, mut rx) = mpsc::channel::<Option<(Url, Url)>>(512);
    let (result_tx, mut result_rx) = mpsc::channel::<ResponseResult>(512);

    tx.send(Some((url.clone(), url.clone()))).await.unwrap();

    let output_tx = tx.clone();
    let output_todo = todo.clone();

    let handle = task::spawn(async move {
        let start: Instant = Instant::now();
        let mut state = ResultState::default();

        tokio::time::sleep(Duration::from_secs(1)).await;
        println!(">>> starting {}", url.host_str().unwrap_or_default());

        while let Some(result) = result_rx.recv().await {
            output_todo.fetch_sub(1, Ordering::SeqCst);
            let todo_value = output_todo.load(Ordering::SeqCst);

            log_result(result, &mut state, todo_value, verbose);

            if todo_value == 0 {
                break;
            }
        }

        output_tx.send(None).await.unwrap();
        result_rx.close();

        let duration = start.elapsed();

        let line = format!(
            "<<< finished {}, time elapsed: {:.1}s, total pages: {:?}, {}",
            url.host_str().unwrap_or_default(),
            duration.as_secs_f64(),
            state.count,
            if state.error_count > 0 {
                format!("errors: {}", state.error_count).red()
            } else {
                "no errors".green()
            }
        );
        let whitespace = " ".repeat(state.last_len.saturating_sub(line.len()));

        println!("{line}{whitespace}");

        state
    });

    let mut seen = HashSet::new();
    let sem = Arc::new(Semaphore::new(args.max_concurrent as usize));
    let running_average_response_time = Arc::new(Mutex::new(1.));

    while let Some(Some((url, from))) = rx.recv().await {
        if let Some(exclude_pattern) = &args.exclude_pattern {
            if exclude_pattern.is_match(url.as_str()) {
                println!("> exclude: {url}");
                continue;
            }
        }
        if !seen.contains(&url) {
            seen.insert(url.clone());
            todo.fetch_add(1, Ordering::SeqCst);

            let inner_tx = tx.clone();
            let inner_result_tx = result_tx.clone();

            let sleep_time = 0_f64.max(*running_average_response_time.lock().await - 0.5).max(1.0);
            sleep(Duration::from_secs_f64(sleep_time)).await;

            let permit = sem.clone().acquire_owned().await.unwrap();

            let client = client.clone();
            let running_average = running_average_response_time.clone();
            task::spawn(async move {
                if verbose {
                    println!("> fetching {url}");
                }
                let result = fetch(url, from, inner_tx, client, permit, running_average).await;
                inner_result_tx.send(result).await.unwrap();
            });
        }
    }

    let state = handle.await.unwrap();

    if state.error_count > 0 {
        std::process::exit(1);
    }
}
