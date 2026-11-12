//! Every code a failure is made with is one of [`homology_ssh::CODES`], the
//! list programs tell failures apart by.

#[test]
fn every_failure_s_code_is_listed() {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut used = Vec::new();
    for entry in std::fs::read_dir(&source).unwrap() {
        let text = std::fs::read_to_string(entry.unwrap().path()).unwrap();
        for (at, _) in text.match_indices("SshError::new(") {
            let rest = text[at + "SshError::new(".len()..].trim_start();
            if let Some(code) = rest.strip_prefix('"').and_then(|rest| rest.split('"').next()) {
                used.push(code.to_string());
            }
        }
    }
    assert!(used.len() > 20, "{used:?}");
    let missing: Vec<&String> = used.iter().filter(|code| !homology_ssh::CODES.contains(&code.as_str())).collect();
    assert!(missing.is_empty(), "not in CODES: {missing:?}");
}
