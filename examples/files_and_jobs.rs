//! Upload a file, start a job, follow its logs, wait for it, download its output.
//!
//! GAIADESK_API_KEY=ak_… GAIADESK_DESK_TOKEN=gdagt_… cargo run --example files_and_jobs -- 123456789

use std::time::Duration;

use futures_util::StreamExt;
use gaiadesk::{Client, JobPriority, JobSpec, LogEvent};

#[tokio::main]
async fn main() -> gaiadesk::Result<()> {
    let client = Client::builder()
        .api_key(std::env::var("GAIADESK_API_KEY").unwrap_or_default())
        .desk_token(std::env::var("GAIADESK_DESK_TOKEN").unwrap_or_default())
        .build()?;
    let desk = client.desk(std::env::args().nth(1).expect("a desk id"));

    desk.upload_bytes("echo building; sleep 2; echo done > out.txt\n", "/tmp/build.sh").await?;
    let job = desk.run_job(JobSpec::new("demo-build", "sh /tmp/build.sh").priority(JobPriority::Low).cwd("/tmp")).await?;
    println!("started {} ({})", job.name, job.state);

    let mut logs = desk.follow_job_logs("demo-build", None)?;
    while let Some(ev) = logs.next().await {
        match ev? {
            LogEvent::Output(t) => print!("{t}"),
            LogEvent::End(job) => println!("[ended: {} exit {:?}]", job.state, job.exit_code),
            _ => {}
        }
    }

    let w = desk.wait_job("demo-build", Some(Duration::from_secs(60))).await?;
    println!("wait: exit {:?}, timed out {}", w.job.exit_code, w.timed_out);
    let out = desk.download_bytes("/tmp/out.txt").await?;
    println!("out.txt: {}", String::from_utf8_lossy(&out));
    println!("stats: {:?}", desk.stats().await?.cpu_percent);
    Ok(())
}
