use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::fs;
use std::path::Path;

use crate::pr::DepBump;

/// Extract dependency updates from a `package.json` unified diff patch.
pub fn extract_package_json_bumps(filename: &str, patch: &str) -> Vec<DepBump> {
    let mut bumps = Vec::new();
    let path = manifest_dir_path(filename);

    // Keep track of removed dependency lines: name -> (old_version, old_spec)
    let mut removed: Vec<(String, String)> = Vec::new();

    for line in patch.lines() {
        if line.starts_with("--- ") || line.starts_with("+++ ") || line.starts_with("@@ ") {
            continue;
        }

        if let Some(rest) = line.strip_prefix('-') {
            if let Some((name, spec)) = parse_package_json_dep_line(rest) {
                removed.push((name, spec));
            }
        } else if let Some(rest) = line.strip_prefix('+') {
            if let Some((name, new_spec)) = parse_package_json_dep_line(rest) {
                // Find matching removed entry with the same package name
                if let Some(pos) = removed.iter().position(|(r_name, _)| r_name == &name) {
                    let (_, old_spec) = removed.remove(pos);
                    bumps.push(DepBump {
                        name,
                        from: old_spec,
                        to: new_spec,
                        path: path.clone(),
                    });
                } else {
                    // Added without an explicit '-' line in the same diff
                    bumps.push(DepBump {
                        name,
                        from: String::new(),
                        to: new_spec,
                        path: path.clone(),
                    });
                }
            }
        }
    }

    bumps
}

/// Convert a filename like "api/package.json" or "package.json" to a path like Some("/api") or None.
fn manifest_dir_path(filename: &str) -> Option<String> {
    let clean = filename.trim_matches('/');
    if clean == "package.json" {
        None
    } else {
        clean
            .strip_suffix("/package.json")
            .map(|dir| format!("/{dir}"))
    }
}

/// Parse a dependency line inside a JSON object: `"name": "version",`
fn parse_package_json_dep_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if !trimmed.starts_with('"') {
        return None;
    }
    let rest = &trimmed[1..];
    let (name, after_name) = rest.split_once('"')?;
    if name.is_empty() {
        return None;
    }
    let after_colon = after_name.trim_start().strip_prefix(':')?.trim_start();
    if !after_colon.starts_with('"') {
        return None;
    }
    let val_rest = &after_colon[1..];
    let (ver_spec, _) = val_rest.split_once('"')?;
    Some((name.to_string(), ver_spec.to_string()))
}

/// Sections in `package.json` where dependencies can be specified.
const DEP_SECTIONS: &[&str] = &[
    "dependencies",
    "devDependencies",
    "peerDependencies",
    "optionalDependencies",
    "overrides",
    "resolutions",
];

/// Apply package bumps directly to a `package.json` file on disk.
///
/// Modifies the JSON AST directly, guaranteeing valid syntax and preserving indentation.
pub fn apply_bumps_to_package_json(path: &Path, bumps: &[&DepBump]) -> Result<usize> {
    if bumps.is_empty() {
        return Ok(0);
    }

    let content = fs::read_to_string(path)
        .with_context(|| format!("reading package manifest `{}`", path.display()))?;

    let indent = detect_indentation(&content);
    let has_trailing_newline = content.ends_with('\n');

    let mut root: Value = serde_json::from_str(&content)
        .with_context(|| format!("parsing JSON in `{}`", path.display()))?;

    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("`{}` root is not a JSON object", path.display()))?;

    let mut applied = 0usize;

    for bump in bumps {
        for section_name in DEP_SECTIONS {
            if let Some(section_val) = root_obj.get_mut(*section_name) {
                if let Some(deps_map) = section_val.as_object_mut() {
                    if let Some(existing_val) = deps_map.get_mut(&bump.name) {
                        let existing_str = existing_val.as_str().unwrap_or_default();
                        let new_ver_str = compute_target_spec(existing_str, &bump.to);
                        *existing_val = Value::String(new_ver_str);
                        applied += 1;
                        break;
                    }
                }
            }
        }
    }

    if applied > 0 {
        let serialized = format_json_with_indent(&root, &indent, has_trailing_newline)?;
        fs::write(path, serialized)
            .with_context(|| format!("writing updated package manifest `{}`", path.display()))?;
    }

    Ok(applied)
}

/// Compute the target version specifier, preserving existing prefix (^, ~, >=, etc.)
/// if `bump_to` is a bare version.
fn compute_target_spec(existing: &str, bump_to: &str) -> String {
    // If bump_to already contains a range prefix, use it as is
    if bump_to.starts_with('^')
        || bump_to.starts_with('~')
        || bump_to.starts_with('>')
        || bump_to.starts_with('<')
        || bump_to.starts_with('=')
    {
        return bump_to.to_string();
    }

    // Otherwise, check if existing spec had a prefix
    let prefix: String = existing
        .chars()
        .take_while(|c| matches!(c, '^' | '~' | '>' | '<' | '=' | ' '))
        .collect();

    if !prefix.is_empty() {
        format!("{prefix}{bump_to}")
    } else {
        bump_to.to_string()
    }
}

/// Detect indentation style (spaces or tabs) from source JSON.
fn detect_indentation(content: &str) -> String {
    for line in content.lines() {
        if line.starts_with("    ") {
            return "    ".to_string();
        } else if line.starts_with("  ") {
            return "  ".to_string();
        } else if line.starts_with('\t') {
            return "\t".to_string();
        }
    }
    "  ".to_string()
}

/// Format JSON with custom indentation and optional trailing newline.
fn format_json_with_indent(value: &Value, indent: &str, trailing_newline: bool) -> Result<String> {
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    serde::Serialize::serialize(value, &mut ser)?;
    let mut res = String::from_utf8(buf)?;
    if trailing_newline {
        res.push('\n');
    }
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dep_line() {
        assert_eq!(
            parse_package_json_dep_line("  \"qs\": \"^6.16.0\","),
            Some(("qs".to_string(), "^6.16.0".to_string()))
        );
        assert_eq!(
            parse_package_json_dep_line("\"happy-dom\": \"21.0.3\""),
            Some(("happy-dom".to_string(), "21.0.3".to_string()))
        );
        assert_eq!(parse_package_json_dep_line("  \"scripts\": {"), None);
    }

    #[test]
    fn extracts_bumps_from_unified_diff() {
        let diff = r#"
--- a/package.json
+++ b/package.json
@@ -10,2 +10,2 @@
-    "qs": "^6.16.0",
+    "qs": "^6.17.0",
-    "vite": "~5.0.0",
+    "vite": "~5.1.0",
"#;
        let bumps = extract_package_json_bumps("package.json", diff);
        assert_eq!(bumps.len(), 2);
        assert_eq!(bumps[0].name, "qs");
        assert_eq!(bumps[0].from, "^6.16.0");
        assert_eq!(bumps[0].to, "^6.17.0");
        assert_eq!(bumps[0].path, None);

        assert_eq!(bumps[1].name, "vite");
        assert_eq!(bumps[1].from, "~5.0.0");
        assert_eq!(bumps[1].to, "~5.1.0");
        assert_eq!(bumps[1].path, None);
    }

    #[test]
    fn extracts_bumps_with_directory_path() {
        let diff = r#"
--- a/api/package.json
+++ b/api/package.json
@@ -5,1 +5,1 @@
-    "qs": "6.16.0"
+    "qs": "6.17.0"
"#;
        let bumps = extract_package_json_bumps("api/package.json", diff);
        assert_eq!(bumps.len(), 1);
        assert_eq!(bumps[0].path, Some("/api".to_string()));
    }

    #[test]
    fn applies_bumps_preserving_prefix_and_indentation() {
        let temp =
            std::env::temp_dir().join(format!("dependabatch-test-manifest-{}", std::process::id()));
        let _ = fs::create_dir_all(&temp);
        let path = temp.join("package.json");

        let original = r#"{
  "name": "example",
  "devDependencies": {
    "happy-dom": "^20.14.6",
    "vite": "^8.3.2"
  }
}
"#;
        fs::write(&path, original).unwrap();

        let bump = DepBump {
            name: "happy-dom".to_string(),
            from: "20.14.6".to_string(),
            to: "21.0.3".to_string(),
            path: None,
        };

        let applied = apply_bumps_to_package_json(&path, &[&bump]).unwrap();
        assert_eq!(applied, 1);

        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.contains("\"happy-dom\": \"^21.0.3\""));
        assert!(updated.contains("\"vite\": \"^8.3.2\""));
        assert!(updated.ends_with('\n'));
        let _ = fs::remove_dir_all(&temp);
    }
}
