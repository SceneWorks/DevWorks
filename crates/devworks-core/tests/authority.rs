//! Pure authority contracts: shell/declared-effect recognition, redaction, provider disclosure
//! and the access-profile catalog. Real-store and real-filesystem cases live in
//! `devworks-adapters`.
mod common;
use devworks_core::authority::destructive::{DestructiveKind, InputNeeded};
use devworks_core::authority::disclosure::{Candidate, ContentKind, Exclusion, disclose};
use devworks_core::authority::redact::{REDACTED, Redactor, SecretRef};
use devworks_core::authority::shell::{
    DeclaredEffect, ShellAssessment, assess_command, assess_command_with, assess_declared,
};
use devworks_core::authority::{AccessProfile, EffectivePolicy};
use devworks_core::ports::storage::GrantKind;
use devworks_core::settings::catalog::{AgentRole, descriptor};
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn shared_tool_choice_defaults_and_policy_digest_are_explicit() {
    let mut policy = EffectivePolicy {
        profile: Some(AccessProfile::Full),
        profile_source: None,
        shell_tools: true,
        web_tools: true,
        web_fetch: true,
        tool_overrides: Some(BTreeMap::new()),
    };
    assert!(policy.tool_selected("devworks/sessions.get", true));
    assert!(!policy.tool_selected("package/example.read", false));
    let before = policy.digest();
    policy
        .tool_overrides
        .as_mut()
        .unwrap()
        .insert("devworks/sessions.get".into(), false);
    assert!(!policy.tool_selected("devworks/sessions.get", true));
    assert_ne!(before, policy.digest());
    policy.tool_overrides = None;
    assert!(!policy.tool_selected("devworks/sessions.get", true));
}

fn argv(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_owned()).collect()
}
fn plans(a: &[&str]) -> Vec<(DestructiveKind, Vec<String>)> {
    match assess_command(&argv(a), Some("/work")) {
        ShellAssessment::Destructive { plans, residual } => {
            assert!(!residual.is_empty(), "residual risk is always reported");
            plans.into_iter().map(|p| (p.kind, p.targets)).collect()
        }
        other => panic!("{a:?}: {other:?}"),
    }
}
fn needs(a: &[&str]) -> InputNeeded {
    match assess_command(&argv(a), Some("/work")) {
        ShellAssessment::NeedsInput(n) => n,
        other => panic!("{a:?}: {other:?}"),
    }
}

#[test]
fn shell_deletions_are_recognized_from_arguments_not_command_strings() {
    use DestructiveKind::*;
    assert_eq!(plans(&["rm", "-rf", "/"]), [(RemoveTree, vec!["/".into()])]);
    assert_eq!(
        plans(&["/bin/rm", "-f", "--", "-r"]),
        [(Unlink, vec!["-r".into()])]
    );
    assert_eq!(
        plans(&["env", "A=1", "sudo", "-n", "rm", "--recursive", "a", "b"]),
        [(RemoveTree, vec!["a".into(), "b".into()])]
    );
    assert_eq!(
        plans(&[
            "bash",
            "-lc",
            "true && rm -r 'my dir' ; echo hi > out.txt 2>/dev/null"
        ]),
        [
            (RemoveTree, vec!["my dir".into()]),
            (Truncate, vec!["out.txt".into()])
        ]
    );
    assert_eq!(
        plans(&["find", "build", "-name", "*.o", "-delete"]),
        [(EraseContents, vec!["build".into()])]
    );
    assert_eq!(
        plans(&["find", ".", "-exec", "rm", "{}", ";"]),
        [(EraseContents, vec![".".into()])]
    );
    assert_eq!(
        plans(&["cmd.exe", "/c", "rd", "/s", "/q", "C:\\"]),
        [(RemoveTree, vec!["C:\\".into()])]
    );
    assert_eq!(
        plans(&["cmd", "/c", "del", "/s", "/q", "D:\\data"]),
        [(EraseContents, vec!["D:\\data".into()])]
    );
    assert_eq!(
        plans(&[
            "pwsh",
            "-Command",
            "Remove-Item",
            "-Recurse",
            "-Force",
            "-Path",
            "C:\\repo"
        ]),
        [(RemoveTree, vec!["C:\\repo".into()])]
    );
    assert_eq!(plans(&["unlink", "link"]), [(Unlink, vec!["link".into()])]);
}

#[test]
fn mkdir_plans_a_write_of_each_new_folder() {
    use DestructiveKind::Truncate;
    assert_eq!(
        plans(&["mkdir", "-p", "-m", "700", "a/b", "/c"]),
        [(Truncate, vec!["a/b".into(), "/c".into()])]
    );
    assert_eq!(
        plans(&["sh", "-c", "mkdir -pv -- new 2>/dev/null"]),
        [(Truncate, vec!["new".into()])]
    );
    assert_eq!(
        plans(&["cmd", "/c", "md", "C:\\x\\y"]),
        [(Truncate, vec!["C:\\x\\y".into()])]
    );
    // An option the model does not know, or a dynamic folder, holds the command.
    assert_eq!(
        needs(&["mkdir", "--context=x", "a"]),
        InputNeeded::UnmodeledConsequentialCommand
    );
    assert_eq!(
        needs(&["sh", "-c", "mkdir -p \"$HOME\"/x"]),
        InputNeeded::UnresolvedExpansion
    );
}

#[test]
fn appending_redirects_plan_a_write_of_their_target() {
    use DestructiveKind::Truncate;
    for script in [
        "echo x >> /a/f",
        "echo x >>/a/f",
        "echo x 1>> /a/f",
        "echo x 2>> /a/f",
        "echo x &>> /a/f",
        "echo x &>>/a/f",
        "echo x 2>>/dev/null >> /a/f",
    ] {
        assert_eq!(
            plans(&["sh", "-c", script]),
            [(Truncate, vec!["/a/f".into()])],
            "{script}"
        );
    }
    assert_eq!(
        plans(&["cmd", "/c", "echo x >> C:\\x\\f"]),
        [(Truncate, vec!["C:\\x\\f".into()])]
    );
    assert_eq!(
        plans(&["pwsh", "-Command", "'x' >> /x/f"]),
        [(Truncate, vec!["/x/f".into()])]
    );
    // A dynamic append target is held like a dynamic `>` target.
    assert_eq!(
        needs(&["sh", "-c", "echo x >> \"$F\""]),
        InputNeeded::UnresolvedExpansion
    );
}

#[test]
fn powershell_write_targets_are_read_as_powershell_reads_them() {
    use DestructiveKind::{Truncate, Unlink};
    let ps = |s: &str| plans(&["pwsh", "-Command", s]);
    let held = |s: &str| needs(&["pwsh", "-Command", s]);
    // A `FileSystem::` provider prefix names the same path; it is removed before planning.
    for script in [
        "Set-Content -Path FileSystem::/x/f -Value hi",
        "Set-Content -Path 'filesystem::/x/f' -Value hi",
        "Add-Content Microsoft.PowerShell.Core\\FileSystem::/x/f hi",
        "'hi' > FileSystem::/x/f",
    ] {
        assert_eq!(ps(script), [(Truncate, vec!["/x/f".into()])], "{script}");
    }
    // Any other provider or drive qualifier is not a file the resolver can check.
    for script in [
        "Set-Content Registry::HKLM\\Software\\x hi",
        "Set-Content Env:X hi",
        "Set-Content Variable:x hi",
        "New-Item -Path Temp:/x",
        "'hi' >> Function:f",
    ] {
        assert_eq!(held(script), InputNeeded::UnresolvedTarget, "{script}");
    }
    // A doubled apostrophe inside a PowerShell single-quoted string is one apostrophe.
    assert_eq!(
        ps("Set-Content '/home/O''Brien/f' hi"),
        [(Truncate, vec!["/home/O'Brien/f".into()])]
    );
    assert_eq!(
        ps("'hi' >> '/home/O''Brien/f'"),
        [(Truncate, vec!["/home/O'Brien/f".into()])]
    );
    // Doubled quotes, backtick escapes and typographic quotes inside a double-quoted (or
    // otherwise quoted) PowerShell string are not decoded here: held.
    for script in [
        "Set-Content \"/home/a\"\"b/f\" hi",
        "Set-Content \"/home/a`tb/f\" hi",
        "Set-Content \"/home/a`\"b/f\" hi",
        "Set-Content \u{2018}/home/a/f\u{2019} hi",
    ] {
        assert_eq!(held(script), InputNeeded::UnresolvedExpansion, "{script}");
    }
    // POSIX keeps adjacent single-quoted segments as concatenation.
    assert_eq!(
        plans(&["sh", "-c", "rm 'a''b'"]),
        [(Unlink, vec!["ab".into()])]
    );
    // `-LiteralPath` (and every path of `New-Item` and `Out-File`) names wildcard characters
    // literally, which a pattern expansion cannot check: held.
    for script in [
        "Set-Content -LiteralPath '/x/a[1]/f' hi",
        "Add-Content -lp /x/a*/f hi",
        "New-Item -Path '/x/a[1]'",
        "mkdir '/x/a?'",
        "'hi' | Out-File '/x/a[1]/f'",
        "'hi' > '/x/a[1]/f'",
    ] {
        assert_eq!(held(script), InputNeeded::UnresolvedTarget, "{script}");
    }
}

#[test]
fn powershell_writers_are_held_unless_every_target_is_checked_exactly() {
    use DestructiveKind::Truncate;
    let held = |s: &str| {
        matches!(
            assess_command(&argv(&["pwsh", "-Command", s]), Some("/work")),
            ShellAssessment::NeedsInput(_)
        )
    };
    let mut cases = vec![
        // An unquoted backtick continues the line; the next line is the path.
        "Set-Content `\n/x/f hi",
        "Set-Content -Path /x/f `\n -Value hi",
        "'hi' > `\n/x/f",
        // PowerShell wildcards match case-insensitively, which the pattern expansion does not:
        // every wildcard target is held, under `-Path` or positional.
        "Set-Content -Path C:/work/devworks*/state.json -Value x",
        "Set-Content -Path '/x/a[1]/f' hi",
        "Add-Content /x/a?/f hi",
        "'hi' | Out-File -FilePath /x/*.log",
        "New-Item /x/[ab]",
        "mkdir /x/new*",
        // `-Force` expands wildcards over hidden entries too.
        "Set-Content -Path /x/*/f -Value hi -Force",
        "Add-Content -Force /x/s?/f hi",
        // Splatting binds named parameters the command text does not show.
        "Set-Content @p",
        "$p = @{Path = '/x/f'; Value = 'hi'}; Set-Content @p",
        "New-Item @('/x/a', '/x/b')",
        // Computed arguments, even in a value position.
        "Set-Content -Path /x/f -Value $v",
        "Set-Content /x/f $(Get-Date)",
        // `New-Item -Name` is joined to a path PowerShell resolves itself (an empty `-Path` is the
        // working directory, an omitted one may bind from the pipeline).
        "New-Item -Path '' -Name '.git/config' -ItemType File -Force",
        "New-Item -Path /x -Name f",
        "New-Item -Name f",
        "New-Item -Path FileSystem::/x -Name f",
        // Pipeline input can bind a writer's target parameters by property name.
        "Import-Csv ./paths.csv | New-Item -Name state.json -ItemType File",
        "Get-Item a | New-Item /x/f",
        "'hi' | Set-Content /x/f",
        "'hi' | Add-Content -Path /x/f",
        "'hi' | Out-File -FilePath /x/f -Append",
        // An alternate data stream.
        "Set-Content -Stream s -Path /x/f -Value hi",
        // A cmdlet reached through an alias or code the analysis does not read.
        "Set-Alias w New-Item; w /x/f",
        "New-Alias w Add-Content; w /x/f hi",
        "iex $code",
        "Invoke-Expression \"ni $p\"",
        "'ni /x/f' | iex",
        "& ([scriptblock]::Create('ni /x/f'))",
        "& ([System.Management.Automation.ScriptBlock]::Create('ac /x/f hi'))",
    ];
    // A drive mapped with `New-PSDrive` can point anywhere, a protected root included.
    cases.extend([
        "New-PSDrive -Name X -PSProvider FileSystem -Root /x; Set-Content X:/a/f hi",
        "ndr X FileSystem /x; ni X:/f",
        "mount X FileSystem /x; ni X:/f",
    ]);
    // PowerShell on Unix reads `\` as a separator too, and a drive-qualified path names a
    // PowerShell drive, never a native relative path.
    if cfg!(not(windows)) {
        cases.extend([
            "Set-Content X:/a/SKILL.md x",
            "Add-Content -LiteralPath C:/x/f hi",
            "New-Item -Path C:/x -Name f",
            "'hi' >> D:/x/f",
            "Set-Content -LiteralPath '/x\\f' -Value hi",
            "New-Item /x\\new",
            "'hi' >> '/x\\f'",
        ]);
    }
    let mut missed: Vec<String> = cases
        .into_iter()
        .filter(|s| !held(s))
        .map(|s| format!("not held: {s:?}"))
        .collect();
    // The call and dot-source operators run the named cmdlet with the same arguments, and
    // `Invoke-Expression` runs its one literal string as a script.
    for script in [
        "& ni /x/f",
        "& 'Set-Content' /x/f hi",
        ". ni /x/f",
        "iex 'ni /x/f'",
        "Invoke-Expression -Command 'Add-Content /x/f hi'",
        "iex -C 'ni /x/f'",
        "Invoke-Expression -Comm 'ni /x/f'",
        "iex -Command:'ni /x/f'",
    ] {
        let got = assess_command(&argv(&["pwsh", "-Command", script]), Some("/work"));
        let exact = matches!(&got, ShellAssessment::Destructive { plans, .. }
            if plans.len() == 1 && plans[0].kind == Truncate && plans[0].targets == ["/x/f"]);
        if !exact {
            missed.push(format!("not planned: {script:?}: {got:?}"));
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n"));
}

#[test]
fn quoted_names_nested_scripts_and_powershell_patterns_are_planned_exactly_or_held() {
    use DestructiveKind::{RemoveTree, Truncate, Unlink};
    let assess = |a: &[&str]| assess_command(&argv(a), Some("/work"));
    // (kind, targets, literal) of every plan.
    let planned = |a: &[&str]| match assess(a) {
        ShellAssessment::Destructive { plans, .. } => plans
            .into_iter()
            .map(|p| (p.kind, p.targets, p.literal))
            .collect::<Vec<_>>(),
        other => panic!("{a:?}: {other:?}"),
    };
    let sh = |s: &str| planned(&["sh", "-c", s]);
    // POSIX: quoted or escaped wildcard characters name themselves; unquoted ones are a pattern.
    assert_eq!(
        sh("rm -rf '/x/a[1]'"),
        [(RemoveTree, vec!["/x/a[1]".into()], true)]
    );
    assert_eq!(
        sh("rm -rf /x/a\\[1\\]"),
        [(RemoveTree, vec!["/x/a[1]".into()], true)]
    );
    assert_eq!(
        sh("rm -f *.o \"/x/b?\""),
        [
            (Unlink, vec!["*.o".into()], false),
            (Unlink, vec!["/x/b?".into()], true)
        ]
    );
    // Quoted and unquoted wildcards in one word cannot be expressed as one pattern: held.
    assert_eq!(
        needs(&["sh", "-c", "rm -rf '/x/a['*"]),
        InputNeeded::UnresolvedExpansion
    );
    // PowerShell: `-LiteralPath` names its wildcard characters; a pattern matches
    // case-insensitively, so a removal or move with one is held (and still planned).
    assert_eq!(
        planned(&[
            "pwsh",
            "-Command",
            "Remove-Item -LiteralPath '/x/a[1]' -Recurse"
        ]),
        [(RemoveTree, vec!["/x/a[1]".into()], true)]
    );
    for script in [
        "Remove-Item -Recurse C:/work/devworks*",
        "Remove-Item -Path /x/a? -Force",
        "rm -r /x/[ab]",
        "Move-Item /x/a* /y",
        "Move-Item -Path /x/a* -Destination /y",
    ] {
        assert!(
            matches!(
                assess(&["pwsh", "-Command", script]),
                ShellAssessment::NeedsInput(InputNeeded::UnresolvedTarget)
            ),
            "{script}"
        );
    }
    // `eval` and `Invoke-Expression` run their literal words as a script.
    assert_eq!(
        sh("eval 'echo x >> /x/f'"),
        [(Truncate, vec!["/x/f".into()], false)]
    );
    assert_eq!(
        sh("eval echo x '>>' /x/f"),
        [(Truncate, vec!["/x/f".into()], false)]
    );
    assert_eq!(
        sh("eval 'rm -rf /x/repo'"),
        [(RemoveTree, vec!["/x/repo".into()], false)]
    );
    assert_eq!(
        planned(&["pwsh", "-Command", "Invoke-Expression 'Set-Content /x/f y'"]),
        [(Truncate, vec!["/x/f".into()], false)]
    );
}

#[test]
fn git_pathspecs_and_first_pass_expansions_are_held_unless_checked_exactly() {
    use DestructiveKind::EraseContents;
    let assess = |a: &[&str]| assess_command(&argv(a), Some("/work"));
    let held = |a: &[&str]| matches!(assess(a), ShellAssessment::NeedsInput(_));
    let mut missed = Vec::new();
    // Git matches pathspecs itself (`*` crosses `/`, magic such as `:(glob)`), quoted or not:
    // a wildcard or magic pathspec is held.
    for script in [
        "git clean -fdx 'cache*'",
        "git clean -fdx cache*",
        "git clean -fdx -- \"*.db\"",
        "git clean -fd 'a[1]'",
        "git clean -fdx ':(glob)**/x'",
        "git clean -fdx ':!keep'",
        "git clean -fdx ':(icase)cache'",
    ] {
        if !held(&["sh", "-c", script]) {
            missed.push(format!(
                "not held: {script}: {:?}",
                assess(&["sh", "-c", script])
            ));
        }
    }
    // A plain pathspec, or one made literal by `--literal-pathspecs`, is a path.
    for (script, target) in [
        ("git clean -fdx cache", "cache"),
        ("git --literal-pathspecs clean -fdx 'a[1]'", "a[1]"),
    ] {
        match assess(&["sh", "-c", script]) {
            ShellAssessment::Destructive { plans, .. }
                if plans.len() == 1
                    && plans[0].kind == EraseContents
                    && plans[0].targets == [target] => {}
            other => missed.push(format!("not planned: {script}: {other:?}")),
        }
    }
    // `eval` runs its words after the shell expanded them: an unquoted glob can name files
    // whose names are code, so only words the first pass leaves as written are reparsed.
    for script in [
        "eval rm -rf /outside/*",
        "eval echo x '>>' /x/f?",
        "eval rm -rf ~user/x",
        "eval rm -rf /x/{a,b}",
    ] {
        if !held(&["sh", "-c", script]) {
            missed.push(format!(
                "not held: {script}: {:?}",
                assess(&["sh", "-c", script])
            ));
        }
    }
    // `Invoke-Expression` takes one string: extra words or parameters are not modeled.
    for script in [
        "iex 'ni /x/f' extra",
        "Invoke-Expression -Verbose 'ni /x/f'",
        "iex \"ni $(Get-Location)/f\"",
        // `-C` binds `-Command`; anything beyond the plain form is held.
        "iex -C 'ac /x/f x' -Verbose",
        "iex -Command:'ni /x/f' -ErrorAction Stop",
        "iex -Command 'ni /x/f' 'more'",
        "iex @a",
        // A writer alias the structured analysis does not reach is held by the backstop.
        "Write-Output 'ac /x/f x'",
        "Write-Output 'ni /x/f'",
        "Write-Output 'md /x/f'",
        "Write-Output 'Add-Content /x/f x'",
        "Write-Output 'New-Item /x/f'",
    ] {
        if !held(&["pwsh", "-Command", script]) {
            missed.push(format!(
                "not held: {script}: {:?}",
                assess(&["pwsh", "-Command", script])
            ));
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n"));
}

#[test]
fn powershell_file_and_folder_writers_plan_a_write_of_each_path() {
    use DestructiveKind::Truncate;
    let ps = |s: &str| plans(&["pwsh", "-Command", s]);
    for script in [
        "New-Item -ItemType Directory -Path /x/f",
        "New-Item /x/f -ItemType File -Force",
        "New-Item -Path:/x/f -Type file -Value hi",
        "ni -p /x/f",
        "mkdir /x/f",
        "md -Force /x/f",
        "Set-Content /x/f hi",
        "Set-Content -Value hi /x/f",
        "sc -LiteralPath /x/f -Value hi -NoNewline",
        "Set-Content -lp /x/f hi",
        "Add-Content -Path /x/f hi",
        "ac /x/f -Value hi -Encoding utf8",
        "Out-File -InputObject hi -LiteralPath /x/f -Force",
        "Out-File /x/f utf8",
        "Set-Content -- /x/f hi",
    ] {
        assert_eq!(ps(script), [(Truncate, vec!["/x/f".into()])], "{script}");
    }
    assert_eq!(
        ps("New-Item -Path /x/a -Path /x/b"),
        [(Truncate, vec!["/x/a".into(), "/x/b".into()])]
    );
    // Unknown parameters, extra positional words, links and dynamic paths hold the command.
    let held = |s: &str| needs(&["pwsh", "-Command", s]);
    for (script, expected) in [
        (
            "New-Item -ItemType SymbolicLink -Path /x/l -Target C:/y",
            InputNeeded::UnmodeledConsequentialCommand,
        ),
        (
            "New-Item -ItemType $t /x/f",
            InputNeeded::UnmodeledConsequentialCommand,
        ),
        (
            "New-Item -Credential $c /x/f",
            InputNeeded::UnmodeledConsequentialCommand,
        ),
        (
            "Add-Content -Wait /x/f hi",
            InputNeeded::UnmodeledConsequentialCommand,
        ),
        (
            "Set-Content /x/f hi there",
            InputNeeded::UnmodeledConsequentialCommand,
        ),
        // `-p` is ambiguous between `-Path` and `-PassThru`.
        (
            "Set-Content -p /x/f hi",
            InputNeeded::UnmodeledConsequentialCommand,
        ),
        ("Add-Content $p hi", InputNeeded::UnresolvedExpansion),
        ("mkdir /x/a,/x/b", InputNeeded::UnresolvedExpansion),
        (
            "New-Item -Path /x -Name $n",
            InputNeeded::UnresolvedExpansion,
        ),
        ("'hi' | Out-File", InputNeeded::UnresolvedTarget),
        ("Set-Content -Value hi", InputNeeded::UnresolvedTarget),
    ] {
        assert_eq!(held(script), expected, "{script}");
    }
}

#[test]
fn unresolved_expansion_and_opaque_targets_are_held() {
    assert_eq!(
        needs(&["sh", "-c", "rm -rf \"$HOME\"/x"]),
        InputNeeded::UnresolvedExpansion
    );
    assert_eq!(
        needs(&["sh", "-c", "rm -rf `pwd`"]),
        InputNeeded::UnresolvedExpansion
    );
    assert_eq!(
        needs(&["sh", "-c", "rm -rf $(ls)"]),
        InputNeeded::UnresolvedExpansion
    );
    assert_eq!(
        needs(&["cmd", "/c", "rd", "/s", "%USERPROFILE%"]),
        InputNeeded::UnresolvedExpansion
    );
    assert_eq!(
        needs(&["xargs", "-0", "rm", "-rf"]),
        InputNeeded::UnresolvedTarget
    );
    // Single quotes are literal.
    assert_eq!(
        plans(&["sh", "-c", "rm '$literal'"]),
        [(DestructiveKind::Unlink, vec!["$literal".into()])]
    );
    // Nothing recognized is reported with residual risk, never as safe.
    for cmd in [
        &["ls", "-la"][..],
        &["python3", "-c", "import shutil"],
        &["sh", "script.sh"],
    ] {
        match assess_command(&argv(cmd), None) {
            ShellAssessment::NoRecognizedDestruction { residual } => {
                assert!(residual.contains("not proven harmless"))
            }
            other => panic!("{other:?}"),
        }
    }
    // Declared MCP/package effects: destructive intent without targets is held.
    assert!(matches!(
        assess_declared(
            &DeclaredEffect {
                destructive: true,
                recursive: true,
                targets: None
            },
            None
        ),
        ShellAssessment::NeedsInput(InputNeeded::UnresolvedDeclaredTarget)
    ));
    match assess_declared(
        &DeclaredEffect {
            destructive: true,
            recursive: true,
            targets: Some(vec!["/srv/x".into()]),
        },
        None,
    ) {
        ShellAssessment::Destructive { plans, .. } => {
            assert_eq!(plans[0].kind, DestructiveKind::RemoveTree)
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn redaction_removes_known_values_headers_typed_fields_and_token_shapes() {
    let mut r = Redactor::new();
    r.register("hunter2-secret-value");
    let text = "login with hunter2-secret-value\nAuthorization: Bearer abc.def\npassword=letmein token: x123 ok\nkey ghp_0123456789abcdefABCDEF here\nsha 0123456789abcdef0123456789abcdef01234567\n";
    let out = r.redact_text(text);
    assert!(!out.contains("hunter2-secret-value"));
    assert!(!out.contains("abc.def"));
    assert!(!out.contains("letmein"));
    assert!(!out.contains("x123"));
    assert!(!out.contains("ghp_0123456789abcdefABCDEF"));
    assert!(out.contains("Authorization: [REDACTED]"));
    assert!(out.contains("password=[REDACTED]"));
    // Ordinary content survives, including a lowercase Git object ID.
    assert!(out.contains("0123456789abcdef0123456789abcdef01234567"));
    assert!(out.contains(" ok\n"));
    let v = r.redact_json(&json!({
        "user": "me",
        "api_key": "k",
        "nested": [{"clientSecret": "s", "note": "uses hunter2-secret-value"}],
        "refresh-token": null,
    }));
    assert_eq!(v["user"], "me");
    assert_eq!(v["api_key"], REDACTED);
    assert_eq!(v["nested"][0]["clientSecret"], REDACTED);
    assert_eq!(v["nested"][0]["note"], format!("uses {REDACTED}"));
    assert!(v["refresh-token"].is_null());
    // Secret references are opaque IDs and never print a value.
    // Secret references use the settings format and survive redaction, as text and as a
    // credential setting value.
    let minted = SecretRef::mint(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
    assert_eq!(minted.as_str(), "dwsecret-0123456789abcdef0123456789abcdef");
    assert_eq!(SecretRef::parse(minted.as_str()), Some(minted.clone()));
    assert_eq!(
        format!("{minted:?}"),
        format!("SecretRef({})", minted.as_str())
    );
    assert!(SecretRef::parse("github-default").is_none());
    assert!(SecretRef::parse("dwsecret-ghp_0123456789abcdefABCD").is_none());
    let line = format!("using {} for GitHub", minted.as_str());
    assert_eq!(r.redact_text(&line), line);
    let setting =
        json!({"credential": {"secretRef": minted.as_str()}, "token": "raw-session-value"});
    let v = r.redact_json(&setting);
    assert_eq!(v["credential"]["secretRef"], minted.as_str());
    assert_eq!(v["token"], REDACTED);
}

#[test]
fn provider_disclosure_matches_what_is_sent() {
    let mut r = Redactor::new();
    r.register("sk-live-0123456789abcdef");
    let (sent, record) = disclose(
        "https://api.provider.invalid",
        "model-a",
        None,
        &r,
        &[
            Candidate {
                kind: ContentKind::File,
                project_id: None,
                source: "src/lib.rs",
                revision: Some("abc"),
                text: "fn main() {}",
                authorized: true,
            },
            Candidate {
                kind: ContentKind::File,
                project_id: None,
                source: ".env",
                revision: None,
                text: "KEY=sk-live-0123456789abcdef",
                authorized: true,
            },
            Candidate {
                kind: ContentKind::Snippet,
                project_id: None,
                source: "other-project/notes.md",
                revision: None,
                text: "private",
                authorized: false,
            },
        ],
    );
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|t| !t.contains("sk-live")));
    assert_eq!(record.items.len(), 2);
    assert!(record.items[1].redacted);
    assert_eq!(record.items[0].bytes, 12);
    let reasons: Vec<_> = record
        .excluded
        .iter()
        .map(|e| (e.source.as_str(), e.reason))
        .collect();
    assert_eq!(
        reasons,
        [
            (".env", Exclusion::SecretRedacted),
            ("other-project/notes.md", Exclusion::Unauthorized)
        ]
    );
    let data = serde_json::to_value(&record).unwrap();
    assert_eq!(data["destination"], "https://api.provider.invalid");
    assert_eq!(data["items"][0]["source"], "src/lib.rs");
}

#[test]
fn access_profile_setting_defaults_to_full_and_names_round_trip() {
    let d = descriptor("safety.accessProfile").unwrap();
    assert_eq!(d.default_value(), json!("full"));
    assert!(
        !d.locked(),
        "the profile is a user choice; mandatory guards are separate policies"
    );
    for p in [AccessProfile::Full, AccessProfile::ProjectScope] {
        assert_eq!(AccessProfile::parse(p.as_str()), Some(p));
    }
    assert_eq!(AccessProfile::parse("sandboxed"), None);
    for r in AgentRole::ALL {
        assert_eq!(AgentRole::parse(r.as_str()), Some(r));
    }
    for k in [
        GrantKind::Operation,
        GrantKind::NativeExecution,
        GrantKind::PrimaryRepositoryDeletion,
    ] {
        assert_eq!(GrantKind::parse(k.as_str()), Some(k));
    }
    for key in [
        "safety.reviewersReadOnly",
        "safety.protectMainBranch",
        "safety.destructiveOperationGuard",
    ] {
        assert!(
            descriptor(key).unwrap().locked(),
            "{key} stays harness-enforced"
        );
    }
}

fn sh(script: &str) -> ShellAssessment {
    assess_command(&argv(&["sh", "-c", script]), Some("/tmp"))
}
fn sh_plans(script: &str) -> Vec<(DestructiveKind, Vec<String>)> {
    match sh(script) {
        ShellAssessment::Destructive { plans, .. } => {
            plans.into_iter().map(|p| (p.kind, p.targets)).collect()
        }
        other => panic!("{script}: {other:?}"),
    }
}
fn sh_needs(script: &str) -> InputNeeded {
    match sh(script) {
        ShellAssessment::NeedsInput(n) => n,
        other => panic!("{script}: {other:?}"),
    }
}

#[test]
fn compound_commands_groups_substitutions_and_wrapper_options_are_seen_through() {
    use DestructiveKind::*;
    let repo = || vec![(RemoveTree, vec!["/x/repo".to_owned()])];
    for script in [
        "if true; then rm -rf /x/repo; fi",
        "(rm -rf /x/repo)",
        "{ rm -rf /x/repo; }",
        "echo $(rm -rf /x/repo)",
        "echo `rm -rf /x/repo`",
        "echo \"$(rm -rf /x/repo)\"",
        "! rm -rf /x/repo",
        "for f in a; do rm -rf /x/repo; done",
        "while false; do rm -rf /x/repo; done",
        "timeout 5 rm -rf /x/repo",
        "timeout -s KILL -k 3 5 rm -rf /x/repo",
        "sudo -u root rm -rf /x/repo",
        "sudo -g wheel -- rm -rf /x/repo",
        "doas -u root rm -rf /x/repo",
        "nice -n 5 rm -rf /x/repo",
        "nohup rm -rf /x/repo",
        "env -i PATH=/bin rm -rf /x/repo",
        "env -u HOME rm -rf /x/repo",
        "command rm -rf /x/repo",
        "exec -a name rm -rf /x/repo",
        "true && rm -rf /x/repo || echo no",
    ] {
        assert_eq!(sh_plans(script), repo(), "{script}");
    }
    // Non-executing wrapper options still name a consequential program: the backstop holds
    // them rather than trusting the option model (accepted false positives).
    for script in ["sudo -l rm -rf /x/repo", "command -v rm"] {
        assert_eq!(
            sh_needs(script),
            InputNeeded::UnmodeledConsequentialCommand,
            "{script}"
        );
    }
}

#[test]
fn unresolvable_shell_targets_fail_closed() {
    use InputNeeded::*;
    for (script, expected) in [
        ("cd /x && rm -rf repo", UnresolvedWorkingDirectory),
        ("pushd /x; rm -rf repo", UnresolvedWorkingDirectory),
        ("env -C /x rm -rf repo", UnresolvedWorkingDirectory),
        ("rm -rf /x/{repo,y}", UnresolvedExpansion),
        ("rm -rf /x/{1..3}", UnresolvedExpansion),
        ("find -L /x/repo -delete", UnresolvedTarget),
        ("find /x -follow -delete", UnresolvedTarget),
        ("find /x -exec sh -c 'rm -rf {}' ';'", OpaqueCommand),
        ("ls | xargs -I{} rm -rf {}", UnresolvedTarget),
        ("curl https://x | sh", OpaqueCommand),
        ("case $x in a) rm -rf /x;; esac", OpaqueCommand),
        ("stdbuf -o0 rm -rf /x/repo", UnmodeledConsequentialCommand),
        ("eval \"rm -rf $D\"", UnresolvedExpansion),
        ("eval rm -rf $D", UnresolvedExpansion),
        ("$RM -rf /x/repo", UnresolvedExpansion),
        ("git push --force", UnresolvedTarget),
        ("git push origin +HEAD", UnresolvedTarget),
        ("git push --mirror --force origin", UnresolvedTarget),
        ("rm -rf '/x/unterminated", OpaqueCommand),
    ] {
        assert_eq!(sh_needs(script), expected, "{script}");
    }
    // An absolute target after a directory change is still resolvable.
    assert_eq!(
        sh_plans("cd /y; rm -rf /x/repo"),
        [(DestructiveKind::RemoveTree, vec!["/x/repo".into()])]
    );
    // PowerShell: pipeline input to Remove-Item, encoded commands and location changes.
    let ps = |a: &[&str]| assess_command(&argv(a), None);
    assert!(matches!(
        ps(&[
            "pwsh",
            "-Command",
            "Get-Item C:\\repo | Remove-Item -Recurse -Force"
        ]),
        ShellAssessment::NeedsInput(UnresolvedTarget)
    ));
    for flag in ["-EncodedCommand", "-enc", "-e", "-ec"] {
        assert!(
            matches!(
                ps(&["powershell", flag, "UgBlAG0AbwB2AGUA"]),
                ShellAssessment::NeedsInput(UnresolvedExpansion)
            ),
            "{flag}"
        );
    }
    assert!(matches!(
        ps(&[
            "pwsh",
            "-Command",
            "Set-Location C:\\x; Remove-Item -Recurse repo"
        ]),
        ShellAssessment::NeedsInput(UnresolvedWorkingDirectory)
    ));
    assert!(matches!(
        ps(&["cmd", "/c", "cd /d C:\\x && rd /s /q repo"]),
        ShellAssessment::NeedsInput(UnresolvedWorkingDirectory)
    ));
}

#[test]
fn git_moves_and_rsync_deletions_become_plans_or_protected_branch_refusals() {
    use DestructiveKind::*;
    for script in [
        "git push -f origin main",
        "git push --force-with-lease origin HEAD:main",
        "git push origin +master",
        "git push origin :main",
        "git push --delete origin main",
        "git push -d origin MAIN",
        "git push origin +refs/heads/Main",
        "git -C /x/repo push --force origin main",
    ] {
        assert!(
            matches!(sh(script), ShellAssessment::ProtectedBranch { .. }),
            "{script}"
        );
    }
    // The remote default branch, when known, is protected the same way.
    let trunk = vec!["trunk".to_owned()];
    assert!(matches!(
        assess_command_with(
            &argv(&["git", "push", "-f", "origin", "trunk"]),
            None,
            &trunk
        ),
        ShellAssessment::ProtectedBranch { .. }
    ));
    // Ordinary and feature-branch pushes are not refused here.
    for script in ["git push origin main", "git push -f origin feature/x"] {
        assert!(
            !matches!(sh(script), ShellAssessment::ProtectedBranch { .. }),
            "{script}"
        );
    }
    assert_eq!(
        sh_plans("git clean -fdx"),
        [(EraseContents, vec![".".into()])]
    );
    assert_eq!(
        sh_plans("git -C /x/repo clean -ffdx"),
        [(EraseContents, vec!["/x/repo".into()])]
    );
    assert!(matches!(
        sh("git clean -n -d"),
        ShellAssessment::NoRecognizedDestruction { .. }
    ));
    assert_eq!(
        sh_plans("git worktree remove --force /x/wt"),
        [(RemoveTree, vec!["/x/wt".into()])]
    );
    // The sources leave, and the destination may be overwritten.
    assert_eq!(
        sh_plans("mv /x/repo /y/"),
        [
            (Move, vec!["/x/repo".into()]),
            (Truncate, vec!["/y/".into()])
        ]
    );
    assert_eq!(
        sh_plans("mv -t /y /x/repo /x/other"),
        [
            (Move, vec!["/x/repo".into(), "/x/other".into()]),
            (Truncate, vec!["/y".into()])
        ]
    );
    assert_eq!(
        sh_plans("mv -ft /y /x/repo"),
        [
            (Move, vec!["/x/repo".into()]),
            (Truncate, vec!["/y".into()])
        ]
    );
    assert_eq!(
        sh_plans("rsync -a --delete /src/ /x/repo/"),
        [(EraseContents, vec!["/x/repo/".into()])]
    );
    assert!(matches!(
        sh("rsync -a --delete /src/ host:/x/repo/"),
        ShellAssessment::NoRecognizedDestruction { .. }
    ));
    assert_eq!(
        sh_plans("dd if=/dev/zero of=/x/disk.img"),
        [(Truncate, vec!["/x/disk.img".into()])]
    );
}

#[test]
fn redaction_covers_userinfo_flags_secret_fields_and_short_pem_blocks() {
    let r = Redactor::new();
    let out = r.redact_text(
        "git clone https://bob:s3cretpass@example.invalid/r.git\nmysql -pS3cretPass9 db\nrun --password hunter2x --token=abc123 --secret xyz789 -pthread\n",
    );
    for leaked in ["s3cretpass", "S3cretPass9", "hunter2x", "abc123", "xyz789"] {
        assert!(!out.contains(leaked), "{leaked} in {out}");
    }
    assert!(out.contains("https://bob:[REDACTED]@example.invalid/r.git"));
    assert!(out.contains("-pthread"), "ordinary flags survive");
    let pem = "before\n-----BEGIN PRIVATE KEY-----\nMIIBVQ==\n-----END PRIVATE KEY-----\nafter";
    assert_eq!(r.redact_text(pem), format!("before\n{REDACTED}\nafter"));
    let open = r.redact_text("-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA");
    assert!(!open.contains("b3BlbnNzaA"));
    let cert = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----";
    assert_eq!(r.redact_text(cert), cert, "public certificates are kept");
    let v = r.redact_json(&json!({
        "aws_secret_access_key": "x",
        "client_secret_id": "y",
        "client_secret": "w",
        "credentialRevision": "7",
        "credentialRef": "store-entry-9",
        "dbPassword": "z",
        "tokenCount": 3,
        "secretRef": "dwsecret-0123456789abcdef0123456789abcdef",
    }));
    assert_eq!(v["aws_secret_access_key"], REDACTED);
    // Metadata about a secret (identifiers, revisions, references) is kept.
    assert_eq!(v["client_secret_id"], "y");
    assert_eq!(v["credentialRevision"], "7");
    assert_eq!(v["credentialRef"], "store-entry-9");
    assert_eq!(v["client_secret"], REDACTED);
    assert_eq!(v["dbPassword"], REDACTED);
    assert_eq!(v["tokenCount"], 3);
    assert_eq!(v["secretRef"], "dwsecret-0123456789abcdef0123456789abcdef");
}

#[test]
fn consequential_git_and_file_writers_are_refused_planned_or_held_never_residual() {
    use DestructiveKind::*;
    for script in [
        "git -C /x/repo branch -D main",
        "git branch --delete --force master",
        "git update-ref -d refs/heads/main",
    ] {
        assert!(
            matches!(sh(script), ShellAssessment::ProtectedBranch { .. }),
            "{script}"
        );
    }
    for (script, target) in [
        ("git -C /x/repo reset --hard", "/x/repo"),
        ("git -C /x/repo rm -rf .", "/x/repo"),
        ("git -C /x/repo gc --prune=now", "/x/repo"),
        ("git -C /x/repo branch -D feature/x", "/x/repo"),
        ("git -C /x/repo checkout -- .", "/x/repo"),
        ("git -C /x/repo stash clear", "/x/repo"),
    ] {
        assert_eq!(
            sh_plans(script),
            [(Consequential, vec![target.to_owned()])],
            "{script}"
        );
    }
    for script in [
        "git push origin '+refs/heads/*:refs/heads/*'",
        "git push -f origin @",
        "git -c alias.p='push -f' p origin main",
        "git -c core.worktree=/x/repo clean -fdx",
        "git --git-dir=/x/repo/.git gc",
        "GIT_DIR=/x/repo/.git git clean -fdx",
        "git filter-branch --force HEAD",
        "cp /dev/null /x/repo/.git/HEAD",
        "echo x | tee /x/repo/.git/HEAD",
        "truncate -s 0 /x/repo/.git/HEAD",
        "ln -sfn /tmp /x/repo/.git/HEAD",
        "rm --unknown-flag /x/repo",
        "perl -e 'unlink q(/x/repo/a)'",
    ] {
        assert!(
            matches!(sh(script), ShellAssessment::NeedsInput(_)),
            "{script}"
        );
    }
    let ps = |s: &str| assess_command(&argv(&["pwsh", "-Command", s]), None);
    // File writers with a literal path are planned as writes (refused for protected roots).
    for script in ["Set-Content /x/repo/.git/HEAD x"] {
        match ps(script) {
            ShellAssessment::Destructive { plans, .. } => assert_eq!(
                plans
                    .into_iter()
                    .map(|p| (p.kind, p.targets))
                    .collect::<Vec<_>>(),
                [(Truncate, vec!["/x/repo/.git/HEAD".to_owned()])],
                "{script}"
            ),
            other => panic!("{script}: {other:?}"),
        }
    }
    // Pipeline input can bind the target: held.
    assert!(matches!(
        ps("'x' | Out-File C:/x/repo/.git/HEAD"),
        ShellAssessment::NeedsInput(_)
    ));
    for script in [
        "Clear-Content C:/x/repo/.git/HEAD",
        "Rename-Item C:/x/repo gone",
        "Copy-Item a C:/x/repo/.git/HEAD",
        "[IO.File]::Delete($p)",
    ] {
        assert!(
            matches!(
                ps(script),
                ShellAssessment::NeedsInput(InputNeeded::UnmodeledConsequentialCommand)
                    | ShellAssessment::NeedsInput(InputNeeded::UnresolvedExpansion)
            ),
            "{script}"
        );
    }
    // A literal .NET delete target becomes a consequential plan (refused for protected roots).
    match ps("[IO.Directory]::Delete('C:/x/repo', $true)") {
        ShellAssessment::Destructive { plans, .. } => {
            assert_eq!(plans[0].kind, Consequential);
            assert_eq!(plans[0].targets, ["C:/x/repo"]);
        }
        other => panic!("{other:?}"),
    }
    // Joined cmd switches and a joined `/c`.
    let cmd = |a: &[&str]| assess_command(&argv(a), None);
    match cmd(&["cmd", "/crd", "/s/q", "C:\\x"]) {
        ShellAssessment::Destructive { plans, .. } => {
            assert_eq!(
                (plans[0].kind, plans[0].targets.clone()),
                (RemoveTree, vec!["C:\\x".to_owned()])
            )
        }
        other => panic!("{other:?}"),
    }
    match cmd(&["cmd", "/c", "del", "/f/s/q", "C:\\x"]) {
        ShellAssessment::Destructive { plans, .. } => assert_eq!(plans[0].kind, EraseContents),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_child_environment_overlays_and_substitutes_like_the_shell() {
    use devworks_core::authority::shell::ChildEnvironment;
    let kv = |k: &str, v: &str| (k.to_owned(), v.to_owned());
    let inherited = [kv("HOME", "/home/app"), kv("PATH", "/bin")];
    let env = ChildEnvironment::effective(&inherited, false, &[kv("HOME", "/tmp/projects")], false);
    assert_eq!(env.home(), Some("/tmp/projects"), "the launch's value wins");
    assert_eq!(env.get("PATH"), Some("/bin"), "inherited");
    assert_eq!(
        env.substitute("rm -rf $HOME/repo ${HOME}/x \"$UNSET/\" %HOME% %NOPE% $ 5$"),
        "rm -rf /tmp/projects/repo /tmp/projects/x \"/\" /tmp/projects %NOPE% $ 5$"
    );
    let cleared = ChildEnvironment::effective(&inherited, true, &[], false);
    assert_eq!((cleared.home(), cleared.get("PATH")), (None, None));
    let windows = ChildEnvironment::effective(&[kv("UserProfile", "C:/Users/a")], false, &[], true);
    assert_eq!(windows.home(), Some("C:/Users/a"));
    assert_eq!(
        windows.substitute("%USERPROFILE%\\repo"),
        "C:/Users/a\\repo"
    );
}

#[test]
fn exact_git_executable_discovery_does_not_admit_relocation_or_extra_commands() {
    for option in ["--version", "--exec-path"] {
        assert!(matches!(
            assess_command(&argv(&["git", option]), Some("/work")),
            ShellAssessment::NoRecognizedDestruction { .. }
        ));
    }
    for args in [
        argv(&["git", "--exec-path=/unreviewed"]),
        argv(&["git", "--exec-path", "commit"]),
        argv(&["git", "--version", "commit"]),
    ] {
        assert!(matches!(
            assess_command(&args, Some("/work")),
            ShellAssessment::NeedsInput(_)
        ));
    }
}

/// WATCH-10: a watcher launches and stops processes with the process tools' authority. A
/// read-only reviewer cannot launch or stop one, the shell-tools switch covers both, and under
/// Project scope a launch needs a reviewed native grant like `process.execute`.
#[test]
fn watchers_launch_and_stop_with_process_authority() {
    use devworks_core::authority::policy::evaluate;
    use devworks_core::authority::{DenialReason, Verdict};
    use devworks_core::context::{HostIngress, PrincipalKind};
    use devworks_core::fingerprint::Fingerprint;
    use devworks_core::operations::{OperationInput, watch};
    use devworks_core::ports::AuthorityRequest;
    use devworks_core::ports::storage::PrincipalBinding;
    use devworks_core::protocol::{ActorId, Counter, Id, Revision};
    use devworks_core::registry::{Mode, find};

    let store = common::MemoryStore::new(&["watch-agent"]);
    let ctx = HostIngress::new(&store)
        .context(
            ActorId(Id::new("watch-agent").unwrap()),
            PrincipalKind::Agent,
            Id::new("watch-invocation").unwrap(),
            Id::new("watch-ingress").unwrap(),
            Counter::new(0).unwrap(),
            Revision::new(1).unwrap(),
        )
        .with_session(Id::new("watch-session").unwrap());
    let binding = |role| PrincipalBinding {
        actor: ActorId(Id::new("watch-agent").unwrap()),
        kind: PrincipalKind::Agent,
        role: Some(role),
        project_id: None,
        tool_families: None,
        active: true,
        revision: 1,
        bound_by: ActorId(Id::new("owner").unwrap()),
        updated_at_ms: 0,
    };
    let policy = |profile, shell_tools| EffectivePolicy {
        profile: Some(profile),
        profile_source: None,
        shell_tools,
        web_tools: true,
        web_fetch: true,
        tool_overrides: Some(BTreeMap::new()),
    };
    let spec = json!({"executableRef":"sh","args":["-c","true"],
        "workingDirectory":{"rootId":"root","relative":"work"},"environment":[],
        "profile":"Full","timeoutMs":"1000","outputLimitBytes":"64"});
    let inputs = [
        (
            "watch.create",
            OperationInput::Watch(watch::Input::Create(
                serde_json::from_value(json!({"process": spec})).unwrap(),
            )),
        ),
        (
            "watch.cancel",
            OperationInput::Watch(watch::Input::Cancel(
                serde_json::from_value(
                    json!({"watcherId":"w","expectedRevision":"1","stopProcess":true}),
                )
                .unwrap(),
            )),
        ),
        (
            "watch.inspect",
            OperationInput::Watch(watch::Input::Inspect(
                serde_json::from_value(json!({"watcherId":"w"})).unwrap(),
            )),
        ),
    ];
    let fingerprint = Fingerprint::from_hex(&"0".repeat(64)).unwrap();
    let verdict = |op: &str, input: &OperationInput, role, policy: &EffectivePolicy| {
        let descriptor = find(op).unwrap();
        let mode = if op == "watch.inspect" {
            Mode::Query
        } else {
            Mode::Command
        };
        let selected = descriptor.binding(mode).unwrap();
        evaluate(
            &ctx,
            Some(&binding(role)),
            policy,
            &AuthorityRequest {
                descriptor,
                binding: selected,
                effect: selected.effect,
                action: None,
                input,
                fingerprint: &fingerprint,
            },
            &[],
        )
    };
    let full = policy(AccessProfile::Full, true);
    for (op, input) in &inputs[..2] {
        assert_eq!(
            verdict(op, input, AgentRole::CodeReviewer, &full),
            Verdict::Deny(DenialReason::ReadOnlyRole),
            "{op}"
        );
        assert_eq!(
            verdict(
                op,
                input,
                AgentRole::Worker,
                &policy(AccessProfile::Full, false)
            ),
            Verdict::Deny(DenialReason::ShellToolsDisabled),
            "{op}"
        );
        assert_eq!(
            verdict(op, input, AgentRole::Worker, &full),
            Verdict::Allow,
            "{op}"
        );
    }
    // A probe launches its command like a process watcher: a reviewer cannot create one.
    let probe = OperationInput::Watch(watch::Input::Create(
        serde_json::from_value(json!({"probe": {"process": spec, "intervalMs": "1000",
            "condition": {}}}))
        .unwrap(),
    ));
    assert_eq!(
        verdict("watch.create", &probe, AgentRole::CodeReviewer, &full),
        Verdict::Deny(DenialReason::ReadOnlyRole)
    );
    assert_eq!(
        verdict("watch.create", &probe, AgentRole::Worker, &full),
        Verdict::Allow
    );
    // A timer only observes: a reviewer may create one, without shell tools or a native grant,
    // and may end a watcher without stopping a process.
    let timer = OperationInput::Watch(watch::Input::Create(
        serde_json::from_value(json!({"timer": {"afterMs": "1000"}})).unwrap(),
    ));
    let observe_cancel = OperationInput::Watch(watch::Input::Cancel(
        serde_json::from_value(json!({"watcherId":"w","expectedRevision":"1","stopProcess":false}))
            .unwrap(),
    ));
    for (policy, label) in [
        (&full, "full"),
        (&policy(AccessProfile::Full, false), "no shell tools"),
        (&policy(AccessProfile::ProjectScope, false), "project scope"),
    ] {
        assert_eq!(
            verdict("watch.create", &timer, AgentRole::CodeReviewer, policy),
            Verdict::Allow,
            "timer, {label}"
        );
        assert_eq!(
            verdict(
                "watch.cancel",
                &observe_cancel,
                AgentRole::CodeReviewer,
                policy
            ),
            Verdict::Allow,
            "observing cancel, {label}"
        );
    }
    // Turning process.execute off for agents turns off launching through a watcher.
    let mut no_execute = policy(AccessProfile::Full, true);
    no_execute
        .tool_overrides
        .as_mut()
        .unwrap()
        .insert("devworks/process.execute".into(), false);
    let (op, input) = &inputs[0];
    assert_eq!(
        verdict(op, input, AgentRole::Worker, &no_execute),
        Verdict::Deny(DenialReason::ToolNotSelected)
    );
    // A binding that selects the watch family but not the process family cannot launch.
    let mut watch_only = binding(AgentRole::Worker);
    watch_only.tool_families = Some(vec!["watch".into()]);
    let descriptor = find("watch.create").unwrap();
    let selected = descriptor.binding(Mode::Command).unwrap();
    assert_eq!(
        evaluate(
            &ctx,
            Some(&watch_only),
            &full,
            &AuthorityRequest {
                descriptor,
                binding: selected,
                effect: selected.effect,
                action: None,
                input: &inputs[0].1,
                fingerprint: &fingerprint,
            },
            &[],
        ),
        Verdict::Deny(DenialReason::ToolNotSelected)
    );
    // Turning process.cancel off turns off stopping a watched process, not cancelling the watch.
    let mut no_cancel = policy(AccessProfile::Full, true);
    no_cancel
        .tool_overrides
        .as_mut()
        .unwrap()
        .insert("devworks/process.cancel".into(), false);
    let (op, input) = &inputs[1];
    assert_eq!(
        verdict(op, input, AgentRole::Worker, &no_cancel),
        Verdict::Deny(DenialReason::ToolNotSelected)
    );
    let observe_only = OperationInput::Watch(watch::Input::Cancel(
        serde_json::from_value(json!({"watcherId":"w","expectedRevision":"1","stopProcess":false}))
            .unwrap(),
    ));
    assert_eq!(
        verdict(op, &observe_only, AgentRole::Worker, &no_cancel),
        Verdict::Allow
    );
    // A watch-only binding cannot stop the watched process either, but can stop observing.
    let descriptor = find("watch.cancel").unwrap();
    let selected = descriptor.binding(Mode::Command).unwrap();
    let cancel_as = |input: &OperationInput| {
        evaluate(
            &ctx,
            Some(&watch_only),
            &full,
            &AuthorityRequest {
                descriptor,
                binding: selected,
                effect: selected.effect,
                action: None,
                input,
                fingerprint: &fingerprint,
            },
            &[],
        )
    };
    assert_eq!(
        cancel_as(input),
        Verdict::Deny(DenialReason::ToolNotSelected)
    );
    assert_eq!(cancel_as(&observe_only), Verdict::Allow);
    // Reading a watcher is not a launch or a stop.
    let (op, input) = &inputs[2];
    assert_eq!(
        verdict(op, input, AgentRole::CodeReviewer, &full),
        Verdict::Allow
    );
    let (op, input) = &inputs[0];
    assert!(matches!(
        verdict(
            op,
            input,
            AgentRole::Worker,
            &policy(AccessProfile::ProjectScope, true)
        ),
        Verdict::NeedsGrant {
            kind: GrantKind::NativeExecution,
            exact: true,
            ..
        }
    ));
}
