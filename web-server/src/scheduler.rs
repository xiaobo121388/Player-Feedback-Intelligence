use std::{str::FromStr, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use chrono_tz::Tz;
use cron::Schedule;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::{
    ai::{AiContext, AiEngine},
    artifacts,
    db::{Database, Job},
};

const JOB_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Clone)]
pub struct Scheduler {
    database: Database,
    ai: AiEngine,
    worker: Arc<Semaphore>,
}

impl Scheduler {
    pub fn new(database: Database, ai: AiEngine) -> Self {
        Self {
            database,
            ai,
            worker: Arc::new(Semaphore::new(1)),
        }
    }

    pub fn start(self) {
        tokio::spawn(async move {
            let _ = self.database.cleanup();
            let _ = self.database.recover_interrupted_jobs();
            let mut cleanup_ticks = 0u16;
            loop {
                if let Err(error) = self.tick().await {
                    eprintln!("scheduler error: {}", redact(&error.to_string()));
                }
                cleanup_ticks = cleanup_ticks.saturating_add(1);
                if cleanup_ticks >= 120 {
                    if let Err(error) = self.database.cleanup() {
                        eprintln!("cleanup error: {}", redact(&error.to_string()));
                    }
                    cleanup_ticks = 0;
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
    }

    pub async fn tick(&self) -> Result<()> {
        let now = Utc::now().timestamp();
        for job in self.database.due_jobs(now)? {
            let scheduled_for = job.next_run_at.unwrap_or(now);
            let next = next_run_at(&job.schedule_value, &job.timezone, now)?;
            if job.running {
                self.database
                    .skip_overlapping_job(&job, scheduled_for, next)?;
                continue;
            }
            let run = self
                .database
                .mark_job_running(&job, Some(scheduled_for), next)?;
            let scheduler = self.clone();
            tokio::spawn(async move {
                if let Err(error) = scheduler.run_marked(job, run, next).await {
                    eprintln!("scheduled job failed: {}", redact(&error.to_string()));
                }
            });
        }
        Ok(())
    }

    pub async fn run_marked(
        &self,
        job: Job,
        run: crate::db::JobRun,
        next: Option<i64>,
    ) -> Result<String> {
        let _permit = self.worker.acquire().await?;
        let timezone = Tz::from_str(&job.timezone).context("时区无效")?;
        let execution_time = Utc::now()
            .with_timezone(&timezone)
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string();
        let history = vec![crate::db::Message {
            id: uuid::Uuid::new_v4().to_string(),
            conversation_id: String::new(),
            role: "user".into(),
            content: format!(
                "本次任务执行时间：{execution_time}（{}）。计算相对日期和统计窗口时必须以此时间为准。\n\n{}",
                job.timezone, job.prompt
            ),
            tool_summary: None,
            created_at: Utc::now().timestamp(),
        }];
        let context = AiContext {
            user_id: job.owner_id.clone(),
            conversation_id: None,
            run_id: Some(run.id.clone()),
            allowed_tools: job.allowed_tools.clone(),
            allow_large: true,
            email_to: job.email_to.clone(),
        };
        let result: Result<crate::ai::AiOutcome> = async {
            let cancel = CancellationToken::new();
            let mut outcome =
                tokio::time::timeout(JOB_TIMEOUT, self.ai.run(&history, context, cancel.clone()))
                    .await
                    .map_err(|_| {
                        cancel.cancel();
                        anyhow::anyhow!("任务超过 15 分钟仍未完成，已自动停止")
                    })??;
            let datasets: Vec<_> = outcome
                .dataset_ids
                .iter()
                .filter_map(|id| self.database.dataset(&job.owner_id, id).ok().flatten())
                .collect();
            let report = artifacts::report_content(&outcome.text, &datasets);
            if job.formats.iter().any(|value| value == "md")
                && !outcome
                    .artifacts
                    .iter()
                    .any(|artifact| artifact.kind == "md")
            {
                outcome.artifacts.push(artifacts::create_markdown(
                    &self.database,
                    &job.owner_id,
                    None,
                    Some(&run.id),
                    &job.name,
                    &report,
                )?);
            }
            if job.formats.iter().any(|value| value == "docx")
                && !outcome
                    .artifacts
                    .iter()
                    .any(|artifact| artifact.kind == "docx")
            {
                outcome.artifacts.push(artifacts::create_docx(
                    &self.database,
                    &job.owner_id,
                    None,
                    Some(&run.id),
                    &job.name,
                    &report,
                )?);
            }
            if job.formats.iter().any(|value| value == "csv") {
                for dataset_id in &outcome.dataset_ids {
                    if let Some(dataset) = self.database.dataset(&job.owner_id, dataset_id)? {
                        outcome.artifacts.push(
                            artifacts::create_csv(
                                &self.database,
                                self.ai.service_for_user(&job.owner_id)?,
                                &dataset,
                                &job.owner_id,
                                None,
                                Some(&run.id),
                            )
                            .await?,
                        );
                    }
                }
            }
            Ok(outcome)
        }
        .await;
        match result {
            Ok(outcome) => {
                self.database.finish_run(
                    &run.id,
                    &job.id,
                    "success",
                    Some(&outcome.text),
                    None,
                    outcome.tool_count,
                    Some(if outcome.email_sent {
                        "sent"
                    } else {
                        "not_sent"
                    }),
                    next,
                )?;
                Ok(run.id)
            }
            Err(error) => {
                let message = redact(&error.to_string());
                let email_status = self
                    .database
                    .email_status(&run.id)?
                    .unwrap_or_else(|| "not_sent".into());
                self.database.finish_run(
                    &run.id,
                    &job.id,
                    "failed",
                    None,
                    Some(&message),
                    0,
                    Some(&email_status),
                    next,
                )?;
                Err(error)
            }
        }
    }
}

pub fn next_run_at(expression: &str, timezone: &str, after: i64) -> Result<Option<i64>> {
    if expression.split_whitespace().count() != 5 {
        bail!("Cron 必须包含 5 段");
    }
    let timezone = Tz::from_str(timezone).context("时区无效")?;
    let schedule = Schedule::from_str(&format!("0 {expression}")).context("Cron 表达式无效")?;
    let after = chrono::DateTime::from_timestamp(after, 0)
        .context("时间无效")?
        .with_timezone(&timezone);
    Ok(schedule.after(&after).next().map(|value| value.timestamp()))
}

pub fn preview(expression: &str, timezone: &str) -> Result<Vec<i64>> {
    let mut current = Utc::now().timestamp();
    let mut values = Vec::new();
    for _ in 0..5 {
        let Some(next) = next_run_at(expression, timezone, current)? else {
            break;
        };
        values.push(next);
        current = next;
    }
    Ok(values)
}

fn redact(value: &str) -> String {
    value
        .replace("sk-", "[secret]-")
        .chars()
        .take(1000)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cron_preview_has_five_future_values() {
        let values = preview("0 9 * * *", "Asia/Shanghai").unwrap();
        assert_eq!(values.len(), 5);
        assert!(values.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
