//! Recognize serde fields even when their empty values are omitted on write.
use std::{cell::RefCell, collections::BTreeSet, rc::Rc};

use serde::{
    Serialize,
    de::{self, DeserializeSeed, IntoDeserializer, Visitor},
};
use serde_json::Value;

type Fields = Rc<RefCell<BTreeSet<Vec<String>>>>;
struct Input {
    value: Value,
    path: Vec<String>,
    fields: Fields,
}

impl Input {
    fn child(&self, name: String, value: Value) -> Self {
        let mut path = self.path.clone();
        path.push(name);
        Self {
            value,
            path,
            fields: self.fields.clone(),
        }
    }
}

impl<'de> de::Deserializer<'de> for Input {
    type Error = serde_json::Error;

    fn deserialize_any<V: Visitor<'de>>(mut self, visitor: V) -> Result<V::Value, Self::Error> {
        match std::mem::take(&mut self.value) {
            Value::Object(values) => visitor.visit_map(Map {
                input: self,
                values: values.into_iter(),
                next: None,
            }),
            Value::Array(values) => visitor.visit_seq(Seq {
                input: self,
                values: values.into_iter(),
                index: 0,
            }),
            value => value.into_deserializer().deserialize_any(visitor),
        }
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        for name in fields {
            let mut path = self.path.clone();
            path.push((*name).to_owned());
            self.fields.borrow_mut().insert(path);
        }
        self.deserialize_any(visitor)
    }
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        if self.value.is_null() {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.value
            .into_deserializer()
            .deserialize_enum(name, variants, visitor)
    }
    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        unit unit_struct seq tuple tuple_struct map identifier ignored_any
    }
}

struct Map {
    input: Input,
    values: serde_json::map::IntoIter,
    next: Option<(String, Value)>,
}
impl<'de> de::MapAccess<'de> for Map {
    type Error = serde_json::Error;
    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        self.next = self.values.next();
        self.next
            .as_ref()
            .map(|(key, _)| seed.deserialize(key.clone().into_deserializer()))
            .transpose()
    }
    fn next_value_seed<V: DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        let (key, value) = self
            .next
            .take()
            .ok_or_else(|| de::Error::custom("missing ownership field"))?;
        seed.deserialize(self.input.child(key, value))
    }
}
struct Seq {
    input: Input,
    values: std::vec::IntoIter<Value>,
    index: usize,
}
impl<'de> de::SeqAccess<'de> for Seq {
    type Error = serde_json::Error;
    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        let Some(value) = self.values.next() else {
            return Ok(None);
        };
        let input = self.input.child(self.index.to_string(), value);
        self.index += 1;
        seed.deserialize(input).map(Some)
    }
}

pub(super) fn known<T: de::DeserializeOwned + Serialize>(
    raw: &Value,
    current: &T,
) -> anyhow::Result<bool> {
    let fields = Fields::default();
    // Use the persisted schema before migrations and the actual serde field
    // descriptors, including aliases. Unknown fields are still detected by
    // comparison; accepting serde's silent ignore alone is not sufficient.
    T::deserialize(Input {
        value: raw.clone(),
        path: Vec::new(),
        fields: fields.clone(),
    })?;
    fn compare(
        raw: &Value,
        known: Option<&Value>,
        fields: &BTreeSet<Vec<String>>,
        path: &mut Vec<String>,
    ) -> bool {
        match raw {
            Value::Object(values) => values.iter().all(|(key, value)| {
                path.push(key.clone());
                let child = known.and_then(|v| v.get(key));
                let matches = (child.is_some() || fields.contains(path))
                    && compare(value, child, fields, path);
                path.pop();
                matches
            }),
            Value::Array(values) => values.iter().enumerate().all(|(index, value)| {
                path.push(index.to_string());
                let matches = compare(value, known.and_then(|v| v.get(index)), fields, path);
                path.pop();
                matches
            }),
            _ => true,
        }
    }
    Ok(compare(
        raw,
        Some(&serde_json::to_value(current)?),
        &fields.borrow(),
        &mut Vec::new(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(serde::Deserialize, Serialize)]
    struct Record {
        #[serde(alias = "legacy_label")]
        label: String,
        nested: Nested,
    }
    #[derive(serde::Deserialize, Serialize)]
    struct Nested {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        checkpoints: Vec<String>,
    }
    #[test]
    fn recognizes_aliases_and_omitted_nested_defaults_without_accepting_unknowns() {
        let raw = serde_json::json!({"legacy_label":"historical", "nested":{"wait":null,"checkpoints":[]}});
        let current: Record = serde_json::from_value(raw.clone()).unwrap();
        assert!(known(&raw, &current).unwrap());
        for value in [
            Value::Null,
            serde_json::json!([]),
            serde_json::json!({}),
            serde_json::json!({"source":"keep"}),
        ] {
            let mut unknown = raw.clone();
            unknown["nested"]["future_authority"] = value;
            assert!(!known(&unknown, &current).unwrap());
        }
    }
}
