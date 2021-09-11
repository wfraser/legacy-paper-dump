use anyhow::{Context, Result};
use dropbox_sdk::default_client::UserAuthDefaultClient;
use dropbox_sdk::paper::{self, ExportFormat, ListPaperDocsArgs, ListPaperDocsContinueArgs, PaperDocExport};
use dropbox_sdk::oauth2::get_auth_from_env_or_prompt;
use regex::bytes::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::ops::Range;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use threadpool::ThreadPool;
use url::Url;

#[derive(Default, Deserialize, Serialize)]
struct DocInfo {
    url: String,
    name: String,
    owner: String,
    path: String,
}

#[derive(Deserialize, Serialize, Default)]
struct DocList {
    docs: Vec<DocInfo>,
}

fn main() -> Result<()> {
    let mut export = true;

    match env::args().nth(1).as_deref() {
        Some("--no-export") => { export = false; }
        None => (),
        _ => {
            eprintln!("{} v{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            eprintln!("usage: legacy-paper-dump [--no-export]");
            eprintln!("unless --no-export is specified, writes all docs to a folder 'docs' in the\
                current directory.");
            std::process::exit(1);
        }
    }

    let client = Arc::new(UserAuthDefaultClient::new(get_auth_from_env_or_prompt()));

    let _ = fs::create_dir("docs");
    if export {
        let _ = fs::create_dir("docs/images");
    }

    let list = match File::open("docs/list.json") {
        Ok(file) => match serde_json::from_reader(file) {
            Ok(list) => list,
            Err(e) => {
                eprintln!("error deserializing docs/list.json: {}", e);
                DocList::default()
            }
        }
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound {
                eprintln!("error opening docs/list.json: {}", e);
            }
            DocList::default()
        }
    };
    let mut map = HashMap::new();
    for doc in list.docs.into_iter() {
        map.insert(doc.url.clone(), doc);
    }
    let map = Arc::new(Mutex::new(map));

    #[allow(deprecated)]
    let mut result = paper::docs_list(&*client, &ListPaperDocsArgs::default())
        .context("paper/docs/list HTTP or transport err")?
        .context("paper/docs/list API err")?;
    let mut ids = result.doc_ids;
    while result.has_more {
        #[allow(deprecated)]
        let next = paper::docs_list_continue(&*client, &ListPaperDocsContinueArgs::new(
                result.cursor.value))
            .context("paper/docs/list/continue HTTP or transport err")?
            .context("paper/docs/list/continue API err")?;
        result = next;
        ids.extend_from_slice(&result.doc_ids);
    }

    let pages_pool = ThreadPool::new(10);
    let images_pool = Arc::new(Mutex::new(ThreadPool::new(10)));

    for id in ids.into_iter() {
        let client = Arc::clone(&client);
        let images_pool = Arc::clone(&images_pool);
        let doc_map = Arc::clone(&map);
        pages_pool.execute(move || {
            let output = fetch_doc(&id, client, export, images_pool, doc_map);
            let out = io::stdout();
            let mut lock = out.lock();
            let _ = writeln!(lock, "{}", output);
        });
    }

    pages_pool.join();

    let mut docs = DocList {
        docs: Arc::try_unwrap(map)
            .unwrap_or_else(|_| panic!("unable to unwrap doc map arc"))
            .into_inner()
            .expect("unable to unwrap doc ma mutex")
            .into_iter()
            .map(|(_k, v)| v)
            .collect(),
    };

    docs.docs.sort_by(|a, b| a.name.cmp(&b.name));

    let mut file = File::create("docs/list.json").expect("failed to create docs/list.json");
    serde_json::to_writer(&mut file, &docs).expect("failed to serialize docs/list.json");

    let mut index = File::create("docs/index.html").expect("failed to create docs/index.html");
    writeln!(&mut index, "<html><head><title>Paper Doc Index</title></head><body>").unwrap();
    for doc in &docs.docs {
        let path_url = url::form_urlencoded::byte_serialize(doc.path.as_bytes())
            .collect::<String>()
            .replace('+', "%20");
        writeln!(&mut index, "<p><a href=\"{}\">{}</a><br><small>{}</small> &middot; <small><a href=\"{}\">link</a></small>",
            path_url,
            doc.name,
            doc.owner,
            doc.url,
        ).unwrap();
    }
    writeln!(&mut index, "</body></html>").unwrap();

    Ok(())
}

fn fetch_doc(
    id: &str,
    client: Arc<UserAuthDefaultClient>,
    export: bool,
    images_pool: Arc<Mutex<ThreadPool>>,
    doc_map: Arc<Mutex<HashMap<String, DocInfo>>>,
) -> String {
    let url = format!("https://paper.dropbox.com/doc/{}", id);

    // buffer output until we're done, so that we don't interleave with other jobs
    let mut output = url.clone() + "\n";

    if doc_map.lock().unwrap().contains_key(&url) {
        output += "already downloaded; skipping\n";
        return output;
    }

    let mut failures = 0;
    let mut export_result = loop {
        if failures >= 3 {
            output += "too many errors; skipping doc\n";
            return output;
        }

        #[allow(deprecated)]
        let download_result = paper::docs_download(
            &*client,
            &PaperDocExport::new(id.to_owned(), ExportFormat::Html),
            if export { None } else { Some(0) },
            if export { None } else { Some(0) },
        );
        match download_result {
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

    let mut filename = export_result.result.title.chars()
        .filter_map(|c| {
            if c.is_ascii() {
                if c == '/' || c == '\\' || c == ':' {
                    Some('_')
                } else {
                    Some(c)
                }
            } else {
                None
            }
        })
        .collect::<String>()
        .trim()
        .to_owned();
    if filename.is_empty() {
        filename += "(unprintable)";
    }
    filename += &format!(" ({}).html", id);

    let path = PathBuf::from("docs").join(&filename);
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

    let doc_info = DocInfo {
        url: url.clone(),
        name: export_result.result.title.clone(),
        owner: export_result.result.owner.clone(),
        path: filename,
    };

    doc_map.lock().unwrap()
        .insert(url.clone(), doc_info);

    let img_re = Regex::new(r#"<img( [^>]+)* src="(?P<url>[^"]+)"[^>]*>"#)
        .expect("bad regular expression");
    let mut images = vec![];
    for m in img_re.captures_iter(&html) {
        let original_tag = match std::str::from_utf8(m.get(0).unwrap().as_bytes()) {
            Ok(s) => s.to_owned(),
            Err(e) => {
                output += &format!("non-UTF8 image tag {:?}: {}\n", &m, e);
                continue;
            }
        };
        let url = std::str::from_utf8(&m["url"]).unwrap().to_owned();
        if url.starts_with("data:") {
            continue;
        }
        let original_range = m.get(0).unwrap().range();
        images.push((original_range, original_tag, url));
    }

    let (tx, rx) = mpsc::channel();
    let image_cnt = images.len();
    let images_pool_locked = images_pool.lock().unwrap();
    for (Range { start, end }, original_tag, url) in images {
        let tx = tx.clone();
        images_pool_locked.execute(move || {
            let result = fetch_image(&url)
                .map(|path| (start, end, original_tag.replace(&url, &path).into_bytes()));
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

    let mut html2 = format!("<!DOCTYPE html><html><head><title>{title}</title></head>\
        <body><p>\
            downloaded on {date} from <a href=\"{url}\">{url}</a><br>
            owned by {owner}</p>\n",
        title=export_result.result.title,
        owner=export_result.result.owner,
        url=url,
        date=chrono::Local::now().to_rfc2822()).into_bytes();
    let mut last_end = 0;
    for (start, end, replacement) in replacements {
        html2.extend_from_slice(&html[last_end .. start]);
        html2.extend_from_slice(&replacement);
        last_end = end;
    }
    html2.extend_from_slice(&html[last_end ..]);
    html2.extend_from_slice(b"</body></html>\n");

    if let Err(e) = file.write_all(&html2) {
        output += &format!("I/O error writing file {:?}: {}\n", path, e);
        return output;
    }

    output
}

fn hash_str(s: &str) -> String {
    use ring::digest::{digest, SHA256};
    let hash = digest(&SHA256, s.as_bytes());
    base64::encode_config(&hash, base64::URL_SAFE_NO_PAD)
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
    let mut filename = if parts.len() == 2 {
        format!("{} __{}.{}", parts[1], hash, parts[0])
    } else {
        format!("{} __{}", parts[0], hash)
    };

    let (path, file) = loop {
        let path = format!("images/{}", filename);
        let docs_path = format!("docs/{}", path);
        match OpenOptions::new().create_new(true).write(true)
            .open(&docs_path)
        {
            Ok(f) => break (docs_path, f),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Ok(path);
            }
            Err(e) => {
                #[cfg(unix)]
                if e.raw_os_error() == Some(libc::ENAMETOOLONG) {
                    if filename != hash {
                        filename = hash.clone();
                        continue;
                    }
                }
                return Err(format!("failed to create file {}: {}", path, e));
            }
        }
    };

    fn inner(mut file: std::fs::File, url: &str) -> Result<(), String> {
        let mut body = match ureq::get(url).call() {
            Ok(response) => {
                let ct = response.header("content-type").unwrap_or("");
                if !ct.starts_with("image/") {
                    return Err(format!("{}: content type is {:?}", url, ct));
                }
                response.into_reader()
            }
            Err(e) => return Err(format!("failed to fetch {}: {}", url, e)),
        };

        io::copy(&mut body, &mut file)
            .map_err(|e| format!("failed to download {}: {}", url, e))
            .map(|_|())
    }

    let result = inner(file, url);

    if result.is_err() {
        let _ = fs::remove_file(&path);
    }

    result.map(|()| path)
}
