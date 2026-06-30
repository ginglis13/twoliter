//! Schnauzer template simulation for PCR 8 prediction.
//!
//! Uses bottlerocket-settings-generators to render the same templates schnauzer
//! would on a running Bottlerocket host, given just the AWS region and defaults TOML.

use bottlerocket_settings_generators::{
    impl_template_importer, render_template_str, JsonSettingsResolver, StaticHelperResolver,
};
use serde_json::{Map, Value};

/// Template importer for offline rendering with in-memory settings.
struct OfflineImporter {
    settings_resolver: JsonSettingsResolver,
    helper_resolver: StaticHelperResolver,
}

impl OfflineImporter {
    fn new(settings: Value) -> Self {
        Self {
            settings_resolver: JsonSettingsResolver::new(settings),
            helper_resolver: StaticHelperResolver::default(),
        }
    }
}

impl_template_importer!(OfflineImporter, JsonSettingsResolver, StaticHelperResolver);

/// Information extracted from the storewolf TOML about the image.
pub struct ImageInfo {
    pub variant_id: String,
    pub arch: String,
}

/// Apply setting-generator simulations to the merged settings JSON.
///
/// Parses the `[metadata]` section of the storewolf defaults to discover generators,
/// then simulates the ones we can replicate given region + image info.
/// Only sets values that are NOT already present (generators don't fire if the setting exists).
pub async fn apply_generators(
    settings: &mut Value,
    defaults_toml: &str,
    region: &str,
    image_info: &ImageInfo,
) {
    let table: toml::Value = match toml::from_str(defaults_toml) {
        Ok(v) => v,
        Err(_) => return,
    };

    let metadata = match table.get("metadata").and_then(|m| m.get("settings")) {
        Some(m) => m,
        None => return,
    };

    // Process each known generator pattern from the metadata
    apply_ecr_generators(settings, metadata, region).await;
    apply_tuf_generators(settings, metadata, region, image_info).await;
    apply_aws_config_generator(settings, metadata, region).await;
    apply_send_metrics_generator(settings, metadata, region);
}

/// Render a schnauzer template string offline with the given settings JSON.
async fn render_offline(template_str: &str, settings: &Value) -> Option<String> {
    let importer = OfflineImporter::new(settings.clone());
    render_template_str(&importer, template_str).await.ok()
}

/// Build the settings JSON that schnauzer expects: `{"settings": {"aws": {"region": ...}}}`.
fn settings_json_for_region(region: &str, extra_settings: &Value) -> Value {
    let mut settings = extra_settings.clone();
    // Ensure settings.aws.region is set
    let aws = settings
        .as_object_mut()
        .unwrap()
        .entry("aws")
        .or_insert_with(|| Value::Object(Map::new()));
    aws.as_object_mut()
        .unwrap()
        .entry("region")
        .or_insert_with(|| Value::String(region.to_string()));

    serde_json::json!({ "settings": settings })
}

/// Apply ECR-prefix generators (host-containers.*.source, bootstrap-containers.source).
async fn apply_ecr_generators(settings: &mut Value, metadata: &toml::Value, region: &str) {
    let render_settings = settings_json_for_region(region, settings);

    // host-containers.admin.source
    if let Some(cmd) = get_generator_command(metadata, &["host-containers", "admin", "source"]) {
        if cmd.contains("ecr-prefix") {
            if let Some(template_str) = build_template_from_command(&cmd) {
                if let Some(rendered) = render_offline(&template_str, &render_settings).await {
                    set_str_if_absent(settings, &["host-containers", "admin", "source"], &rendered);
                }
            }
        }
    }

    // host-containers.control.source
    if let Some(cmd) = get_generator_command(metadata, &["host-containers", "control", "source"]) {
        if cmd.contains("ecr-prefix") {
            if let Some(template_str) = build_template_from_command(&cmd) {
                if let Some(rendered) = render_offline(&template_str, &render_settings).await {
                    set_str_if_absent(
                        settings,
                        &["host-containers", "control", "source"],
                        &rendered,
                    );
                }
            }
        }
    }

    // bootstrap-containers.source (depth=1, applies to all named bootstrap containers)
    if let Some(cmd) = get_generator_command(metadata, &["bootstrap-containers", "source"]) {
        if cmd.contains("ecr-prefix") {
            if let Some(template_str) = build_template_from_command(&cmd) {
                if let Some(rendered) = render_offline(&template_str, &render_settings).await {
                    // Apply to all bootstrap containers that exist in the current settings
                    if let Some(bc) = settings.get("bootstrap-containers") {
                        if let Some(bc_obj) = bc.as_object() {
                            let container_names: Vec<String> = bc_obj.keys().cloned().collect();
                            for name in container_names {
                                set_str_if_absent(
                                    settings,
                                    &["bootstrap-containers", &name, "source"],
                                    &rendered,
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Apply TUF-prefix generators (updates.targets-base-url, updates.metadata-base-url).
async fn apply_tuf_generators(
    settings: &mut Value,
    metadata: &toml::Value,
    region: &str,
    image_info: &ImageInfo,
) {
    let render_settings = settings_json_for_region(region, settings);

    // updates.targets-base-url
    if let Some(cmd) = get_generator_command(metadata, &["updates", "targets-base-url"]) {
        if let Some(template_str) = build_template_from_command(&cmd) {
            if let Some(rendered) = render_offline(&template_str, &render_settings).await {
                set_str_if_absent(settings, &["updates", "targets-base-url"], &rendered);
            }
        }
    }

    // updates.metadata-base-url
    // This template uses os.variant_id and os.arch, so we need to add those to the context
    if let Some(cmd) = get_generator_command(metadata, &["updates", "metadata-base-url"]) {
        if let Some(template_str) = build_template_from_command(&cmd) {
            // Add os info to the settings context
            let mut full_settings = render_settings.clone();
            full_settings.as_object_mut().unwrap().insert(
                "os".to_string(),
                serde_json::json!({
                    "variant_id": image_info.variant_id,
                    "arch": image_info.arch,
                }),
            );
            let importer = OfflineImporter::new(full_settings);
            if let Ok(rendered) = render_template_str(&importer, &template_str).await {
                set_str_if_absent(settings, &["updates", "metadata-base-url"], &rendered);
            }
        }
    }
}

/// Apply aws-config generator (settings.aws.config).
async fn apply_aws_config_generator(
    settings: &mut Value,
    metadata: &toml::Value,
    region: &str,
) {
    if let Some(cmd) = get_generator_command(metadata, &["aws", "config"]) {
        if let Some(template_str) = build_template_from_command(&cmd) {
            let render_settings = settings_json_for_region(region, settings);
            if let Some(rendered) = render_offline(&template_str, &render_settings).await {
                if !rendered.is_empty() {
                    set_str_if_absent(settings, &["aws", "config"], &rendered);
                }
            }
        }
    }
}

/// Apply send-metrics generator (settings.metrics.send-metrics).
/// This simulates `shibaken is-partition` which doesn't use schnauzer templates.
fn apply_send_metrics_generator(settings: &mut Value, metadata: &toml::Value, region: &str) {
    if let Some(cmd) = get_generator_command(metadata, &["metrics", "send-metrics"]) {
        // Parse which partitions from the command, e.g.:
        // "shibaken is-partition --partition aws --partition aws-us-gov"
        let partitions: Vec<&str> = cmd
            .split("--partition")
            .skip(1)
            .filter_map(|s| s.trim().split_whitespace().next())
            .collect();
        if !partitions.is_empty() {
            let result = is_partition_bool(region, &partitions);
            set_value_if_absent(settings, &["metrics", "send-metrics"], Value::Bool(result));
        }
    }
}

/// Check if a region's partition is in the allowed list.
fn is_partition_bool(region: &str, allowed: &[&str]) -> bool {
    let partition = bottlerocket_settings_generators::helpers::partition_for_region(region);
    allowed.contains(&partition)
}

/// Build a full template string (with frontmatter) from a schnauzer-v2 render command.
///
/// Parses: `schnauzer-v2 render --requires 'aws@v1(helpers=[ecr-prefix])' --template '{{ ecr-prefix ... }}'`
/// into:   `[required-extensions.aws]\nversion = "v1"\nhelpers = ["ecr-prefix"]\n+++\n{{ ecr-prefix ... }}`
fn build_template_from_command(cmd: &str) -> Option<String> {
    let template_body = extract_template_body(cmd)?;
    let requires = extract_requires(cmd);

    let mut frontmatter_lines = Vec::new();
    for req in &requires {
        let (name, version, helpers) = parse_requirement(req)?;
        frontmatter_lines.push(format!("[required-extensions.{name}]"));
        frontmatter_lines.push(format!("version = \"{version}\""));
        if !helpers.is_empty() {
            let helpers_str = helpers
                .iter()
                .map(|h| format!("\"{h}\""))
                .collect::<Vec<_>>()
                .join(", ");
            frontmatter_lines.push(format!("helpers = [{helpers_str}]"));
        }
    }

    let frontmatter = frontmatter_lines.join("\n");
    Some(format!("{frontmatter}\n+++\n{template_body}"))
}

/// Extract the --template value from a schnauzer-v2 command string.
fn extract_template_body(cmd: &str) -> Option<&str> {
    let marker = "--template";
    let idx = cmd.find(marker)?;
    let after = cmd[idx + marker.len()..].trim_start();
    // Template is quoted with ' or "
    let (quote, rest) = if after.starts_with('\'') {
        ('\'', &after[1..])
    } else if after.starts_with('"') {
        ('"', &after[1..])
    } else {
        return Some(after.split_whitespace().next().unwrap_or(after));
    };
    let end = rest.rfind(quote)?;
    Some(&rest[..end])
}

/// Extract all --requires values from a schnauzer-v2 command string.
fn extract_requires(cmd: &str) -> Vec<&str> {
    let mut results = Vec::new();
    let mut remaining = cmd;
    while let Some(idx) = remaining.find("--requires") {
        let after = &remaining[idx + "--requires".len()..];
        let after = after.trim_start();
        let (quote, rest) = if after.starts_with('\'') {
            ('\'', &after[1..])
        } else if after.starts_with('"') {
            ('"', &after[1..])
        } else {
            let end = after.find(|c: char| c.is_whitespace()).unwrap_or(after.len());
            results.push(&after[..end]);
            remaining = &after[end..];
            continue;
        };
        if let Some(end) = rest.find(quote) {
            results.push(&rest[..end]);
            remaining = &rest[end + 1..];
        } else {
            break;
        }
    }
    results
}

/// Parse a requirement string like `aws@v1(helpers=[ecr-prefix, aws-config])`.
fn parse_requirement(req: &str) -> Option<(String, String, Vec<String>)> {
    let (name_version, helpers_part) = if let Some(paren_idx) = req.find('(') {
        (&req[..paren_idx], Some(&req[paren_idx..]))
    } else {
        (req, None)
    };

    let (name, version) = if let Some(at_idx) = name_version.find('@') {
        (
            name_version[..at_idx].to_string(),
            name_version[at_idx + 1..].to_string(),
        )
    } else {
        (name_version.to_string(), "v1".to_string())
    };

    let helpers = if let Some(hp) = helpers_part {
        // Parse: (helpers=[ecr-prefix, aws-config])
        if let Some(start) = hp.find('[') {
            if let Some(end) = hp.find(']') {
                hp[start + 1..end]
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    Some((name, version, helpers))
}

/// Get the setting-generator command string from metadata for a given path.
fn get_generator_command(metadata: &toml::Value, path: &[&str]) -> Option<String> {
    let mut current = metadata;
    for key in path {
        current = current.get(*key)?;
    }
    let gen = current.get("setting-generator")?;
    match gen {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Table(t) => t.get("command").and_then(|c| c.as_str()).map(String::from),
        _ => None,
    }
}

/// Set a nested string value in a JSON object, only if the key path doesn't already exist.
fn set_str_if_absent(root: &mut Value, path: &[&str], value: &str) {
    set_value_if_absent(root, path, Value::String(value.to_string()));
}

/// Set a nested value in a JSON object, only if the key path doesn't already exist.
fn set_value_if_absent(root: &mut Value, path: &[&str], value: Value) {
    if path.is_empty() {
        return;
    }
    // Check if value already exists at the full path
    let mut check = root as &Value;
    let mut exists = true;
    for key in path {
        match check.get(*key) {
            Some(v) => check = v,
            None => {
                exists = false;
                break;
            }
        }
    }
    if exists {
        return;
    }

    // Set the value
    let mut current = root;
    for key in &path[..path.len() - 1] {
        current = current
            .as_object_mut()
            .unwrap()
            .entry(key.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    let last_key = path[path.len() - 1];
    if let Some(obj) = current.as_object_mut() {
        obj.entry(last_key.to_string()).or_insert(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ecr_rendering_via_library() {
        let template = build_template_from_command(
            "schnauzer-v2 render --requires 'aws@v1(helpers=[ecr-prefix])' --template '{{ ecr-prefix settings.aws.region }}/bottlerocket-admin:v0.21.0'",
        ).unwrap();

        let settings = serde_json::json!({
            "settings": { "aws": { "region": "us-west-2" } }
        });

        let result = render_offline(&template, &settings).await.unwrap();
        assert_eq!(
            result,
            "328549459982.dkr.ecr.us-west-2.amazonaws.com/bottlerocket-admin:v0.21.0"
        );
    }

    #[tokio::test]
    async fn test_tuf_rendering_via_library() {
        let template = build_template_from_command(
            "schnauzer-v2 render --requires 'aws@v1' --requires 'updates@v1(helpers=[tuf-prefix])' --template '{{ tuf-prefix settings.aws.region }}/targets/'",
        ).unwrap();

        let settings = serde_json::json!({
            "settings": { "aws": { "region": "us-west-2" } }
        });

        let result = render_offline(&template, &settings).await.unwrap();
        assert_eq!(result, "https://updates.bottlerocket.aws/targets/");
    }

    #[tokio::test]
    async fn test_tuf_rendering_china() {
        let template = build_template_from_command(
            "schnauzer-v2 render --requires 'aws@v1' --requires 'updates@v1(helpers=[tuf-prefix])' --template '{{ tuf-prefix settings.aws.region }}/targets/'",
        ).unwrap();

        let settings = serde_json::json!({
            "settings": { "aws": { "region": "cn-north-1" } }
        });

        let result = render_offline(&template, &settings).await.unwrap();
        // tuf-prefix for cn-north-1 returns the full endpoint URL with .com.cn TLD and /latest
        assert_eq!(
            result,
            "https://bottlerocket-updates-cn-north-1.s3.dualstack.cn-north-1.amazonaws.com.cn/latest/targets/"
        );
    }

    #[test]
    fn test_build_template_from_command() {
        let cmd = "schnauzer-v2 render --requires 'aws@v1(helpers=[ecr-prefix])' --template '{{ ecr-prefix settings.aws.region }}/bottlerocket-admin:v0.21.0'";
        let result = build_template_from_command(cmd).unwrap();
        assert!(result.contains("[required-extensions.aws]"));
        assert!(result.contains("version = \"v1\""));
        assert!(result.contains("helpers = [\"ecr-prefix\"]"));
        assert!(result.contains("+++"));
        assert!(result.contains("{{ ecr-prefix settings.aws.region }}/bottlerocket-admin:v0.21.0"));
    }

    #[test]
    fn test_build_template_multiple_requires() {
        let cmd = "schnauzer-v2 render --requires 'aws@v1' --requires 'updates@v1(helpers=[metadata-prefix, tuf-prefix])' --template '{{ tuf-prefix settings.aws.region }}{{ metadata-prefix settings.aws.region }}/2020-07-07/'";
        let result = build_template_from_command(cmd).unwrap();
        assert!(result.contains("[required-extensions.aws]"));
        assert!(result.contains("[required-extensions.updates]"));
        assert!(result.contains("helpers = [\"metadata-prefix\", \"tuf-prefix\"]"));
    }

    #[test]
    fn test_parse_requirement() {
        let (name, version, helpers) =
            parse_requirement("aws@v1(helpers=[ecr-prefix, aws-config])").unwrap();
        assert_eq!(name, "aws");
        assert_eq!(version, "v1");
        assert_eq!(helpers, vec!["ecr-prefix", "aws-config"]);
    }

    #[test]
    fn test_parse_requirement_no_helpers() {
        let (name, version, helpers) = parse_requirement("aws@v1").unwrap();
        assert_eq!(name, "aws");
        assert_eq!(version, "v1");
        assert!(helpers.is_empty());
    }

    #[test]
    fn test_extract_template_body() {
        let cmd = "schnauzer-v2 render --requires 'aws@v1' --template '{{ ecr-prefix settings.aws.region }}/foo:v1'";
        let body = extract_template_body(cmd).unwrap();
        assert_eq!(body, "{{ ecr-prefix settings.aws.region }}/foo:v1");
    }

    #[test]
    fn test_is_partition_standard() {
        assert!(is_partition_bool("us-west-2", &["aws", "aws-us-gov"]));
    }

    #[test]
    fn test_is_partition_govcloud() {
        assert!(is_partition_bool("us-gov-west-1", &["aws", "aws-us-gov"]));
    }

    #[test]
    fn test_is_partition_china_excluded() {
        assert!(!is_partition_bool("cn-north-1", &["aws", "aws-us-gov"]));
    }

    #[test]
    fn test_set_if_absent_creates() {
        let mut settings = Value::Object(Map::new());
        set_str_if_absent(&mut settings, &["aws", "config"], "test_value");
        assert_eq!(settings["aws"]["config"], "test_value");
    }

    #[test]
    fn test_set_if_absent_preserves() {
        let mut settings: Value =
            serde_json::from_str(r#"{"aws":{"config":"existing"}}"#).unwrap();
        set_str_if_absent(&mut settings, &["aws", "config"], "new_value");
        assert_eq!(settings["aws"]["config"], "existing");
    }

    #[tokio::test]
    async fn test_apply_generators_ecr() {
        let defaults = r#"
[settings.host-containers.admin]
enabled = true

[metadata.settings.host-containers.admin.source.setting-generator]
command = "schnauzer-v2 render --requires 'aws@v1(helpers=[ecr-prefix])' --template '{{ ecr-prefix settings.aws.region }}/bottlerocket-admin:v0.21.0'"
strength = "weak"
"#;

        let mut settings: Value = serde_json::from_str(
            r#"{"host-containers": {"admin": {"enabled": true}}}"#,
        )
        .unwrap();

        let image_info = ImageInfo {
            variant_id: "aws-ecs-2".to_string(),
            arch: "x86_64".to_string(),
        };

        apply_generators(&mut settings, defaults, "us-west-2", &image_info).await;

        assert_eq!(
            settings["host-containers"]["admin"]["source"],
            "328549459982.dkr.ecr.us-west-2.amazonaws.com/bottlerocket-admin:v0.21.0"
        );
    }
}
