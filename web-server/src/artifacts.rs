use std::{fs::File, io::Write, sync::Arc};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use docx_rs::{Docx, Paragraph, Run};
use uuid::Uuid;

use crate::{
    api::{CommentQuery, FeedbackQuery, Service},
    db::{Artifact, Database, DatasetSpec, safe_filename},
};

pub fn report_content(summary: &str, datasets: &[DatasetSpec]) -> String {
    let mut output = format!(
        "生成时间：{}\n隐私说明：报告默认匿名化玩家昵称和 UID；原始数据仅在 CSV 导出中保留。\n\n",
        Utc::now().to_rfc3339()
    );
    if datasets.is_empty() {
        output.push_str("数据范围：本次回答没有可关联的数据集。\n\n");
    } else {
        output.push_str("数据范围与统计：\n");
        for dataset in datasets {
            let label = if dataset.kind == "comments" {
                "组件评论"
            } else {
                "玩家反馈"
            };
            output.push_str(&format!(
                "- {label}：匹配 {} 条；筛选条件 {}\n",
                dataset.total,
                serde_json::to_string(&dataset.query).unwrap_or_else(|_| "{}".into())
            ));
        }
        output.push('\n');
    }
    output.push_str("分析总结：\n");
    output.push_str(summary.trim());
    output
}

pub fn create_markdown(
    database: &Database,
    owner_id: &str,
    conversation_id: Option<&str>,
    run_id: Option<&str>,
    title: &str,
    content: &str,
) -> Result<Artifact> {
    let filename = format!("{}.md", safe_filename(title));
    let path = database
        .artifact_root
        .join(format!("{}.md", Uuid::new_v4()));
    std::fs::write(&path, format!("# {}\n\n{}\n", title.trim(), content.trim()))?;
    database.add_artifact(owner_id, conversation_id, run_id, "md", &filename, &path)
}

pub fn create_docx(
    database: &Database,
    owner_id: &str,
    conversation_id: Option<&str>,
    run_id: Option<&str>,
    title: &str,
    content: &str,
) -> Result<Artifact> {
    let filename = format!("{}.docx", safe_filename(title));
    let path = database
        .artifact_root
        .join(format!("{}.docx", Uuid::new_v4()));
    let mut document = Docx::new()
        .add_paragraph(Paragraph::new().add_run(Run::new().add_text(title.trim()).size(32).bold()));
    for block in content.split("\n\n") {
        let text = block.trim();
        if !text.is_empty() {
            document = document
                .add_paragraph(Paragraph::new().add_run(Run::new().add_text(text).size(22)));
        }
    }
    document
        .build()
        .pack(File::create(&path)?)
        .context("Word 文档生成失败")?;
    database.add_artifact(owner_id, conversation_id, run_id, "docx", &filename, &path)
}

pub async fn create_csv(
    database: &Database,
    service: Arc<Service>,
    dataset: &DatasetSpec,
    owner_id: &str,
    conversation_id: Option<&str>,
    run_id: Option<&str>,
) -> Result<Artifact> {
    if dataset.owner_id != owner_id {
        bail!("数据集不属于当前用户");
    }
    let label = if dataset.kind == "comments" {
        "组件评论"
    } else if dataset.kind == "feedback" {
        "玩家反馈"
    } else {
        bail!("该数据集不支持 CSV 导出");
    };
    let filename = format!("{}-{}.csv", label, Utc::now().format("%Y%m%d-%H%M%S"));
    let path = database
        .artifact_root
        .join(format!("{}.csv", Uuid::new_v4()));
    let file = csv_file(&path)?;
    let mut writer = csv::WriterBuilder::new().from_writer(file);
    let time_range = DatasetTimeRange::from_query(&dataset.query)?;
    let useful_only = dataset
        .query
        .get("useful_only")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if dataset.kind == "comments" {
        writer.write_record([
            "评论ID",
            "组件IID",
            "组件名称",
            "玩家UID",
            "昵称",
            "标签",
            "评分",
            "评论内容",
            "发布时间",
        ])?;
        let mut query: CommentQuery = serde_json::from_value(dataset.query.clone())?;
        query.offset = 0;
        query.limit = 100;
        // AI range reports enforce dates locally because the remote comment
        // date filter is intermittent and feedback has no date filter at all.
        query.start_date = None;
        query.end_date = None;
        loop {
            let page = service
                .list_comments(query.clone())
                .await
                .map_err(|error| anyhow::anyhow!(error.message))?;
            let before_start = time_range
                .page_is_before_start(page.items.iter().map(|item| item.published_at.as_str()));
            for item in page.items {
                if !time_range.contains(&item.published_at) {
                    continue;
                }
                if useful_only && !comment_is_useful_candidate(&item.content, &item.tag) {
                    continue;
                }
                writer.write_record([
                    item.id,
                    item.resource_id,
                    item.resource_name,
                    item.player_uid,
                    item.nickname,
                    item.tag,
                    item.stars,
                    item.content,
                    item.published_at,
                ])?;
            }
            if before_start || !page.has_more {
                break;
            }
            query.offset = query.offset.saturating_add(query.limit);
        }
    } else {
        writer.write_record([
            "反馈ID",
            "组件IID",
            "组件名称",
            "玩家UID",
            "昵称",
            "类型",
            "反馈内容",
            "提交时间",
            "已有回复",
            "禁止回复",
        ])?;
        let mut query: FeedbackQuery = serde_json::from_value(dataset.query.clone())?;
        query.offset = 0;
        query.limit = 100;
        loop {
            let page = service
                .list_feedback(query.clone())
                .await
                .map_err(|error| anyhow::anyhow!(error.message))?;
            let before_start = time_range
                .page_is_before_start(page.items.iter().map(|item| item.created_at.as_str()));
            for item in page.items {
                if !time_range.contains(&item.created_at) {
                    continue;
                }
                writer.write_record([
                    item.id,
                    item.resource_id,
                    item.resource_name,
                    item.player_uid,
                    item.nickname,
                    item.feedback_type_label,
                    item.content,
                    item.created_at,
                    item.developer_reply.unwrap_or_default(),
                    item.forbid_reply.to_string(),
                ])?;
            }
            if before_start || !page.has_more {
                break;
            }
            query.offset = query.offset.saturating_add(query.limit);
        }
    }
    writer.flush()?;
    drop(writer);
    database.add_artifact(owner_id, conversation_id, run_id, "csv", &filename, &path)
}

fn comment_is_useful_candidate(content: &str, tag: &str) -> bool {
    let content = content.trim().to_lowercase();
    if content.is_empty() {
        return false;
    }
    let compact: String = content
        .chars()
        .filter(|value| value.is_alphanumeric())
        .collect();
    if matches!(
        compact.as_str(),
        "好玩"
            | "不好玩"
            | "很好玩"
            | "非常好玩"
            | "不错"
            | "很好"
            | "一般"
            | "垃圾"
            | "666"
            | "牛逼"
            | "支持"
            | "加油"
    ) {
        return false;
    }
    if content.chars().count() >= 24 {
        return true;
    }
    let signals = [
        "问题",
        "建议",
        "希望",
        "能不能",
        "可以加",
        "请加",
        "增加",
        "添加",
        "优化",
        "修复",
        "改进",
        "不能",
        "无法",
        "用不了",
        "打不开",
        "进不去",
        "不生效",
        "失效",
        "报错",
        "错误",
        "bug",
        "崩溃",
        "闪退",
        "卡顿",
        "掉帧",
        "延迟",
        "冲突",
        "兼容",
        "丢失",
        "消失",
        "缺少",
        "不支持",
        "版本",
    ];
    let tag = tag.to_lowercase();
    signals
        .iter()
        .any(|signal| content.contains(signal) || tag.contains(signal))
}

#[derive(Clone, Copy, Default)]
struct DatasetTimeRange {
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
}

impl DatasetTimeRange {
    fn from_query(query: &serde_json::Value) -> Result<Self> {
        Ok(Self {
            start: parse_dataset_time(query.get("start_date"), false)?,
            end: parse_dataset_time(query.get("end_date"), true)?,
        })
    }

    fn contains(self, value: &str) -> bool {
        let Ok(value) = DateTime::parse_from_rfc3339(value) else {
            return true;
        };
        let value = value.with_timezone(&Utc);
        self.start.is_none_or(|start| value >= start) && self.end.is_none_or(|end| value <= end)
    }

    fn page_is_before_start<'a>(self, values: impl Iterator<Item = &'a str>) -> bool {
        let Some(start) = self.start else {
            return false;
        };
        values
            .filter_map(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
            .max()
            .is_some_and(|newest| newest < start)
    }
}

fn parse_dataset_time(
    value: Option<&serde_json::Value>,
    end_of_day: bool,
) -> Result<Option<DateTime<Utc>>> {
    let Some(value) = value
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if let Ok(value) = DateTime::parse_from_rfc3339(value) {
        return Ok(Some(value.with_timezone(&Utc)));
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        let time = if end_of_day {
            date.and_hms_opt(23, 59, 59)
        } else {
            date.and_hms_opt(0, 0, 0)
        };
        return Ok(time.map(|value| value.and_utc()));
    }
    bail!("数据集日期范围无效")
}

fn csv_file(path: &std::path::Path) -> Result<File> {
    let mut file = File::create(path)?;
    file.write_all(&[0xEF, 0xBB, 0xBF])?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_has_a_heading() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("key"), [9u8; 32]).unwrap();
        let db = Database::open(
            &directory.path().join("db"),
            &directory.path().join("key"),
            &directory.path().join("files"),
        )
        .unwrap();
        let mut password = "artifact-test-password".to_string();
        let owner = db
            .ensure_admin("owner@example.test", &mut password)
            .unwrap();
        let artifact = create_markdown(&db, &owner.id, None, None, "测试报告", "正文").unwrap();
        let bytes = std::fs::read(artifact.path).unwrap();
        assert!(bytes.starts_with("# 测试报告".as_bytes()));
    }

    #[test]
    fn docx_is_a_valid_zip_container() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("key"), [5u8; 32]).unwrap();
        let db = Database::open(
            &directory.path().join("db"),
            &directory.path().join("key"),
            &directory.path().join("files"),
        )
        .unwrap();
        let mut password = "artifact-test-password".to_string();
        let owner = db
            .ensure_admin("owner@example.test", &mut password)
            .unwrap();
        let artifact = create_docx(&db, &owner.id, None, None, "测试报告", "中文正文").unwrap();
        let bytes = std::fs::read(artifact.path).unwrap();
        assert!(bytes.starts_with(b"PK"));
        assert!(bytes.len() > 500);
    }

    #[test]
    fn report_scope_contains_filters_and_anonymization() {
        let dataset = DatasetSpec {
            id: "dataset".into(),
            owner_id: "owner".into(),
            conversation_id: None,
            run_id: None,
            kind: "comments".into(),
            query: serde_json::json!({"keyword":"崩溃"}),
            total: 12,
            created_at: 0,
        };
        let content = report_content("摘要", &[dataset]);
        assert!(content.contains("匹配 12 条"));
        assert!(content.contains("匿名化"));
        assert!(content.contains("崩溃"));
    }

    #[test]
    fn dataset_time_range_uses_inclusive_boundaries() {
        let range = DatasetTimeRange::from_query(&serde_json::json!({
            "start_date":"2026-08-24T12:00:00+08:00",
            "end_date":"2026-08-25T12:00:00+08:00"
        }))
        .unwrap();
        assert!(range.contains("2026-08-24T04:00:00Z"));
        assert!(range.contains("2026-08-25T04:00:00Z"));
        assert!(!range.contains("2026-08-25T04:00:01Z"));
    }

    #[test]
    fn csv_starts_with_utf8_bom() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.csv");
        drop(csv_file(&path).unwrap());
        assert_eq!(&std::fs::read(path).unwrap()[..3], &[0xEF, 0xBB, 0xBF]);
    }
}
