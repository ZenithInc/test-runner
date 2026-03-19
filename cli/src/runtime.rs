use anyhow::{Result, bail};
use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::dsl::{Assertion, AssertionKind};

#[derive(Debug, Clone)]
pub struct RuntimeContext {
    root: Map<String, Value>,
    template_regex: Regex,
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
            self.set_var(key, self.evaluate_expr_value(expression)?);
        }
        Ok(())
    }

    pub fn resolve_value(&self, value: &Value) -> Result<Value> {
        match value {
            Value::String(raw) => self.resolve_string(raw),
            Value::Array(items) => Ok(Value::Array(
                items
                    .iter()
                    .map(|item| self.resolve_value(item))
                    .collect::<Result<Vec<_>>>()?,
            )),
            Value::Object(map) => {
                let mut resolved = Map::new();
                for (key, value) in map {
                    resolved.insert(key.clone(), self.resolve_value(value)?);
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
            let value = self.evaluate_expr_value(expression.as_str())?;
            rendered.push_str(&value_to_string(value));
            last = whole.end();
        }
        rendered.push_str(&raw[last..]);
        Ok(rendered)
    }

    pub fn evaluate_condition(&self, expression: &str) -> Result<bool> {
        Ok(truthy(&self.evaluate_expr_value(
            strip_wrapped(expression, "${", "}").unwrap_or(expression),
        )?))
    }

    pub fn evaluate_expr_value(&self, expression: &str) -> Result<Value> {
        let expression = expression.trim();
        if expression.is_empty() {
            return Ok(Value::Null);
        }

        for operator in ["==", "!=", ">=", "<=", ">", "<"] {
            if let Some((left, right)) = split_comparison(expression, operator) {
                let left = self.evaluate_expr_value(left)?;
                let right = self.evaluate_expr_value(right)?;
                let result = compare_values(&left, &right, operator)?;
                return Ok(Value::Bool(result));
            }
        }

        if let Some(inner) = expression
            .strip_prefix("len(")
            .and_then(|value| value.strip_suffix(')'))
        {
            let value = self.evaluate_expr_value(inner)?;
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
        Ok(Value::String(expression.to_string()))
    }

    fn resolve_string(&self, raw: &str) -> Result<Value> {
        if let Some(expression) = strip_wrapped(raw, "${", "}") {
            return self.evaluate_expr_value(expression);
        }

        if self.template_regex.is_match(raw) {
            return Ok(Value::String(self.render_string(raw)?));
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

    fn should_resolve_bare_expression(&self, raw: &str) -> bool {
        let trimmed = raw.trim();
        trimmed.contains('.')
            || trimmed.contains('[')
            || self.root.contains_key(trimmed)
            || self.lookup_var(trimmed).is_some()
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
    for assertion in assertions {
        if let Some(message) = evaluate_assertion(assertion, context)? {
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
        bail!("{name} expects exactly one argument");
    }
    let value = context.resolve_value(&assertion.args[0])?;
    if predicate(value.clone()) {
        Ok(None)
    } else {
        Ok(Some(format!(
            "assert {name} failed: actual={}",
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
        bail!("{name} expects exactly two arguments");
    }
    let left = context.resolve_value(&assertion.args[0])?;
    let right = context.resolve_value(&assertion.args[1])?;
    if predicate(left.clone(), right.clone())? {
        Ok(None)
    } else {
        Ok(Some(format!(
            "assert {name} failed: left={} right={}",
            serde_json::to_string(&left)?,
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
}
