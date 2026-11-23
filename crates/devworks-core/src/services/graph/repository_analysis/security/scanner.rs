//! Bounded defensive checks over captured bytes. Findings contain locations and rule names only.
use super::{Category, Finding, Severity};
use crate::services::graph::budget::Budget;
use regex::Regex;
use std::sync::LazyLock;
use url::Url;

struct Rule {
    name: &'static str,
    pattern: Regex,
    severity: Severity,
    csharp: bool,
    entropy: bool,
}

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("retained security rule")
}

static SECRET_RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    use Severity::*;
    [
        ("AWS Access Key", r"AKIA[0-9A-Z]{16}", Critical, false, false),
        ("Service API Token", r"(?:gh[pousr]_[A-Za-z0-9]{20,255}|sk-(?:proj-)?[A-Za-z0-9_-]{20,255}|AIza[0-9A-Za-z_-]{35})", Critical, false, false),
        ("Private Key", r"-----BEGIN\s+(RSA|EC|DSA|OPENSSH)\s+PRIVATE\s+KEY-----", Critical, false, false),
        ("Connection String Password", r#"(?i)(?:password|pwd)["']?\s*[:=]\s*["']?([^;"'\s]{3,})"#, High, false, true),
        ("Credentialed URL", r#"(?i)https?://[^/\s:@"']+:([^@\s/"']{3,})@[^/\s"']+"#, High, false, false),
        ("Generic API Key", r#"(?i)["']?(?:api[_-]?key|apikey|secret[_-]?key)["']?\s*[:=]\s*["']([A-Za-z0-9_:/+=-]{20,})["']"#, Medium, false, true),
        ("Config Secret", r#"(?i)["']?(?:client[_-]?secret|access[_-]?token|refresh[_-]?token|auth[_-]?token|secret[_-]?access[_-]?key)["']?\s*[:=]\s*["']([^"']{10,})["']"#, High, false, true),
        ("JWT/Signing Secret", r#"(?i)["']?(?:jwt[_-]?secret|signing[_-]?key)["']?\s*[:=]\s*["']([^"']{10,})["']"#, High, false, true),
        ("Hardcoded Password Assignment", r#"(?i)(?:password|passwd)["']?\s*[:=]\s*"([^"]{3,})""#, High, true, true),
    ].into_iter().map(|(name, pattern, severity, csharp, entropy)| {
        Rule { name, pattern: regex(pattern), severity, csharp, entropy }
    }).collect()
});

static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"(?i)changeme|todo|placeholder|xxx|your[_-]?key|replace[_-]?me|dummy|fake")
});
static FALSE_POSITIVE: LazyLock<Regex> = LazyLock::new(|| {
    regex(concat!(
        r#"(?i)(?:password|passwd|pwd|pass)\s*=\s*["']\s*["']"#,
        r"|(?:password|passwd|pwd)\s*=\s*\w+\.\w+",
        r#"|(?:string|var|object)\s+(?:password|passwd|pass)\s*=\s*[^\s"']"#,
        r"|string\s+(?:password|passwd|pass|pwd)\s*[,)]",
        r"|(?:string|void|bool|Task|int)\s+\w+\s*\([^)]*(?:password|passwd|pass|pwd)\s*=",
        r#"|(?:GetValue|GetSection|GetConnectionString)\s*\(\s*["'](?:password|pwd)"#,
        r#"|\[\s*["'](?:password|pwd)["']\s*\]"#,
        r"|(?:IsNullOrEmpty|IsNullOrWhiteSpace)\s*\(\s*(?:password|passwd|pwd)",
        r"|(?:password|passwd|pwd)\s*(?:==|!=)\s*null",
        r"|(?:public|private|protected|internal)\s+string\??\s+(?:Password|Passwd|Pwd)\s*\{",
        r"|(?:password|passwd|pwd)\s*=\s*(?:null|default|string\.Empty)",
        r"|\[(?:Password|DataType\s*\(\s*DataType\.Password)",
        r"|(?:Hash|Validate|Verify|Check|Reset|Change|Encode|Decode|Encrypt|Decrypt)Password",
    ))
});

static SQL: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"(?:ExecuteAsync|QueryAsync|QueryFirstAsync|QuerySingleAsync|FromSqlRaw|SqlCommand)(?:<[^>]+>)?\s*\(\s*\$""#,
    )
});
static SERIALIZATION: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"BinaryFormatter|JavaScriptSerializer|TypeNameHandling\s*\.\s*(?:All|Auto|Objects|Arrays)",
    )
});
static CORS: LazyLock<Regex> =
    LazyLock::new(|| regex(r"AllowAnyOrigin\s*\(|SetIsOriginAllowed\s*\(\s*[^)]*=>\s*true"));
static CREDENTIALS: LazyLock<Regex> = LazyLock::new(|| regex(r"AllowCredentials\s*\("));
static TLS: LazyLock<Regex> = LazyLock::new(|| {
    regex(concat!(
        r"(?:ServerCertificateCustomValidationCallback|RemoteCertificateValidationCallback)",
        r"(?:\s*=\s*(?:HttpClientHandler\.)?DangerousAcceptAnyServerCertificateValidator",
        r"|\s*=\s*(?:\([^)]*\)|\w+)\s*=>\s*true|[\s\S]{0,160}return\s+true\s*;)",
    ))
});
static HTTP: LazyLock<Regex> = LazyLock::new(|| regex(r#"(?i)["'](http://[^"'\s]+)["']"#));

const NONPRODUCTION: &[&str] = &[
    "test",
    "tests",
    "spec",
    "specs",
    "__tests__",
    "fixture",
    "fixtures",
    "sample",
    "samples",
    "example",
    "examples",
    "mock",
    "mocks",
    "stub",
    "stubs",
    "demo",
    "demos",
    "benchmark",
    "benchmarks",
];

pub(in crate::services::graph::repository_analysis) fn nonproduction(path: &str) -> bool {
    path.split(['/', '\\']).any(|part| {
        let part = part.to_ascii_lowercase();
        NONPRODUCTION.contains(&part.as_str())
            || part
                .split(['.', '_', '-', ' '])
                .any(|part| NONPRODUCTION.contains(&part))
    })
}

pub(in crate::services::graph::repository_analysis) fn sensitive(path: &str) -> bool {
    let file = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    (file.starts_with("appsettings") && file.ends_with(".json"))
        || file.starts_with(".env")
        || matches!(
            file.as_str(),
            "web.config"
                | "app.config"
                | "environment.config"
                | "launchsettings.json"
                | "secrets.json"
        )
        || (file.starts_with("docker-compose")
            && (file.ends_with(".yml") || file.ends_with(".yaml")))
}

pub(in crate::services::graph::repository_analysis) fn scan(
    path: &str,
    source: &str,
    budget: &mut Budget,
    findings: &mut Vec<Finding>,
) {
    if nonproduction(path) {
        return;
    }
    let csharp = path.to_ascii_lowercase().ends_with(".cs");
    let sensitive = sensitive(path);
    let mut add = |category, severity, title: &str, line, budget: &mut Budget| {
        if !budget.retain(path.len() + title.len() + 128) {
            return false;
        }
        findings.push(Finding::source(category, severity, title, path, line));
        true
    };
    for (index, line) in source.split('\n').enumerate() {
        if !budget.visit() {
            return;
        }
        if !line.trim().is_empty()
            && !line.trim_start().starts_with("//")
            && !PLACEHOLDER.is_match(line)
            && !FALSE_POSITIVE.is_match(line)
        {
            for rule in &*SECRET_RULES {
                if rule.csharp && !csharp {
                    continue;
                }
                if let Some(captures) = rule.pattern.captures(line) {
                    if rule.entropy
                        && !sensitive
                        && !captures
                            .get(1)
                            .or_else(|| captures.get(0))
                            .is_some_and(|m| looks_secret(m.as_str()))
                    {
                        continue;
                    }
                    if !add(
                        Category::Secret,
                        rule.severity,
                        rule.name,
                        index + 1,
                        budget,
                    ) {
                        return;
                    }
                }
            }
        }
        if csharp {
            for (rule, title) in [
                (&*SQL, "Potential SQL injection"),
                (&*SERIALIZATION, "Unsafe deserialization"),
            ] {
                if rule.is_match(line)
                    && !add(
                        Category::AttackSurface,
                        Severity::High,
                        title,
                        index + 1,
                        budget,
                    )
                {
                    return;
                }
            }
        }
    }
    let line = |byte: usize| {
        1 + source.as_bytes()[..byte]
            .iter()
            .filter(|b| **b == b'\n')
            .count()
    };
    if csharp {
        if let Some(found) = CORS.find(source) {
            let (severity, title) = if CREDENTIALS.is_match(source) {
                (Severity::High, "CORS allows any origin with credentials")
            } else {
                (Severity::Medium, "Permissive CORS policy")
            };
            if !add(
                Category::AttackSurface,
                severity,
                title,
                line(found.start()),
                budget,
            ) {
                return;
            }
        }
        if let Some(found) = TLS.find(source)
            && !add(
                Category::AttackSurface,
                Severity::High,
                "TLS certificate validation bypass",
                line(found.start()),
                budget,
            )
        {
            return;
        }
    }
    if let Some(found) = HTTP
        .captures_iter(source)
        .find(|captures| captures.get(1).is_some_and(|m| nonlocal_http(m.as_str())))
        .and_then(|captures| captures.get(0))
        && !add(
            Category::AttackSurface,
            Severity::Medium,
            "Non-local HTTP endpoint",
            line(found.start()),
            budget,
        )
    {
        return;
    }
    if csharp
        && (source.contains("[ApiController]") || source.contains("[Route("))
        && !source.contains("[Authorize")
        && !source.contains("[AllowAnonymous")
        && let Some(index) = source
            .split('\n')
            .position(|line| line.contains("class ") && line.contains("Controller"))
    {
        add(
            Category::AttackSurface,
            Severity::Medium,
            "Controller without authorization",
            index + 1,
            budget,
        );
    }
}

fn nonlocal_http(url: &str) -> bool {
    let host = Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned));
    host.is_none_or(|host| {
        !matches!(
            host.as_str(),
            "localhost" | "127.0.0.1" | "0.0.0.0" | "host.docker.internal" | "[::1]"
        )
    })
}

fn looks_secret(value: &str) -> bool {
    let length = value.chars().count();
    if length < 3 {
        return false;
    }
    let special = value
        .chars()
        .any(|c| "!@#$%^&*()+=\\{}[]|:;<>?,~".contains(c));
    if special && length >= 6 {
        return true;
    }
    let digit = value.chars().any(|c| c.is_numeric());
    let upper = value.chars().filter(|c| c.is_uppercase()).count();
    let lower = value.chars().any(char::is_lowercase);
    let entropy = entropy(value);
    if digit && upper > 0 && lower && entropy > 2.5 {
        return true;
    }
    if !digit && !special && upper >= 2 && upper < length && value.chars().all(char::is_alphabetic)
    {
        return false;
    }
    entropy > 4.0
}

fn entropy(value: &str) -> f64 {
    let mut counts = [0usize; 128];
    let mut total = 0;
    for b in value.bytes().filter(u8::is_ascii) {
        counts[usize::from(b)] += 1;
        total += 1;
    }
    counts
        .into_iter()
        .filter(|count| *count > 0)
        .map(|count| {
            let fraction = count as f64 / total as f64;
            -fraction * fraction.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::data::QueryLimits;

    fn run(path: &str, body: &str) -> Vec<Finding> {
        let mut found = Vec::new();
        scan(
            path,
            body,
            &mut Budget::new(&QueryLimits {
                max_results: 10,
                max_visited: 1000,
                deadline_ms: 30_000,
            }),
            &mut found,
        );
        found
    }

    #[test]
    fn only_sanitized_rule_names_and_locations_survive_secret_detection() {
        let source = concat!(
            "string password = \"replace-me\";\n",
            "string password = \"\";\n",
            "string password = configuration.Password;\n",
            "string password = \"ResetClientPassword\";\n",
            "string password = \"Ab7!mQ92JaP\";\n",
            "password = \"kL4%tQ2W8nV\";\n",
        );
        let findings = run("src/Options.cs", source);
        assert_eq!(findings.len(), 4);
        for line in [5, 6] {
            assert!(findings.iter().any(|finding| {
                finding.line == Some(line) && finding.title == "Hardcoded Password Assignment"
            }));
        }
        let json = serde_json::to_string(&findings).unwrap();
        assert!(!json.contains("Ab7"));
        assert!(!json.contains("kL4"));
        assert!(run("src/fixtures/Options.cs", source).is_empty());
        assert!(!run("appsettings.json", "password = \"short\"").is_empty());
    }

    #[test]
    fn loopback_urls_are_exempt_and_nonlocal_http_gets_one_location() {
        let findings = run(
            "src/client.cs",
            "\"http://localhost:81\"\n\"http://service.test.invalid/path\"\n",
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].line, Some(2));
        assert_eq!(findings[0].category, Category::AttackSurface);
        assert!(run("src/client.cs", "\"http://[::1]:81\"").is_empty());
    }
}
