//! D1 / JsValue 绑定辅助。
//!
//! worker 0.8 的 `D1PreparedStatement::bind` 收 `&[JsValue]`，所以每种可能的
//! 列类型都要显式转成 JsValue。这里把转换集中起来，避免到处写 `.into()`。

use serde::Serialize;
use worker::wasm_bindgen::JsValue;
use worker::Result;

pub trait BindValue {
    fn to_js(&self) -> Result<JsValue>;
}

impl BindValue for String {
    fn to_js(&self) -> Result<JsValue> {
        Ok(JsValue::from_str(self))
    }
}

impl BindValue for &str {
    fn to_js(&self) -> Result<JsValue> {
        Ok(JsValue::from_str(self))
    }
}

impl BindValue for i64 {
    fn to_js(&self) -> Result<JsValue> {
        Ok(JsValue::from_f64(*self as f64))
    }
}

impl BindValue for usize {
    fn to_js(&self) -> Result<JsValue> {
        Ok(JsValue::from_f64(*self as f64))
    }
}

impl BindValue for bool {
    fn to_js(&self) -> Result<JsValue> {
        Ok(JsValue::from_bool(*self))
    }
}

impl BindValue for JsValue {
    fn to_js(&self) -> Result<JsValue> {
        Ok(self.clone())
    }
}

/// `None` → JS null（写入 NULL），`Some(v)` → 对应类型。
impl<T: BindValue> BindValue for Option<T> {
    fn to_js(&self) -> Result<JsValue> {
        match self {
            Some(v) => v.to_js(),
            None => Ok(JsValue::NULL),
        }
    }
}

/// 把一组值打包成 `bind()` 需要的切片。
///
/// 参数类型写成 `&[&dyn BindValue]` 而不是泛型，这样字符串字面量
/// （`&"offline"`）能在调用点自动做 unsize coercion，不必到处写 `.to_string()`。
pub fn params(values: &[&dyn BindValue]) -> Result<Vec<JsValue>> {
    values.iter().map(|v| v.to_js()).collect()
}

/// 便于从已经算好的 JsValue 向量直接绑定。
pub fn raw_params(values: Vec<JsValue>) -> Vec<JsValue> {
    values
}

/// 毫秒时间戳（本项目所有时间列统一用它）。
pub fn now_ms() -> i64 {
    worker::Date::now().as_millis() as i64
}

/// 把 serde 值序列化成 compact JSON 字符串，用于 raw_json 列。
pub fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// 从 JSON 里安全地取字符串数组的第 0 个元素，用于解析 tags 之类。
pub fn json_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

pub fn json_i64(v: &serde_json::Value, key: &str) -> Option<i64> {
    v.get(key).and_then(|x| x.as_i64())
}
