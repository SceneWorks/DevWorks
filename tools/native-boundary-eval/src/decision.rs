//! Native deterministic router boundary subset. No model execution or real upstream.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
enum Tier {
    Max,
    High,
    Mid,
    Low,
}
#[derive(Clone, Debug)]
struct Model {
    id: &'static str,
    tier: Option<Tier>,
    efforts: BTreeSet<&'static str>,
    enabled: bool,
    decision: bool,
}
fn models() -> Vec<Model> {
    let mut out = vec![];
    for (tier, a, b) in [
        (Tier::Max, "fable", "astra"),
        (Tier::High, "opus", "sol"),
        (Tier::Mid, "sonnet", "terra"),
        (Tier::Low, "haiku", "luna"),
    ] {
        for id in [a, b] {
            out.push(Model {
                id,
                tier: Some(tier),
                efforts: BTreeSet::from(["low", "high"]),
                enabled: true,
                decision: false,
            })
        }
    }
    out.push(Model {
        id: "jev",
        tier: None,
        efforts: BTreeSet::new(),
        enabled: true,
        decision: true,
    });
    out
}
fn allocate<'a>(
    models: &'a [Model],
    tier: Tier,
    effort: Option<&str>,
    cursor: &mut usize,
    pin: Option<&str>,
) -> Option<&'a str> {
    let eligible: Vec<_> = models
        .iter()
        .filter(|m| {
            m.enabled
                && !m.decision
                && m.tier == Some(tier)
                && effort.is_none_or(|e| m.efforts.contains(e))
        })
        .collect();
    if let Some(p) = pin {
        if let Some(m) = eligible.iter().find(|m| m.id == p) {
            return Some(m.id);
        }
    }
    if eligible.is_empty() {
        return None;
    }
    let id = eligible[*cursor % eligible.len()].id;
    *cursor += 1;
    Some(id)
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum Question {
    Choice {
        instructions: String,
        criteria: BTreeMap<String, Option<String>>,
    },
    Noul {
        instructions: String,
    },
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
    Box {
        instructions: String,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionRequest {
    model: String,
    state: Value,
    questions: BTreeMap<String, Question>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    images: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    videos: Vec<String>,
}
impl DecisionRequest {
    fn validate(
        &self,
        types: &BTreeSet<&str>,
        images: bool,
        videos: bool,
    ) -> Result<(), &'static str> {
        if self.model.is_empty() || self.questions.is_empty() || self.questions.len() > 128 {
            return Err("invalid decision request");
        }
        for q in self.questions.values() {
            let (kind, instructions) = match q {
                Question::Choice {
                    instructions,
                    criteria,
                } => {
                    if criteria.len() < 2 || criteria.len() > 256 {
                        return Err("choice bounds");
                    }
                    ("choice", instructions)
                }
                Question::Noul { instructions } => ("noul", instructions),
                Question::Score {
                    instructions,
                    criteria,
                } => {
                    if criteria.len() < 2 || criteria.len() > 256 {
                        return Err("score bounds");
                    }
                    ("score", instructions)
                }
                Question::Box { instructions } => ("box", instructions),
            };
            if instructions.is_empty() || !types.contains(kind) {
                return Err("question unsupported");
            }
        }
        if !images && !self.images.is_empty() || !videos && !self.videos.is_empty() {
            return Err("media unsupported");
        }
        if serde_json::to_vec(self).map_err(|_| "serialization")?.len() > 1024 * 1024 {
            return Err("request size");
        }
        Ok(())
    }
}
pub fn run() -> Result<Value, Box<dyn std::error::Error>> {
    let mut checks = Vec::new();
    let mut all = models();
    let mut cursors = BTreeMap::new();
    for (tier, a, b) in [
        (Tier::Max, "fable", "astra"),
        (Tier::High, "opus", "sol"),
        (Tier::Mid, "sonnet", "terra"),
        (Tier::Low, "haiku", "luna"),
    ] {
        let cursor = cursors.entry(tier).or_insert(0);
        if allocate(&all, tier, None, cursor, None) != Some(a)
            || allocate(&all, tier, None, cursor, None) != Some(b)
            || allocate(&all, tier, None, cursor, Some(a)) != Some(a)
        {
            return Err("tier round robin/pin fixture".into());
        }
        checks.push(format!("{tier:?}_allocates_pair_and_reuses_pin"));
    }
    all.push(Model {
        id: "custom",
        tier: Some(Tier::Max),
        efforts: BTreeSet::from(["high"]),
        enabled: true,
        decision: false,
    });
    let mut cursor = 2;
    if allocate(&all, Tier::Max, Some("high"), &mut cursor, None) != Some("custom") {
        return Err("custom missing".into());
    }
    checks.push("custom_member_participates_without_model_alias_translation".into());
    if allocate(
        &all,
        Tier::Max,
        Some("unavailable"),
        &mut cursor,
        Some("fable"),
    )
    .is_some()
    {
        return Err("effort silently dropped".into());
    }
    checks.push("explicit_effort_excludes_incompatible_routes".into());
    for m in &mut all {
        if m.tier == Some(Tier::Max) {
            m.enabled = false;
        }
    }
    if allocate(&all, Tier::Max, None, &mut cursor, None).is_some() {
        return Err("empty tier fallback".into());
    }
    checks.push("empty_tier_does_not_cross_tier_or_select_jev".into());
    let request: DecisionRequest = serde_json::from_value(
        json!({"model":"jev","state":{"input":"fixture"},"questions":{"a":{"type":"choice","instructions":"Choose","criteria":{"yes":null,"no":null}},"b":{"type":"noul","instructions":"Is this true?"},"c":{"type":"score","instructions":"Score","criteria":["low","high"]},"d":{"type":"box","instructions":"Locate"}},"images":["data:image/png;base64,fixture"],"videos":["https://fixture.invalid/video"]}),
    )?;
    let types = BTreeSet::from(["choice", "noul", "score", "box"]);
    request.validate(&types, true, true)?;
    checks.push("all_four_typed_question_variants_and_media_validate".into());
    if request
        .validate(&BTreeSet::from(["choice"]), true, true)
        .is_ok()
        || request.validate(&types, false, true).is_ok()
        || request.validate(&types, true, false).is_ok()
    {
        return Err("capability check omitted".into());
    }
    checks.push("unsupported_question_image_or_video_rejected_before_send".into());
    let mut bad = serde_json::to_value(&request)?;
    bad["questions"]["x"] = json!({"type":"execute","instructions":"Run tool"});
    if serde_json::from_value::<DecisionRequest>(bad).is_ok() {
        return Err("unknown question accepted".into());
    }
    checks.push("unknown_question_cannot_become_execution".into());
    // Controlled localhost HTTP transport: verify upstream identity and structured body without inference.
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::Duration,
    };
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let raw_reply=json!({"answers":{"a":{"pick":"yes","probabilities":{"yes":0.8,"no":0.2}},"b":{"pick":true,"confidence":0.75},"c":{"pick":"high"},"d":{"box":[0.1,0.2,0.4,0.5]}},"usage":{"input_tokens":9},"upstream_extension":{"preserved":true}}).to_string();
    let reply = raw_reply.clone();
    let server = std::thread::spawn(move || -> Result<Value, String> {
        let (mut stream, _) = listener.accept().map_err(|e| e.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buf = [0u8; 4096];
        let header_end;
        let content_len;
        loop {
            let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("EOF".into());
            }
            bytes.extend_from_slice(&buf[..n]);
            if let Some(end) = bytes.windows(4).position(|x| x == b"\r\n\r\n") {
                header_end = end + 4;
                let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                if !headers.starts_with("post /v1/systemone ")
                    || !headers.contains("authorization: bearer fixture-only")
                {
                    return Err("path/auth binding".into());
                }
                content_len = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .ok_or("length")?
                    .parse::<usize>()
                    .map_err(|e| e.to_string())?;
                break;
            }
            if bytes.len() > 16384 {
                return Err("header bound".into());
            }
        }
        while bytes.len() < header_end + content_len {
            let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("body EOF".into());
            }
            bytes.extend_from_slice(&buf[..n]);
        }
        let body: Value = serde_json::from_slice(&bytes[header_end..header_end + content_len])
            .map_err(|e| e.to_string())?;
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",reply.len(),reply).map_err(|e|e.to_string())?;
        Ok(body)
    });
    let mut body = serde_json::to_value(&request)?;
    body["model"] = json!("jev-upstream");
    let response = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?
        .post(format!("http://{addr}/v1/systemone"))
        .bearer_auth("fixture-only")
        .json(&body)
        .send()?
        .error_for_status()?
        .text()?;
    let observed = server.join().map_err(|_| "HTTP fixture panicked")??;
    if observed != body || response != raw_reply {
        return Err("decision wire round trip changed body".into());
    }
    checks.push("native_local_http_preserves_state_questions_media_and_upstream_model".into());
    checks.push("structured_answers_usage_and_unknown_fields_preserved_as_untrusted_output".into());
    Ok(
        json!({"host":"macOS arm64","passed":checks.len(),"checks":checks,"noInference":true,"fixtureHttpOnly":true,"limits":["Native deterministic allocation/capability and localhost wire subset, not production router state, durable pins, pricing/permissions, credentials or live Jev qualification.","Example probabilities are fixture data and grant no operation authority. Native public SystemOne schema and actual per-model output semantics remain qualification work."]}),
    )
}
