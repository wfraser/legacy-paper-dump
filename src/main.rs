use anyhow::{Context, Result};
use dropbox_sdk::default_client::UserAuthDefaultClient;
use dropbox_sdk::paper::{self, ExportFormat, ListPaperDocsArgs, ListPaperDocsContinueArgs, PaperDocExport};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use threadpool::ThreadPool;

fn get_oauth2_token() -> String {
    env::var("DBX_OAUTH_TOKEN")
        .expect("need environment variable DBX_OAUTH_TOKEN to be set")
}

fn main() -> Result<()> {
    let mut export = false;
    if env::args().skip(1).next().as_deref() == Some("--export") {
        export = true;
    }

    let client = Arc::new(UserAuthDefaultClient::new(get_oauth2_token()));

    if export {
        let _ = fs::create_dir("docs");
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

    let pool = ThreadPool::new(10);
    for id in ids.into_iter() {
        let client = Arc::clone(&client);
        pool.execute(move || {
            let output = fetch_doc(&id, client, export);
            let out = io::stdout();
            let mut lock = out.lock();
            let _ = write!(lock, "{}\n", output);
        });
    }

    pool.join();

    Ok(())
}

fn fetch_doc(id: &str, client: Arc<UserAuthDefaultClient>, export: bool) -> String {
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
    match OpenOptions::new().create_new(true).write(true).open(&path) {
        Ok(mut f) => {
            let body = export_result.body.as_mut().expect("response must have body");
            if let Err(e) = io::copy(body, &mut f) {
                output += &format!("I/O error writing doc {:?}: {}\n", path, e);
            }
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            output += "file already downloaded; skipping\n";
        }
        Err(e) => {
            output += &format!("failed to create file {:?}: {}\n", path, e);
        }
    }

    output
}
