use std::fs;
use std::path::PathBuf;

fn extract_crontext_examples(content: &str) -> Vec<String> {
    let mut examples = Vec::new();
    let mut in_crontext_block = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```crontext") {
            in_crontext_block = true;
            continue;
        }
        if in_crontext_block && trimmed.starts_with("```") {
            in_crontext_block = false;
            continue;
        }
        if in_crontext_block {
            let expr = trimmed.split('#').next().unwrap_or("").trim();
            if !expr.is_empty() {
                examples.push(expr.to_owned());
            }
        }
    }
    examples
}

#[test]
fn all_doc_crontext_examples_parse_cleanly() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files_to_check = vec![root.join("README.md")];

    let docs_dir = root.join("docs");
    if docs_dir.exists() {
        for entry in fs::read_dir(docs_dir).expect("read docs dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "md") {
                files_to_check.push(path);
            }
        }
    }

    let mut total_examples = 0;
    for file in files_to_check {
        if !file.exists() {
            continue;
        }
        let content = fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", file.display()));
        let examples = extract_crontext_examples(&content);
        for expr in examples {
            total_examples += 1;
            let result = crontext::parse(&expr);
            assert!(
                result.is_ok(),
                "example `{expr}` in {} failed to parse: {:?}",
                file.display(),
                result.err()
            );
        }
    }

    assert!(
        total_examples > 0,
        "expected to find at least one crontext code block in README.md or docs/"
    );
}
