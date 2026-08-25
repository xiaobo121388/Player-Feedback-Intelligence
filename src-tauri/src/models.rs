use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Debug, Clone, Serialize)]
pub struct Page<T> {
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub has_more: bool,
    pub items: Vec<T>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountStatus {
    pub session_state: SessionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub level: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_sale_item_count: Option<i32>,
}

impl AccountStatus {
    pub fn missing() -> Self {
        Self {
            session_state: SessionState::Missing,
            nickname: None,
            level: None,
            on_sale_item_count: None,
        }
    }

    pub fn expired() -> Self {
        Self {
            session_state: SessionState::Expired,
            ..Self::missing()
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Valid,
    Missing,
    Expired,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoginOutcome {
    pub account: AccountStatus,
    pub persisted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayerComment {
    pub id: String,
    pub resource_id: String,
    pub resource_name: String,
    pub player_uid: String,
    pub nickname: String,
    pub tag: String,
    pub stars: String,
    pub content: String,
    pub published_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlayerFeedback {
    pub id: String,
    pub resource_id: String,
    pub resource_name: String,
    pub player_uid: String,
    pub nickname: String,
    pub feedback_type: String,
    pub feedback_type_label: String,
    pub content: String,
    pub created_at: String,
    pub developer_reply: Option<String>,
    pub image_urls: Vec<String>,
    pub log_file_url: Option<String>,
    pub forbid_reply: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ApiEnvelope<T> {
    pub status: String,
    pub msg: Option<String>,
    pub data: Option<T>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct UserInfoRaw {
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub level: i32,
    #[serde(default, rename = "onsale_item_count")]
    pub on_sale_item_count: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CommentListRaw {
    #[serde(default)]
    pub count: usize,
    #[serde(default)]
    pub data: Vec<CommentRaw>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CommentRaw {
    #[serde(default, rename = "_id")]
    pub id: String,
    #[serde(default, deserialize_with = "deserialize_stringish")]
    pub iid: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(
        default,
        rename = "publish_time",
        deserialize_with = "deserialize_i64ish"
    )]
    pub publish_time: i64,
    #[serde(default, rename = "res_name")]
    pub res_name: String,
    #[serde(default, deserialize_with = "deserialize_stringish")]
    pub stars: String,
    #[serde(default, deserialize_with = "deserialize_stringish")]
    pub uid: String,
    #[serde(default, rename = "comment_tag")]
    pub comment_tag: String,
    #[serde(default, rename = "user_comment")]
    pub user_comment: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FeedbackListRaw {
    #[serde(default)]
    pub count: usize,
    #[serde(default)]
    pub data: Vec<FeedbackRaw>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FeedbackRaw {
    #[serde(default, rename = "_id")]
    pub id: String,
    #[serde(default, deserialize_with = "deserialize_stringish")]
    pub iid: String,
    #[serde(default, rename = "res_name")]
    pub res_name: String,
    #[serde(
        default,
        deserialize_with = "deserialize_stringish",
        rename = "commit_uid"
    )]
    pub commit_uid: String,
    #[serde(default, rename = "commit_nickname")]
    pub commit_nickname: String,
    #[serde(default, deserialize_with = "deserialize_stringish", rename = "type")]
    pub feedback_type: String,
    #[serde(default)]
    pub content: String,
    #[serde(default, rename = "create_time")]
    pub create_time: i64,
    #[serde(default)]
    pub reply: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_nullable_strings",
        rename = "pic_list"
    )]
    pub pic_list: Vec<String>,
    #[serde(default, rename = "feedback_log_file")]
    pub feedback_log_file: Option<String>,
    #[serde(default, rename = "forbid_reply")]
    pub forbid_reply: bool,
}

pub(crate) fn timestamp_to_rfc3339(timestamp: i64) -> String {
    OffsetDateTime::from_unix_timestamp(timestamp)
        .ok()
        .and_then(|value| value.format(&Rfc3339).ok())
        .unwrap_or_else(|| timestamp.to_string())
}

pub(crate) fn feedback_type_label(value: &str) -> &'static str {
    match value {
        "0" => "故障问题",
        "1" => "玩法建议",
        "2" => "内容侵权",
        "3" => "其他",
        "4" => "组件冲突",
        "5" => "我的山头",
        "6" => "性能反馈",
        _ => "未知类型",
    }
}

pub(crate) fn is_safe_http_url(value: &str) -> bool {
    url::Url::parse(value)
        .map(|url| matches!(url.scheme(), "http" | "https"))
        .unwrap_or(false)
}

fn deserialize_stringish<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Ok(match value {
        Value::String(value) => value,
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    })
}

fn deserialize_i64ish<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    Ok(match value {
        Value::Number(value) => value.as_i64().unwrap_or_default(),
        Value::String(value) => value.parse().unwrap_or_default(),
        _ => 0,
    })
}

fn deserialize_nullable_strings<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<Option<String>>::deserialize(deserializer)?;
    Ok(values.into_iter().flatten().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_fixture_tolerates_null_images_and_numeric_ids() {
        let raw: FeedbackListRaw = serde_json::from_str(
            r#"{"count":1,"data":[{"_id":"f1","iid":42,"commit_uid":7,"type":6,"pic_list":[null,"https://example.test/a.png"],"unknown":true}]}"#,
        )
        .unwrap();
        assert_eq!(raw.data[0].iid, "42");
        assert_eq!(raw.data[0].commit_uid, "7");
        assert_eq!(raw.data[0].feedback_type, "6");
        assert_eq!(raw.data[0].pic_list.len(), 1);
    }

    #[test]
    fn comment_fixture_accepts_string_timestamp() {
        let raw: CommentListRaw = serde_json::from_str(
            r#"{"count":1,"data":[{"_id":"c1","publish_time":"1724400000"}]}"#,
        )
        .unwrap();
        assert_eq!(raw.data[0].publish_time, 1_724_400_000);
    }

    #[test]
    fn unix_seconds_are_returned_as_rfc3339() {
        assert_eq!(timestamp_to_rfc3339(0), "1970-01-01T00:00:00Z");
    }
}
