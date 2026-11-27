//! File complexity uses the retained, language-specific scoring conventions.
use super::scores::Complexity;

pub(super) fn compute(extension: &str, source: &str) -> Complexity {
    match extension {
        "cs" => csharp(source),
        "rb" => super::ruby::compute(source),
        "sh" | "bash" | "zsh" => super::shell::compute(source),
        other => super::brace::compute(other, source).unwrap_or_else(|| indentation(source)),
    }
}

pub(super) fn csharp(source: &str) -> Complexity {
    let mut result = Complexity::default();
    let mut depth: isize = 0;
    let mut active = 0;
    for line in source.split('\n') {
        result.lines += 1;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        for character in line.bytes() {
            match character {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            }
        }
        let effective_depth = depth.max(0) as usize;
        result.nesting = result.nesting.max(effective_depth);
        result.deep_lines += usize::from(effective_depth >= 3);
        let declaration = depth == 2
            && !line.starts_with("//")
            && line.contains('(')
            && [
                "void ",
                "async ",
                "Task",
                "string ",
                "int ",
                "bool ",
                "public ",
                "private ",
                "protected ",
                "internal ",
            ]
            .iter()
            .any(|hint| line.contains(hint));
        if declaration {
            result.longest_function = result.longest_function.max(active);
            result.functions += 1;
            active = 1;
        } else if active > 0 {
            active += 1;
        }
        result.branches += usize::from(
            ["if ", "if(", "switch ", "switch("]
                .iter()
                .any(|prefix| line.starts_with(prefix)),
        );
    }
    result.longest_function = result.longest_function.max(active);
    result
}

pub(super) fn indentation(source: &str) -> Complexity {
    let unit = indent_unit(source);
    let mut result = Complexity::default();
    let mut active = 0;
    let mut previous = 0;
    for line in source.split('\n') {
        result.lines += 1;
        if line.trim().is_empty() {
            continue;
        }
        let indent: usize = line
            .bytes()
            .take_while(|b| matches!(b, b' ' | b'\t'))
            .map(|b| if b == b'\t' { unit } else { 1 })
            .sum();
        let depth = indent / unit;
        result.nesting = result.nesting.max(depth);
        result.deep_lines += usize::from(depth >= 3);
        if depth == 1 && previous == 0 {
            result.longest_function = result.longest_function.max(active);
            result.functions += 1;
            active = 1;
        } else if active > 0 {
            active += 1;
        }
        previous = depth;
    }
    result.longest_function = result.longest_function.max(active);
    result
}

fn indent_unit(source: &str) -> usize {
    let mut counts = [0; 9];
    let mut observed = Vec::new();
    for line in source.split('\n').filter(|line| !line.trim().is_empty()) {
        if line.starts_with('\t') {
            return 4;
        }
        let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
        if (1..=8).contains(&spaces) {
            if counts[spaces] == 0 {
                observed.push(spaces);
            }
            counts[spaces] += 1;
        }
    }
    // The source's stable frequency sort breaks ties by first observation.
    observed
        .into_iter()
        .rev()
        .max_by_key(|spaces| counts[*spaces])
        .unwrap_or(4)
}

#[derive(Default)]
pub(super) struct Line {
    pub events: Vec<Event>,
    pub declaration: bool,
    pub branches: usize,
    pub ends_statement: bool,
}

pub(super) enum Event {
    Open { function_start: Option<usize> },
    Close,
    InlineFunction,
}

pub(super) fn blocks(
    source: &str,
    mut scan: impl FnMut(&str, usize) -> Option<Line>,
) -> Complexity {
    let mut result = Complexity::default();
    let mut depth = 0usize;
    let mut function: Option<(usize, usize)> = None;
    for (index, text) in source.split('\n').enumerate() {
        result.lines += 1;
        let Some(line) = scan(text, index) else {
            continue;
        };

        if let Some((opening, start)) = function
            && depth <= opening
            && line.declaration
        {
            result.longest_function = result.longest_function.max(index - start);
            function = None;
        }
        if function.is_none() && line.declaration {
            result.functions += 1;
            function = Some((depth, index));
        }
        let mut in_function = function.is_some();
        let mut line_depth = depth;
        for event in line.events {
            match event {
                Event::InlineFunction => {
                    if function.is_none() {
                        result.functions += 1;
                        result.longest_function = result.longest_function.max(1);
                    }
                }
                Event::Open { function_start } => {
                    if let Some(start) = function_start
                        && function.is_none()
                    {
                        result.functions += 1;
                        function = Some((depth, start));
                        in_function = true;
                    }
                    depth += 1;
                    line_depth = line_depth.max(depth);
                }
                Event::Close => depth = depth.saturating_sub(1),
            }
        }
        if in_function {
            result.branches += line.branches;
        }
        result.nesting = result.nesting.max(line_depth);
        result.deep_lines += usize::from(line_depth >= 3);
        if let Some((opening, start)) = function
            && (depth < opening
                || (depth == opening && (line_depth > opening || line.ends_statement)))
        {
            result.longest_function = result.longest_function.max(index - start + 1);
            function = None;
        }
    }
    if let Some((_, start)) = function {
        result.longest_function = result.longest_function.max(result.lines - start);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_language_vectors_match_the_pinned_source() {
        #[derive(serde::Deserialize)]
        struct Fixture {
            cases: Vec<Case>,
        }
        #[derive(serde::Deserialize)]
        struct Case {
            line: usize,
            extension: String,
            source: String,
            nesting: usize,
            functions: usize,
            span: usize,
            branches: usize,
        }
        let fixture: Fixture =
            serde_json::from_str(include_str!("pinned-complexity.json")).unwrap();
        let mut differences = Vec::new();
        for case in fixture.cases {
            let actual = compute(&case.extension, &case.source);
            let actual = (
                actual.nesting,
                actual.functions,
                actual.longest_function,
                actual.branches,
            );
            let expected = (case.nesting, case.functions, case.span, case.branches);
            if actual != expected {
                differences.push(format!(
                    "{}:{}: {actual:?} vs {expected:?}",
                    case.extension, case.line
                ));
            }
        }
        assert!(differences.is_empty(), "{}", differences.join("\n"));
    }

    #[test]
    fn csharp_preserves_legacy_depth_and_statement_detection() {
        let score = csharp(
            "namespace N\n{\n  class A\n  {\n    public void F()\n    {\n      if (x) {\n        G();\n      }\n    }\n  }\n}\n",
        );
        assert_eq!(score.nesting, 4);
        assert_eq!(score.functions, 1);
        assert_eq!(score.branches, 1);
        assert_eq!(score.deep_lines, 4);
        assert_eq!(score.longest_function, 8);
        assert!(score.score() > 0);
    }

    #[test]
    fn first_observed_frequency_wins_and_ties_match_original_indentation() {
        assert_eq!(indent_unit("a\n    b\n  c"), 4);
        assert_eq!(indent_unit("a\n  b\n    c"), 2);
        let result = indentation("def f():\n  if x:\n    if y:\n      f()\n  g()\n");
        assert_eq!(
            (result.nesting, result.deep_lines, result.functions),
            (3, 1, 1)
        );
    }
}
