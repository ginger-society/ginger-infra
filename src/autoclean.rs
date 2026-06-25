use tokio_cron_scheduler::{JobScheduler, Job};
use tokio::process::Command;
use chrono::Local;

async fn run_cleanup_script(clean: bool) {
    let clean_flag = if clean { "--clean" } else { "" };

    let script_cmd = format!(
        r#"curl -fsSL https://raw.githubusercontent.com/ginger-society/infra-as-code-repo/main/ginger-infra-helpers/docker-cleanup.sh | bash -s -- {}"#,
        clean_flag
    );

    println!(
        "[{}] Running docker cleanup (clean={})",
        Local::now().format("%Y-%m-%d %H:%M:%S"),
        clean
    );

    let output = Command::new("bash")
        .arg("-c")
        .arg(&script_cmd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await;

    match output {
        Ok(out) => {
            println!("{}", String::from_utf8_lossy(&out.stdout));
            if !out.stderr.is_empty() {
                eprintln!("{}", String::from_utf8_lossy(&out.stderr));
            }
        }
        Err(e) => eprintln!("Failed to run cleanup script: {}", e),
    }
}

pub async fn start_autoclean_scheduler() {
    let scheduler = JobScheduler::new().await.expect("Failed to create scheduler");

    // Daily dry-run at 8am
    scheduler.add(
        Job::new_async("0 8 * * * *", |_, _| {
            Box::pin(async {
                run_cleanup_script(false).await;
            })
        }).expect("Failed to create dry-run job")
    ).await.expect("Failed to add dry-run job");

    // Actual clean every Sunday at 2am
    scheduler.add(
        Job::new_async("0 2 * * 0 *", |_, _| {
            Box::pin(async {
                run_cleanup_script(true).await;
            })
        }).expect("Failed to create clean job")
    ).await.expect("Failed to add clean job");

    println!("Autoclean scheduler started (dry-run: daily 8am, clean: Sunday 2am)");
    scheduler.start().await.expect("Failed to start scheduler");
}