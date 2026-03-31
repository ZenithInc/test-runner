use anyhow::{Context, Result, bail};
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::dsl::{Assertion, AssertionKind};

const RESERVED_ROOT_KEYS: &[&str] = &[
    "env", "project", "case", "api", "data", "vars", "response", "result", "request", "workflow",
];

#[derive(Debug, Clone)]
pub struct RuntimeContext {
    root: Map<String, Value>,
    template_regex: Regex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpressionMode {
    Lenient,
    Explicit,
}

impl RuntimeContext {
    pub fn new(mut root: Map<String, Value>) -> Result<Self> {
        root.entry("vars".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        Ok(Self {
            root,
            template_regex: Regex::new(r"\{\{\s*([^}]+?)\s*\}\}")?,
        })
    }

    pub fn root(&self) -> &Map<String, Value> {
        &self.root
    }

    pub fn root_mut(&mut self) -> &mut Map<String, Value> {
        &mut self.root
    }

    pub fn set_root_value(&mut self, key: &str, value: Value) {
        self.root.insert(key.to_string(), value);
    }

    pub fn set_var(&mut self, name: &str, value: Value) {
        self.root
            .entry("vars".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(vars) = self.root.get_mut("vars").and_then(Value::as_object_mut) {
            vars.insert(name.to_string(), value);
        }
    }

    pub fn lookup_var(&self, name: &str) -> Option<Value> {
        self.root
            .get("vars")
            .and_then(Value::as_object)
            .and_then(|vars| vars.get(name))
            .cloned()
    }

    pub fn restore_var(&mut self, name: &str, previous: Option<Value>) {
        match previous {
            Some(value) => self.set_var(name, value),
            None => {
                if let Some(vars) = self.root.get_mut("vars").and_then(Value::as_object_mut) {
                    vars.remove(name);
                }
            }
        }
    }

    pub fn apply_extracts(&mut self, extract: &IndexMap<String, String>) -> Result<()> {
        for (key, expression) in extract {
            let value = self
                .evaluate_explicit_expr_value(expression)
                .with_context(|| {
                    format!("failed to resolve extract `{key}` from `{expression}`")
                })?;
            self.set_var(key, value);
        }
        Ok(())
    }

    pub fn resolve_value(&self, value: &Value) -> Result<Value> {
        match value {
            Value::String(raw) => self.resolve_string(raw),
            Value::Array(items) => Ok(Value::Array(
                items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        self.resolve_value(item)
                            .with_context(|| format!("failed to resolve array item [{index}]"))
                    })
                    .collect::<Result<Vec<_>>>()?,
            )),
            Value::Object(map) => {
                let mut resolved = Map::new();
                for (key, value) in map {
                    resolved.insert(
                        key.clone(),
                        self.resolve_value(value)
                            .with_context(|| format!("failed to resolve object field `{key}`"))?,
                    );
                }
                Ok(Value::Object(resolved))
            }
            _ => Ok(value.clone()),
        }
    }

    pub fn render_string(&self, raw: &str) -> Result<String> {
        let mut rendered = String::new();
        let mut last = 0usize;
        for captures in self.template_regex.captures_iter(raw) {
            let whole = captures.get(0).expect("whole match");
            let expression = captures.get(1).expect("expression match");
            rendered.push_str(&raw[last..whole.start()]);
            let value = self
                .evaluate_explicit_expr_value(expression.as_str())
                .with_context(|| {
                    format!(
                        "failed to evaluate template expression `{{{{ {} }}}}`",
                        expression.as_str().trim()
                    )
                })?;
            rendered.push_str(&value_to_string(value));
            last = whole.end();
        }
        rendered.push_str(&raw[last..]);
        Ok(rendered)
    }

    pub fn evaluate_condition(&self, expression: &str) -> Result<bool> {
        let raw_expression = expression.trim();
        let expression = strip_wrapped(raw_expression, "${", "}").unwrap_or(raw_expression);
        let value = self
            .evaluate_explicit_expr_value(expression)
            .with_context(|| format!("failed to evaluate condition `{raw_expression}`"))?;
        Ok(truthy(&value))
    }

    pub fn evaluate_expr_value(&self, expression: &str) -> Result<Value> {
        self.evaluate_expr_value_with_mode(expression, ExpressionMode::Lenient)
    }

    pub fn evaluate_explicit_expr_value(&self, expression: &str) -> Result<Value> {
        self.evaluate_expr_value_with_mode(expression, ExpressionMode::Explicit)
    }

    fn evaluate_expr_value_with_mode(
        &self,
        expression: &str,
        mode: ExpressionMode,
    ) -> Result<Value> {
        let expression = expression.trim();
        if expression.is_empty() {
            return Ok(Value::Null);
        }

        for operator in ["==", "!=", ">=", "<=", ">", "<"] {
            if let Some((left, right)) = split_comparison(expression, operator) {
                let left = self
                    .evaluate_expr_value_with_mode(left, mode)
                    .with_context(|| {
                        format!("failed to resolve the left side of `{expression}`")
                    })?;
                let right = self
                    .evaluate_expr_value_with_mode(right, mode)
                    .with_context(|| {
                        format!("failed to resolve the right side of `{expression}`")
                    })?;
                let result = compare_values(&left, &right, operator)
                    .with_context(|| format!("failed to compare `{expression}`"))?;
                return Ok(Value::Bool(result));
            }
        }

        if let Some(inner) = expression
            .strip_prefix("len(")
            .and_then(|value| value.strip_suffix(')'))
        {
            let value = self
                .evaluate_expr_value_with_mode(inner, mode)
                .with_context(|| format!("failed to evaluate len() argument `{inner}`"))?;
            let length = match value {
                Value::Array(items) => items.len(),
                Value::Object(map) => map.len(),
                Value::String(text) => text.chars().count(),
                Value::Null => 0,
                _ => 1,
            };
            return Ok(Value::Number(length.into()));
        }

        if expression.eq_ignore_ascii_case("true") {
            return Ok(Value::Bool(true));
        }
        if expression.eq_ignore_ascii_case("false") {
            return Ok(Value::Bool(false));
        }
        if expression.eq_ignore_ascii_case("null") {
            return Ok(Value::Null);
        }
        if let Some(stripped) = expression
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
        {
            return Ok(Value::String(stripped.to_string()));
        }
        if let Some(stripped) = expression
            .strip_prefix('\'')
            .and_then(|value| value.strip_suffix('\''))
        {
            return Ok(Value::String(stripped.to_string()));
        }
        if let Ok(number) = expression.parse::<i64>() {
            return Ok(json!(number));
        }
        if let Ok(number) = expression.parse::<f64>() {
            return Ok(json!(number));
        }
        if let Some(value) = self.lookup_path(expression) {
            return Ok(value);
        }

        if mode == ExpressionMode::Explicit {
            return self.lookup_path_strict(expression);
        }

        Ok(Value::String(expression.to_string()))
    }

    fn resolve_string(&self, raw: &str) -> Result<Value> {
        if let Some(expression) = strip_wrapped(raw, "${", "}") {
            return self.evaluate_explicit_expr_value(expression);
        }

        if self.template_regex.is_match(raw) {
            return Ok(Value::String(self.render_string(raw)?));
        }

        if self.should_require_strict_bare_reference(raw) {
            return self.lookup_path_strict(raw);
        }

        if self.should_resolve_bare_expression(raw) {
            let resolved = self.evaluate_expr_value(raw)?;
            if !matches!(&resolved, Value::String(text) if text == raw) {
                return Ok(resolved);
            }
        }

        Ok(Value::String(raw.to_string()))
    }

    fn lookup_path(&self, raw: &str) -> Option<Value> {
        let path = raw.trim();
        if path.is_empty() {
            return None;
        }
        let segments = parse_path(path)?;
        let first = match segments.first()? {
            PathSegment::Key(value) => value.clone(),
            PathSegment::Index(_) => return None,
        };
        let mut current = if let Some(value) = self.root.get(&first).cloned() {
            value
        } else {
            self.root
                .get("vars")
                .and_then(Value::as_object)
                .and_then(|vars| vars.get(&first))
                .cloned()?
        };

        for segment in segments.iter().skip(1) {
            current = match segment {
                PathSegment::Key(key) => current.as_object()?.get(key)?.clone(),
                PathSegment::Index(index) => current.as_array()?.get(*index)?.clone(),
            };
        }
        Some(current)
    }

    fn lookup_path_strict(&self, raw: &str) -> Result<Value> {
        let path = raw.trim();
        if path.is_empty() {
            bail!("expression cannot be empty");
        }

        let segments = parse_path(path)
            .ok_or_else(|| anyhow::anyhow!("expression `{path}` is not a valid path"))?;
        let first = match segments.first() {
            Some(PathSegment::Key(value)) => value.clone(),
            Some(PathSegment::Index(index)) => {
                bail!("expression `{path}` cannot start with array index [{index}]")
            }
            None => bail!("expression cannot be empty"),
        };

        let mut current_path = first.clone();
        let mut current = if let Some(value) = self.root.get(&first).cloned() {
            value
        } else if let Some(value) = self.lookup_var(&first) {
            value
        } else {
            let available_roots = self.available_root_keys();
            let available_vars = self.available_var_keys();
            let mut message = format!("expression `{path}` could not resolve `{first}`");
            if !available_roots.is_empty() {
                message.push_str(&format!(
                    "; available roots: [{}]",
                    available_roots.join(", ")
                ));
            }
            if !available_vars.is_empty() {
                message.push_str(&format!(
                    "; available vars: [{}]",
                    available_vars.join(", ")
                ));
            }
            bail!(message);
        };

        for segment in segments.iter().skip(1) {
            match segment {
                PathSegment::Key(key) => {
                    let Some(map) = current.as_object() else {
                        bail!(
                            "expression `{path}` tried to read key `{key}` from `{current_path}`, but `{current_path}` resolved to {}",
                            describe_value_for_error(&current)
                        );
                    };
                    let Some(next) = map.get(key) else {
                        let available_keys = map.keys().take(10).cloned().collect::<Vec<_>>();
                        if available_keys.is_empty() {
                            bail!(
                                "expression `{path}` could not find key `{key}` under `{current_path}`; `{current_path}` is an empty object"
                            );
                        }
                        bail!(
                            "expression `{path}` could not find key `{key}` under `{current_path}`; available keys: [{}]",
                            available_keys.join(", ")
                        );
                    };
                    current = next.clone();
                    current_path.push('.');
                    current_path.push_str(key);
                }
                PathSegment::Index(index) => {
                    let Some(items) = current.as_array() else {
                        bail!(
                            "expression `{path}` tried to read index [{index}] from `{current_path}`, but `{current_path}` resolved to {}",
                            describe_value_for_error(&current)
                        );
                    };
                    let Some(next) = items.get(*index) else {
                        bail!(
                            "expression `{path}` tried to read index [{index}] from `{current_path}`, but the array length is {}",
                            items.len()
                        );
                    };
                    current = next.clone();
                    current_path = format!("{current_path}[{index}]");
                }
            }
        }

        Ok(current)
    }

    fn should_resolve_bare_expression(&self, raw: &str) -> bool {
        let trimmed = raw.trim();
        trimmed.contains('.')
            || trimmed.contains('[')
            || self.root.contains_key(trimmed)
            || self.lookup_var(trimmed).is_some()
    }

    fn should_require_strict_bare_reference(&self, raw: &str) -> bool {
        let trimmed = raw.trim();
        if trimmed.is_empty() || (!trimmed.contains('.') && !trimmed.contains('[')) {
            return false;
        }

        let Some(segments) = parse_path(trimmed) else {
            return false;
        };
        let Some(PathSegment::Key(first)) = segments.first() else {
            return false;
        };

        RESERVED_ROOT_KEYS.iter().any(|key| key == &first.as_str())
            || self.root.contains_key(first)
            || self.lookup_var(first).is_some()
    }

    fn available_root_keys(&self) -> Vec<String> {
        let mut keys = self
            .root
            .keys()
            .map(|key| key.to_string())
            .collect::<Vec<_>>();
        for reserved in RESERVED_ROOT_KEYS {
            if !keys.iter().any(|key| key == reserved) {
                keys.push((*reserved).to_string());
            }
        }
        keys.sort();
        keys
    }

    fn available_var_keys(&self) -> Vec<String> {
        let mut keys = self
            .root
            .get("vars")
            .and_then(Value::as_object)
            .map(|vars| vars.keys().map(|key| key.to_string()).collect::<Vec<_>>())
            .unwrap_or_default();
        keys.sort();
        keys
    }
}

pub fn apply_assertions(assertions: &[Assertion], context: &RuntimeContext) -> Result<()> {
    if let Some(message) = first_failed_assertion(assertions, context)? {
        bail!("{message}");
    }
    Ok(())
}

pub fn assertions_match(assertions: &[Assertion], context: &RuntimeContext) -> Result<bool> {
    Ok(first_failed_assertion(assertions, context)?.is_none())
}

pub fn value_to_string(value: Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(boolean) => boolean.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text,
        other => serde_json::to_string(&other).unwrap_or_else(|_| "null".to_string()),
    }
}

pub(crate) fn split_comparison<'a>(
    expression: &'a str,
    operator: &str,
) -> Option<(&'a str, &'a str)> {
    let mut quoted = false;
    let mut quote_char = '\0';
    let mut index = 0usize;
    while index + operator.len() <= expression.len() {
        let ch = expression[index..].chars().next()?;
        if matches!(ch, '\'' | '"') {
            if !quoted {
                quoted = true;
                quote_char = ch;
            } else if quote_char == ch {
                quoted = false;
            }
        }
        if !quoted && expression[index..].starts_with(operator) {
            let left = expression[..index].trim();
            let right = expression[index + operator.len()..].trim();
            if !left.is_empty() && !right.is_empty() {
                return Some((left, right));
            }
        }
        index += ch.len_utf8();
    }
    None
}

fn first_failed_assertion(
    assertions: &[Assertion],
    context: &RuntimeContext,
) -> Result<Option<String>> {
    for (index, assertion) in assertions.iter().enumerate() {
        if let Some(message) = evaluate_assertion(assertion, context).with_context(|| {
            format!(
                "failed to evaluate assertion #{} `{}`",
                index + 1,
                describe_assertion(assertion)
            )
        })? {
            return Ok(Some(message));
        }
    }
    Ok(None)
}

fn evaluate_assertion(assertion: &Assertion, context: &RuntimeContext) -> Result<Option<String>> {
    match assertion.kind {
        AssertionKind::Eq => binary_assert(assertion, context, "eq", |left, right| {
            Ok(values_equal(&left, &right))
        }),
        AssertionKind::Ne => binary_assert(assertion, context, "ne", |left, right| {
            Ok(!values_equal(&left, &right))
        }),
        AssertionKind::Contains => binary_assert(assertion, context, "contains", contains_value),
        AssertionKind::NotEmpty => unary_assert(assertion, context, "not_empty", |value| {
            !is_empty_value(&value)
        }),
        AssertionKind::Exists => {
            unary_assert(assertion, context, "exists", |value| !value.is_null())
        }
        AssertionKind::Gt => binary_assert(assertion, context, "gt", |left, right| {
            compare_values(&left, &right, ">")
        }),
        AssertionKind::Ge => binary_assert(assertion, context, "ge", |left, right| {
            compare_values(&left, &right, ">=")
        }),
        AssertionKind::Lt => binary_assert(assertion, context, "lt", |left, right| {
            compare_values(&left, &right, "<")
        }),
        AssertionKind::Le => binary_assert(assertion, context, "le", |left, right| {
            compare_values(&left, &right, "<=")
        }),
    }
}

fn unary_assert<F>(
    assertion: &Assertion,
    context: &RuntimeContext,
    name: &str,
    predicate: F,
) -> Result<Option<String>>
where
    F: Fn(Value) -> bool,
{
    if assertion.args.len() != 1 {
        bail!(
            "{name} expects exactly one argument, got {}",
            assertion.args.len()
        );
    }
    let raw = &assertion.args[0];
    let value = resolve_assertion_subject(raw, context, name, "argument")?;
    if predicate(value.clone()) {
        Ok(None)
    } else {
        Ok(Some(format!(
            "assert `{name}` failed: argument `{}` resolved to {}",
            format_assertion_arg(raw),
            serde_json::to_string(&value)?
        )))
    }
}

fn binary_assert<F>(
    assertion: &Assertion,
    context: &RuntimeContext,
    name: &str,
    predicate: F,
) -> Result<Option<String>>
where
    F: Fn(Value, Value) -> Result<bool>,
{
    if assertion.args.len() != 2 {
        bail!(
            "{name} expects exactly two arguments, got {}",
            assertion.args.len()
        );
    }
    let left_raw = &assertion.args[0];
    let right_raw = &assertion.args[1];
    let left = resolve_assertion_subject(left_raw, context, name, "the left argument")?;
    let right = context.resolve_value(right_raw).with_context(|| {
        format!(
            "assert `{name}` could not resolve the right argument `{}`",
            format_assertion_arg(right_raw)
        )
    })?;
    if predicate(left.clone(), right.clone())? {
        Ok(None)
    } else {
        Ok(Some(format!(
            "assert `{name}` failed: left `{}` -> {}, right `{}` -> {}",
            format_assertion_arg(left_raw),
            serde_json::to_string(&left)?,
            format_assertion_arg(right_raw),
            serde_json::to_string(&right)?
        )))
    }
}

fn contains_value(left: Value, right: Value) -> Result<bool> {
    Ok(match left {
        Value::String(text) => text.contains(&value_to_string(right)),
        Value::Array(items) => items.iter().any(|item| values_equal(item, &right)),
        Value::Object(map) => right
            .as_str()
            .map(|candidate| map.contains_key(candidate))
            .unwrap_or(false),
        _ => false,
    })
}

fn compare_values(left: &Value, right: &Value, operator: &str) -> Result<bool> {
    match operator {
        "==" => Ok(values_equal(left, right)),
        "!=" => Ok(!values_equal(left, right)),
        ">" | ">=" | "<" | "<=" => {
            if let (Some(left_number), Some(right_number)) = (as_f64(left), as_f64(right)) {
                return Ok(match operator {
                    ">" => left_number > right_number,
                    ">=" => left_number >= right_number,
                    "<" => left_number < right_number,
                    "<=" => left_number <= right_number,
                    _ => false,
                });
            }
            let left_text = value_to_string(left.clone());
            let right_text = value_to_string(right.clone());
            Ok(match operator {
                ">" => left_text > right_text,
                ">=" => left_text >= right_text,
                "<" => left_text < right_text,
                "<=" => left_text <= right_text,
                _ => false,
            })
        }
        _ => bail!("unsupported comparison operator `{operator}`"),
    }
}

fn values_equal(left: &Value, right: &Value) -> bool {
    if left == right {
        return true;
    }
    match (as_f64(left), as_f64(right)) {
        (Some(left), Some(right)) => (left - right).abs() < f64::EPSILON,
        _ => value_to_string(left.clone()) == value_to_string(right.clone()),
    }
}

fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse::<f64>().ok(),
        _ => None,
    }
}

fn is_empty_value(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Bool(boolean) => *boolean,
        Value::Null => false,
        Value::Number(number) => number.as_f64().map(|value| value != 0.0).unwrap_or(false),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn strip_wrapped<'a>(raw: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    raw.trim()
        .strip_prefix(prefix)?
        .strip_suffix(suffix)
        .map(str::trim)
}

fn describe_assertion(assertion: &Assertion) -> String {
    let name = match assertion.kind {
        AssertionKind::Eq => "eq",
        AssertionKind::Ne => "ne",
        AssertionKind::Contains => "contains",
        AssertionKind::NotEmpty => "not_empty",
        AssertionKind::Exists => "exists",
        AssertionKind::Gt => "gt",
        AssertionKind::Ge => "ge",
        AssertionKind::Lt => "lt",
        AssertionKind::Le => "le",
    };
    let args = assertion
        .args
        .iter()
        .map(format_assertion_arg)
        .collect::<Vec<_>>()
        .join(", ");
    format!("{name}({args})")
}

fn format_assertion_arg(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "<invalid-json>".to_string()),
    }
}

fn resolve_assertion_subject(
    raw: &Value,
    context: &RuntimeContext,
    name: &str,
    label: &str,
) -> Result<Value> {
    match raw {
        Value::String(text) => context.evaluate_explicit_expr_value(text).with_context(|| {
            format!(
                "assert `{name}` could not resolve {label} `{}`",
                format_assertion_arg(raw)
            )
        }),
        _ => context.resolve_value(raw).with_context(|| {
            format!(
                "assert `{name}` could not resolve {label} `{}`",
                format_assertion_arg(raw)
            )
        }),
    }
}

pub(crate) fn describe_value_for_error(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(boolean) => format!("boolean {}", boolean),
        Value::Number(number) => format!("number {}", number),
        Value::String(text) => format!("string {:?}", truncate_for_error(text, 80)),
        Value::Array(items) => format!("array(len={})", items.len()),
        Value::Object(map) => {
            let keys = map.keys().take(6).cloned().collect::<Vec<_>>();
            if keys.is_empty() {
                "empty object".to_string()
            } else {
                format!("object(keys=[{}])", keys.join(", "))
            }
        }
    }
}

fn truncate_for_error(raw: &str, max_chars: usize) -> String {
    let mut chars = raw.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

pub(crate) fn format_error_chain(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(": ")
}

fn parse_path(path: &str) -> Option<Vec<PathSegment>> {
    let mut tokens = Vec::new();
    let mut buffer = String::new();
    let mut chars = path.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '.' => {
                if !buffer.is_empty() {
                    tokens.push(PathSegment::Key(std::mem::take(&mut buffer)));
                }
            }
            '[' => {
                if !buffer.is_empty() {
                    tokens.push(PathSegment::Key(std::mem::take(&mut buffer)));
                }
                let mut index = String::new();
                while let Some(next) = chars.next() {
                    if next == ']' {
                        break;
                    }
                    index.push(next);
                }
                tokens.push(PathSegment::Index(index.parse().ok()?));
            }
            _ => buffer.push(ch),
        }
    }
    if !buffer.is_empty() {
        tokens.push(PathSegment::Key(buffer));
    }
    if tokens.is_empty() {
        None
    } else {
        Some(tokens)
    }
}

#[derive(Debug, Clone)]
enum PathSegment {
    Key(String),
    Index(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expression_evaluator_resolves_templates_paths_and_comparisons() {
        let mut root = Map::new();
        root.insert(
            "response".to_string(),
            json!({
                "status": 201,
                "json": {
                    "items": [
                        { "sku": "SKU-BOOK", "quantity": 2, "unit_price": 4500, "line_total": 9000 },
                        { "sku": "SKU-PEN", "quantity": 1, "unit_price": 1200, "line_total": 1200 }
                    ],
                    "flags": {
                        "has_discount": true
                    },
                    "note": null
                }
            }),
        );
        let mut context = RuntimeContext::new(root).expect("build runtime context");
        context.set_var("subtotal", json!(10200));
        context.set_var("discount", json!(1020));

        assert_eq!(
            context
                .evaluate_expr_value("len(response.json.items)")
                .expect("len expression"),
            json!(2)
        );
        assert_eq!(
            context
                .evaluate_expr_value("subtotal > discount")
                .expect("comparison expression"),
            json!(true)
        );
        assert_eq!(
            context
                .resolve_value(&Value::String("response.json.note".to_string()))
                .expect("null path"),
            Value::Null
        );
        assert_eq!(
            context
                .resolve_value(&Value::String("order {{ response.status }}".to_string()))
                .expect("rendered template"),
            Value::String("order 201".to_string())
        );
        assert!(
            context
                .evaluate_condition("${response.json.flags.has_discount == true}")
                .expect("conditional expression")
        );
    }

    #[test]
    fn split_comparison_ignores_operators_inside_quotes() {
        assert_eq!(
            split_comparison("'vip>standard' == 'vip>standard'", "=="),
            Some(("'vip>standard'", "'vip>standard'"))
        );
        assert_eq!(split_comparison("'vip>standard'", ">"), None);
    }

    #[test]
    fn assertion_failures_include_raw_and_resolved_values() {
        let mut root = Map::new();
        root.insert(
            "response".to_string(),
            json!({
                "status": 201
            }),
        );
        let context = RuntimeContext::new(root).expect("context");
        let assertion = Assertion {
            kind: AssertionKind::Eq,
            args: vec![Value::String("response.status".to_string()), json!(200)],
        };

        let message = first_failed_assertion(&[assertion], &context)
            .expect("assertion evaluation")
            .expect("assertion should fail");

        assert!(message.contains("response.status"));
        assert!(message.contains("201"));
        assert!(message.contains("200"));
    }

    #[test]
    fn explicit_expression_reports_available_context_for_unknown_identifier() {
        let context = RuntimeContext::new(Map::new()).expect("context");

        let error = context
            .evaluate_explicit_expr_value("missing_flag")
            .expect_err("missing identifier must fail");

        let message = format_error_chain(&error);
        assert!(message.contains("expression `missing_flag` could not resolve `missing_flag`"));
        assert!(message.contains("available roots:"));
    }

    #[test]
    fn explicit_expression_reports_missing_key_and_index_details() {
        let mut root = Map::new();
        root.insert(
            "response".to_string(),
            json!({
                "json": {
                    "items": [ { "sku": "SKU-1" } ]
                }
            }),
        );
        let context = RuntimeContext::new(root).expect("context");

        let missing_key = context
            .evaluate_explicit_expr_value("response.json.order_id")
            .expect_err("missing key must fail");
        let missing_key_message = format_error_chain(&missing_key);
        assert!(
            missing_key_message.contains("could not find key `order_id` under `response.json`")
        );
        assert!(missing_key_message.contains("available keys: [items]"));

        let missing_index = context
            .evaluate_explicit_expr_value("response.json.items[3]")
            .expect_err("missing index must fail");
        let missing_index_message = format_error_chain(&missing_index);
        assert!(
            missing_index_message.contains("tried to read index [3] from `response.json.items`")
        );
        assert!(missing_index_message.contains("array length is 1"));
    }

    #[test]
    fn unary_assertions_fail_when_subject_is_unresolved() {
        let context = RuntimeContext::new(Map::new()).expect("context");
        let assertion = Assertion {
            kind: AssertionKind::NotEmpty,
            args: vec![Value::String("order_id".to_string())],
        };

        let error = first_failed_assertion(&[assertion], &context)
            .expect_err("unresolved assertion subject must fail");
        let message = format_error_chain(&error);

        assert!(message.contains("failed to evaluate assertion #1 `not_empty(order_id)`"));
        assert!(message.contains("assert `not_empty` could not resolve argument `order_id`"));
        assert!(message.contains("expression `order_id` could not resolve `order_id`"));
    }

    #[test]
    fn binary_assertions_fail_when_left_subject_is_unresolved() {
        let context = RuntimeContext::new(Map::new()).expect("context");
        let assertion = Assertion {
            kind: AssertionKind::Eq,
            args: vec![Value::String("response.status".to_string()), json!(200)],
        };

        let error = first_failed_assertion(&[assertion], &context)
            .expect_err("unresolved left subject must fail");
        let message = format_error_chain(&error);

        assert!(message.contains("failed to evaluate assertion #1 `eq(response.status, 200)`"));
        assert!(
            message.contains("assert `eq` could not resolve the left argument `response.status`")
        );
        assert!(message.contains("expression `response.status` could not resolve `response`"));
    }
}
