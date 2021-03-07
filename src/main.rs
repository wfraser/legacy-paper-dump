use anyhow::{Context, Result};
use dropbox_sdk::default_client::UserAuthDefaultClient;
use dropbox_sdk::paper::{self, ExportFormat, ListPaperDocsArgs, ListPaperDocsContinueArgs, PaperDocExport};
use regex::bytes::Regex;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use threadpool::ThreadPool;
use url::Url;

fn get_oauth2_token() -> String {
    env::var("DBX_OAUTH_TOKEN")
        .expect("need environment variable DBX_OAUTH_TOKEN to be set")
}

fn main() -> Result<()> {
    let mut export = false;
    if env::args().nth(1).as_deref() == Some("--export") {
        export = true;
    }

    let client = Arc::new(UserAuthDefaultClient::new(get_oauth2_token()));

    if export {
        let _ = fs::create_dir("docs");
        let _ = fs::create_dir("docs/images");
    }

    let mut result = paper::docs_list(&*client, &ListPaperDocsArgs::default())
        .context("paper/docs/list HTTP or transport err")?
        .context("paper/docs/list API err")?;
    let mut ids = result.doc_ids;
    while result.has_more {
        result = paper::docs_list_continue(&*client, &ListPaperDocsContinueArgs::new(
                result.cursor.value))
            .context("paper/docs/list/continue HTTP or transport err")?
            .context("paper/docs/list/continue API err")?;
        ids.extend_from_slice(&result.doc_ids);
    }

    let pages_pool = ThreadPool::new(10);
    let images_pool = Arc::new(Mutex::new(ThreadPool::new(10)));

    for id in ids.into_iter() {
        let client = Arc::clone(&client);
        let images_pool = Arc::clone(&images_pool);
        pages_pool.execute(move || {
            let output = fetch_doc(&id, client, export, images_pool);
            let out = io::stdout();
            let mut lock = out.lock();
            let _ = writeln!(lock, "{}", output);
        });
    }

    pages_pool.join();

    Ok(())
}

fn fetch_doc(
    id: &str,
    client: Arc<UserAuthDefaultClient>,
    export: bool,
    images_pool: Arc<Mutex<ThreadPool>>,
) -> String {
    // buffer output until we're done, so that we don't interleave with other jobs
    let mut output = format!("https://paper.dropbox.com/doc/{}\n", id);

    let mut failures = 0;
    let mut export_result = loop {
        if failures >= 3 {
            output += "too many errors; skipping doc\n";
            return output;
        }

        match paper::docs_download(
            &*client,
            &PaperDocExport::new(id.to_owned(), ExportFormat::Html),
            if export { None } else { Some(0) },
            if export { None } else { Some(0) },
        ) {
            Ok(Ok(result)) => break result,
            Ok(Err(api_err)) => {
                output += &format!("API error: {}\n", api_err);
                // Not retriable. Skip this doc.
                return output;
            }
            Err(dropbox_sdk::Error::ServerError(_)) => {
                // Don't print the error; it's got a big HTML page text in it.
                output += "HTTP 503; retrying\n";
            }
            Err(e) => {
                output += &format!("HTTP transport error: {}; retrying\n", e);
            }
        }
        failures += 1;
        thread::sleep(Duration::from_secs(3));
    };

    output += &format!("title: {}\nowner: {}\n",
        export_result.result.title,
        export_result.result.owner);

    if !export {
        return output;
    }

    let path = PathBuf::from("docs")
        .join(format!("{} ({}).html", export_result.result.title.replace('/', "_"), id));
    let mut file = match OpenOptions::new().create_new(true).write(true).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            output += "file already downloaded; skipping\n";
            return output;
        }
        Err(e) => {
            output += &format!("failed to create file {:?}: {}\n", path, e);
            return output;
        }
    };

    let mut html = vec![];
    if let Err(e) = export_result.body.as_mut().expect("response must have body")
        .read_to_end(&mut html)
    {
        output += &format!("I/O error reading doc: {}\n", e);
        return output;
    }

    let img_re = Regex::new(r#"(?P<stuff1><img( [^>]*)+) src="(?P<url>[^"]+)"(?P<stuff2>[^>]*>)"#)
        .expect("bad regular expression");
    let mut images = vec![];
    for m in img_re.captures_iter(&html) {
        let url = match std::str::from_utf8(&m["url"]) {
            Ok(s) => s.to_owned(),
            Err(e) => {
                output += &format!("non-UTF8 url {:?}: {}\n", &m["url"], e);
                continue;
            }
        };
        images.push((m.get(0).unwrap().range(), (&m["stuff1"]).to_vec(), url, (&m["stuff2"]).to_vec()));
    }

    let (tx, rx) = mpsc::channel();
    let image_cnt = images.len();
    let images_pool_locked = images_pool.lock().unwrap();
    for (Range { start, end }, stuff1, url, stuff2) in images {
        let tx = tx.clone();
        images_pool_locked.execute(move || {
            let result = fetch_image(&url)
                .map(|path| {
                    let mut replacement = stuff1;
                    replacement.extend_from_slice(format!(" src=\"{}\"", path).as_bytes());
                    replacement.extend_from_slice(&stuff2);
                    (start, end, replacement)
                });
            tx.send(result).expect("channel busted");
        })
    }
    drop(images_pool_locked);

    let mut response_cnt = 0;
    let mut replacements = vec![];
    while response_cnt < image_cnt {
        response_cnt += 1;
        match rx.recv() {
            Ok(Ok(replacement)) => replacements.push(replacement),
            Ok(Err(e)) => {
                output += &format!("failed to fetch image: {}\n", e);
            }
            Err(e) => {
                output += &format!("image thread died?!?: {}\n", e);
            }
        }
    }
    replacements.sort_by(|a, b| a.0.cmp(&b.0));
    output += &format!("downloaded {} of {} images\n", replacements.len(), response_cnt);

    let mut html2 = vec![];
    let mut last_end = 0;
    for (start, end, replacement) in replacements {
        html2.extend_from_slice(&html[last_end .. start]);
        html2.extend_from_slice(&replacement);
        last_end = end;
    }
    html2.extend_from_slice(&html[last_end ..]);

    if let Err(e) = file.write_all(&html2) {
        output += &format!("I/O error writing file {:?}: {}\n", path, e);
        return output;
    }

    output
}

fn hash_str(s: &str) -> String {
    use ring::digest::*;

    let mut ctx = Context::new(&SHA256);
    ctx.update(s.as_bytes());
    let digest = ctx.finish();

    let mut out = String::new();
    for byte in digest.as_ref() {
        // Rather than doing base64 or something, use Unicode blocks Latin-Extended A and B, which
        // provide >256 contiguous, printable, filename-safe characters.
        out.push(std::char::from_u32(0x0100 + *byte as u32).unwrap());
    }
    out
}

fn fetch_image(url: &str) -> Result<String, String> {
    let filename = Url::parse(url)
        .map_err(|e| format!("invalid url {}: {}", url, e))?
        .path_segments()
        .ok_or_else(|| format!("url has no path?! {}", url))?
        .last().unwrap()
        .to_owned();

    let hash = hash_str(url);

    let parts = filename.rsplitn(2, '.').collect::<Vec<_>>();
    let filename = if parts.len() == 2 {
        format!("{} __{}.{}", parts[1], hash, parts[0])
    } else {
        format!("{} __{}", parts[0], hash)
    };

    let path = format!("images/{}", filename);
    let docs_path = format!("docs/{}", path);

    let mut file = match OpenOptions::new().create_new(true).write(true)
        .open(&docs_path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Ok(path);
        }
        Err(e) => {
            return Err(format!("failed to create file {}: {}", path, e));
        }
    };

    let mut body = match ureq::get(url).call() {
        Ok(response) => response.into_reader(),
        Err(e) => {
            drop(file);
            let _ = fs::remove_file(&docs_path);
            return Err(format!("failed to fetch {}: {}", url, e));
        }
    };

    match io::copy(&mut body, &mut file) {
        Ok(_) => Ok(path),
        Err(e) => {
            drop(file);
            let _ = fs::remove_file(&docs_path);
            Err(format!("failed to download {} to {}: {}", url, path, e))
        }
    }
}
